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
mod i18n;
mod intent;
#[cfg(test)]
mod tests;
mod textfit;
mod theme;
mod ui;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, EventStream},
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

use crate::app::{AppState, HelpPage, ListSummary, Modal, ALL_LIST_ID};
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

    // ROOT CAUSE of the empty `tui.log`: `EnvFilter::from_default_env()` with no
    // `RUST_LOG` set produces a filter with NO directives, whose default level is
    // OFF — so every `info!`/`warn!`/`error!` was silently dropped and the file
    // stayed 0 bytes. Default to `info` when `RUST_LOG` is unset (still
    // overridable via the env var) so meaningful events actually land in the log.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    Ok(guard)
}

/// Install a panic hook that records the panic message + location to the log
/// BEFORE the rest of the panic machinery runs, so an unexpected exit (e.g. the
/// SQLite WAL-contention crash) leaves a diagnosable trail in `tui.log`. Chains
/// to the previously-installed hook so terminal teardown / default output still
/// happen.
fn install_panic_logging() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        tracing::error!(panic.location = %location, "PANIC: {msg}");
        prev(info);
    }));
}

// ---------------------------------------------------------------------------
// Single-instance lock
// ---------------------------------------------------------------------------

/// Holds an exclusive advisory `flock(2)` on `<data_dir>/kwire.lock` for the
/// whole process lifetime. The lock is released automatically when the file
/// descriptor closes — i.e. when this value is dropped or the process exits
/// (even on crash), which is exactly the behaviour we want for an
/// "is another instance running?" guard.
struct InstanceLock {
    // Kept alive solely so the fd — and thus the flock — outlives `main`.
    _file: std::fs::File,
}

/// Outcome of trying to acquire the single-instance lock.
enum LockOutcome {
    /// We hold the lock; keep the guard alive for the process lifetime.
    Acquired(InstanceLock),
    /// Another live instance already holds it; we must refuse to start.
    AlreadyRunning,
}

