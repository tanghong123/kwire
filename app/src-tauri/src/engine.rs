//! The goal-driven execution engine (driver task) — `docs/EXECUTION_MODEL.md` §6.
//!
//! A single long-lived task continuously reconciles each book's CURRENT state
//! (its `status`) toward its `goal`, doing ALL network I/O **off** the library
//! lock. The flow of one tick is:
//!
//!   1. PLAN — under a BRIEF library lock, clone every list's per-orchestrator
//!      `Arc<Mutex<Orchestrator>>`; drop the library lock. Then, under each
//!      orchestrator's OWN brief lock, snapshot it and collect ACTIONABLE books
//!      (`status < goal`, not blocked, within budgets).
//!   2. EXECUTE — for each actionable book, spawn a bounded worker that locks
//!      ONLY that orchestrator, runs the single-book transition (search OR
//!      download — network, NO library lock), persists the result, emits events.
//!   3. WAIT — on the wake `Notify` (a command changed a goal, or a worker
//!      finished) or a short idle timer, then repeat.
//!
//! The library lock is therefore never held across network I/O, so no command is
//! ever starved → the app never freezes (the whole point). Transitions are
//! idempotent + monotonic (they re-read the current state before committing), so
//! a command that changes the goal concurrently is safe.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, Mutex, Semaphore};

use libgen_core::model::{DownloadList, Goal, JobState, RequestStatus};
use libgen_core::orchestrator::{Event, Orchestrator};
use libgen_core::queue::{Progress, Scheduler};

use crate::commands;
use crate::state::{AppState, EngineHandles};
use crate::{bridge, viewmodel};

/// How long the driver sleeps between ticks when nothing wakes it. A safety net:
/// goal/state changes wake it immediately via the `Notify`.
const IDLE_TICK: Duration = Duration::from_millis(750);

/// Minimum spacing between in-session "completed-but-stuck download" reconciliation
/// sweeps (see [`reconcile_inflight_sweep`]). The tick loop wakes often (≤750 ms,
/// plus on every goal/state change), so this throttles the sweep to a modest
/// cadence instead of re-walking every list on each wake.
const RECONCILE_CADENCE: Duration = Duration::from_secs(30);

/// One unit of reconciliation work the engine will perform: a single book and the
/// transition it needs next.
#[derive(Clone)]
struct WorkItem {
    list_id: String,
    orch: Arc<Mutex<Orchestrator>>,
    group_path: Vec<usize>,
    book_index: usize,
    kind: WorkKind,
    /// For a Download item, the specific variation to fetch — so each variation is
    /// its own dispatch unit (parallel). `None` for Query/Reverify (per book).
    md5: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkKind {
    /// New → Querying → resolved (search + match).
    Query,
    /// Matched/Ready → Downloading → Done (drive the scheduler).
    Download,
    /// A `Done` book whose goal was (re-)raised to Complete after a Re-query:
    /// re-verify against the fresh top candidate (file kept).
    Reverify,
}

/// A per-book state-change event for the UI (extends `query://book` +
/// `download://progress`). Emitted as `engine://book` whenever the engine
/// advances a book or a command changes its goal. JSON shape (see report):
/// `{ list_id, book_id, status, goal }` where `status` and `goal` are the
/// snake_case strings of the engine's `RequestStatus` / `Goal`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BookStatePayload {
    pub list_id: String,
    pub book_id: String,
    pub status: String,
    pub goal: String,
}

