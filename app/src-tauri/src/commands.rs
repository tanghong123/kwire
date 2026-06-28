//! Tauri commands: the IPC surface the front end calls via `invoke(...)`.
//!
//! Each command is a thin, JSON-friendly wrapper over the engine. The app keeps
//! a [`Library`] of one [`Orchestrator`] per persisted list (the multi-list
//! sidebar), all backed by one on-disk SQLite database so state survives a
//! relaunch. A single shared [`Scheduler`] drives downloads for every list, so
//! the lifecycle commands (pause/cancel/resume) can reach in-flight work.
//! Errors are surfaced to JS as `Err(String)` (Tauri rejects the promise).

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, State};
use tokio::sync::Mutex;

use libgen_core::model::{DownloadList, Format, Goal, RequestStatus};
use libgen_core::orchestrator::Orchestrator;
use libgen_core::parse;
use libgen_core::queue::{Progress, Scheduler};
use libgen_core::search::MirrorConfig;
use libgen_core::series::SeriesClient;
use libgen_core::slum::SlumClient;
use reqwest::Client;

use crate::bridge;
use crate::state::{AppSettings, AppState, Config, Library, LoadedList};
use crate::viewmodel::{self, ViewAppConfig, ViewLibrary, ViewSiteHealth};

// Functions moved to the engine crate — re-export the public ones so integration
// tests that import `libgen_app_lib::commands::reconcile_completed_inflight` etc.
// continue to work unchanged.
pub use libgen_engine::{
    build_scheduler, build_search, ensure_scheduler_from, open_store, reconcile_completed_inflight,
    RECONCILE_MAX_ATTEMPTS,
};

/// Convert any engine error into the `String` Tauri hands back to JS.
fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Parse-only preview for the Import sheet: no persistence, no network.
#[tauri::command]
pub fn parse_preview(text: String, is_json: bool) -> Result<DownloadList, String> {
    parse::parse_auto(&text, is_json).map_err(err)
}

/// Project the whole [`Library`] into the multi-list [`ViewLibrary`] the UI
/// renders (sidebar + "All downloads" aggregate). Each orchestrator is snapshotted
/// from its store, so freshly-persisted job/lifecycle state is reflected.
///
/// The library guard is NOT held here: the caller clones `(current, arcs)` under a
/// brief library lock, drops it, then calls this — so each orchestrator is
/// snapshotted under its OWN brief lock without holding the library lock across
/// (or contending it from under) a per-orch lock. `docs/EXECUTION_MODEL.md` §A.
async fn build_library_from(
    current: String,
    arcs: Vec<(String, Arc<Mutex<Orchestrator>>)>,
    cfg: &Config,
) -> Result<ViewLibrary, String> {
    let mut lists = Vec::with_capacity(arcs.len());
    for (id, orch) in &arcs {
        let snap = {
            let guard = orch.lock().await;
            guard.snapshot().map_err(err)?
        };
        lists.push(viewmodel::build_with_id(id.clone(), &snap));
    }
    Ok(ViewLibrary {
        lists,
        current,
        config: app_config_view(cfg),
        search_mirrors: search_mirror_hosts(cfg),
    })
}

/// Refresh the whole library view for `state`: brief library lock to clone
/// `(current, arcs)`, drop it, snapshot each orchestrator under its own brief
/// lock, and project. The config lock is a `std::sync::Mutex` held only to read
/// the global app settings, never across an `.await`.
async fn refresh_library(state: &AppState) -> Result<ViewLibrary, String> {
    let (current, arcs) = {
        let lib = state.library.lock().await;
        (lib.current.clone(), lib.all_arcs())
    };
    let cfg = state.config.lock().expect("config mutex poisoned").clone();
    build_library_from(current, arcs, &cfg).await
}

/// Project the global app settings into the JSON shape the Settings sheet reads.
fn app_config_view(cfg: &Config) -> ViewAppConfig {
    let a = &cfg.app;
    ViewAppConfig {
        out_dir: cfg.effective_out_dir().to_string_lossy().into_owned(),
        max_concurrent_downloads: a.max_concurrent_downloads,
        query_concurrency: a.query_concurrency,
        max_attempts: a.max_attempts,
        hedge_enabled: a.hedge_enabled,
    }
}

/// Best-effort read of the configured search-mirror hosts (read-only surfacing;
/// `mirrors.toml` is hand-edited). Empty on any load error.
fn search_mirror_hosts(cfg: &Config) -> Vec<String> {
    MirrorConfig::load(&cfg.mirrors)
        .map(|m| m.search_mirrors.iter().map(|s| s.host.clone()).collect())
        .unwrap_or_default()
}

/// Return the current multi-list library projection (no engine work).
#[tauri::command]
pub async fn library(state: State<'_, AppState>) -> Result<ViewLibrary, String> {
    refresh_library(&state).await
}

/// Switch the active list (a list id, or `"__all__"` for the aggregate).
#[tauri::command]
pub async fn select_list(
    state: State<'_, AppState>,
    list_id: String,
) -> Result<ViewLibrary, String> {
    {
        let mut lib = state.library.lock().await;
        lib.current = list_id;
    }
    refresh_library(&state).await
}

/// Parse + persist a NEW list into the shared database, add a fresh orchestrator
/// for it, select it, and return the refreshed library. Unlike before, this is
/// ADDITIVE — existing lists stay loaded (the multi-list sidebar).
#[tauri::command]
pub async fn load_list(
    state: State<'_, AppState>,
    text: String,
    is_json: bool,
) -> Result<ViewLibrary, String> {
    let list = parse::parse_auto(&text, is_json).map_err(err)?;
    let cfg = state.config.lock().expect("config mutex poisoned").clone();

    let mut store = open_store(&cfg)?;
    let search = build_search(&cfg)?;
    // REJECT a duplicate-titled import. Replacing or merging an existing list risks
    // silently dropping books that are in the current list but absent from the new
    // file (along with their metadata). To re-import, Remove the existing list first
    // (right-click the list → Remove list), or rename your list.
    if store.list_id_by_title(&list.title).map_err(err)?.is_some() {
        return Err(libgen_core::model::ui_msg(
            "err.list_exists",
            &[("name", &list.title)],
        ));
    }
    let store_id = store.insert_list(&list).map_err(err)?;
    // Attach a fresh orchestrator to the new persisted list.
    let store2 = open_store(&cfg)?;
    let orch = Orchestrator::attach(store2, store_id, search, cfg.effective_out_dir())
        .with_query_concurrency(cfg.app.query_concurrency);
    let id = Library::id_for(store_id);

    {
        let mut lib = state.library.lock().await;
        lib.lists.retain(|l| l.id != id);
        lib.current = id.clone();
        lib.lists.push(LoadedList::new(id.clone(), orch));
    }
    // Import = implicit Start (goal=Complete): kick discovery + downloads.
    set_goal_for(&state, &id, Goal::Complete).await?;
    state.wake_engine();
    refresh_library(&state).await
}

/// The title of the singleton mutable list.
const MANUAL_LIST_TITLE: &str = "Manual";

/// The naming template the Manual list uses: NO leading sequence number (the
/// user curates books individually, so a positional number is meaningless). The
/// 6-hex md5 tag is appended by the naming code as a suffix (see
/// `naming::filename`), so it is NOT a template token — the result is
/// `Author - Title - <md5:6>.ext`.
const MANUAL_NAMING_TEMPLATE: &str = "{authors} - {title}.{ext}";

/// **Add a book to the mutable Manual list** (the UI's manual-add). Finds — or
/// creates — the singleton list titled `"Manual"` (with `is_manual = true` and a
/// no-seq naming template), appends one new book from `title`/`author`, drives it
/// (goal = `Complete`) so the engine queries + downloads it, and returns the
/// refreshed library. Rejects an empty title.
#[tauri::command]
pub async fn add_manual_book(
    state: State<'_, AppState>,
    title: String,
    author: Option<String>,
) -> Result<ViewLibrary, String> {
    add_manual_book_inner(&state, title, author).await
}

/// Testable core of [`add_manual_book`] — operates on `&AppState` directly (no
/// Tauri `State` wrapper) so integration tests can exercise the find-or-create +
/// append path headlessly.
pub(crate) async fn add_manual_book_inner(
    state: &AppState,
    title: String,
    author: Option<String>,
) -> Result<ViewLibrary, String> {
    if title.trim().is_empty() {
        return Err(libgen_core::model::ui_msg("err.empty_title", &[]));
    }
    let authors: Vec<String> = author
        .unwrap_or_default()
        .split(',')
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect();

    // Find-or-create the singleton Manual list, returning its loaded orchestrator.
    let id = ensure_manual_list(state).await?;
    let orch = {
        let lib = state.library.lock().await;
        lib.arc_for(&id)
            .ok_or_else(|| format!("list {id} not loaded"))?
    };
    {
        let mut guard = orch.lock().await;
        // No author given → treat the whole title field as a FREE-FORM "title author"
        // query (matches the CLI/TUI default). An explicit author opts into a
        // structured title-vs-author match.
        let (group_path, book_index) = if authors.is_empty() {
            guard.add_book_freeform(title.trim()).map_err(err)?
        } else {
            guard.add_book(&title, authors).map_err(err)?
        };
        // Drive the new book to completion (discover + download).
        guard
            .set_goal_one(&group_path, book_index, Goal::Complete)
            .map_err(err)?;
    }
    state.wake_engine();
    refresh_library(state).await
}

