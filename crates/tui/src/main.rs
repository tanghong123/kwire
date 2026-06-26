//! `libgen-tui` — ratatui terminal frontend for `libgen-engine`.
//!
//! § Architecture (TUI.md §3):
//! - Terminal setup: raw mode + alternate screen + mouse capture.
//! - A `TerminalGuard` (+ panic hook) guarantees teardown on any exit path.
//! - File-only tracing to `$XDG_STATE_HOME/kwire/tui.log`; never stdout/stderr.
//! - Event loop: `tokio::select!` over terminal input, engine events, and a
//!   120 ms redraw tick.
//! - `AppState::on_input` is pure; the event loop dispatches its `Intent`.
//! - Stage 4: live engine mount, replaces fixture/stub bootstrap.
//!
//! § CLI subcommands (see `cli/`):
//! - `kwire search <query…>` — one-shot mirror search, printed to stdout.
//! - `kwire get <arg…>` — download by MD5 or title search.
//! - Bare `kwire` — launches the TUI as today.

mod app;
mod cli;
mod guard;
mod intent;
#[cfg(test)]
mod tests;
mod theme;
mod ui;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    event::{EnableMouseCapture, EventStream},
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use directories::ProjectDirs;
use futures::StreamExt;
use libgen_core::model::Goal;
use libgen_core::orchestrator::Event;
use libgen_core::queue::Progress;
use libgen_engine::viewmodel::build_with_id;
use libgen_engine::{
    build_search, ensure_scheduler_from, open_store, spawn_with, AppSettings,
    AppState as EngineAppState, BookStatePayload, Config, EngineEmitter, EngineHandles, Library,
    LoadedList,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio::time;
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, EnvFilter};

use crate::app::{AppState, ListSummary, Modal};
use crate::guard::TerminalGuard;
use crate::intent::Intent;

// ---------------------------------------------------------------------------
// CLI top-level parser
// ---------------------------------------------------------------------------

/// kwire — libgen TUI and one-shot downloader.
///
/// Run without a subcommand to launch the interactive TUI.
#[derive(Parser)]
#[command(name = "kwire", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<cli::Commands>,
}

// ---------------------------------------------------------------------------
// Logging setup
// ---------------------------------------------------------------------------