/// Spawn the long-lived engine driver task, wired to the Tauri [`AppHandle`]
/// (which owns the managed [`AppState`] and is the event sink). Idempotent: only
/// the first call spawns; subsequent calls are no-ops (guarded by
/// `engine_started`). The task re-fetches the managed state each tick from the
/// handle, so it shares the SAME `AppState` every command sees.
pub fn spawn(app: AppHandle) {
    let handles = {
        let state = app.state::<AppState>();
        if state.engine_started.swap(true, Ordering::SeqCst) {
            return;
        }
        state.engine_handles()
    };
    let emitter = TauriEmitter { app };
    // Spawn on Tauri's managed async runtime — this runs from the `setup` hook,
    // which is NOT inside a Tokio runtime context, so a bare `tokio::spawn` would
    // panic ("no reactor running"). `tauri::async_runtime::spawn` works from any
    // context. (Workers spawned inside `run_engine` are fine — they run within
    // this task, which is on the runtime.)
    tauri::async_runtime::spawn(async move {
        run_engine(handles, emitter).await;
    });
}

/// Spawn an engine driver for a test / headless harness against explicit shared
/// [`EngineHandles`] and an [`EngineEmitter`]. Returns nothing; the task runs
/// until the runtime is dropped. `#[doc(hidden)]` — for the integration tests.
#[doc(hidden)]
pub fn spawn_with<E: EngineEmitter>(handles: EngineHandles, emitter: E) {
    tokio::spawn(async move {
        run_engine(handles, emitter).await;
    });
}

/// A no-op [`EngineEmitter`] for headless tests (drops every event).
#[doc(hidden)]
#[derive(Clone, Copy, Default)]
pub struct NoopEmitter;

impl EngineEmitter for NoopEmitter {
    fn emit_event(&self, _list_id: &str, _shape: &DownloadList, _ev: &Event) {}
    fn emit_book_state(&self, _payload: BookStatePayload) {}
}

/// Sink for the engine's live events. The Tauri front end consumes them; tests
/// substitute a collector / no-op so the driver can run headlessly.
pub trait EngineEmitter: Send + Sync + 'static {
    /// Forward an orchestrator [`Event`] (query stage / planned / download
    /// progress) for `list_id`, given a `shape` snapshot to translate tree
    /// positions to flat `bkN` ids.
    fn emit_event(&self, list_id: &str, shape: &DownloadList, ev: &Event);
    /// Emit a per-book state/goal change (`engine://book`).
    fn emit_book_state(&self, payload: BookStatePayload);
    /// Ask the front end to refresh the whole library view (`library://refresh`).
    /// Used after a background job-state change the per-tick events don't cover —
    /// e.g. the in-session completed-but-stuck reconciliation flips a job to Done.
    fn emit_refresh(&self) {}
}

/// The production emitter: forwards engine events to the front end as the same
/// `query://book` / `download://progress` events the UI already consumes, plus
/// `engine://book` for per-book state.
struct TauriEmitter {
    app: AppHandle,
}

impl EngineEmitter for TauriEmitter {
    fn emit_event(&self, list_id: &str, shape: &DownloadList, ev: &Event) {
        match ev {
            Event::QueryStage {
                group_path,
                book_index,
                title,
                stage,
            } => {
                let book_id = bridge::flat_id_in(shape, group_path, *book_index)
                    .unwrap_or_else(|| format!("bk{book_index}"));
                let _ = self.app.emit(
                    "query://book",
                    commands::QueryStagePayload {
                        list_id: list_id.to_string(),
                        book_id,
                        title: title.clone(),
                        stage: stage.clone(),
                    },
                );
            }
            Event::Download(p) => {
                if let Some(payload) = commands::ProgressPayload::from_progress(p) {
                    let _ = self.app.emit("download://progress", payload);
                }
            }
            Event::Done => {
                let _ = self
                    .app
                    .emit("download://progress", commands::ProgressPayload::AllDone);
            }
            // Planned / StatusChanged carry no extra UI signal beyond the above +
            // engine://book + the refreshed library, so they are not forwarded.
            _ => {}
        }
    }

    fn emit_book_state(&self, payload: BookStatePayload) {
        let _ = self.app.emit("engine://book", payload);
    }

    fn emit_refresh(&self) {
        let _ = self.app.emit("library://refresh", ());
    }
}