/// Find the singleton **Manual** list (by title) and ensure it's loaded with an
/// attached orchestrator, creating + persisting it on first use exactly like
/// `load_list` does (insert list + attach orch + `state.library` insert + goal
/// Complete + wake). Returns its loaded UI id. Idempotent: reuses an existing
/// Manual list's store id + loaded orchestrator.
async fn ensure_manual_list(state: &AppState) -> Result<String, String> {
    let cfg = state.config.lock().expect("config mutex poisoned").clone();
    let mut store = open_store(&cfg)?;

    // Reuse the persisted Manual list if it exists; otherwise create it.
    let store_id = match store.list_id_by_title(MANUAL_LIST_TITLE).map_err(err)? {
        Some(existing) => existing,
        None => {
            let settings = libgen_core::model::ListSettings {
                naming_template: MANUAL_NAMING_TEMPLATE.to_string(),
                is_manual: true,
                ..Default::default()
            };
            let list = DownloadList {
                title: MANUAL_LIST_TITLE.to_string(),
                settings,
                // A single root group holds the manually-added books (flat).
                groups: vec![libgen_core::model::Group::new(MANUAL_LIST_TITLE)],
            };
            store.insert_list(&list).map_err(err)?
        }
    };
    let id = Library::id_for(store_id);

    // Already loaded? Reuse the loaded orchestrator.
    {
        let lib = state.library.lock().await;
        if lib.arc_for(&id).is_some() {
            return Ok(id);
        }
    }

    // Attach a fresh orchestrator to the persisted Manual list (mirrors load_list).
    let search = build_search(&cfg)?;
    let store2 = open_store(&cfg)?;
    let orch = Orchestrator::attach(store2, store_id, search, cfg.effective_out_dir())
        .with_query_concurrency(cfg.app.query_concurrency);
    {
        let mut lib = state.library.lock().await;
        // Re-check under the lock (another task may have inserted it meanwhile).
        if lib.arc_for(&id).is_none() {
            lib.lists.push(LoadedList::new(id.clone(), orch));
        }
    }
    set_goal_for(state, &id, Goal::Complete).await?;
    state.wake_engine();
    Ok(id)
}

/// **Remove a book from a mutable list** (the Manual list's per-book remove).
/// Errors unless that list's `settings.is_manual` is true (imported lists stay
/// immutable). Aborts any in-flight downloads for the book first, then deletes
/// the book's tracking from the store. Downloaded FILES on disk are left
/// untouched (consistent with Remove-list / Remove-download? — no: like
/// Remove-list, files are kept).
#[tauri::command]
pub async fn remove_book(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
) -> Result<ViewLibrary, String> {
    remove_book_inner(&state, list_id, book_id).await
}