/// Install a file-only tracing subscriber.  Returns the `WorkerGuard` that must
/// be kept alive for the duration of the process (dropping it flushes + closes
/// the log file).
fn setup_logging() -> Result<WorkerGuard> {
    let dirs = ProjectDirs::from("", "", "kwire-tui").context("cannot determine XDG state dir")?;
    let state_dir = dirs.state_dir().unwrap_or_else(|| {
        // Fallback: use data_local_dir if state_dir isn't supported on this OS.
        dirs.data_local_dir()
    });
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating state dir {}", state_dir.display()))?;

    let file_appender = tracing_appender::rolling::never(state_dir, "tui.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    Ok(guard)
}

// ---------------------------------------------------------------------------
// Engine event types
// ---------------------------------------------------------------------------

/// Forwards engine events into the TUI loop via an mpsc channel.
struct TuiEmitter {
    tx: mpsc::UnboundedSender<EngineEvent>,
}

/// An event from the engine destined for the TUI event loop.
enum EngineEvent {
    /// A Progress telemetry update (download bytes/speed/eta).
    Progress(Progress),
    /// The active list's ViewModel should be re-projected.
    Refresh,
}

impl EngineEmitter for TuiEmitter {
    fn emit_event(&self, _list_id: &str, _shape: &libgen_core::model::DownloadList, ev: &Event) {
        match ev {
            Event::Download(p) => {
                let _ = self.tx.send(EngineEvent::Progress(p.clone()));
            }
            Event::StatusChanged { .. } | Event::Done => {
                let _ = self.tx.send(EngineEvent::Refresh);
            }
            _ => {}
        }
    }

    fn emit_book_state(&self, _payload: BookStatePayload) {
        let _ = self.tx.send(EngineEvent::Refresh);
    }

    fn emit_refresh(&self) {
        let _ = self.tx.send(EngineEvent::Refresh);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Parse args first so --help / --version work before any terminal setup.
    let cli = Cli::parse();

    // If a subcommand was given, run it as a plain CLI (no TUI).
    if let Some(cmd) = cli.command {
        return cli::run(cmd).await;
    }

    // No subcommand → fall through to the TUI.

    // (1) Logging — file only; must be set up BEFORE we enter the alternate screen.
    let _log_guard = setup_logging().unwrap_or_else(|e| {
        // If we can't open the log file, swallow the error so the TUI still
        // starts.  We'll just lose log output.
        eprintln!("warning: could not set up file logging: {e}");
        let (_, guard) = tracing_appender::non_blocking(tracing_appender::rolling::never(
            std::env::temp_dir(),
            "kwire-tui-fallback.log",
        ));
        guard
    });

    info!("libgen-tui starting");

    // (2) Terminal setup.
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    // (3) Install teardown guard (also sets panic hook).
    let _guard = TerminalGuard::install();

    // (4) Build engine config (TUI uses its OWN XDG data dir so it's independent of the desktop app).
    let dirs = ProjectDirs::from("", "", "kwire-tui").context("cannot determine XDG data dir")?;
    let data_dir = dirs.data_dir().to_path_buf();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;

    // Build TUI-specific config with its own DB.
    let mut cfg = Config::from_env();
    cfg.db_path = data_dir.join("library.sqlite3");
    let cfg_arc = Arc::new(std::sync::Mutex::new(cfg.clone()));

    // Open/create the store, load existing lists.
    let engine_state = {
        let stored = {
            let store = open_store(&cfg).map_err(|e| anyhow::anyhow!(e))?;
            store.all_lists().context("loading stored lists")?
        };
        let engine_state = EngineAppState {
            config: Arc::clone(&cfg_arc),
            ..Default::default()
        };
        if !stored.is_empty() {
            let mut lib = engine_state.library.lock().await;
            for sl in stored {
                let search = build_search(&cfg).map_err(|e| anyhow::anyhow!(e))?;
                let store2 = open_store(&cfg).map_err(|e| anyhow::anyhow!(e))?;
                let orch = libgen_core::orchestrator::Orchestrator::attach(
                    store2,
                    sl.id,
                    search,
                    cfg.effective_out_dir(),
                )
                .with_query_concurrency(cfg.app.query_concurrency);
                let id = Library::id_for(sl.id);
                if lib.current.is_empty() {
                    lib.current = id.clone();
                }
                lib.lists.push(LoadedList::new(id, orch));
            }
        }
        engine_state
    };

    // (5) Project initial ViewModel for the active list (if any).
    let mut app = AppState::new();
    {
        let lib = engine_state.library.lock().await;
        let active_id = lib.current.clone();
        if let Some(orch_arc) = lib.arc_for(&active_id) {
            drop(lib); // drop library lock before taking orch lock
            let guard = orch_arc.lock().await;
            if let Ok(snap) = guard.snapshot() {
                let vm = build_with_id(active_id.clone(), &snap);
                app.set_view(vm);
            }
        }
    }

    // (5b-extra) Populate all-list summaries so the list strip shows every list.
    {
        let lib = engine_state.library.lock().await;
        let current_id = lib.current.clone();
        let pairs: Vec<(String, _)> = lib
            .lists
            .iter()
            .filter_map(|ll| lib.arc_for(&ll.id).map(|a| (ll.id.clone(), a)))
            .collect();
        drop(lib);
        let mut summaries: Vec<ListSummary> = Vec::new();
        for (id, orch_arc) in pairs {
            let guard = orch_arc.lock().await;
            if let Ok(snap) = guard.snapshot() {
                let vm = build_with_id(id.clone(), &snap);
                let total: usize = vm.groups.iter().map(|g| g.books.len()).sum();
                let done: usize = vm
                    .groups
                    .iter()
                    .flat_map(|g| g.books.iter())
                    .filter(|b| {
                        b.acquisition
                            .as_ref()
                            .map(|a| a.done >= 1 && a.active == 0)
                            .unwrap_or(false)
                    })
                    .count();
                summaries.push(ListSummary {
                    id,
                    title: vm.title,
                    done,
                    total,
                });
            }
        }
        let active_idx = summaries
            .iter()
            .position(|s| s.id == current_id)
            .unwrap_or(0);
        app.all_lists = summaries;
        app.active_list_idx = active_idx;
    }

    // (5b) Spawn the engine.
    let engine_handles = engine_state.engine_handles();
    let (eng_tx, eng_rx) = mpsc::unbounded_channel::<EngineEvent>();
    let emitter = TuiEmitter { tx: eng_tx };
    spawn_with(engine_handles.clone(), emitter);
    info!("engine spawned");

    // (6) Event loop.
    run_loop(&mut terminal, &mut app, engine_handles, eng_rx).await?;

    Ok(())
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
    handles: EngineHandles,
    mut eng_rx: mpsc::UnboundedReceiver<EngineEvent>,
) -> Result<()> {
    let mut input = EventStream::new();
    let mut tick_interval = time::interval(Duration::from_millis(120));

    // Initial draw.
    terminal.draw(|f| ui::render(f, app))?;

    loop {
        let quit = tokio::select! {
            // Terminal input
            Some(Ok(ev)) = input.next() => {
                let intent = app.on_input(ev);
                dispatch_intent(intent, app, &handles).await?
            }
            // Engine events
            Some(ev) = eng_rx.recv() => {
                match ev {
                    EngineEvent::Progress(p) => {
                        app.apply_progress(&p);
                    }
                    EngineEvent::Refresh => {
                        refresh_active_view(app, &handles).await;
                    }
                }
                false
            }
            // Redraw tick (spinner + progress bars)
            _ = tick_interval.tick() => {
                app.on_tick();
                false
            }
        };

        terminal.draw(|f| ui::render(f, app))?;

        if quit {
            break;
        }
    }

    info!("libgen-tui exiting");
    Ok(())
}

// ---------------------------------------------------------------------------
// Intent dispatcher
// ---------------------------------------------------------------------------

/// Dispatch an [`Intent`] produced by `on_input`. Returns `true` when the
/// event loop should exit.
async fn dispatch_intent(
    intent: Intent,
    app: &mut AppState,
    handles: &EngineHandles,
) -> Result<bool> {
    match intent {
        Intent::Quit => return Ok(true),
        Intent::Command(line) => {
            if handle_command_async(&line, app, handles).await? {
                return Ok(true);
            }
        }
        Intent::Select {
            group_path,
            book_index,
            md5,
        } => {
            select_candidate(app, handles, group_path, book_index, md5).await;
        }
        Intent::Retry {
            group_path,
            book_index,
        } => {
            retry_book(app, handles, group_path, book_index).await;
        }
        Intent::Pause {
            group_path,
            book_index,
        } => {
            pause_book(app, handles, group_path, book_index).await;
        }
        Intent::Cancel {
            group_path,
            book_index,
        } => {
            cancel_book(app, handles, group_path, book_index).await;
        }
        Intent::OpenDetail { flat_index } => {
            app.modal = Some(Modal::Detail {
                book_flat_index: flat_index,
                selected: 0,
            });
        }
        Intent::OpenPicker { flat_index } => {
            app.modal = Some(Modal::Picker {
                book_flat_index: flat_index,
                selected: 0,
            });
        }
        Intent::OpenHelp => {
            app.modal = Some(Modal::Help);
        }
        Intent::SwitchList { id } => {
            open_list(app, handles, &id).await;
            refresh_all_list_summaries(app, handles).await;
        }
        Intent::OpenFile(path) => {
            let _ = std::process::Command::new("open").arg(&path).spawn();
        }
        Intent::RevealFile(path) => {
            let parent = std::path::Path::new(&path)
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_owned();
            let _ = std::process::Command::new("open")
                .arg("-R")
                .arg(&path)
                .spawn()
                .or_else(|_| std::process::Command::new("open").arg(&parent).spawn());
        }
        Intent::SaveSettings => {
            save_settings(app, handles).await;
        }
        Intent::DiscardSettings => {
            // Draft already cleared and modal closed in on_input.
        }
        Intent::ConfirmDelete { id } => {
            execute_delete_list(app, handles, &id).await;
        }
        Intent::Redraw => {}
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Command-line dispatcher
// ---------------------------------------------------------------------------

/// Async `:command` handler. Returns `true` when the command requests quit.
async fn handle_command_async(
    line: &str,
    app: &mut AppState,
    handles: &EngineHandles,
) -> Result<bool> {
    let line = line.trim();
    info!("command: {:?}", line);
    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "quit" | "q" => return Ok(true),
        "settings" => {
            let app_settings: AppSettings = {
                let cfg = handles.config.lock().unwrap();
                cfg.app.clone()
            };
            app.open_settings(&app_settings);
        }
        "help" => app.modal = Some(Modal::Help),
        "import" => {
            if arg.is_empty() {
                tracing::warn!("import: no file path given");
            } else {
                import_file(app, handles, arg).await;
            }
        }
        "add" => {
            if arg.is_empty() {
                tracing::warn!("add: no title given");
            } else {
                add_manual(app, handles, arg).await;
            }
        }
        "open" => {
            if !arg.is_empty() {
                open_list(app, handles, arg).await;
            }
        }
        "requery" => {
            requery_active(app, handles).await;
        }
        "pause-all" => {
            pause_all_active(app, handles).await;
        }
        // #45 — per-list pause/start/resume
        "pause" => {
            pause_list(app, handles, if arg.is_empty() { None } else { Some(arg) }).await;
        }
        "start" | "resume" => {
            start_list(app, handles, if arg.is_empty() { None } else { Some(arg) }).await;
        }
        // #45 — all-list start/resume
        "start-all" | "resume-all" => {
            start_all_lists(app, handles).await;
        }
        // #48 — delete list (shows confirm modal)
        "delete" => {
            show_delete_confirm(app, handles, if arg.is_empty() { None } else { Some(arg) }).await;
        }
        // #53 — misc commands
        "refresh-mirrors" => {
            refresh_mirrors_cmd(app, handles).await;
        }
        "cleanup" => {
            cleanup_cmd(app, handles).await;
        }
        "add-md5" => {
            if arg.is_empty() {
                app.status_msg = Some("Usage: add-md5 <md5>".into());
            } else {
                add_md5_cmd(app, handles, arg).await;
            }
        }
        "" => {}
        other => {
            tracing::warn!("unknown command: {:?}", other);
            app.status_msg = Some(format!("Unknown command: {other}"));
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Command argument helpers (pub(crate) so tests can call them directly)
// ---------------------------------------------------------------------------

/// Expand a leading `~` or `~/` to `$HOME`.
pub(crate) fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        if path == "~" {
            home
        } else {
            format!("{}{}", home, &path[1..])
        }
    } else {
        path.to_string()
    }
}

/// Parse a `:add` argument into `(title, authors)`.
///
/// If `arg` contains a comma, split on the **last** comma: everything before
/// is the title, everything after is a single author string (both trimmed).
/// If there is no comma, return the whole arg as the title with no authors.
pub(crate) fn parse_add_arg(arg: &str) -> (String, Vec<String>) {
    if let Some(pos) = arg.rfind(',') {
        let title = arg[..pos].trim().to_string();
        let author = arg[pos + 1..].trim().to_string();
        if title.is_empty() {
            (arg.trim().to_string(), vec![])
        } else {
            (title, vec![author])
        }
    } else {
        (arg.trim().to_string(), vec![])
    }
}

// ---------------------------------------------------------------------------
// Engine helper functions
// ---------------------------------------------------------------------------

async fn active_orch(
    handles: &EngineHandles,
) -> Option<Arc<tokio::sync::Mutex<libgen_core::orchestrator::Orchestrator>>> {
    let lib = handles.library.lock().await;
    let id = lib.current.clone();
    lib.arc_for(&id)
}

async fn refresh_active_view(app: &mut AppState, handles: &EngineHandles) {
    let (id, orch_arc) = {
        let lib = handles.library.lock().await;
        let id = lib.current.clone();
        let orch = lib.arc_for(&id);
        (id, orch)
    };
    if let Some(orch_arc) = orch_arc {
        let snap = orch_arc.lock().await.snapshot();
        if let Ok(snap) = snap {
            let vm = build_with_id(id, &snap);
            app.set_view(vm);
        }
    }
    refresh_all_list_summaries(app, handles).await;
}

/// Recompute summaries for ALL loaded lists and update `app.all_lists`.
async fn refresh_all_list_summaries(app: &mut AppState, handles: &EngineHandles) {
    let (current_id, pairs) = {
        let lib = handles.library.lock().await;
        let current = lib.current.clone();
        let p: Vec<(String, _)> = lib
            .lists
            .iter()
            .filter_map(|ll| lib.arc_for(&ll.id).map(|a| (ll.id.clone(), a)))
            .collect();
        (current, p)
    };

    let mut summaries: Vec<ListSummary> = Vec::new();
    for (id, orch_arc) in pairs {
        let guard = orch_arc.lock().await;
        if let Ok(snap) = guard.snapshot() {
            let vm = build_with_id(id.clone(), &snap);
            let total: usize = vm.groups.iter().map(|g| g.books.len()).sum();
            let done: usize = vm
                .groups
                .iter()
                .flat_map(|g| g.books.iter())
                .filter(|b| {
                    b.acquisition
                        .as_ref()
                        .map(|a| a.done >= 1 && a.active == 0)
                        .unwrap_or(false)
                })
                .count();
            summaries.push(ListSummary {
                id,
                title: vm.title,
                done,
                total,
            });
        }
    }

    let active_idx = summaries
        .iter()
        .position(|s| s.id == current_id)
        .unwrap_or(0);
    app.all_lists = summaries;
    app.active_list_idx = active_idx;
}

async fn select_candidate(
    app: &mut AppState,
    handles: &EngineHandles,
    group_path: Vec<usize>,
    book_index: usize,
    md5: String,
) {
    let Some(orch_arc) = active_orch(handles).await else {
        return;
    };
    {
        let mut guard = orch_arc.lock().await;
        if let Err(e) = guard.select_candidate(&group_path, book_index, &md5) {
            tracing::warn!("select_candidate failed: {e}");
            return;
        }
        // Ensure goal = Complete so the engine downloads it.
        let _ = guard.set_goal_one(&group_path, book_index, Goal::Complete);
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
}

async fn retry_book(
    app: &mut AppState,
    handles: &EngineHandles,
    group_path: Vec<usize>,
    book_index: usize,
) {
    let Some(orch_arc) = active_orch(handles).await else {
        return;
    };
    {
        let mut guard = orch_arc.lock().await;
        if let Err(e) = guard.retry(&group_path, book_index) {
            tracing::warn!("retry failed: {e}");
            return;
        }
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
}

async fn pause_book(
    app: &mut AppState,
    handles: &EngineHandles,
    group_path: Vec<usize>,
    book_index: usize,
) {
    let Some(orch_arc) = active_orch(handles).await else {
        return;
    };
    // Get in-flight md5s for this book.
    let inflight = {
        let guard = orch_arc.lock().await;
        guard
            .snapshot()
            .map(|snap| {
                use libgen_core::model::JobState;
                snap.groups
                    .get(group_path.first().copied().unwrap_or(0))
                    .and_then(|g| g.books.get(book_index))
                    .map(|b| {
                        b.candidates
                            .iter()
                            .filter(|c| {
                                matches!(
                                    c.job.as_ref().map(|j| &j.state),
                                    Some(
                                        JobState::Resolving
                                            | JobState::Downloading
                                            | JobState::Verifying
                                    )
                                )
                            })
                            .map(|c| c.md5.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    };
    if let Ok(sched) = ensure_scheduler_from(&handles.scheduler, &handles.config, None).await {
        for md5 in &inflight {
            sched.pause(md5).await;
        }
    }
    refresh_active_view(app, handles).await;
}

async fn cancel_book(
    app: &mut AppState,
    handles: &EngineHandles,
    group_path: Vec<usize>,
    book_index: usize,
) {
    let Some(orch_arc) = active_orch(handles).await else {
        return;
    };
    let inflight = {
        let guard = orch_arc.lock().await;
        guard
            .snapshot()
            .map(|snap| {
                use libgen_core::model::JobState;
                snap.groups
                    .get(group_path.first().copied().unwrap_or(0))
                    .and_then(|g| g.books.get(book_index))
                    .map(|b| {
                        b.candidates
                            .iter()
                            .filter(|c| {
                                matches!(
                                    c.job.as_ref().map(|j| &j.state),
                                    Some(
                                        JobState::Resolving
                                            | JobState::Downloading
                                            | JobState::Verifying
                                    )
                                )
                            })
                            .map(|c| c.md5.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    };
    if let Ok(sched) = ensure_scheduler_from(&handles.scheduler, &handles.config, None).await {
        for md5 in &inflight {
            sched.cancel(md5).await;
        }
    }
    refresh_active_view(app, handles).await;
}

async fn import_file(app: &mut AppState, handles: &EngineHandles, path: &str) {
    // (a) Expand a leading tilde.
    let expanded = expand_tilde(path);

    let content = match std::fs::read_to_string(&expanded) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("import: cannot read {expanded}: {e}");
            app.status_msg = Some(format!("Import failed: {e}"));
            return;
        }
    };

    // (b) Dispatch to JSON or Markdown parser based on extension / fallback.
    let is_json = expanded.ends_with(".json");
    let list = if is_json {
        match libgen_core::parse::parse_json(&content) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("import: JSON parse error: {e}");
                app.status_msg = Some(format!("Import failed: {e}"));
                return;
            }
        }
    } else {
        match libgen_core::parse::parse_markdown(&content) {
            Ok(l) => l,
            Err(md_err) => {
                // Markdown failed — try JSON as a fallback.
                match libgen_core::parse::parse_json(&content) {
                    Ok(l) => l,
                    Err(json_err) => {
                        tracing::warn!(
                            "import: parse failed (markdown: {md_err}, json: {json_err})"
                        );
                        app.status_msg = Some(format!("Import failed: {md_err}"));
                        return;
                    }
                }
            }
        }
    };

    // Record book count and title before the list is consumed.
    let book_count: usize = list.groups.iter().map(|g| g.books.len()).sum();
    let list_title = list.title.clone();

    let cfg = handles.config.lock().expect("config poisoned").clone();
    let mut store = match open_store(&cfg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("import: open_store: {e}");
            app.status_msg = Some(format!("Import failed: {e}"));
            return;
        }
    };
    // Reject duplicate titles.
    match store.list_id_by_title(&list.title) {
        Ok(Some(_)) => {
            tracing::warn!("import: list '{}' already exists", list.title);
            app.status_msg = Some(format!("List \"{}\" already exists", list.title));
            return;
        }
        Err(e) => {
            tracing::warn!("import: list_id_by_title: {e}");
            app.status_msg = Some(format!("Import failed: {e}"));
            return;
        }
        Ok(None) => {}
    }
    let store_id = match store.insert_list(&list) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("import: insert_list: {e}");
            app.status_msg = Some(format!("Import failed: {e}"));
            return;
        }
    };
    let search = match build_search(&cfg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("import: build_search: {e}");
            app.status_msg = Some(format!("Import failed: {e}"));
            return;
        }
    };
    let store2 = match open_store(&cfg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("import: open_store2: {e}");
            app.status_msg = Some(format!("Import failed: {e}"));
            return;
        }
    };
    let orch = libgen_core::orchestrator::Orchestrator::attach(
        store2,
        store_id,
        search,
        cfg.effective_out_dir(),
    )
    .with_query_concurrency(cfg.app.query_concurrency);
    let id = Library::id_for(store_id);
    {
        let mut lib = handles.library.lock().await;
        lib.lists.retain(|l| l.id != id);
        lib.current = id.clone();
        lib.lists.push(LoadedList::new(id.clone(), orch));
    }
    // Set goal=Complete so engine discovers + downloads.
    if let Some(orch_arc) = {
        let lib = handles.library.lock().await;
        lib.arc_for(&id)
    } {
        let mut guard = orch_arc.lock().await;
        let _ = guard.set_goal_all(Goal::Complete);
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
    info!("imported list '{}' as {id}", list_title);
    // (c) Visible feedback.
    app.status_msg = Some(format!(
        "Imported {} book(s) from \"{}\"",
        book_count, list_title
    ));
}

async fn add_manual(app: &mut AppState, handles: &EngineHandles, arg: &str) {
    const MANUAL_TITLE: &str = "Manual";
    let cfg = handles.config.lock().expect("config poisoned").clone();

    // (B) Parse title and optional author from the argument.
    let (book_title, book_authors) = parse_add_arg(arg);

    // Find the Manual list id in the store.
    let store_id = {
        let mut store = match open_store(&cfg) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("add: open_store: {e}");
                app.status_msg = Some(format!("Add failed: {e}"));
                return;
            }
        };
        match store.list_id_by_title(MANUAL_TITLE) {
            Ok(Some(id)) => id,
            Ok(None) => {
                // Create it.
                use libgen_core::model::{DownloadList, Group, ListSettings};
                let settings = ListSettings {
                    naming_template: "{authors} - {title}.{ext}".into(),
                    is_manual: true,
                    ..Default::default()
                };
                let list = DownloadList {
                    title: MANUAL_TITLE.into(),
                    settings,
                    groups: vec![Group::new(MANUAL_TITLE)],
                };
                match store.insert_list(&list) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::warn!("add: insert_list: {e}");
                        app.status_msg = Some(format!("Add failed: {e}"));
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("add: list_id_by_title: {e}");
                app.status_msg = Some(format!("Add failed: {e}"));
                return;
            }
        }
    };

    let id = Library::id_for(store_id);

    // Load orch if not already loaded.
    let already = {
        let lib = handles.library.lock().await;
        lib.arc_for(&id).is_some()
    };
    if !already {
        let search = match build_search(&cfg) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("add: build_search: {e}");
                app.status_msg = Some(format!("Add failed: {e}"));
                return;
            }
        };
        let store2 = match open_store(&cfg) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("add: open_store2: {e}");
                app.status_msg = Some(format!("Add failed: {e}"));
                return;
            }
        };
        let orch = libgen_core::orchestrator::Orchestrator::attach(
            store2,
            store_id,
            search,
            cfg.effective_out_dir(),
        )
        .with_query_concurrency(cfg.app.query_concurrency);
        let mut lib = handles.library.lock().await;
        lib.lists.push(LoadedList::new(id.clone(), orch));
    }

    // Add book to the manual list.
    let mut add_status: Option<String> = None;
    if let Some(orch_arc) = {
        let lib = handles.library.lock().await;
        lib.arc_for(&id)
    } {
        let mut guard = orch_arc.lock().await;
        match guard.add_book(&book_title, book_authors) {
            Ok((group_path, book_index)) => {
                let _ = guard.set_goal_one(&group_path, book_index, Goal::Complete);
                info!("added book '{}' to Manual list", book_title);
                add_status = Some(format!("Added '{}' to Manual", book_title));
            }
            Err(e) => {
                tracing::warn!("add: add_book: {e}");
                add_status = Some(format!("Add failed: {e}"));
            }
        }
    }

    // (A) Switch the active view to the Manual list so the user sees the result.
    {
        let mut lib = handles.library.lock().await;
        lib.current = id.clone();
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;

    // (c) Report outcome.
    app.status_msg = add_status;
}

async fn open_list(app: &mut AppState, handles: &EngineHandles, id: &str) {
    {
        let mut lib = handles.library.lock().await;
        lib.current = id.to_string();
    }
    refresh_active_view(app, handles).await;
}

async fn requery_active(app: &mut AppState, handles: &EngineHandles) {
    let Some(orch_arc) = active_orch(handles).await else {
        return;
    };
    let inflight = {
        let mut guard = orch_arc.lock().await;
        let pre = match guard.snapshot() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("requery: snapshot: {e}");
                return;
            }
        };
        use libgen_core::model::{JobState, RequestStatus};
        let to_cancel: Vec<String> = pre
            .groups
            .iter()
            .flat_map(|g| g.books.iter())
            .filter(|b| b.status != RequestStatus::Done)
            .flat_map(|b| b.candidates.iter())
            .filter(|c| {
                matches!(
                    c.job.as_ref().map(|j| &j.state),
                    Some(JobState::Resolving | JobState::Downloading | JobState::Verifying)
                )
            })
            .map(|c| c.md5.clone())
            .collect();
        if let Err(e) = guard.requery_reset() {
            tracing::warn!("requery_reset: {e}");
        }
        let _ = guard.set_goal_all(Goal::Complete);
        to_cancel
    };
    if let Ok(sched) = ensure_scheduler_from(&handles.scheduler, &handles.config, None).await {
        for md5 in &inflight {
            sched.cancel(md5).await;
        }
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
}

async fn pause_all_active(app: &mut AppState, handles: &EngineHandles) {
    let Some(orch_arc) = active_orch(handles).await else {
        return;
    };
    if let Ok(sched) = ensure_scheduler_from(&handles.scheduler, &handles.config, None).await {
        if let Err(e) = orch_arc.lock().await.pause_all(&sched).await {
            tracing::warn!("pause_all: {e}");
        }
    }
    refresh_active_view(app, handles).await;
}

// ---------------------------------------------------------------------------
// #45 — per-list pause / start / resume
// ---------------------------------------------------------------------------

/// Resolve the engine-id of the list matching `name_hint` (case-insensitive
/// substring of the title), falling back to the active list if `None`.
async fn resolve_list_id(handles: &EngineHandles, name_hint: Option<&str>) -> Option<String> {
    let lib = handles.library.lock().await;
    if let Some(hint) = name_hint {
        let lh = hint.to_lowercase();
        // Collect (id, orch) pairs first so we can drop the lib lock before
        // locking each orch.
        let pairs: Vec<(String, _)> = lib
            .lists
            .iter()
            .filter_map(|ll| lib.arc_for(&ll.id).map(|a| (ll.id.clone(), a)))
            .collect();
        drop(lib);
        for (id, orch_arc) in pairs {
            let guard = orch_arc.lock().await;
            if let Ok(snap) = guard.snapshot() {
                if snap.title.to_lowercase().contains(&lh) {
                    return Some(id);
                }
            }
        }
        None
    } else {
        Some(lib.current.clone())
    }
}

/// Pause a list (by id): pauses all in-flight downloads and marks jobs Paused.
async fn pause_list_by_id(app: &mut AppState, handles: &EngineHandles, id: &str) {
    let orch_arc = {
        let lib = handles.library.lock().await;
        lib.arc_for(id)
    };
    let Some(orch_arc) = orch_arc else { return };
    if let Ok(sched) = ensure_scheduler_from(&handles.scheduler, &handles.config, None).await {
        if let Err(e) = orch_arc.lock().await.pause_all(&sched).await {
            tracing::warn!("pause_list: {e}");
        }
    }
    refresh_active_view(app, handles).await;
}

/// Pause the active list (or a named one if `name_hint` is given).
async fn pause_list(app: &mut AppState, handles: &EngineHandles, name_hint: Option<&str>) {
    let Some(id) = resolve_list_id(handles, name_hint).await else {
        if let Some(hint) = name_hint {
            app.status_msg = Some(format!("No list matching '{hint}'"));
        }
        return;
    };
    pause_list_by_id(app, handles, &id).await;
    app.status_msg = Some("Paused".into());
}

/// Start/resume a list (by id): set goal Complete + resume paused/cancelled jobs.
async fn start_list_by_id(app: &mut AppState, handles: &EngineHandles, id: &str) {
    let orch_arc = {
        let lib = handles.library.lock().await;
        lib.arc_for(id)
    };
    let Some(orch_arc) = orch_arc else { return };
    {
        let mut guard = orch_arc.lock().await;
        let _ = guard.set_goal_all(Goal::Complete);
        if let Err(e) = guard.resume_all() {
            tracing::warn!("start_list resume_all: {e}");
        }
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
}

/// Start/resume the active list (or a named one if `name_hint` is given).
async fn start_list(app: &mut AppState, handles: &EngineHandles, name_hint: Option<&str>) {
    let Some(id) = resolve_list_id(handles, name_hint).await else {
        if let Some(hint) = name_hint {
            app.status_msg = Some(format!("No list matching '{hint}'"));
        }
        return;
    };
    start_list_by_id(app, handles, &id).await;
    app.status_msg = Some("Resumed".into());
}

/// Start/resume ALL loaded lists.
async fn start_all_lists(app: &mut AppState, handles: &EngineHandles) {
    let ids: Vec<String> = {
        let lib = handles.library.lock().await;
        lib.lists.iter().map(|ll| ll.id.clone()).collect()
    };
    for id in &ids {
        let orch_arc = {
            let lib = handles.library.lock().await;
            lib.arc_for(id)
        };
        if let Some(orch_arc) = orch_arc {
            let mut guard = orch_arc.lock().await;
            let _ = guard.set_goal_all(Goal::Complete);
            if let Err(e) = guard.resume_all() {
                tracing::warn!("start_all resume_all for {id}: {e}");
            }
        }
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
    app.status_msg = Some(format!("Resumed {} list(s)", ids.len()));
}

// ---------------------------------------------------------------------------
// #48 — delete list
// ---------------------------------------------------------------------------

/// Populate the Confirm modal for `:delete [<name>]`.
async fn show_delete_confirm(app: &mut AppState, handles: &EngineHandles, name_hint: Option<&str>) {
    let Some(id) = resolve_list_id(handles, name_hint).await else {
        if let Some(hint) = name_hint {
            app.status_msg = Some(format!("No list matching '{hint}'"));
        } else {
            app.status_msg = Some("No active list".into());
        }
        return;
    };
    // Snapshot to get title + book count.
    let orch_arc = {
        let lib = handles.library.lock().await;
        lib.arc_for(&id)
    };
    let Some(orch_arc) = orch_arc else {
        app.status_msg = Some("List not found".into());
        return;
    };
    let (title, n_books) = {
        let guard = orch_arc.lock().await;
        match guard.snapshot() {
            Ok(snap) => {
                let n: usize = snap.groups.iter().map(|g| g.books.len()).sum();
                (snap.title.clone(), n)
            }
            Err(_) => ("(unknown)".to_string(), 0),
        }
    };
    app.modal = Some(crate::app::Modal::Confirm {
        title,
        n_books,
        target_id: id,
    });
}

/// Actually delete a list after the user confirmed in the modal.
async fn execute_delete_list(app: &mut AppState, handles: &EngineHandles, id: &str) {
    let store_id: i64 = match id.strip_prefix("list").and_then(|s| s.parse().ok()) {
        Some(n) => n,
        None => {
            app.status_msg = Some(format!("Bad list id: {id}"));
            return;
        }
    };

    // Pause any in-flight downloads first.
    let orch_arc = {
        let lib = handles.library.lock().await;
        lib.arc_for(id)
    };
    if let Some(orch_arc) = orch_arc {
        let inflight: Vec<String> = {
            let guard = orch_arc.lock().await;
            guard
                .snapshot()
                .map(|snap| {
                    use libgen_core::model::JobState;
                    snap.groups
                        .iter()
                        .flat_map(|g| g.books.iter())
                        .flat_map(|b| b.candidates.iter())
                        .filter(|c| {
                            matches!(
                                c.job.as_ref().map(|j| &j.state),
                                Some(
                                    JobState::Resolving
                                        | JobState::Downloading
                                        | JobState::Verifying
                                )
                            )
                        })
                        .map(|c| c.md5.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        if !inflight.is_empty() {
            if let Ok(sched) =
                ensure_scheduler_from(&handles.scheduler, &handles.config, None).await
            {
                for md5 in &inflight {
                    sched.pause(md5).await;
                }
            }
        }
    }

    // Delete from store.
    let cfg = handles.config.lock().expect("config poisoned").clone();
    let mut store = match open_store(&cfg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("delete_list: open_store: {e}");
            app.status_msg = Some(format!("Delete failed: {e}"));
            return;
        }
    };
    if let Err(e) = store.delete_list(store_id) {
        tracing::warn!("delete_list: delete_list: {e}");
        app.status_msg = Some(format!("Delete failed: {e}"));
        return;
    }

    // Remove from library and switch active list.
    {
        let mut lib = handles.library.lock().await;
        lib.lists.retain(|l| l.id != id);
        if lib.current == id {
            lib.current = lib.lists.first().map(|l| l.id.clone()).unwrap_or_default();
        }
    }
    tracing::info!(list = %id, "list deleted");
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
    refresh_all_list_summaries(app, handles).await;
    app.status_msg = Some("List deleted".into());
}

// ---------------------------------------------------------------------------
// #53 — misc commands
// ---------------------------------------------------------------------------

/// `:refresh-mirrors` — fetch live SLUM availability and cache it.
async fn refresh_mirrors_cmd(app: &mut AppState, handles: &EngineHandles) {
    match libgen_core::slum::SlumClient::live().fetch().await {
        Ok(report) => {
            let cfg = handles.config.lock().expect("config poisoned").clone();
            let _ = report.save(cfg.slum_cache_path());
            let n = report.sites.len();
            info!("refresh_mirrors: cached {n} site(s)");
            app.status_msg = Some(format!("Mirror availability refreshed ({n} sites)"));
        }
        Err(e) => {
            tracing::warn!("refresh_mirrors: {e}");
            app.status_msg = Some(format!("Refresh failed: {e}"));
        }
    }
}

/// `:cleanup` — move leftover `.part` files to the Trash.
async fn cleanup_cmd(app: &mut AppState, handles: &EngineHandles) {
    let out_dir = handles
        .config
        .lock()
        .expect("config poisoned")
        .effective_out_dir();
    match tokio::task::spawn_blocking(move || libgen_core::orchestrator::trash_part_files(&out_dir))
        .await
    {
        Ok((count, bytes)) => {
            let msg = if count == 0 {
                "No .part files to clean up.".into()
            } else {
                format!(
                    "Moved {} .part file(s) ({:.1} MB) to Trash.",
                    count,
                    bytes as f64 / 1_048_576.0
                )
            };
            info!("cleanup: {msg}");
            app.status_msg = Some(msg);
        }
        Err(e) => {
            tracing::warn!("cleanup spawn_blocking: {e}");
            app.status_msg = Some(format!("Cleanup failed: {e}"));
        }
    }
}

/// `:add-md5 <md5>` — inject an MD5 as a manual candidate for the currently
/// selected book in the active list, then set its goal to Complete.
async fn add_md5_cmd(app: &mut AppState, handles: &EngineHandles, md5: &str) {
    let Some(orch_arc) = active_orch(handles).await else {
        app.status_msg = Some("No active list".into());
        return;
    };
    // Get the selected book's position.
    let pos = match app.flat.get(app.selected) {
        Some(fb) => (vec![fb.group_index], fb.book_index_in_group),
        None => {
            app.status_msg = Some("No book selected".into());
            return;
        }
    };
    {
        let mut guard = orch_arc.lock().await;
        if let Err(e) = guard.add_manual_candidate(&pos.0, pos.1, md5) {
            tracing::warn!("add_md5: {e}");
            app.status_msg = Some(format!("Add MD5 failed: {e}"));
            return;
        }
        let _ = guard.set_goal_one(&pos.0, pos.1, Goal::Complete);
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
    app.status_msg = Some(format!("Added MD5 {}", &md5[..md5.len().min(8)]));
}

/// Persist the staged settings draft: per-list settings → orchestrator,
/// app-wide settings → `app-config.json` next to the DB.
async fn save_settings(app: &mut AppState, handles: &EngineHandles) {
    use libgen_core::model::{Format, ListSettings};
    use std::path::Path;

    // Take the draft so we can move out of it.
    let Some(draft) = app.settings_draft.take() else {
        return;
    };

    // ── 1. Per-list settings ────────────────────────────────────────────────
    let list_settings = {
        let fmt_pref: Vec<Format> = draft.format_pref.iter().map(|s| Format::parse(s)).collect();
        let language = if draft.language.is_empty() || draft.language == "any" {
            None
        } else {
            Some(draft.language.clone())
        };
        // snapshot to get current "rest" of settings, then overlay our fields
        let base = if let Some(orch_arc) = active_orch(handles).await {
            orch_arc
                .lock()
                .await
                .snapshot()
                .map(|s| s.settings)
                .unwrap_or_default()
        } else {
            ListSettings::default()
        };
        ListSettings {
            format_pref: fmt_pref,
            language,
            auto_threshold: draft.auto_threshold,
            near_threshold: draft.near_threshold,
            keep_top: draft.keep_top,
            naming_template: draft.naming_template.clone(),
            seq_per_group: draft.seq_per_group,
            ..base
        }
    };

    if let Some(orch_arc) = active_orch(handles).await {
        let mut guard = orch_arc.lock().await;
        if let Err(e) = guard.update_settings(list_settings) {
            tracing::warn!("save_settings: update_settings failed: {e}");
        }
    }

    // ── 2. App-wide settings ─────────────────────────────────────────────────
    let cfg_dir: std::path::PathBuf = {
        let cfg = handles.config.lock().unwrap();
        cfg.db_path.parent().unwrap_or(Path::new(".")).to_path_buf()
    };
    {
        let mut cfg = handles.config.lock().unwrap();
        cfg.app.out_dir = draft.out_dir.clone();
        cfg.app.max_concurrent_downloads = draft.max_concurrent;
        cfg.app.max_attempts = draft.max_attempts;
        cfg.app.hedge_enabled = draft.hedge_enabled;
        if let Err(e) = cfg.app.save(&cfg_dir.join("app-config.json")) {
            tracing::warn!("save_settings: app-config save failed: {e}");
        }
    }

    // ── 3. Refresh view to reflect the new settings ───────────────────────────
    refresh_active_view(app, handles).await;
}