/// Identity of one in-flight transition, so the driver never spawns a second
/// worker for the same unit. Includes the variation md5 for Download items so two
/// variations of ONE book dispatch as SEPARATE units and download in PARALLEL
/// (`None` for per-book Query/Reverify, which stay one-per-book).
type BookKey = (String, Vec<usize>, usize, Option<String>);

fn item_key(item: &WorkItem) -> BookKey {
    (
        item.list_id.clone(),
        item.group_path.clone(),
        item.book_index,
        item.md5.clone(),
    )
}

/// The driver loop. Runs until the process exits.
///
/// Workers are **detached**: a tick spawns the actionable work and returns
/// immediately to WAIT, so a long download never blocks planning/executing work
/// for OTHER books or lists. Re-planning is safe because (a) every transition is
/// idempotent + monotonic and (b) an `inflight` set keyed by book stops the same
/// book being spawned twice before its status flips.
async fn run_engine<E: EngineEmitter>(handles: EngineHandles, emitter: E) {
    let emitter = Arc::new(emitter);
    let handles = Arc::new(handles);
    // Global query/reverify concurrency budget (downloads are bounded separately
    // by the per-host scheduler). Persistent across ticks.
    let query_budget = handles
        .config
        .lock()
        .map(|c| c.app.query_concurrency.max(1))
        .unwrap_or(8);
    let gate = Arc::new(Semaphore::new(query_budget));
    let inflight: Arc<Mutex<std::collections::HashSet<BookKey>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));

    // Download worker pool (pull model — docs/DOWNLOAD_SCHEDULING.md). A pool of
    // `G = max_concurrent_downloads` workers each pull ONE queued book and download
    // it, so a book is never "spawned then parked on a slot": it waits in this
    // queue (state: queued) until a free worker pulls it (state: downloading). The
    // number of downloading books therefore equals the number of busy workers (≤ G).
    //
    // The pool is RESIZED LIVE to track the setting (see the reconcile step in the
    // loop), so changing "Max concurrent downloads" takes effect without a restart.
    let (dl_tx, dl_rx) = mpsc::unbounded_channel::<WorkItem>();
    let dl_rx = Arc::new(Mutex::new(dl_rx));
    let dl_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // In-session reconciliation cadence: a completed-but-stuck download (its
    // `Progress::Done` lost to an edit/re-query race) is otherwise frozen in the
    // downloading view until relaunch. Sweep all lists every `RECONCILE_CADENCE`
    // and promote any whose file is complete + md5-verifies. First sweep runs on
    // the first tick (`Instant::now() - cadence`).
    let mut last_reconcile = std::time::Instant::now() - RECONCILE_CADENCE;

    loop {
        // Reconcile the pool size up to the current setting: GROW by spawning
        // workers here (a worker retires itself when the target drops — see
        // `download_worker`). On the first iteration this spawns the initial `G`.
        let target = handles
            .config
            .lock()
            .map(|c| c.app.max_concurrent_downloads.max(1))
            .unwrap_or(5);
        while dl_count.load(Ordering::SeqCst) < target {
            dl_count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(download_worker(
                Arc::clone(&handles),
                Arc::clone(&emitter),
                Arc::clone(&dl_rx),
                Arc::clone(&inflight),
                Arc::clone(&dl_count),
            ));
        }
        tick(&handles, &emitter, &gate, &inflight, &dl_tx).await;
        // In-session "completed-but-stuck download" reconciliation, throttled to
        // `RECONCILE_CADENCE` (the loop wakes far more often). Cheap when idle: for
        // a settled list the worklist is empty and no file is hashed.
        if last_reconcile.elapsed() >= RECONCILE_CADENCE {
            last_reconcile = std::time::Instant::now();
            reconcile_inflight_sweep(&handles, &emitter, &inflight).await;
        }
        // WAIT: woken by a goal/state change or a finished worker, else idle tick.
        tokio::select! {
            _ = handles.engine_wake.notified() => {}
            _ = tokio::time::sleep(IDLE_TICK) => {}
        }
    }
}