/// Testable core of [`remove_book`] — operates on `&AppState` directly so an
/// integration test can verify the `is_manual` guard + removal headlessly.
pub(crate) async fn remove_book_inner(
    state: &AppState,
    list_id: Option<String>,
    book_id: String,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(state, list_id).await?;
    // Resolve position + guard mutability + snapshot the book's in-flight md5s
    // under a brief orch lock, then signal the scheduler SEPARATELY (no nested
    // locks — `docs/SYNCHRONIZATION.md` §5), then delete the tracking.
    let inflight = {
        let mut guard = orch.lock().await;
        let pos = position_for(&guard, &book_id)?;
        let snap = guard.snapshot().map_err(err)?;
        if !snap.settings.is_manual {
            return Err(
                "This list is read-only — only the Manual list lets you remove books.".to_string(),
            );
        }
        let to_cancel = book_at(&snap, &pos.group_path, pos.book_index)
            .map(|b| {
                use libgen_core::model::JobState;
                b.candidates
                    .iter()
                    .filter(|c| {
                        matches!(
                            c.job.as_ref().map(|j| &j.state),
                            Some(JobState::Resolving | JobState::Downloading | JobState::Verifying)
                        )
                    })
                    .map(|c| c.md5.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        guard
            .remove_book(&pos.group_path, pos.book_index)
            .map_err(err)?;
        to_cancel
    };
    if !inflight.is_empty() {
        if let Ok(scheduler) = ensure_scheduler(state, None).await {
            for md5 in &inflight {
                scheduler.cancel(md5).await;
            }
        }
    }
    state.wake_engine();
    refresh_library(state).await
}

// ---------------------------------------------------------------------------
// Goal-setters: the UI's Start / Stop / Re-query controls. Each takes a BRIEF
// lock to mutate goal/state + persist, notifies the engine, and returns the
// refreshed library IMMEDIATELY (it does NOT wait for the engine's network work).
// `docs/EXECUTION_MODEL.md` §5/§D. The engine then drives each book current→goal.
// ---------------------------------------------------------------------------

/// Set the execution goal for EVERY book in a list (brief per-orch lock), persist
/// it, and wake the engine. The internal primitive behind start/stop. Errors if
/// the list isn't loaded.
async fn set_goal_for(state: &AppState, id: &str, goal: Goal) -> Result<(), String> {
    let orch = {
        let lib = state.library.lock().await;
        lib.arc_for(id)
            .ok_or_else(|| format!("list {id} not loaded"))?
    };
    {
        let mut guard = orch.lock().await;
        guard.set_goal_all(goal).map_err(err)?;
    }
    state.wake_engine();
    Ok(())
}

/// **Start** (per-list): set goal = `Complete` for every book in the list, so the
/// engine discovers AND downloads. Defaults to the active list. Returns the
/// refreshed library immediately; the engine drives the work in the background.
#[tauri::command]
pub async fn start(
    state: State<'_, AppState>,
    list_id: Option<String>,
) -> Result<ViewLibrary, String> {
    let id = {
        let lib = state.library.lock().await;
        active_id(&lib, list_id)?
    };
    set_goal_for(&state, &id, Goal::Complete).await?;
    tracing::info!(list = %id, "list start: goal → Complete (engine will pursue downloads)");
    refresh_library(&state).await
}

/// **Stop** (per-list): set goal = `Idle` for every book in the list. In-flight
/// downloads pause via the scheduler; the engine stops planning new work for the
/// list. Defaults to the active list.
#[tauri::command]
pub async fn stop(
    state: State<'_, AppState>,
    list_id: Option<String>,
) -> Result<ViewLibrary, String> {
    let id = {
        let lib = state.library.lock().await;
        active_id(&lib, list_id)?
    };
    let orch = {
        let lib = state.library.lock().await;
        lib.arc_for(&id)
            .ok_or_else(|| format!("list {id} not loaded"))?
    };
    // Flip intent (goal → Idle) AND snapshot the in-flight md5s under the orch lock,
    // then signal the scheduler SEPARATELY (no nested locks — `docs/SYNCHRONIZATION.md`
    // §3/§5). Stop = pause-keep-partial so a later Start resumes from the `.part`.
    let inflight = {
        let mut guard = orch.lock().await;
        guard.set_goal_all(Goal::Idle).map_err(err)?;
        let snap = guard.snapshot().map_err(err)?;
        inflight_md5s_in(&snap, &|_| true)
    };
    if !inflight.is_empty() {
        if let Ok(scheduler) = ensure_scheduler(&state, None).await {
            for md5 in &inflight {
                scheduler.pause(md5).await;
            }
        }
    }
    // Observability: how many in-flight downloads this Stop actually paused. If a
    // list's dot stays green after Stop with `inflight_paused=0`, the activity is a
    // shared-md5 "free ride" driven by ANOTHER (still-active) list, not this one.
    tracing::info!(list = %id, inflight_paused = inflight.len(), "list stop: goal → Idle");
    state.wake_engine();
    refresh_library(&state).await
}

/// **Remove a list**: delete it (and its whole tree) from the store and drop its
/// orchestrator. Pauses any in-flight downloads for it first so they don't keep
/// running for a gone list. Downloaded FILES on disk are left untouched.
#[tauri::command]
pub async fn delete_list(
    state: State<'_, AppState>,
    list_id: Option<String>,
) -> Result<ViewLibrary, String> {
    let id = {
        let lib = state.library.lock().await;
        active_id(&lib, list_id)?
    };
    let store_id: i64 = id
        .strip_prefix("list")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad list id {id}"))?;

    // Pause any in-flight downloads for this list before deleting (no orphan transfers).
    if let Some(orch) = {
        let lib = state.library.lock().await;
        lib.arc_for(&id)
    } {
        let inflight = {
            let guard = orch.lock().await;
            guard
                .snapshot()
                .map(|snap| inflight_md5s_in(&snap, &|_| true))
                .unwrap_or_default()
        };
        if !inflight.is_empty() {
            if let Ok(scheduler) = ensure_scheduler(&state, None).await {
                for md5 in &inflight {
                    scheduler.pause(md5).await;
                }
            }
        }
    }

    let cfg = state.config.lock().expect("config mutex poisoned").clone();
    let mut store = open_store(&cfg)?;
    store.delete_list(store_id).map_err(err)?;
    {
        let mut lib = state.library.lock().await;
        lib.lists.retain(|l| l.id != id);
        if lib.current == id {
            lib.current = "__all__".to_string();
        }
    }
    tracing::info!(list = %id, "list removed (deleted from store; files kept)");
    state.wake_engine();
    refresh_library(&state).await
}

/// **Start downloading for all lists** (global): set goal = `Complete` for every
/// book of EVERY loaded list — a fan-out of the per-list Start.
#[tauri::command]
pub async fn start_all(state: State<'_, AppState>) -> Result<ViewLibrary, String> {
    let arcs = {
        let lib = state.library.lock().await;
        lib.all_arcs()
    };
    for (_, orch) in &arcs {
        let mut guard = orch.lock().await;
        guard.set_goal_all(Goal::Complete).map_err(err)?;
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// **Stop all** (global): set goal = `Idle` for every book of EVERY loaded list.
#[tauri::command]
pub async fn stop_all(state: State<'_, AppState>) -> Result<ViewLibrary, String> {
    let arcs = {
        let lib = state.library.lock().await;
        lib.all_arcs()
    };
    for (_, orch) in &arcs {
        let mut guard = orch.lock().await;
        guard.set_goal_all(Goal::Idle).map_err(err)?;
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// Back-compat alias the legacy UI may still call: behaves like per-list
/// **Start** (goal = Complete, then the engine discovers + downloads). Discovery
/// alone is no longer a separate command (the engine drives current→goal).
#[tauri::command]
pub async fn query_and_match(
    state: State<'_, AppState>,
    list_id: Option<String>,
) -> Result<ViewLibrary, String> {
    start(state, list_id).await
}

/// **Re-query** (per-list): re-discover with the CURRENT algorithm AND finish the
/// downloads ("push this list to completion"). For non-`Done` books: status →
/// `Queued` + candidates cleared (so the engine re-discovers them). `Done` books
/// are left intact and re-verified by the engine. Then goal = `Complete` for the
/// whole list + wake the engine. Returns the refreshed library immediately.
///
/// Strictly scoped to one explicitly-selected list (never the `__all__`
/// aggregate); the UI disables the action unless a single list is selected.
#[tauri::command]
pub async fn requery(
    state: State<'_, AppState>,
    list_id: Option<String>,
) -> Result<ViewLibrary, String> {
    let id = match list_id {
        Some(s) if !s.is_empty() && s != "__all__" => s,
        _ => return Err(libgen_core::model::ui_msg("err.select_list_requery", &[])),
    };
    let orch = {
        let lib = state.library.lock().await;
        lib.arc_for(&id)
            .ok_or_else(|| format!("list {id} not loaded"))?
    };
    // Re-query of a NON-`Done` book is a HARD cancel of any in-flight transfer
    // (the partial is discarded — `docs/SYNCHRONIZATION.md` §5). Snapshot those
    // md5s under the orch lock, flip intent, then signal the scheduler SEPARATELY
    // (no nested locks). A `Done` book's copy is never in flight (it's settled), so
    // `accept` excludes it — its re-verify keeps the file.
    let inflight = {
        let mut guard = orch.lock().await;
        let pre = guard.snapshot().map_err(err)?;
        let to_cancel = inflight_md5s_in(&pre, &|r| r.status != RequestStatus::Done);
        // Rewind not-yet-acquired books to Queued (clears stale candidates) so the
        // engine re-discovers them; Done books are kept for re-verify.
        guard.requery_reset().map_err(err)?;
        // Push the whole list to completion (re-discover + download + re-verify).
        guard.set_goal_all(Goal::Complete).map_err(err)?;
        to_cancel
    };
    if !inflight.is_empty() {
        if let Ok(scheduler) = ensure_scheduler(&state, None).await {
            for md5 in &inflight {
                scheduler.cancel(md5).await;
            }
        }
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// Replace a downloaded book's copy with the recommended one: enrol the
/// `recommended_md5` variation for download (Pending) and record the old file to
/// move to Trash once the new copy finishes (see
/// [`Orchestrator::replace_download`] and [`Orchestrator::trash_after_replace_done`],
/// which `start_downloads` invokes when it observes the recommended copy reaching
/// `Done`). The UI's "Replace with recommended" action. Returns the refreshed
/// library; the caller follows with `start_downloads` to fetch the new copy.
#[tauri::command]
pub async fn replace_download(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
    recommended_md5: String,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(&state, list_id).await?;
    // Replace abates any in-flight transfer on the affected book (its connection +
    // `.part`) before enrolling the recommended copy — `docs/SYNCHRONIZATION.md` §5.
    // Snapshot the in-flight md5s for that book under the orch lock, mutate state,
    // then signal the scheduler SEPARATELY (no nested locks).
    let inflight = {
        let mut guard = orch.lock().await;
        let pos = variation_for(&guard, &book_id, &recommended_md5)?;
        let pre = guard.snapshot().map_err(err)?;
        // Only THIS book's in-flight variations (scoped by tree position).
        let to_cancel = book_at(&pre, &pos.group_path, pos.book_index)
            .map(|b| {
                use libgen_core::model::JobState;
                b.candidates
                    .iter()
                    .filter(|c| {
                        matches!(
                            c.job.as_ref().map(|j| &j.state),
                            Some(JobState::Resolving | JobState::Downloading | JobState::Verifying)
                        )
                    })
                    .map(|c| c.md5.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        guard
            .replace_download(&pos.group_path, pos.book_index, &pos.md5)
            .map_err(err)?;
        // The replacement is now a Pending recommended copy: drive it (goal =
        // Complete) so the engine downloads it and trashes the old file on success.
        guard
            .set_goal_one(&pos.group_path, pos.book_index, Goal::Complete)
            .map_err(err)?;
        to_cancel
    };
    if !inflight.is_empty() {
        if let Ok(scheduler) = ensure_scheduler(&state, None).await {
            for md5 in &inflight {
                scheduler.cancel(md5).await;
            }
        }
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// Manually remove a downloaded variation: move its file to Trash, clear its
/// download state, and re-evaluate the book's status. Aborts any in-flight
/// transfer for that md5 first (no nested locks: mutate under the orch lock, then
/// signal the scheduler separately — `docs/SYNCHRONIZATION.md` §5).
#[tauri::command]
pub async fn remove_download(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
    md5: String,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(&state, list_id).await?;
    {
        let mut guard = orch.lock().await;
        let pos = variation_for(&guard, &book_id, &md5)?;
        guard
            .remove_variation(&pos.group_path, pos.book_index, &pos.md5)
            .map_err(err)?;
    }
    if let Ok(scheduler) = ensure_scheduler(&state, None).await {
        scheduler.cancel(&md5).await;
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// Pick a specific candidate (by md5) for a book, transitioning it to ready. Under
/// the engine this is the resolution of a `NeedsSelection` book → `Matched`; if
/// the book's goal is `Complete` the engine then downloads it.
#[tauri::command]
pub async fn select_candidate(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
    md5: String,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(&state, list_id).await?;
    // Resolve position + title under a brief lock, RELEASED before the cross-list
    // scan (which locks the other orchestrators — never nest).
    let (group_path, book_index, my_title) = {
        let guard = orch.lock().await;
        let pos = position_for(&guard, &book_id)?;
        let snap = guard.snapshot().map_err(err)?;
        let title = book_at(&snap, &pos.group_path, pos.book_index)
            .map(|b| b.input.title.clone())
            .unwrap_or_default();
        (pos.group_path, pos.book_index, title)
    };
    // Guard: refuse to commit one file to two differently-titled books.
    if let Some(other) = conflicting_claim(&state, &md5, &my_title).await {
        return Err(format!(
            "This file is already chosen for “{other}”. The same file can't also be \
             “{my_title}” — pick a different copy for this book."
        ));
    }
    {
        let mut guard = orch.lock().await;
        guard
            .select_candidate(&group_path, book_index, &md5)
            .map_err(err)?;
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// Mark a `NeedsSelection` book as not-found by user choice — it moves to the
/// "Cannot download" list. No engine work; just a state change.
#[tauri::command]
pub async fn mark_not_found(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(&state, list_id).await?;
    {
        let mut guard = orch.lock().await;
        let pos = position_for(&guard, &book_id)?;
        guard
            .mark_not_found(&pos.group_path, pos.book_index)
            .map_err(err)?;
    }
    refresh_library(&state).await
}

/// Re-queue a failed/not-found book so the engine re-discovers it.
#[tauri::command]
pub async fn retry(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(&state, list_id).await?;
    {
        let mut guard = orch.lock().await;
        let pos = position_for(&guard, &book_id)?;
        guard.retry(&pos.group_path, pos.book_index).map_err(err)?;
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// Correct a book's title/author and re-query it. `author` is a free-text string
/// (comma-separated authors); empty entries are dropped. For a not-found book the
/// search couldn't match under its imported title. Resets the book + re-discovers.
#[tauri::command]
pub async fn edit_book(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
    title: String,
    author: String,
) -> Result<ViewLibrary, String> {
    let authors: Vec<String> = author
        .split(',')
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect();
    let orch = resolve_arc(&state, list_id).await?;
    {
        let mut guard = orch.lock().await;
        let pos = position_for(&guard, &book_id)?;
        guard
            .edit_book_input(&pos.group_path, pos.book_index, &title, authors)
            .map_err(err)?;
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// Return a locally-cached cover thumbnail as a `data:image/jpeg;base64,…` URL so
/// the webview can show it WITHOUT relying on the asset protocol (which is
/// scope/CSP-sensitive and was unreliable). Restricted to `.jpg` files under the
/// configured output dir. Returns an error string the UI swallows (→ placeholder).
#[tauri::command]
pub fn cover_data_url(state: State<'_, AppState>, path: String) -> Result<String, String> {
    use base64::Engine;
    let p = std::path::PathBuf::from(&path);
    let out = state
        .config
        .lock()
        .expect("config mutex poisoned")
        .effective_out_dir();
    // Safety: only serve cached thumbnails under the output dir.
    if !p.starts_with(&out) || p.extension().and_then(|e| e.to_str()) != Some("jpg") {
        return Err("not an allowed cover path".into());
    }
    match std::fs::read(&p) {
        Ok(bytes) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(format!("data:image/jpeg;base64,{b64}"))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Accept the currently-downloaded copy of a book under review ("check download"),
/// clearing the review flag so it settles as Done without replacing the file.
#[tauri::command]
pub async fn accept_download(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(&state, list_id).await?;
    {
        let mut guard = orch.lock().await;
        let pos = position_for(&guard, &book_id)?;
        guard
            .accept_review(&pos.group_path, pos.book_index)
            .map_err(err)?;
    }
    refresh_library(&state).await
}

/// Mark a single **variation** (by md5) of a book for download. The engine
/// fetches every requested (Pending) variation, so this is how the UI's
/// per-variation "Download"/"Retry"/"Re-download" buttons enrol a specific copy
/// without disturbing the book's other variations.
#[tauri::command]
pub async fn request_variation(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
    md5: String,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(&state, list_id).await?;
    // Resolve the position + this book's title under a brief lock, RELEASED before
    // the cross-list scan (which locks the other orchestrators — never nest).
    let (group_path, book_index, md5_resolved, my_title) = {
        let guard = orch.lock().await;
        let pos = variation_for(&guard, &book_id, &md5)?;
        let snap = guard.snapshot().map_err(err)?;
        let title = book_at(&snap, &pos.group_path, pos.book_index)
            .map(|b| b.input.title.clone())
            .unwrap_or_default();
        (pos.group_path, pos.book_index, pos.md5, title)
    };
    // Guard: refuse to commit one file to two differently-titled books.
    if let Some(other) = conflicting_claim(&state, &md5_resolved, &my_title).await {
        return Err(format!(
            "This file is already chosen for “{other}”. The same file can't also be \
             “{my_title}” — pick a different copy for this book."
        ));
    }
    {
        let mut guard = orch.lock().await;
        guard
            .request_variation(&group_path, book_index, &md5_resolved)
            .map_err(err)?;
        // Enrolling a variation expresses intent to download it: ensure the book's
        // goal is Complete so the engine actually fetches it.
        guard
            .set_goal_one(&group_path, book_index, Goal::Complete)
            .map_err(err)?;
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// Extract a 32-char md5 from a bare md5 OR a libgen URL (`ads.php?md5=…`,
/// `file.php?md5=…`, `/md5/<hex>`). Returns the first EXACTLY-32 hex run.
fn extract_md5(input: &str) -> Option<String> {
    let s = input.trim();
    let b = s.as_bytes();
    let mut run = 0usize;
    let mut start = 0usize;
    for (i, &c) in b.iter().enumerate() {
        if c.is_ascii_hexdigit() {
            if run == 0 {
                start = i;
            }
            run += 1;
            if run == 32 && (i + 1 >= b.len() || !b[i + 1].is_ascii_hexdigit()) {
                return Some(s[start..=i].to_ascii_lowercase());
            }
        } else {
            run = 0;
        }
    }
    None
}

/// The UI's "Enter manually" for a cannot-download book: accept a user-supplied
/// md5 (or libgen URL carrying one), inject it as a manual candidate, and drive
/// the book to download it. Errors if no 32-char md5 is present in the input.
#[tauri::command]
pub async fn add_manual_download(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
    input: String,
) -> Result<ViewLibrary, String> {
    let md5 = extract_md5(&input)
        .ok_or_else(|| "no 32-character md5 found — paste an md5 or a libgen URL".to_string())?;
    let orch = resolve_arc(&state, list_id).await?;
    {
        let mut guard = orch.lock().await;
        let pos = position_for(&guard, &book_id)?;
        guard
            .add_manual_candidate(&pos.group_path, pos.book_index, &md5)
            .map_err(err)?;
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// Unmark a variation (the UI's "Cancel" on a not-yet-started variation),
/// returning it to the `available` state.
#[tauri::command]
pub async fn cancel_variation(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
    md5: String,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(&state, list_id).await?;
    {
        let mut guard = orch.lock().await;
        let pos = variation_for(&guard, &book_id, &md5)?;
        guard
            .cancel_variation(&pos.group_path, pos.book_index, &pos.md5)
            .map_err(err)?;
    }
    refresh_library(&state).await
}

// ---------------------------------------------------------------------------
// Lifecycle controls (pause / resume / cancel of in-flight or queued downloads)
// ---------------------------------------------------------------------------

/// Pause an in-flight or queued **variation** (by md5). Signals the shared
/// scheduler to stop an active stream (keeping its `.part` + resume offset) and
/// marks the persisted job `Paused`. Calls [`Orchestrator::pause_variation`].
#[tauri::command]
pub async fn pause_variation(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
    md5: String,
) -> Result<ViewLibrary, String> {
    let scheduler = ensure_scheduler(&state, None).await?;
    let orch = resolve_arc(&state, list_id).await?;
    {
        let mut guard = orch.lock().await;
        let pos = variation_for(&guard, &book_id, &md5)?;
        guard
            .pause_variation(&scheduler, &pos.group_path, pos.book_index, &pos.md5)
            .await
            .map_err(err)?;
    }
    refresh_library(&state).await
}

/// Resume a paused/cancelled variation (by md5): its job goes back to `Pending`
/// so the engine continues it. Calls [`Orchestrator::resume_variation`].
#[tauri::command]
pub async fn resume_variation(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
    md5: String,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(&state, list_id).await?;
    {
        let mut guard = orch.lock().await;
        let pos = variation_for(&guard, &book_id, &md5)?;
        guard
            .resume_variation(&pos.group_path, pos.book_index, &pos.md5)
            .map_err(err)?;
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// Cancel a variation's download (by md5) — whether in-flight or queued.
/// Signals the shared scheduler to abort an active stream and marks the
/// persisted job `Cancelled`. Calls [`Orchestrator::cancel_download`].
#[tauri::command]
pub async fn cancel_download(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
    md5: String,
) -> Result<ViewLibrary, String> {
    let scheduler = ensure_scheduler(&state, None).await?;
    let orch = resolve_arc(&state, list_id).await?;
    {
        let mut guard = orch.lock().await;
        let pos = variation_for(&guard, &book_id, &md5)?;
        guard
            .cancel_download(&scheduler, &pos.group_path, pos.book_index, &pos.md5)
            .await
            .map_err(err)?;
    }
    refresh_library(&state).await
}

/// Pause every requested variation across the targeted list(s) (the toolbar's
/// "Pause queue"). Signals the shared scheduler to stop all in-flight downloads
/// and marks every non-`Done` job `Paused`. Calls [`Orchestrator::pause_all`] per
/// list. (Equivalent to Stop for download purposes; the goal is left as-is.)
#[tauri::command]
pub async fn pause_all(
    state: State<'_, AppState>,
    list_id: Option<String>,
) -> Result<ViewLibrary, String> {
    let scheduler = ensure_scheduler(&state, None).await?;
    let arcs = scoped_arcs(&state, list_id).await;
    for (_, orch) in &arcs {
        orch.lock().await.pause_all(&scheduler).await.map_err(err)?;
    }
    refresh_library(&state).await
}

/// Resume every paused/cancelled variation across the targeted list(s) (the
/// toolbar's "Resume queue"). Each such job returns to `Pending`; the engine then
/// continues them.
#[tauri::command]
pub async fn resume_all(
    state: State<'_, AppState>,
    list_id: Option<String>,
) -> Result<ViewLibrary, String> {
    let arcs = scoped_arcs(&state, list_id).await;
    for (_, orch) in &arcs {
        orch.lock().await.resume_all().map_err(err)?;
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// Set the list's ranked preferred formats (most-preferred first), as driven by
/// the toolbar's reorderable "Preferred formats" control. Affects which
/// candidate the matcher prefers on the next query/match pass.
#[tauri::command]
pub async fn set_format_pref(
    state: State<'_, AppState>,
    list_id: Option<String>,
    formats: Vec<String>,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(&state, list_id).await?;
    {
        let mut guard = orch.lock().await;
        let parsed: Vec<Format> = formats.iter().map(|f| Format::parse(f)).collect();
        guard.set_format_pref(parsed).map_err(err)?;
    }
    refresh_library(&state).await
}

/// The per-list settings the Settings sheet edits, as sent from the front end.
/// A JSON-friendly mirror of the engine's [`ListSettings`].
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SettingsPayload {
    pub format_pref: Vec<String>,
    /// Empty string = any language.
    pub language: String,
    pub naming_template: String,
    pub auto_threshold: f32,
    pub near_threshold: f32,
    pub seq_per_group: bool,
    pub keep_top: usize,
    /// Title-match confidence to auto-download (defaulted for older front ends).
    #[serde(default = "default_title_match")]
    pub title_match_threshold: f32,
}

fn default_title_match() -> f32 {
    0.9
}

/// Replace the whole per-list [`ListSettings`] for a list (defaults to the
/// active list) and persist it via [`Orchestrator::update_settings`]. This is
/// the Settings sheet's per-list save: preferred formats, match thresholds,
/// naming template, kept variations, sequence scope, and preferred language.
#[tauri::command]
pub async fn set_settings(
    state: State<'_, AppState>,
    list_id: Option<String>,
    settings: SettingsPayload,
) -> Result<ViewLibrary, String> {
    let orch = resolve_arc(&state, list_id).await?;
    // Preserve the existing `is_manual` flag — the Settings sheet doesn't send it,
    // and a save must never flip the mutable Manual list back to immutable.
    let is_manual = {
        let guard = orch.lock().await;
        guard
            .snapshot()
            .map(|l| l.settings.is_manual)
            .unwrap_or(false)
    };
    let s = libgen_core::model::ListSettings {
        format_pref: settings
            .format_pref
            .iter()
            .map(|f| Format::parse(f))
            .collect(),
        language: {
            let l = settings.language.trim();
            if l.is_empty() {
                None
            } else {
                Some(l.to_string())
            }
        },
        naming_template: settings.naming_template,
        auto_threshold: settings.auto_threshold.clamp(0.0, 1.0),
        near_threshold: settings.near_threshold.clamp(0.0, 1.0),
        seq_per_group: settings.seq_per_group,
        keep_top: settings.keep_top.max(1),
        title_match_threshold: settings.title_match_threshold.clamp(0.0, 1.0),
        is_manual,
    };
    orch.lock().await.update_settings(s).map_err(err)?;
    refresh_library(&state).await
}

/// The global app settings the Settings sheet edits, as sent from the front end.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ConfigPayload {
    pub out_dir: String,
    /// Global cap on total concurrent downloads (`G`). Optional for back-compat
    /// with older front-end payloads (0/absent → keep the default).
    #[serde(default)]
    pub max_concurrent_downloads: usize,
    pub query_concurrency: usize,
    pub max_attempts: u32,
    /// Speculative (hedged) download toggle. Optional for back-compat with older
    /// front-end payloads (defaults to false / off).
    #[serde(default)]
    pub hedge_enabled: bool,
}

/// Return just the global app-config view (the Settings sheet's "App settings").
#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<ViewAppConfig, String> {
    let cfg = state.config.lock().expect("config mutex poisoned");
    Ok(app_config_view(&cfg))
}

/// Replace + persist the global app settings (download folder, site failover
/// order, concurrency/politeness). Persisted to `app-config.json` in the DB
/// directory. Resets the shared scheduler so the next `start_downloads` rebuilds
/// it with the new sites/limits, and re-applies query concurrency + the output
/// directory to every loaded list. Returns the refreshed library.
#[tauri::command]
pub async fn set_config(
    state: State<'_, AppState>,
    config: ConfigPayload,
) -> Result<ViewLibrary, String> {
    let new_app = AppSettings {
        out_dir: config.out_dir.trim().to_string(),
        max_concurrent_downloads: if config.max_concurrent_downloads > 0 {
            config.max_concurrent_downloads
        } else {
            crate::state::default_max_concurrent_downloads()
        },
        query_concurrency: config.query_concurrency.max(1),
        max_attempts: config.max_attempts.max(1),
        hedge_enabled: config.hedge_enabled,
    };

    // Persist + swap into the live config.
    let cfg_snapshot = {
        let mut cfg = state.config.lock().expect("config mutex poisoned");
        cfg.app = new_app;
        cfg.app.save(&cfg.config_path()).map_err(err)?;
        cfg.clone()
    };

    // Drop the cached scheduler so it rebuilds with the new sites/limits.
    *state.scheduler.lock().await = None;

    // Re-attach each loaded orchestrator against the SAME persisted list so the
    // new output directory + query concurrency take effect without a relaunch
    // (the engine exposes these only at construction time). Re-attaching reads
    // from the same DB, so no list state is lost. Each orchestrator is swapped
    // under its OWN lock (brief library lock just to grab the handles first).
    let out = cfg_snapshot.effective_out_dir();
    let arcs = {
        let lib = state.library.lock().await;
        lib.all_arcs()
    };
    for (_, orch) in &arcs {
        let mut guard = orch.lock().await;
        let list_id = guard.list_id();
        let store = match open_store(&cfg_snapshot) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let search = match build_search(&cfg_snapshot) {
            Ok(s) => s,
            Err(_) => continue,
        };
        *guard = Orchestrator::attach(store, list_id, search, out.clone())
            .with_query_concurrency(cfg_snapshot.app.query_concurrency);
    }
    refresh_library(&state).await
}

/// Fetch live shadow-library mirror availability from **open-slum.org** (SLUM)
/// and return per-site health for the Settings sheet's "Mirror health" panel.
///
/// This is an explicit, user-initiated "refresh from the live monitor" action,
/// so it always hits the network (even in the app's offline/replay mode); a
/// failure surfaces to JS as `Err(String)` and the UI shows "couldn't refresh".
/// See [`libgen_core::slum`] for the endpoint/JSON details.
#[tauri::command]
pub async fn refresh_mirrors(state: State<'_, AppState>) -> Result<Vec<ViewSiteHealth>, String> {
    let report = SlumClient::live().fetch().await.map_err(err)?;
    // Cache the snapshot so the next scheduler/search build can auto-order mirrors
    // by live availability without a network call. Best-effort.
    {
        let cfg = state.config.lock().expect("config mutex poisoned");
        let _ = report.save(cfg.slum_cache_path());
    }
    Ok(report
        .sites
        .into_iter()
        .map(|s| ViewSiteHealth {
            host: s.host,
            name: s.name,
            group: s.group,
            up: s.up,
            ping_ms: s.ping_ms,
            uptime_24h: s.uptime_24h,
        })
        .collect())
}

/// **Reorganize downloaded files** to the current `<list>/<sub-group>/<seq> - name`
/// layout: move files that finished under an older (flat / one-level) layout into
/// place across every loaded list. Explicit, user-triggered (it moves files on
/// disk). Safe: moves (never overwrites/deletes), idempotent. Returns a summary
/// string for the UI.
#[tauri::command]
pub async fn reorganize_files(state: State<'_, AppState>) -> Result<String, String> {
    let arcs = {
        let lib = state.library.lock().await;
        lib.all_arcs()
    };
    // Each list's output folder, so a book that appears in two lists is DUPLICATED
    // into each (copied) rather than moved back and forth between them.
    let mut folders: Vec<std::path::PathBuf> = Vec::with_capacity(arcs.len());
    for (_, orch) in &arcs {
        let g = orch.lock().await;
        folders.push(g.list_folder().unwrap_or_default());
    }
    let (mut moved, mut skipped, mut errors) = (0usize, 0usize, 0usize);
    for (i, (_, orch)) in arcs.iter().enumerate() {
        let siblings: Vec<std::path::PathBuf> = folders
            .iter()
            .enumerate()
            .filter(|(j, f)| *j != i && !f.as_os_str().is_empty())
            .map(|(_, f)| f.clone())
            .collect();
        let mut g = orch.lock().await;
        if let Ok((m, s, e)) = g.relocate_downloads_to_current_scheme(&siblings) {
            moved += m;
            skipped += s;
            errors += e;
        }
    }
    Ok(format!(
        "Reorganized: {moved} moved, {skipped} skipped, {errors} error(s)."
    ))
}

/// Dry-run: would [`reorganize_files`] move ANY file? Drives the UI's enable/gray
/// state for the "Reorganize now" button (true = at least one list has a
/// finished file not yet in the canonical layout).
#[tauri::command]
pub async fn reorganize_needed(state: State<'_, AppState>) -> Result<usize, String> {
    let arcs = {
        let lib = state.library.lock().await;
        lib.all_arcs()
    };
    // How many downloaded files (across all lists) are NOT at their canonical path
    // and would be moved — drives the UI's "Reorganize now (N)" label + gray state.
    let mut total = 0usize;
    for (_, orch) in &arcs {
        let mut g = orch.lock().await;
        total += g.reorganize_plan_diff().map(|d| d.len()).unwrap_or(0);
    }
    Ok(total)
}

/// Move every `.part` file (incomplete/abandoned downloads + leftover hedge-leg
/// temps) under the download directory to the Trash. User-triggered maintenance;
/// runs off-thread (filesystem walk + Trash moves are blocking).
#[tauri::command]
pub async fn cleanup_part_files(state: State<'_, AppState>) -> Result<String, String> {
    let out = {
        state
            .config
            .lock()
            .expect("config mutex poisoned")
            .effective_out_dir()
    };
    let (count, bytes) = tauri::async_runtime::spawn_blocking(move || {
        libgen_core::orchestrator::trash_part_files(&out)
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(if count == 0 {
        "No .part files to clean up.".into()
    } else {
        format!(
            "Moved {count} .part file(s) ({:.1} MB) to Trash.",
            bytes as f64 / 1_048_576.0
        )
    })
}

/// The exact (current path → correct path) pairs that [`reorganize_files`] would
/// move, across all lists — drives the "Reorganize now" details view so the user
/// can SEE which files are misplaced and where they'd go, not just a count.
#[tauri::command]
pub async fn reorganize_diff(state: State<'_, AppState>) -> Result<Vec<(String, String)>, String> {
    let arcs = {
        let lib = state.library.lock().await;
        lib.all_arcs()
    };
    let mut out = Vec::new();
    for (_, orch) in &arcs {
        let mut g = orch.lock().await;
        if let Ok(diff) = g.reorganize_plan_diff() {
            out.extend(diff);
        }
    }
    Ok(out)
}

/// A `query://book` event payload: a per-book query-stage transition emitted
/// during a `query_and_match` / `requery` pass so the UI can show live query
/// progress. `stage` is one of `"querying"` (being queried), `"matched"`,
/// `"needs_selection"`, or `"not_found"`. `book_id` is the flat UI id (`"bk7"`).
#[derive(Debug, Clone, Serialize)]
pub struct QueryStagePayload {
    pub list_id: String,
    pub book_id: String,
    pub title: String,
    pub stage: String,
}

/// A `download://progress` event payload (a flattened, JSON-friendly mirror of
/// the engine's [`Progress`]).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProgressPayload {
    Resolved {
        md5: String,
        leg_id: u64,
        is_hedge: bool,
        host: String,
        total_bytes: Option<u64>,
    },
    /// Continuing from an existing on-disk partial (informational; the UI ignores
    /// it — the chronicle is persisted and shown via "Show history").
    Resuming {
        md5: String,
        leg_id: u64,
        is_hedge: bool,
        host: String,
        offset: u64,
    },
    Bytes {
        md5: String,
        leg_id: u64,
        is_hedge: bool,
        host: String,
        bytes_done: u64,
        total_bytes: Option<u64>,
        /// Smoothed throughput in bytes/sec, `None` until measurable.
        speed_bps: Option<u64>,
        /// Estimated seconds remaining, `None` when total/speed unknown or zero.
        eta_secs: Option<u64>,
    },
    /// The download is crawling/hung on `host`; the scheduler is racing a hedge.
    /// Drives the UI's "trying another mirror" hint. Informational only.
    Stalled {
        md5: String,
        leg_id: u64,
        is_hedge: bool,
        host: String,
        speed_bps: Option<u64>,
    },
    Retrying {
        md5: String,
        leg_id: u64,
        is_hedge: bool,
        host: String,
        attempt: u32,
        error: String,
    },
    FailingOver {
        md5: String,
        leg_id: u64,
        is_hedge: bool,
        from_host: String,
        error: String,
    },
    Done {
        md5: String,
        host: String,
        path: String,
        bytes_written: u64,
    },
    Failed {
        md5: String,
        error: String,
    },
    /// A download was paused (recoverable) — its `.part` + resume offset kept.
    Paused {
        md5: String,
        resume_offset: u64,
    },
    /// A download was cancelled (its `.part` removed).
    Cancelled {
        md5: String,
    },
    /// A specific download leg ended (any exit path). The UI removes exactly this
    /// `leg_id` from the md5's active-leg set; survivors remain.
    LegEnded {
        md5: String,
        leg_id: u64,
    },
    /// All downloads for this run finished.
    AllDone,
}

impl ProgressPayload {
    /// Map a scheduler [`Progress`] to its UI payload. Returns `None` for events
    /// that carry NO new UI signal — currently [`Progress::Note`], a history-only
    /// download-path diagnostic (edge rotation, Range-ignored restart) that the
    /// engine persists into the chronicle but the frontend does not render.
    pub(crate) fn from_progress(p: &Progress) -> Option<Self> {
        Some(match p {
            Progress::Resolved {
                md5,
                leg_id,
                is_hedge,
                host,
                total_bytes,
            } => ProgressPayload::Resolved {
                md5: md5.clone(),
                leg_id: *leg_id,
                is_hedge: *is_hedge,
                host: host.clone(),
                total_bytes: *total_bytes,
            },
            Progress::Resuming {
                md5,
                leg_id,
                is_hedge,
                host,
                offset,
            } => ProgressPayload::Resuming {
                md5: md5.clone(),
                leg_id: *leg_id,
                is_hedge: *is_hedge,
                host: host.clone(),
                offset: *offset,
            },
            Progress::Bytes {
                md5,
                leg_id,
                is_hedge,
                host,
                bytes_done,
                total_bytes,
                speed_bps,
                eta_secs,
            } => ProgressPayload::Bytes {
                md5: md5.clone(),
                leg_id: *leg_id,
                is_hedge: *is_hedge,
                host: host.clone(),
                bytes_done: *bytes_done,
                total_bytes: *total_bytes,
                speed_bps: *speed_bps,
                eta_secs: *eta_secs,
            },
            Progress::Stalled {
                md5,
                leg_id,
                is_hedge,
                host,
                speed_bps,
                ..
            } => ProgressPayload::Stalled {
                md5: md5.clone(),
                leg_id: *leg_id,
                is_hedge: *is_hedge,
                host: host.clone(),
                speed_bps: *speed_bps,
            },
            Progress::Retrying {
                md5,
                leg_id,
                is_hedge,
                host,
                attempt,
                error,
                ..
            } => ProgressPayload::Retrying {
                md5: md5.clone(),
                leg_id: *leg_id,
                is_hedge: *is_hedge,
                host: host.clone(),
                attempt: *attempt,
                error: error.clone(),
            },
            Progress::FailingOver {
                md5,
                leg_id,
                is_hedge,
                from_host,
                error,
            } => ProgressPayload::FailingOver {
                md5: md5.clone(),
                leg_id: *leg_id,
                is_hedge: *is_hedge,
                from_host: from_host.clone(),
                error: error.clone(),
            },
            Progress::Done {
                md5,
                host,
                path,
                bytes_written,
            } => ProgressPayload::Done {
                md5: md5.clone(),
                host: host.clone(),
                path: path.to_string_lossy().into_owned(),
                bytes_written: *bytes_written,
            },
            Progress::Failed { md5, error } => ProgressPayload::Failed {
                md5: md5.clone(),
                error: error.clone(),
            },
            // Pause and cancel are distinct, recoverable lifecycle events the UI
            // reflects directly (the engine persists the precise Paused/Cancelled
            // job state regardless).
            Progress::Cancelled {
                md5,
                paused,
                resume_offset,
            } => {
                if *paused {
                    ProgressPayload::Paused {
                        md5: md5.clone(),
                        resume_offset: *resume_offset,
                    }
                } else {
                    ProgressPayload::Cancelled { md5: md5.clone() }
                }
            }
            // A leg ended: the UI removes exactly this leg_id from the md5's set.
            Progress::LegEnded { md5, leg_id } => ProgressPayload::LegEnded {
                md5: md5.clone(),
                leg_id: *leg_id,
            },
            // History-only diagnostic — not surfaced to the UI.
            Progress::Note { .. } => return None,
        })
    }
}

/// **Start** the targeted list(s): set goal = `Complete` so the execution engine
/// discovers + downloads every book, doing all network I/O off the library lock.
/// Defaults to the active list; `"__all__"`/empty fans out to every loaded list.
/// Returns the refreshed library IMMEDIATELY — it does NOT wait for downloads
/// (the engine drives them in the background and streams `download://progress`).
///
/// Kept under the name `start_downloads` for back-compat with the existing UI/IPC
/// surface (the new explicit per-list `start`/`stop` + global `start_all`/
/// `stop_all` commands are the goal-setters the spec defines). The `site` param,
/// when given, pre-builds the shared scheduler with that resolver chain (so a test
/// can pin a `{md5}` mock direct-URL or a specific mirror) before the engine
/// fetches; otherwise the configured failover chain is used.
#[tauri::command]
pub async fn start_downloads(
    state: State<'_, AppState>,
    list_id: Option<String>,
    site: Option<String>,
) -> Result<ViewLibrary, String> {
    // Pre-build the scheduler with the requested site so the engine's downloads
    // use it (idempotent: a no-op if one already exists for this session).
    let _ = ensure_scheduler(&state, site.as_deref()).await?;

    // Which lists to drive: the named/active list, or all of them for "__all__".
    let target_arcs = {
        let lib = state.library.lock().await;
        let target = list_id.clone().unwrap_or_else(|| lib.current.clone());
        if target == "__all__" || target.is_empty() {
            lib.all_arcs()
        } else {
            match lib.arc_for(&target) {
                Some(o) => vec![(target, o)],
                None => Vec::new(),
            }
        }
    };
    for (_, orch) in &target_arcs {
        orch.lock()
            .await
            .set_goal_all(Goal::Complete)
            .map_err(err)?;
    }
    state.wake_engine();
    refresh_library(&state).await
}

/// **Download whole series**: look up the book's series on Open Library, seed a
/// NEW list with its ordered members, and drive it to completion.
///
/// Locking (per `docs/SYNCHRONIZATION.md`): the book's `(title, author)` is read
/// under a BRIEF per-orch lock, which is then DROPPED. The Open Library lookup —
/// the network phase — runs with NO lock held. Only after it returns do we take
/// the library lock again to persist + register the new list. No lock is ever
/// held across the OL calls.
///
/// `Err` when the book isn't part of a known series. On success the new list is
/// persisted via the SAME path as [`load_list`] (de-duped by title), every member
/// is set to goal `Complete`, the engine is woken, and the refreshed library is
/// returned with the new list selected.
#[tauri::command]
pub async fn download_series(
    state: State<'_, AppState>,
    list_id: Option<String>,
    book_id: String,
) -> Result<ViewLibrary, String> {
    // 1. Resolve the book's input (title + author) under a BRIEF orch lock.
    let orch = resolve_arc(&state, list_id).await?;
    let (title, author) = {
        let guard = orch.lock().await;
        let pos = position_for(&guard, &book_id)?;
        let list = guard.snapshot().map_err(err)?;
        let book = book_at(&list, &pos.group_path, pos.book_index)
            .ok_or_else(|| format!("book {book_id} not found"))?;
        (book.input.title.clone(), book.input.authors.join(", "))
    }; // orch lock dropped here — the network lookup runs OFF any lock.

    if title.trim().is_empty() {
        return Err(libgen_core::model::ui_msg("err.series_no_title", &[]));
    }

    // 2. Open Library lookup — NO lock held across the network (per docs).
    let series_client = {
        let cfg = state.config.lock().expect("config mutex poisoned").clone();
        build_series_client(&cfg)
    };
    let series = series_client
        .lookup(&title, &author)
        .await
        .map_err(|e| format!("Open Library lookup failed: {e}"))?;

    // 3. None → not in a series. Some → build + persist a fresh list, run it.
    let series = match series {
        Some(s) if !s.members.is_empty() => s,
        _ => return Err(libgen_core::model::ui_msg("err.not_in_series", &[])),
    };

    let list = series_to_list(&series);

    // Persist via the SAME path as `load_list`: de-dupe by title (re-running
    // refreshes the same list instead of duplicating it), attach a fresh orch.
    let cfg = state.config.lock().expect("config mutex poisoned").clone();
    let mut store = open_store(&cfg)?;
    let search = build_search(&cfg)?;
    let store_id = match store.list_id_by_title(&list.title).map_err(err)? {
        Some(existing) => {
            store.upsert_list(existing, &list).map_err(err)?;
            existing
        }
        None => store.insert_list(&list).map_err(err)?,
    };
    let store2 = open_store(&cfg)?;
    let new_orch = Orchestrator::attach(store2, store_id, search, cfg.effective_out_dir())
        .with_query_concurrency(cfg.app.query_concurrency);
    let id = Library::id_for(store_id);

    {
        let mut lib = state.library.lock().await;
        lib.lists.retain(|l| l.id != id);
        lib.current = id.clone();
        lib.lists.push(LoadedList::new(id.clone(), new_orch));
    }
    // Goal = Complete: the engine discovers + downloads every member.
    set_goal_for(&state, &id, Goal::Complete).await?;
    state.wake_engine();
    refresh_library(&state).await
}

/// Project a detected [`Series`] into a [`DownloadList`]: one book per member, in
/// reading order, under a single group named after the series. The list title is
/// `"<series name> (series)"` so a re-run de-dupes onto the same list.
fn series_to_list(series: &libgen_core::series::Series) -> DownloadList {
    use libgen_core::model::{BookInput, BookRequest, Group};
    let title = format!("{} (series)", series.name);
    let mut group = Group::new(series.name.clone());
    for m in &series.members {
        group.books.push(BookRequest::new(BookInput {
            title: m.title.clone(),
            ..Default::default()
        }));
    }
    DownloadList {
        title,
        settings: Default::default(),
        groups: vec![group],
    }
}

/// Reveal a finished file in the OS file manager (Finder on macOS).
#[tauri::command]
pub fn reveal(app: AppHandle, state: State<'_, AppState>, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    // The recorded path is usually right — but an older download may sit under a
    // different layout/sequence number than what's now stored. If the recorded
    // file is missing, locate the real one by its sequence-stripped name under the
    // output dir and reveal that instead.
    let mut target = std::path::PathBuf::from(&path);
    if !target.exists() {
        let out_dir = state
            .config
            .lock()
            .expect("config mutex poisoned")
            .effective_out_dir();
        if let Some(stable) = target
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| libgen_core::orchestrator::strip_seq_prefix(n).to_string())
        {
            let mut found: Option<std::path::PathBuf> = None;
            libgen_core::orchestrator::collect_files_recursive(&out_dir, &mut |f| {
                if found.is_none()
                    && f.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| libgen_core::orchestrator::strip_seq_prefix(n))
                        == Some(stable.as_str())
                {
                    found = Some(f.to_path_buf());
                }
            });
            if let Some(f) = found {
                target = f;
            }
        }
    }
    app.opener().reveal_item_in_dir(target).map_err(err)
}

// ---------------------------------------------------------------------------
// Startup: resume-on-launch
// ---------------------------------------------------------------------------

/// Load every persisted list from the on-disk store on startup, attaching an
/// orchestrator to each. **Launch is paused** (`docs/EXECUTION_MODEL.md` §4/§9):
/// every loaded book's `goal` is reset to `Idle` and any in-flight
/// `Querying`/`Downloading` is rewound to its pre-flight state
/// (`Queued`/`Matched`-`Pending`) via the normalizer. The engine therefore does
/// NOTHING until a command (Start / Re-query) raises a goal. The persisted
/// matched/done/review state is surfaced in the sidebar intact.
///
/// Best-effort: a missing/empty DB simply yields no lists.
pub fn resume_on_launch(state: &AppState, cfg: &Config) {
    // Read the persisted list ids from a throwaway store, then attach a fresh
    // store (own connection) per orchestrator against the SAME db file.
    let stored = match open_store(cfg).and_then(|s| s.all_lists().map_err(err)) {
        Ok(s) => s,
        Err(_) => return,
    };
    if stored.is_empty() {
        return;
    }

    // Setup runs before any command can contend the lock, so a non-blocking
    // acquire is safe here (and avoids `blocking_lock` panicking on a runtime
    // worker thread).
    let mut lib = match state.library.try_lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    for sl in stored {
        let store = match open_store(cfg) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let search = match build_search(cfg) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut orch = Orchestrator::attach(store, sl.id, search, cfg.effective_out_dir())
            .with_query_concurrency(cfg.app.query_concurrency);
        // Launch = paused: rewind interrupted in-flight jobs AND park every book
        // at goal Idle so nothing runs until the user Starts/Re-queries.
        let _ = orch.reset_inflight_for_resume();
        let _ = orch.rewind_inflight_status();
        let _ = orch.set_goal_all(Goal::Idle);
        let id = Library::id_for(sl.id);
        lib.lists.retain(|l| l.id != id);
        lib.lists.push(LoadedList::new(id, orch));
    }
    if lib.current.is_empty() {
        lib.current = lib.lists.first().map(|l| l.id.clone()).unwrap_or_default();
    }
}

/// Spawn the background **cover backfill** loop: periodically look up missing book
/// covers (Open Library), cache a local thumbnail under `<list>/thumbnails/`, and
/// point the book's cover at that local file — all OFF the orchestrator lock (a
/// brief lock to read targets, the network lookup with no lock held, a brief lock
/// to persist). Skipped entirely in replay/offline mode. Low-priority + best
/// effort: a failure for one book never affects the others or any download.
/// Background integrity scan (run once on launch, off the launch path so it never
/// delays startup): demote any `Done` variation whose file is missing on disk to
/// `Failed` ("data lost"), then push a UI refresh if anything changed. Catches
/// files moved/deleted/lost-to-a-collision out from under a "Done" record.
pub fn spawn_download_verify(library: Arc<Mutex<Library>>, app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let arcs = {
            let lib = library.lock().await;
            lib.all_arcs()
        };
        let mut total = 0usize;
        let mut reconciled = 0usize;
        // At startup NOTHING is live (the engine task hasn't dispatched any
        // transport yet), so the liveness set is empty: every stuck variation
        // qualifies for reconciliation.
        let live_md5s = std::collections::HashSet::new();
        for (_, orch) in arcs {
            // 0. Session-gated reconciliation of in-flight-but-not-Done variations
            //    (the inverse of the demotions below). A download can finish — the
            //    `.part` promoted to the full-size `output_path` — yet the job stay
            //    in-flight with stale bytes because the `Progress::Done` reconciliation
            //    was lost (it raced with an edit/re-query). Reuse the SAME judgement
            //    the running engine uses in-session (`reconcile_completed_inflight`):
            //    complete file → Done; partial/absent → re-queue (resume); over the
            //    attempt cap → Failed. This runs FIRST so a reconciled-Done job is
            //    never re-downloaded; the engine is paused at launch anyway (every goal
            //    Idle), so a re-queued variation simply waits for a Start.
            let n = reconcile_completed_inflight(&orch, &live_md5s, "startup integrity scan").await;
            reconciled += n;
            total += n;
            // 1. Existence (cheap, under a brief lock): a Done variation with no
            //    file on disk → Failed ("data lost").
            total += {
                let mut g = orch.lock().await;
                g.flag_missing_downloads().unwrap_or(0)
            };
            // 2. Content (md5) verification, OFF-lock: hash each remaining Done
            //    file and demote any whose content doesn't match the requested md5
            //    (overwritten by a same-name collision, or corrupt). Hashing is
            //    expensive, so it runs without holding the per-list lock.
            let dones = {
                let g = orch.lock().await;
                g.done_variations().unwrap_or_default()
            };
            for (gp, bi, md5, output_path) in dones {
                let path = match output_path {
                    Some(p) if !p.is_empty() => p,
                    _ => continue, // missing path handled by step 1
                };
                let actual = libgen_core::download::md5_of_file(std::path::Path::new(&path)).await;
                let mismatch = matches!(actual, Ok(h) if !h.eq_ignore_ascii_case(&md5));
                if mismatch {
                    let demoted = {
                        let mut g = orch.lock().await;
                        g.demote_variation(
                            &gp,
                            bi,
                            &md5,
                            "downloaded file doesn't match its md5 (data lost) — wrong/overwritten content, re-download",
                        )
                        .unwrap_or(false)
                    };
                    if demoted {
                        total += 1;
                    }
                }
            }
        }
        if total > 0 {
            tracing::warn!(
                count = total,
                reconciled,
                demoted = total - reconciled,
                "startup integrity scan: changed variations (reconciled completed-but-stuck + demoted missing/mismatched)"
            );
            use tauri::Emitter;
            let _ = app.emit("library://refresh", ());
        } else {
            tracing::info!("startup integrity scan: all downloaded files present + verified");
        }
    });
}

pub fn spawn_cover_backfill(library: Arc<Mutex<Library>>, config: Arc<std::sync::Mutex<Config>>) {
    // Offline/replay → no network cover lookups.
    if config
        .lock()
        .map(|c| c.replay_dir.is_some())
        .unwrap_or(true)
    {
        return;
    }
    // Spawn on Tauri's managed runtime: this runs from the `setup` hook, which is
    // NOT inside a Tokio runtime, so a bare `tokio::spawn` panics ("no reactor
    // running"). Inner tokio::time/mutex calls run fine on this runtime.
    tauri::async_runtime::spawn(async move {
        let covers = libgen_core::covers::CoverClient::live();
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap_or_default();
        loop {
            let arcs = {
                let lib = library.lock().await;
                lib.all_arcs()
            };
            for (_, orch) in arcs {
                // Brief lock: which books still need a cover + the thumbnail dir.
                let (list_dir, targets) = match {
                    let g = orch.lock().await;
                    g.cover_targets()
                } {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                for t in targets {
                    // OFF-lock: pick a source URL (an existing remote cover from
                    // search, else an Open Library lookup), then cache it locally.
                    let online_url = match t.existing_remote.clone() {
                        Some(u) => Some(u),
                        None => match covers
                            .cover_url(&t.title, &t.author, t.isbn.as_deref())
                            .await
                        {
                            Ok(Some(u)) => Some(u),
                            _ => None,
                        },
                    };

                    // Prefer an online cover, but ONLY if it caches as a usable image
                    // (store_thumbnail now rejects 1×1/placeholder/corrupt responses).
                    // If there's no usable online cover, fall through to GENERATING one
                    // locally (epub embedded image / pdf first page / synthetic) — the
                    // bug was that a placeholder URL blocked generation entirely.
                    let online = match online_url {
                        Some(url) => {
                            libgen_core::covers::store_thumbnail(&client, &list_dir, &t.key, &url)
                                .await
                                .ok()
                                .map(|p| p.to_string_lossy().into_owned())
                        }
                        None => None,
                    };
                    let local = match online {
                        Some(p) => p,
                        None => match t.local_file.clone() {
                            Some(file) => {
                                let dest = libgen_core::covers::thumbnail_path(&list_dir, &t.key);
                                let title = t.title.clone();
                                let author = t.author.clone();
                                let generated = tauri::async_runtime::spawn_blocking(move || {
                                    let bytes = libgen_core::cover_gen::generate_cover(
                                        std::path::Path::new(&file),
                                        &title,
                                        &author,
                                    )?;
                                    if let Some(parent) = dest.parent() {
                                        std::fs::create_dir_all(parent).ok()?;
                                    }
                                    std::fs::write(&dest, &bytes).ok()?;
                                    Some(dest.to_string_lossy().into_owned())
                                })
                                .await;
                                match generated {
                                    Ok(Some(path)) => path,
                                    _ => continue,
                                }
                            }
                            None => continue, // no online cover, no local file — later
                        },
                    };
                    // Brief lock: persist.
                    let mut g = orch.lock().await;
                    let _ = g.apply_cover(&t.group_path, t.book_index, &local);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The list id to act on: an explicit `list_id`, else the active list. Errors
/// when neither resolves to a loaded list.
fn active_id(lib: &Library, list_id: Option<String>) -> Result<String, String> {
    let id = list_id.unwrap_or_else(|| lib.current.clone());
    if id.is_empty() || id == "__all__" {
        // No single list targeted — fall back to the first loaded list.
        return lib
            .lists
            .first()
            .map(|l| l.id.clone())
            .ok_or_else(|| "no list loaded".to_string());
    }
    Ok(id)
}

/// Resolve an optional `list_id` (else the active list) to its per-orchestrator
/// `Arc<Mutex<…>>` handle, taking only a BRIEF library lock to clone it. The
/// caller then locks the returned handle for the (possibly network-bound) work —
/// never holding the library lock across it. Errors when no list resolves.
async fn resolve_arc(
    state: &AppState,
    list_id: Option<String>,
) -> Result<Arc<Mutex<Orchestrator>>, String> {
    let lib = state.library.lock().await;
    let id = active_id(&lib, list_id)?;
    lib.arc_for(&id)
        .ok_or_else(|| format!("list {id} not loaded"))
}

/// Clone the per-orchestrator handles a "queue control" command targets: a single
/// named/active list, or EVERY loaded list for the `__all__`/empty aggregate.
/// Brief library lock only.
async fn scoped_arcs(
    state: &AppState,
    list_id: Option<String>,
) -> Vec<(String, Arc<Mutex<Orchestrator>>)> {
    let lib = state.library.lock().await;
    let target = list_id.unwrap_or_else(|| lib.current.clone());
    if target.is_empty() || target == "__all__" {
        lib.all_arcs()
    } else {
        match lib.arc_for(&target) {
            Some(o) => vec![(target, o)],
            None => Vec::new(),
        }
    }
}

/// Return the shared scheduler, building it on first use from the configured
/// sites + politeness limits. An explicit non-empty `site` (e.g. a test's
/// `{md5}` direct-URL template, or a pinned mirror) overrides the configured
/// failover chain; otherwise the app-config sites are used.
async fn ensure_scheduler(state: &AppState, site: Option<&str>) -> Result<Arc<Scheduler>, String> {
    ensure_scheduler_from(&state.scheduler, &state.config, site).await
}

/// Collect the md5s of every variation in `list` whose download job is actively
/// in flight (`Resolving`/`Downloading`/`Verifying`) — i.e. holding a connection
/// and writing a `.part`. These are the md5s a command must signal the scheduler
/// to abort when it takes the owning book out of "wanting to download"
/// (`docs/SYNCHRONIZATION.md` §5). `accept` filters which books count (e.g. all
/// books, only one, or only non-`Done` ones). Pure read over a snapshot — no lock
/// interaction.
fn inflight_md5s_in(
    list: &DownloadList,
    accept: &impl Fn(&libgen_core::model::BookRequest) -> bool,
) -> Vec<String> {
    use libgen_core::model::JobState;
    let mut out = Vec::new();
    fn walk(
        groups: &[libgen_core::model::Group],
        accept: &impl Fn(&libgen_core::model::BookRequest) -> bool,
        out: &mut Vec<String>,
    ) {
        for g in groups {
            for b in &g.books {
                if !accept(b) {
                    continue;
                }
                for c in &b.candidates {
                    if matches!(
                        c.job.as_ref().map(|j| &j.state),
                        Some(JobState::Resolving | JobState::Downloading | JobState::Verifying)
                    ) {
                        out.push(c.md5.clone());
                    }
                }
            }
            walk(&g.subgroups, accept, out);
        }
    }
    walk(&list.groups, accept, &mut out);
    out
}

/// Resolve a book by its `(group_path, book_index)` tree position in a snapshot.
fn book_at<'a>(
    list: &'a DownloadList,
    group_path: &[usize],
    book_index: usize,
) -> Option<&'a libgen_core::model::BookRequest> {
    let mut groups = &list.groups;
    let (last, parents) = group_path.split_last()?;
    for &gi in parents {
        groups = &groups.get(gi)?.subgroups;
    }
    groups.get(*last)?.books.get(book_index)
}

/// Resolve a UI book id (`"bk12"`) to its tree position within the loaded list.
/// Normalize a title for the same-book comparison: lowercase, and split on any
/// non-alphanumeric run so punctuation/whitespace differences don't matter
/// ("Garden!" == "garden"). Distinct books ("The Secret Garden" vs "A Little Princess") stay distinct.
fn norm_title(s: &str) -> String {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Cross-list guard: is `md5` already claimed by a book whose title DIFFERS from
/// `my_title`? Returns that conflicting title if so. Scans every loaded list —
/// the same file legitimately appears under the SAME title across lists (a book
/// duplicated between reading lists), but two DIFFERENT titles pointing at one
/// file is a mistaken selection (e.g. an "A Little Princess" file picked for "The Secret Garden"). Locks
/// each orchestrator independently; callers must hold no orchestrator lock.
async fn conflicting_claim(state: &AppState, md5: &str, my_title: &str) -> Option<String> {
    let my_norm = norm_title(my_title);
    let arcs = { state.library.lock().await.all_arcs() };
    for (_id, arc) in arcs {
        let titles = {
            let guard = arc.lock().await;
            guard.titles_claiming_md5(md5).unwrap_or_default()
        };
        if let Some(other) = titles.into_iter().find(|t| norm_title(t) != my_norm) {
            return Some(other);
        }
    }
    None
}

fn position_for(orch: &Orchestrator, book_id: &str) -> Result<bridge::Position, String> {
    let flat = bridge::parse_book_id(book_id).ok_or_else(|| format!("bad book id {book_id}"))?;
    let list = orch.snapshot().map_err(err)?;
    bridge::position_of(&list, flat).ok_or_else(|| format!("book {book_id} not found"))
}

/// Resolve a `(book_id, md5)` pair to a variation position within the loaded
/// list, validating the md5 belongs to that book.
fn variation_for(
    orch: &Orchestrator,
    book_id: &str,
    md5: &str,
) -> Result<bridge::VariationPosition, String> {
    let list = orch.snapshot().map_err(err)?;
    bridge::variation_of(&list, book_id, md5)
        .ok_or_else(|| format!("variation {md5} of book {book_id} not found"))
}

/// Build the Open Library series client from config: replay (offline) when a
/// replay dir is configured, else live — mirroring [`libgen_engine::build_search`].
/// The replay dir's `series/` subdirectory holds the recorded Open Library responses.
fn build_series_client(cfg: &Config) -> SeriesClient {
    match &cfg.replay_dir {
        Some(dir) => SeriesClient::replay(dir.join("series")),
        None => SeriesClient::live(),
    }
}

// ---------------------------------------------------------------------------
// Test support (headless): construct an AppState + load lists + set goals
// without a Tauri runtime, so the integration tests under `tests/` can drive the
// engine and the per-orchestrator locking exactly as the commands do. `#[doc
// (hidden)]`; not part of the front-end contract.
// ---------------------------------------------------------------------------

/// Headless helpers for integration tests (and only tests).
#[doc(hidden)]
pub mod testsupport {
    use super::*;
    use libgen_core::model::DownloadList;
    use std::path::PathBuf;

    /// Build an [`AppState`] whose config points at `db_path` with search served
    /// from the `replay_dir` fixtures (offline). The `mirrors` path supplies the
    /// search-mirror config. No engine task is started — the test spawns one.
    pub fn app_state(db_path: PathBuf, replay_dir: PathBuf, mirrors: PathBuf) -> AppState {
        let app = AppSettings {
            // Low query concurrency keeps replay deterministic + observable.
            query_concurrency: 4,
            ..Default::default()
        };
        let cfg = Config {
            mirrors,
            out_dir: db_path.parent().unwrap().join("out"),
            db_path,
            replay_dir: Some(replay_dir),
            app,
        };
        AppState {
            config: Arc::new(std::sync::Mutex::new(cfg)),
            ..Default::default()
        }
    }

    /// Persist `list` into the state's store and attach + load an orchestrator for
    /// it (mirrors `load_list`, sans network/Tauri). Returns the stable UI list id.
    pub async fn load(state: &AppState, list: &DownloadList) -> Result<String, String> {
        let cfg = state.config.lock().expect("config").clone();
        let mut store = open_store(&cfg)?;
        let search = build_search(&cfg)?;
        let store_id = store.insert_list(list).map_err(err)?;
        let store2 = open_store(&cfg)?;
        let orch = Orchestrator::attach(store2, store_id, search, cfg.effective_out_dir())
            .with_query_concurrency(cfg.app.query_concurrency);
        let id = Library::id_for(store_id);
        let mut lib = state.library.lock().await;
        lib.current = id.clone();
        lib.lists.push(LoadedList::new(id.clone(), orch));
        Ok(id)
    }

    /// Set goal = `Complete` for every book of `id` and wake the engine (the
    /// per-list Start path, headless).
    pub async fn start(state: &AppState, id: &str) -> Result<(), String> {
        set_goal_for(state, id, Goal::Complete).await
    }

    /// Set an explicit goal for every book of `id` and wake the engine. `Match`
    /// drives discovery only (no download), which is deterministic offline.
    pub async fn set_goal(state: &AppState, id: &str, goal: Goal) -> Result<(), String> {
        set_goal_for(state, id, goal).await
    }

    /// Run resume-on-launch against the state's config (launch = paused).
    pub fn resume(state: &AppState) {
        let cfg = state.config.lock().expect("config").clone();
        resume_on_launch(state, &cfg);
    }

    /// Arm a specific variation (by md5) of the book at `(group_path, book_index)`
    /// for download and wake the engine — the test analogue of the UI's per-variation
    /// "Download" button. Used to add a second variation MID-FLIGHT.
    pub async fn request_variation_at(
        state: &AppState,
        id: &str,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
    ) -> Result<(), String> {
        let orch = {
            let lib = state.library.lock().await;
            lib.arc_for(id).ok_or_else(|| "no such list".to_string())?
        };
        {
            let mut g = orch.lock().await;
            g.request_variation(group_path, book_index, md5)
                .map_err(err)?;
            g.set_goal_one(group_path, book_index, Goal::Complete)
                .map_err(err)?;
        }
        state.wake_engine();
        Ok(())
    }

    /// Snapshot one loaded list (its persisted [`DownloadList`]).
    pub async fn snapshot(state: &AppState, id: &str) -> Option<DownloadList> {
        let orch = {
            let lib = state.library.lock().await;
            lib.arc_for(id)?
        };
        let guard = orch.lock().await;
        guard.snapshot().ok()
    }

    /// Add a book to the (find-or-created) mutable Manual list, headless — the
    /// test analogue of the `add_manual_book` command.
    pub async fn add_manual_book(
        state: &AppState,
        title: &str,
        author: Option<&str>,
    ) -> Result<ViewLibrary, String> {
        super::add_manual_book_inner(state, title.to_string(), author.map(|s| s.to_string())).await
    }

    /// Remove a book (by UI id) from a mutable list, headless — the test analogue
    /// of the `remove_book` command (enforces the `is_manual` guard).
    pub async fn remove_book(
        state: &AppState,
        id: &str,
        book_id: &str,
    ) -> Result<ViewLibrary, String> {
        super::remove_book_inner(state, Some(id.to_string()), book_id.to_string()).await
    }

    /// Inject a pre-built scheduler (e.g. a mock-host one) so the engine's
    /// `ensure_scheduler` reuses it instead of building from config sites. Call
    /// BEFORE spawning the engine so the download workers pick it up.
    pub async fn set_scheduler(state: &AppState, sched: Arc<Scheduler>) {
        *state.scheduler.lock().await = Some(sched);
    }

    /// Override the global download-worker count (`G`). Call BEFORE spawning the
    /// engine (the pool size is read once at startup).
    pub fn set_max_concurrent_downloads(state: &AppState, n: usize) {
        state
            .config
            .lock()
            .expect("config")
            .app
            .max_concurrent_downloads = n;
    }

    /// Spawn the long-lived execution engine with a no-op emitter (headless), so a
    /// test can drive the real tick → worker-pool → download path end to end.
    pub fn spawn_engine(state: &AppState) {
        libgen_engine::spawn_with(state.engine_handles(), libgen_engine::NoopEmitter);
    }
}

#[cfg(test)]
mod md5_extract_tests {
    use super::extract_md5;

    #[test]
    fn extracts_md5_from_bare_or_url_and_rejects_non_md5() {
        let md5 = "1111111111111111111111111111abcd";
        assert_eq!(extract_md5(md5).as_deref(), Some(md5));
        assert_eq!(
            extract_md5("https://libgen.li/ads.php?md5=1111111111111111111111111111abcd&key=X")
                .as_deref(),
            Some(md5)
        );
        // Trimmed + lowercased.
        assert_eq!(
            extract_md5("  https://libgen.li/file.php?md5=1111111111111111111111111111ABCD  ")
                .as_deref(),
            Some(md5)
        );
        assert_eq!(
            extract_md5("https://annas-archive.org/md5/1111111111111111111111111111abcd")
                .as_deref(),
            Some(md5)
        );
        // No 32-hex run.
        assert_eq!(extract_md5("not a hash"), None);
        assert_eq!(extract_md5("deadbeef"), None);
        // A 40-hex (sha1) run is NOT a valid md5 (must be exactly 32 hex).
        assert_eq!(extract_md5(&"a".repeat(40)), None);
    }
}