/// Try to take the exclusive single-instance lock on `<data_dir>/kwire.lock`.
///
/// Uses a non-blocking `flock(LOCK_EX | LOCK_NB)`; a held lock returns
/// `AlreadyRunning` rather than blocking. Real I/O failures (can't open the
/// lock file, unexpected errno) propagate as `Err`.
fn acquire_instance_lock(data_dir: &std::path::Path) -> Result<LockOutcome> {
    use std::os::unix::io::AsRawFd;

    let lock_path = data_dir.join("kwire.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening lock file {}", lock_path.display()))?;

    // SAFETY: `file` owns a valid fd for the duration of this call (and beyond,
    // since we move it into `InstanceLock` on success).
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(LockOutcome::Acquired(InstanceLock { _file: file }));
    }

    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // Lock is held by another instance.
        Some(libc::EWOULDBLOCK) => Ok(LockOutcome::AlreadyRunning),
        _ => Err(err).with_context(|| format!("locking {}", lock_path.display())),
    }
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

    // Log panics (message + location) before anything else runs on a panic, so an
    // unexpected exit leaves a trail in tui.log. (TerminalGuard's hook, installed
    // later, chains on top of this one.)
    install_panic_logging();

    info!("libgen-tui starting");

    // (2) Resolve the TUI's OWN XDG data dir (independent of the desktop app) and
    // acquire a single-instance advisory lock BEFORE touching the terminal or the
    // DB. Two instances on the same data dir contend on SQLite's WAL write lock
    // and one would crash, so refuse to start a second instance up front.
    let dirs = ProjectDirs::from("", "", "kwire-tui").context("cannot determine XDG data dir")?;
    let data_dir = dirs.data_dir().to_path_buf();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;

    let _instance_lock = match acquire_instance_lock(&data_dir)? {
        LockOutcome::Acquired(lock) => lock,
        LockOutcome::AlreadyRunning => {
            let lock_path = data_dir.join("kwire.lock");
            tracing::warn!(
                lock = %lock_path.display(),
                "refusing to start: another instance already holds the lock"
            );
            eprintln!(
                "Kwire is already running (another instance holds {}). Close it first.",
                lock_path.display()
            );
            std::process::exit(1);
        }
    };

    // (3) Terminal setup.
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    // (4) Install teardown guard (also sets panic hook).
    let _guard = TerminalGuard::install();

    // (5) Build engine config (TUI uses its OWN XDG data dir, resolved above).
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
                    is_manual: vm.is_manual,
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
            tracing::debug!(?group_path, book_index, %md5, "dispatch Intent::Select");
            select_candidate(app, handles, group_path, book_index, md5).await;
        }
        Intent::RequestVariations {
            group_path,
            book_index,
            md5s,
        } => {
            request_variations(app, handles, group_path, book_index, md5s).await;
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
            // Pre-select the focused variation sub-row (if any) inside the detail.
            let selected = app.detail_variation_index(flat_index);
            app.modal = Some(Modal::Detail {
                book_flat_index: flat_index,
                selected,
                sub_focus: crate::app::DetailSubFocus::Variations,
                history_selected: 0,
            });
        }
        Intent::OpenPicker { flat_index } => {
            app.modal = Some(Modal::Picker {
                book_flat_index: flat_index,
                selected: 0,
            });
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
        Intent::ReQueryBook {
            group_path,
            book_index,
            title,
        } => {
            requery_book(app, handles, group_path, book_index, title).await;
        }
        Intent::EditBook {
            group_path,
            book_index,
            title,
            authors,
        } => {
            edit_book(app, handles, group_path, book_index, title, authors).await;
        }
        Intent::RemoveBook {
            group_path,
            book_index,
        } => {
            remove_book(app, handles, group_path, book_index).await;
        }
        Intent::MarkNotFound {
            group_path,
            book_index,
        } => {
            mark_not_found(app, handles, group_path, book_index).await;
        }
        Intent::PauseTransfer { md5 } => {
            pause_transfer(app, handles, md5).await;
        }
        Intent::CancelTransfer { md5 } => {
            cancel_transfer(app, handles, md5).await;
        }
        Intent::ResumeTransfer { md5 } => {
            resume_transfer(app, handles, md5).await;
        }
        Intent::ApplyReorganize => {
            apply_reorganize(app, handles).await;
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
        "help" => {
            app.modal = Some(Modal::Help {
                page: HelpPage::List,
                parent: None,
            })
        }
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
            } else if crate::cli::cmd_get::is_md5(arg) {
                // Auto-detect: a bare 32-hex-char argument is an MD5 → add by MD5.
                add_md5_cmd(app, handles, arg).await;
            } else {
                add_manual(app, handles, arg).await;
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
        "reorganize" => {
            reorganize_cmd(app, handles).await;
        }
        // #53 — add the selected book's series siblings to the current list
        "download-series" | "series" => {
            download_series_cmd(app, handles).await;
        }
        // #55 — toggle mouse capture on/off
        "mouse" => {
            app.toggle_mouse_capture();
            // Apply the crossterm command immediately so the terminal responds.
            if app.mouse_capture {
                let _ = execute!(io::stdout(), EnableMouseCapture);
            } else {
                let _ = execute!(io::stdout(), DisableMouseCapture);
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
///
/// Retained for tests and potential structured-add callers; the default `:add`
/// path now treats the whole argument as a single free-form query.
#[cfg_attr(not(test), allow(dead_code))]
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
    if id == ALL_LIST_ID {
        // Aggregate "All" stop: merge every loaded list into one view.
        if let Some((vm, origins)) = build_aggregate_view(handles).await {
            app.aggregate_origins = origins;
            app.set_view(vm);
        }
    } else if let Some(orch_arc) = orch_arc {
        let snap = orch_arc.lock().await.snapshot();
        if let Ok(snap) = snap {
            let vm = build_with_id(id, &snap);
            // Leaving the aggregate view: drop the stale origin map.
            app.aggregate_origins.clear();
            app.set_view(vm);
        }
    }
    refresh_all_list_summaries(app, handles).await;
}

/// Build the aggregate "All" [`ViewModel`]: every loaded list's groups
/// concatenated into one view (each group name prefixed with its list title so
/// the origin stays clear). `format_pref`/settings come from the first list.
/// Returns the merged `ViewModel` plus an `origins` map: for each aggregate
/// group index, the (owning list id, original group index) it came from, so
/// actions taken in the All view can route back to the owning list's orchestrator.
async fn build_aggregate_view(
    handles: &EngineHandles,
) -> Option<(libgen_engine::ViewModel, Vec<(String, usize)>)> {
    let pairs = {
        let lib = handles.library.lock().await;
        lib.all_arcs()
    };
    let mut groups: Vec<libgen_engine::ViewGroup> = Vec::new();
    let mut origins: Vec<(String, usize)> = Vec::new();
    let mut settings: Option<libgen_engine::ViewListSettings> = None;
    let mut format_pref: Vec<String> = Vec::new();
    for (id, orch_arc) in pairs {
        let snap = orch_arc.lock().await.snapshot();
        if let Ok(snap) = snap {
            let vm = build_with_id(id.clone(), &snap);
            if settings.is_none() {
                settings = Some(vm.settings.clone());
                format_pref = vm.format_pref.clone();
            }
            let list_title = vm.title.clone();
            for (orig_gi, mut g) in vm.groups.into_iter().enumerate() {
                g.name = format!("{} \u{203a} {}", list_title, g.name);
                groups.push(g);
                origins.push((id.clone(), orig_gi));
            }
        }
    }
    let total: usize = groups.iter().map(|g| g.books.len()).sum();
    Some((
        libgen_engine::ViewModel {
            id: ALL_LIST_ID.to_string(),
            title: "All".to_string(),
            subtitle: format!("{total} book(s) across all lists"),
            format_pref,
            settings: settings?,
            is_manual: false,
            groups,
        },
        origins,
    ))
}

/// Resolve the orchestrator + real group_path for a book action.
///
/// In a normal single-list view this is just the active orchestrator with the
/// group_path unchanged. In the aggregate "All" view it remaps the leading
/// (aggregate) group index back to the OWNING list's orchestrator and original
/// group index via `app.aggregate_origins`, so Enter/`a`/`d`/`r`/`p`/`c`/… act
/// for real instead of no-op'ing.
async fn resolve_book_orch(
    app: &AppState,
    handles: &EngineHandles,
    group_path: &[usize],
) -> Option<(
    Arc<tokio::sync::Mutex<libgen_core::orchestrator::Orchestrator>>,
    Vec<usize>,
)> {
    let lib = handles.library.lock().await;
    if lib.current == ALL_LIST_ID {
        let agg_idx = *group_path.first()?;
        let (list_id, orig_group) = app.aggregate_origin(agg_idx)?;
        let orch = lib.arc_for(&list_id)?;
        let mut gp = group_path.to_vec();
        gp[0] = orig_group;
        Some((orch, gp))
    } else {
        let id = lib.current.clone();
        let orch = lib.arc_for(&id)?;
        Some((orch, group_path.to_vec()))
    }
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
                is_manual: vm.is_manual,
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
    let Some((orch_arc, group_path)) = resolve_book_orch(app, handles, &group_path).await else {
        tracing::warn!(book_index, %md5, "select_candidate: resolve_book_orch returned None");
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
        tracing::info!(?group_path, book_index, %md5, "select_candidate: armed + goal=Complete");
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
}

/// Arm one or more candidate variations of a book for download at once (each
/// md5 fetched as its own copy). Used by the Picker's multi-select and the
/// "fetch all preferred formats" actions. Drives the book to `Complete` so the
/// engine downloads every armed variation.
async fn request_variations(
    app: &mut AppState,
    handles: &EngineHandles,
    group_path: Vec<usize>,
    book_index: usize,
    md5s: Vec<String>,
) {
    let Some((orch_arc, group_path)) = resolve_book_orch(app, handles, &group_path).await else {
        return;
    };
    {
        let mut guard = orch_arc.lock().await;
        for md5 in &md5s {
            if let Err(e) = guard.request_variation(&group_path, book_index, md5) {
                tracing::warn!("request_variation({md5}) failed: {e}");
            }
        }
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
    let Some((orch_arc, group_path)) = resolve_book_orch(app, handles, &group_path).await else {
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
    let Some((orch_arc, group_path)) = resolve_book_orch(app, handles, &group_path).await else {
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
    let Some((orch_arc, group_path)) = resolve_book_orch(app, handles, &group_path).await else {
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

// ---------------------------------------------------------------------------
// Per-transfer controls (#51) — Activity pane p / c / r
// ---------------------------------------------------------------------------

/// Pause a single in-flight transfer (identified by md5) via the scheduler.
async fn pause_transfer(app: &mut AppState, handles: &EngineHandles, md5: String) {
    if let Ok(sched) = ensure_scheduler_from(&handles.scheduler, &handles.config, None).await {
        sched.pause(&md5).await;
    }
    refresh_active_view(app, handles).await;
}

/// Cancel a single in-flight transfer (identified by md5) via the scheduler.
async fn cancel_transfer(app: &mut AppState, handles: &EngineHandles, md5: String) {
    if let Ok(sched) = ensure_scheduler_from(&handles.scheduler, &handles.config, None).await {
        sched.cancel(&md5).await;
    }
    refresh_active_view(app, handles).await;
}

/// Resume / retry a paused or cancelled transfer by md5.
///
/// Finds the variation's owning book via the current flat list, then calls
/// `Orchestrator::resume_variation` to set its job state back to `Pending`.
async fn resume_transfer(app: &mut AppState, handles: &EngineHandles, md5: String) {
    // Locate the book that contains this variation using the already-rendered
    // flat list — no extra I/O needed for the lookup.
    let location: Option<(Vec<usize>, usize)> = app.flat.iter().find_map(|fb| {
        if fb.book.versions.iter().any(|v| v.md5 == md5) {
            Some((vec![fb.group_index], fb.book_index_in_group))
        } else {
            None
        }
    });
    if let Some((group_path, book_index)) = location {
        let Some((orch_arc, group_path)) = resolve_book_orch(app, handles, &group_path).await
        else {
            return;
        };
        let mut guard = orch_arc.lock().await;
        if let Err(e) = guard.resume_variation(&group_path, book_index, &md5) {
            tracing::warn!("resume_transfer {md5}: {e}");
        }
    }
    handles.engine_wake.notify_one();
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

    // (B) Treat the whole argument as a single FREE-FORM "title + author" query
    // (matched against each candidate's title+author combined), rather than
    // splitting it into a structured title/author pair — a comma-free
    // "Steve Jobs Walter Isaacson" otherwise mis-ranks malformed catalog entries.
    let book_title = arg.trim().to_string();

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
        match guard.add_book_freeform(&book_title) {
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

// ---------------------------------------------------------------------------
// #52 — reorganize already-downloaded files to the current naming/folder scheme
// ---------------------------------------------------------------------------

/// The cross-list reorganize diff: every `(current path → correct path)` pair a
/// reorganize would move, gathered over all loaded lists. Mirrors the desktop's
/// `reorganize_diff` command. Empty ⇒ the on-disk layout is already canonical.
async fn reorganize_diff_all(handles: &EngineHandles) -> Vec<(String, String)> {
    let arcs = {
        let lib = handles.library.lock().await;
        lib.all_arcs()
    };
    let mut out = Vec::new();
    for (_, orch) in &arcs {
        let mut g = orch.lock().await;
        if let Ok(diff) = g.reorganize_plan_diff() {
            out.extend(diff);
        }
    }
    out
}

/// Apply the reorganize across all loaded lists, mirroring the desktop's
/// `reorganize_files`: each list's own download folder is passed as a *sibling
/// root* to the others, so a book shared by two lists is duplicated (copied) into
/// each rather than moved back and forth. Returns `(moved, skipped, errors)`.
async fn reorganize_apply_all(handles: &EngineHandles) -> (usize, usize, usize) {
    let arcs = {
        let lib = handles.library.lock().await;
        lib.all_arcs()
    };
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
    (moved, skipped, errors)
}

/// `:reorganize` — preview the moves that would bring already-downloaded files
/// into the current naming/folder/sub-grouping layout, opening the Reorganize
/// modal. If nothing is misplaced, just sets a status message.
async fn reorganize_cmd(app: &mut AppState, handles: &EngineHandles) {
    let diff = reorganize_diff_all(handles).await;
    if diff.is_empty() {
        app.status_msg = Some("Nothing to reorganize — layout is already current.".into());
    } else {
        let n = diff.len();
        info!("reorganize: {n} file(s) would move");
        app.modal = Some(crate::app::Modal::Reorganize { diff, selected: 0 });
    }
}

/// Apply a previewed reorganize (the user pressed `y` in the modal), move files
/// on disk, then refresh the view and report the outcome.
async fn apply_reorganize(app: &mut AppState, handles: &EngineHandles) {
    let (moved, skipped, errors) = reorganize_apply_all(handles).await;
    info!("reorganize applied: moved={moved} skipped={skipped} errors={errors}");
    refresh_active_view(app, handles).await;
    app.status_msg = Some(format!(
        "Reorganized: {moved} moved, {skipped} skipped, {errors} error(s)."
    ));
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

/// Decide what `:download-series` should add, given the seed book's title and
/// the (already-performed) series lookup result. Pure and I/O-free so the
/// no-title / no-series / found-siblings paths can be unit-tested without a
/// network or an engine.
///
/// On success returns `(member titles in reading order, series name)`. `Err`
/// carries a user-facing message for each failure path.
pub(crate) fn plan_series_add(
    seed_title: &str,
    series: Option<&libgen_core::series::Series>,
) -> Result<(Vec<String>, String), String> {
    if seed_title.trim().is_empty() {
        return Err("No title on the selected book".into());
    }
    let series = match series {
        Some(s) if !s.members.is_empty() => s,
        _ => return Err("No series found for the selected book".into()),
    };
    let titles: Vec<String> = series
        .members
        .iter()
        .map(|m| m.title.clone())
        .filter(|t| !t.trim().is_empty())
        .collect();
    if titles.is_empty() {
        return Err("No series found for the selected book".into());
    }
    Ok((titles, series.name.clone()))
}

/// `:download-series` (alias `:series`) — for the currently selected book, look
/// up the series it belongs to and add the sibling volumes to the CURRENT list,
/// then query them.
///
/// Mirrors `commands::download_series`' locking: the seed `(title, author)` is
/// read under a BRIEF orch lock that is then DROPPED; the series lookup (the
/// network phase) runs with NO lock held; only afterwards is the orch re-locked
/// to add the discovered siblings.
async fn download_series_cmd(app: &mut AppState, handles: &EngineHandles) {
    let Some(orch_arc) = active_orch(handles).await else {
        app.status_msg = Some("No active list".into());
        return;
    };
    let Some(fb) = app.flat.get(app.selected) else {
        app.status_msg = Some("No book selected".into());
        return;
    };
    let group_index = fb.group_index;
    let book_index = fb.book_index_in_group;

    // 1. Resolve the seed (title, author) under a BRIEF orch lock, then drop it.
    let (title, author) = {
        let guard = orch_arc.lock().await;
        let snap = match guard.snapshot() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("download_series: snapshot: {e}");
                app.status_msg = Some(format!("Series lookup failed: {e}"));
                return;
            }
        };
        match snap
            .groups
            .get(group_index)
            .and_then(|g| g.books.get(book_index))
        {
            Some(b) => (b.input.title.clone(), b.input.authors.join(", ")),
            None => {
                app.status_msg = Some("No book selected".into());
                return;
            }
        }
    }; // orch lock dropped here — the network lookup runs OFF any lock.

    if title.trim().is_empty() {
        app.status_msg = Some("No title on the selected book".into());
        return;
    }

    // 2. Series lookup — NO lock held across the network (replay when configured).
    let series_client = {
        let cfg = handles.config.lock().expect("config poisoned").clone();
        match &cfg.replay_dir {
            Some(dir) => libgen_core::series::SeriesClient::replay(dir.join("series")),
            None => libgen_core::series::SeriesClient::live(),
        }
    };
    let series = match series_client.lookup(&title, &author).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("download_series: lookup: {e}");
            app.status_msg = Some(format!("Series lookup failed: {e}"));
            return;
        }
    };

    // 3. Decide what to add (pure; also surfaces the no-series error path).
    let (titles, series_name) = match plan_series_add(&title, series.as_ref()) {
        Ok(plan) => plan,
        Err(e) => {
            app.status_msg = Some(e);
            return;
        }
    };

    // 4. Add the siblings to the CURRENT list and queue each for discovery.
    let mut added = 0usize;
    {
        let mut guard = orch_arc.lock().await;
        for t in &titles {
            match guard.add_book(t, vec![]) {
                Ok((group_path, book_idx)) => {
                    let _ = guard.set_goal_one(&group_path, book_idx, Goal::Complete);
                    added += 1;
                }
                Err(e) => tracing::warn!("download_series: add_book '{t}': {e}"),
            }
        }
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
    app.status_msg = Some(format!(
        "Added {added} book(s) from the series \"{series_name}\""
    ));
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
    // Capture the layout-affecting fields *before* this save so we can tell
    // whether the on-disk layout (naming template / sub-grouping) just changed —
    // and, if so, offer to reorganize already-downloaded files (#52).
    let (list_settings, old_naming, old_seq) = {
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
        let old_naming = base.naming_template.clone();
        let old_seq = base.seq_per_group;
        let settings = ListSettings {
            format_pref: fmt_pref,
            language,
            auto_threshold: draft.auto_threshold,
            near_threshold: draft.near_threshold,
            keep_top: draft.keep_top,
            naming_template: draft.naming_template.clone(),
            seq_per_group: draft.seq_per_group,
            ..base
        };
        (settings, old_naming, old_seq)
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
    let old_out_dir = {
        let cfg = handles.config.lock().unwrap();
        cfg.app.out_dir.clone()
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

    // ── 4. If a layout-affecting setting changed (naming template, download
    //       folder, or sub-grouping), check whether already-downloaded files are
    //       now out of place and, if so, prompt to reorganize them (#52). ───────
    let layout_changed = draft.naming_template != old_naming
        || draft.seq_per_group != old_seq
        || draft.out_dir != old_out_dir;
    if layout_changed {
        let n = reorganize_diff_all(handles).await.len();
        if n > 0 {
            info!("save_settings: layout changed, {n} file(s) can be reorganized");
            app.status_msg = Some(format!(
                "Layout changed: {n} downloaded file(s) can be moved — run :reorganize"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// #49 / #50 — book-level actions
// ---------------------------------------------------------------------------

/// Re-query a single book with a user-supplied corrected title.
/// Calls `edit_book_input` to update the metadata + reset to Queued, then
/// wakes the engine so it re-runs discovery on its next tick.
async fn requery_book(
    app: &mut AppState,
    handles: &EngineHandles,
    group_path: Vec<usize>,
    book_index: usize,
    title: String,
) {
    let Some((orch_arc, group_path)) = resolve_book_orch(app, handles, &group_path).await else {
        app.status_msg = Some("No active list".into());
        return;
    };
    {
        let mut guard = orch_arc.lock().await;
        match guard.edit_book_input(&group_path, book_index, &title, vec![]) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("requery_book: edit_book_input: {e}");
                app.status_msg = Some(format!("Re-query failed: {e}"));
                return;
            }
        }
        let _ = guard.set_goal_one(&group_path, book_index, Goal::Complete);
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
    app.status_msg = Some(format!("Re-querying '{title}'…"));
}

/// Edit a book's title/authors metadata and re-queue it for discovery.
async fn edit_book(
    app: &mut AppState,
    handles: &EngineHandles,
    group_path: Vec<usize>,
    book_index: usize,
    title: String,
    authors: Vec<String>,
) {
    let Some((orch_arc, group_path)) = resolve_book_orch(app, handles, &group_path).await else {
        app.status_msg = Some("No active list".into());
        return;
    };
    {
        let mut guard = orch_arc.lock().await;
        match guard.edit_book_input(&group_path, book_index, &title, authors) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("edit_book: edit_book_input: {e}");
                app.status_msg = Some(format!("Edit failed: {e}"));
                return;
            }
        }
        let _ = guard.set_goal_one(&group_path, book_index, Goal::Complete);
    }
    handles.engine_wake.notify_one();
    refresh_active_view(app, handles).await;
    app.status_msg = Some(format!("Updated '{title}' — re-queuing…"));
}

/// Remove a book from the active list after user confirmation.
async fn remove_book(
    app: &mut AppState,
    handles: &EngineHandles,
    group_path: Vec<usize>,
    book_index: usize,
) {
    let Some((orch_arc, group_path)) = resolve_book_orch(app, handles, &group_path).await else {
        app.status_msg = Some("No active list".into());
        return;
    };
    {
        let mut guard = orch_arc.lock().await;
        match guard.remove_book(&group_path, book_index) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("remove_book: {e}");
                app.status_msg = Some(format!("Remove failed: {e}"));
                return;
            }
        }
    }
    // Clamp selection so we don't point past the end of the (now-shorter) list.
    if app.selected > 0 {
        app.selected -= 1;
    }
    refresh_active_view(app, handles).await;
    app.status_msg = Some("Book removed".into());
}

/// Mark a book as not-found (user-initiated; the engine won't retry it).
async fn mark_not_found(
    app: &mut AppState,
    handles: &EngineHandles,
    group_path: Vec<usize>,
    book_index: usize,
) {
    let Some((orch_arc, group_path)) = resolve_book_orch(app, handles, &group_path).await else {
        app.status_msg = Some("No active list".into());
        return;
    };
    {
        let mut guard = orch_arc.lock().await;
        match guard.mark_not_found(&group_path, book_index) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("mark_not_found: {e}");
                app.status_msg = Some(format!("Mark failed: {e}"));
                return;
            }
        }
    }
    refresh_active_view(app, handles).await;
    app.status_msg = Some("Marked as not found".into());
}