/// One reconciliation tick: PLAN actionable work, then EXECUTE it by spawning a
/// detached worker per book (skipping books already in flight). Returns promptly —
/// it does NOT await the workers.
async fn tick<E: EngineEmitter>(
    handles: &Arc<EngineHandles>,
    emitter: &Arc<E>,
    gate: &Arc<Semaphore>,
    inflight: &Arc<Mutex<std::collections::HashSet<BookKey>>>,
    dl_tx: &mpsc::UnboundedSender<WorkItem>,
) {
    // --- PLAN ---
    // BRIEF library lock: clone the per-orch Arcs, then drop the guard.
    let arcs = {
        let lib = handles.library.lock().await;
        lib.all_arcs()
    };

    // For each orchestrator, take its OWN brief lock, snapshot, collect work.
    let mut work: Vec<WorkItem> = Vec::new();
    for (list_id, orch) in &arcs {
        let snap = {
            let guard = orch.lock().await;
            match guard.snapshot() {
                Ok(s) => s,
                Err(_) => continue,
            }
        };
        collect_actionable(list_id, orch, &snap, &mut work);
    }
    if work.is_empty() {
        return;
    }

    // --- EXECUTE ---
    // Build the scheduler lazily iff any download work exists (avoids spinning one
    // up just to discover/reverify). If it can't be built, drop download items.
    let needs_download = work.iter().any(|w| w.kind == WorkKind::Download);
    let scheduler: Option<Arc<Scheduler>> = if needs_download {
        commands::ensure_scheduler_from(&handles.scheduler, &handles.config, None)
            .await
            .ok()
    } else {
        None
    };

    // Reserve in-flight keys up front (one short lock) so a book is handled once.
    // Download items are ENQUEUED to the worker pool (a free worker pulls them);
    // query/reverify items are spawned directly under the query budget.
    let mut to_run: Vec<WorkItem> = Vec::new();
    {
        let mut guard = inflight.lock().await;
        for item in work {
            if item.kind == WorkKind::Download && scheduler.is_none() {
                continue;
            }
            let key = item_key(&item);
            if guard.insert(key) {
                if item.kind == WorkKind::Download {
                    // Queue for the download worker pool; a worker transitions the
                    // book to `downloading` only when it actually pulls it.
                    let _ = dl_tx.send(item);
                } else {
                    to_run.push(item);
                }
            }
        }
    }

    for item in to_run {
        let key = item_key(&item);
        let gate = Arc::clone(gate);
        let emitter = Arc::clone(emitter);
        let wake = Arc::clone(&handles.engine_wake);
        let inflight = Arc::clone(inflight);
        tokio::spawn(async move {
            // Query/reverify share the query budget.
            let _permit = gate.acquire_owned().await.expect("engine gate");
            run_item(&emitter, None, item).await;
            inflight.lock().await.remove(&key);
            // A finished worker may unblock the next transition (e.g. a Matched
            // book is now downloadable): wake the driver to re-plan promptly.
            wake.notify_one();
        });
    }
}

/// In-session sweep for **stuck (not-Done) downloads**: across every loaded list,
/// reuse [`commands::reconcile_completed_inflight`] (the SAME judgement the startup
/// integrity scan uses) to fix any in-flight variation that has NO live transport:
/// a complete file → `Done`, a partial/absent file → re-queued to resume, and one
/// over the attempt cap → `Failed`.
///
/// LIVENESS AUTHORITY: the engine's `inflight` set is the source of truth for what
/// is genuinely mid-download. We snapshot the md5s it holds (a download `BookKey`
/// carries `Some(md5)`) and pass them as the live set, so a slow-but-alive transfer
/// is never touched. If any list changed, ask the front end to refresh so the
/// reconciled book updates live (no relaunch). Cheap when nothing is stuck: a
/// settled list yields an empty worklist and hashes no files.
async fn reconcile_inflight_sweep<E: EngineEmitter>(
    handles: &Arc<EngineHandles>,
    emitter: &Arc<E>,
    inflight: &Arc<Mutex<std::collections::HashSet<BookKey>>>,
) {
    let arcs = {
        let lib = handles.library.lock().await;
        lib.all_arcs()
    };
    // Snapshot the live download md5s (a download key is `(.., Some(md5))`).
    let live_md5s: std::collections::HashSet<String> = {
        let guard = inflight.lock().await;
        guard.iter().filter_map(|k| k.3.clone()).collect()
    };
    let mut fixed = 0usize;
    for (_, orch) in &arcs {
        fixed +=
            commands::reconcile_completed_inflight(orch, &live_md5s, "engine in-session sweep")
                .await;
    }
    if fixed > 0 {
        emitter.emit_refresh();
    }
}

/// One download worker (there are `G` of them — `max_concurrent_downloads`). Each
/// loops forever: pull the next queued book, ensure the shared scheduler, run the
/// single-book download transition, then free its in-flight key and wake the
/// driver. Holding the receiver lock across `recv()` serializes the PULL so each
/// queued book goes to exactly one free worker; the (slow) download runs with the
/// lock released, so all `G` workers download concurrently.
async fn download_worker<E: EngineEmitter>(
    handles: Arc<EngineHandles>,
    emitter: Arc<E>,
    rx: Arc<Mutex<mpsc::UnboundedReceiver<WorkItem>>>,
    inflight: Arc<Mutex<std::collections::HashSet<BookKey>>>,
    dl_count: Arc<std::sync::atomic::AtomicUsize>,
) {
    loop {
        // SHRINK: if the live concurrency setting dropped below the current pool
        // size, retire this worker (after any job it held has finished).
        let target = handles
            .config
            .lock()
            .map(|c| c.app.max_concurrent_downloads.max(1))
            .unwrap_or(5);
        if dl_count.load(Ordering::SeqCst) > target {
            dl_count.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        // Pull the next queued book, but wake periodically so an IDLE worker can
        // still notice a shrink and retire. (When items are flowing, `recv`
        // returns immediately, so this poll adds no latency to active downloads.)
        let item = {
            let mut guard = rx.lock().await;
            match tokio::time::timeout(Duration::from_secs(2), guard.recv()).await {
                Ok(Some(i)) => i,
                Ok(None) => {
                    dl_count.fetch_sub(1, Ordering::SeqCst);
                    return; // channel closed → shutting down
                }
                Err(_) => continue, // recv timeout → re-check the target
            }
        };
        let key = item_key(&item);
        // Reuse the shared scheduler (built lazily once).
        let scheduler = commands::ensure_scheduler_from(&handles.scheduler, &handles.config, None)
            .await
            .ok();
        if let Some(s) = scheduler {
            run_item(&emitter, Some(&s), item).await;
        }
        inflight.lock().await.remove(&key);
        handles.engine_wake.notify_one();
    }
}

/// Scan one list's snapshot and append the actionable work for it. A book is
/// actionable when its CURRENT `status` is behind its `goal` AND it is not blocked
/// (NeedsSelection awaiting a pick, NotFound, Failed, or already in-flight).
fn collect_actionable(
    list_id: &str,
    orch: &Arc<Mutex<Orchestrator>>,
    snap: &DownloadList,
    out: &mut Vec<WorkItem>,
) {
    let positions = bridge::positions(snap);
    for p in positions {
        let req = match group_book(snap, &p.group_path, p.book_index) {
            Some(r) => r,
            None => continue,
        };
        let kind = match actionable_kind(req) {
            Some(k) => k,
            None => continue,
        };
        if kind == WorkKind::Download {
            // One item PER pending variation, so a book's variations dispatch to
            // separate workers and download in parallel (not one-book-at-a-time).
            for c in &req.candidates {
                if matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Pending)) {
                    out.push(WorkItem {
                        list_id: list_id.to_string(),
                        orch: Arc::clone(orch),
                        group_path: p.group_path.clone(),
                        book_index: p.book_index,
                        kind,
                        md5: Some(c.md5.clone()),
                    });
                }
            }
        } else {
            out.push(WorkItem {
                list_id: list_id.to_string(),
                orch: Arc::clone(orch),
                group_path: p.group_path,
                book_index: p.book_index,
                kind,
                md5: None,
            });
        }
    }
}

/// Decide what transition (if any) a book needs next, honoring its goal and the
/// blocked/in-flight rules.
fn actionable_kind(req: &libgen_core::model::BookRequest) -> Option<WorkKind> {
    // Idle goal → never act.
    if req.goal == Goal::Idle {
        return None;
    }
    // New: discover it (both Match and Complete goals want this).
    if req.status == RequestStatus::Queued {
        return Some(WorkKind::Query);
    }
    // A requested variation still in `Pending` should be DOWNLOADED whenever the
    // goal wants completion — REGARDLESS of the coarse book status. That status is
    // a roll-up and can lag the per-variation state: a book stuck reading
    // `Downloading` while its only variation is actually `Pending` (no worker
    // running), or a user picking a copy on a `NeedsSelection`/`Failed` book. Keying
    // dispatch off the pending variation (not the status) un-sticks those. The
    // engine's `inflight` set dedups books that are GENUINELY mid-download — a real
    // in-flight transfer has its variation in `Downloading`/`Resolving`/`Verifying`
    // (not `Pending`), so `has_pending_variation` is false for it and this never
    // double-dispatches an active download.
    if req.goal >= Goal::Complete && has_pending_variation(req) {
        return Some(WorkKind::Download);
    }
    // A `Done` book a Re-query just raised to `Complete` (with no pending variation)
    // → re-verify it ONCE against the fresh top candidate (the file is kept).
    // `run_item` then lowers the goal to `Match` so it settles and isn't re-verified
    // every tick. A Done book at goal `Match`/`Idle` has reached its goal.
    if req.status == RequestStatus::Done && req.goal >= Goal::Complete {
        return Some(WorkKind::Reverify);
    }
    None
}

/// Whether the book has a requested variation still in `Pending` (queued for
/// download but not yet started / done).
fn has_pending_variation(req: &libgen_core::model::BookRequest) -> bool {
    req.candidates
        .iter()
        .any(|c| matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Pending)))
}

/// Run one work item: lock ONLY its orchestrator, run the single-book transition
/// (network off the library lock), and forward events. After the transition,
/// emit an `engine://book` state event from the freshly-persisted book.
async fn run_item<E: EngineEmitter>(
    emitter: &Arc<E>,
    scheduler: Option<&Arc<Scheduler>>,
    item: WorkItem,
) {
    // A `shape` snapshot (brief lock) lets the emitter translate tree positions to
    // flat ids; transitions mutate statuses, not tree shape, so it stays valid.
    let (tx, mut rx) = mpsc::channel::<Event>(1024);
    let list_id = item.list_id.clone();
    let emitter_pump = Arc::clone(emitter);
    let shape = {
        let guard = item.orch.lock().await;
        match guard.snapshot() {
            Ok(s) => s,
            Err(_) => return,
        }
    };
    let pump = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            emitter_pump.emit_event(&list_id, &shape, &ev);
        }
    });

    // CRITICAL (see docs/EXECUTION_MODEL.md §sync): the per-list orchestrator lock
    // guards the store/state ONLY. The network (search/download) must run with the
    // lock RELEASED, or every book in a list serializes. The `inflight` set already
    // gives per-book mutual exclusion, so it's safe to release between phases.
    match item.kind {
        WorkKind::Query => {
            // begin (brief lock) → search + match OFF-lock → finish (brief lock).
            let prep = {
                let mut g = item.orch.lock().await;
                match g.begin_query(&item.group_path, item.book_index, &tx).await {
                    Ok(Some((input, settings))) => Some((input, settings, g.search_client())),
                    _ => None,
                }
            };
            if let Some((input, settings, search)) = prep {
                // No lock held here — concurrent with every other book's search.
                let candidates = search.search(&input).await.unwrap_or_default();
                let outcome = libgen_core::matching::evaluate(&input, candidates, &settings);
                let mut g = item.orch.lock().await;
                let _ = g
                    .finish_query(&item.group_path, item.book_index, outcome, &tx)
                    .await;
            }
        }
        WorkKind::Reverify => {
            // begin (brief lock) → search + match OFF-lock → finish (brief lock).
            // Mirrors the Query dance so re-verifies in one list run concurrently.
            let prep = {
                let g = item.orch.lock().await;
                g.begin_reverify(&item.group_path, item.book_index)
                    .ok()
                    .flatten()
            };
            if let Some(prep) = prep {
                // No lock held here — concurrent with every other book's search.
                let fresh = prep.search.search(&prep.input).await.unwrap_or_default();
                let outcome = libgen_core::matching::evaluate(&prep.input, fresh, &prep.settings);
                let mut g = item.orch.lock().await;
                let _ = g
                    .finish_reverify(
                        &item.group_path,
                        item.book_index,
                        outcome,
                        &prep.downloaded_md5,
                        &tx,
                    )
                    .await;
                // Settle the verified book so it isn't re-verified every tick.
                let _ = g.set_goal_one(&item.group_path, item.book_index, Goal::Match);
            }
        }
        WorkKind::Download => {
            if let Some(s) = scheduler {
                // begin (brief lock → plan + mark Downloading + spawn scheduler.run)
                // → drain rx OFF-lock (apply_progress under a BRIEF lock per tick)
                // → finish (brief lock → settle). The per-list lock is NEVER held
                // across the transfer, so books in a list download concurrently
                // (`docs/SYNCHRONIZATION.md` §4).
                let session = {
                    let mut g = item.orch.lock().await;
                    g.begin_download(
                        s,
                        &item.group_path,
                        item.book_index,
                        item.md5.as_deref(),
                        &tx,
                    )
                    .await
                    .ok()
                    .flatten()
                };
                if let Some(mut session) = session {
                    let mut completed: Vec<String> = Vec::new();
                    // Drain progress with NO orchestrator lock held across rx.recv().
                    while let Some(prog) = session.rx.recv().await {
                        if let Progress::Done { md5, .. } = &prog {
                            completed.push(md5.clone());
                        }
                        {
                            let mut g = item.orch.lock().await;
                            if let Err(e) = g.apply_progress(&session.pending, &prog) {
                                tracing::warn!(error = %e, "apply_progress failed");
                            }
                        }
                        let _ = tx.send(Event::Download(prog)).await;
                    }
                    let _ = session.run.await;
                    let mut g = item.orch.lock().await;
                    let _ = g
                        .finish_download(&item.group_path, item.book_index, &completed)
                        .await;
                    // Settle a normally-completed Done book's goal to `Match` so the
                    // engine doesn't then re-verify it (re-verify needs an explicit
                    // Re-query to raise the goal back to Complete).
                    if let Ok(snap) = g.snapshot() {
                        if let Some(req) = group_book(&snap, &item.group_path, item.book_index) {
                            let settled = req.status == RequestStatus::Done
                                && !req.candidates.iter().any(|c| {
                                    matches!(
                                        c.job.as_ref().map(|j| &j.state),
                                        Some(JobState::Pending)
                                    )
                                });
                            if settled {
                                let _ =
                                    g.set_goal_one(&item.group_path, item.book_index, Goal::Match);
                            }
                        }
                    }
                }
            }
        }
    }

    // Emit the freshly-persisted per-book state for the UI (brief lock).
    {
        let guard = item.orch.lock().await;
        if let Ok(snap) = guard.snapshot() {
            if let Some(req) = group_book(&snap, &item.group_path, item.book_index) {
                let book_id = bridge::flat_id_in(&snap, &item.group_path, item.book_index)
                    .unwrap_or_else(|| format!("bk{}", item.book_index));
                emitter.emit_book_state(BookStatePayload {
                    list_id: item.list_id.clone(),
                    book_id,
                    status: viewmodel::discovery_str(&req.status).to_string(),
                    goal: goal_str(req.goal).to_string(),
                });
            }
        }
    }
    drop(tx);
    let _ = pump.await;
}

/// The snake_case string of a [`Goal`] for the `engine://book` event.
#[allow(dead_code)]
fn goal_str(goal: Goal) -> &'static str {
    match goal {
        Goal::Idle => "idle",
        Goal::Match => "match",
        Goal::Complete => "complete",
    }
}

/// Resolve a book by tree position within a snapshot (engine-local helper).
fn group_book<'a>(
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

#[cfg(test)]
mod tests {
    use super::*;
    use libgen_core::model::{BookInput, BookRequest, Candidate, DownloadJob};

    fn cand(state: Option<JobState>) -> Candidate {
        Candidate {
            md5: "0".repeat(32),
            title: "T".into(),
            authors: vec![],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: None,
            size_bytes: None,
            source_host: None,
            cover_url: None,
            score: 0.0,
            job: state.map(|s| DownloadJob {
                state: s,
                ..Default::default()
            }),
        }
    }

    fn req(status: RequestStatus, goal: Goal, var: Option<JobState>) -> BookRequest {
        let mut r = BookRequest::new(BookInput {
            title: "T".into(),
            ..Default::default()
        });
        r.status = status;
        r.goal = goal;
        r.candidates = vec![cand(var)];
        r
    }

    #[test]
    fn pending_variation_dispatches_download_regardless_of_stale_status() {
        // The bug: a book stuck reading `Downloading` (or `NeedsSelection`) while
        // its variation is actually `Pending` must still be dispatched.
        for status in [
            RequestStatus::Matched,
            RequestStatus::Downloading, // stale roll-up, nothing actually running
            RequestStatus::NeedsSelection, // user picked a copy on an ambiguous book
            RequestStatus::Failed { error: "x".into() },
        ] {
            assert_eq!(
                actionable_kind(&req(
                    status.clone(),
                    Goal::Complete,
                    Some(JobState::Pending)
                )),
                Some(WorkKind::Download),
                "status {status:?} with a pending variation should download"
            );
        }
    }

    #[test]
    fn genuinely_downloading_is_not_re_dispatched() {
        // A real in-flight transfer has its variation in Downloading (not Pending),
        // so it is NOT re-dispatched here (the inflight set + this both protect it).
        assert_eq!(
            actionable_kind(&req(
                RequestStatus::Downloading,
                Goal::Complete,
                Some(JobState::Downloading)
            )),
            None
        );
    }

    #[test]
    fn idle_and_user_blocked_without_pending_do_nothing() {
        // Idle goal never acts.
        assert_eq!(
            actionable_kind(&req(
                RequestStatus::Matched,
                Goal::Idle,
                Some(JobState::Pending)
            )),
            None
        );
        // NeedsSelection with NO pending variation waits for the user.
        assert_eq!(
            actionable_kind(&req(RequestStatus::NeedsSelection, Goal::Complete, None)),
            None
        );
    }

    #[test]
    fn done_complete_reverifies_when_nothing_pending() {
        assert_eq!(
            actionable_kind(&req(RequestStatus::Done, Goal::Complete, None)),
            Some(WorkKind::Reverify)
        );
        // …but a pending (recommended) variation on a Done book downloads first.
        assert_eq!(
            actionable_kind(&req(
                RequestStatus::Done,
                Goal::Complete,
                Some(JobState::Pending)
            )),
            Some(WorkKind::Download)
        );
    }
}
