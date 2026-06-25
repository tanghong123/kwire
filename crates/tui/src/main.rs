//! `libgen-tui` — ratatui terminal frontend for `libgen-engine`.
//!
//! § Architecture (TUI.md §3):
//! - Terminal setup: raw mode + alternate screen + mouse capture.
//! - A `TerminalGuard` (+ panic hook) guarantees teardown on any exit path.
//! - File-only tracing to `$XDG_STATE_HOME/kwire/tui.log`; never stdout/stderr.
//! - Event loop: `tokio::select!` over terminal input, engine events, and a
//!   120 ms redraw tick.
//! - `AppState::on_input` is pure; the event loop dispatches its `Intent`.
//! - Stage 2: loads a fixture list and projects a `ViewModel` to render.

mod app;
mod guard;
mod intent;
#[cfg(test)]
mod tests;
mod theme;
mod ui;

use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{EnableMouseCapture, EventStream},
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use directories::ProjectDirs;
use futures::StreamExt;
use libgen_core::parse::parse_markdown;
use libgen_engine::viewmodel::build_with_id;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::time;
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, EnvFilter};

use crate::app::{AppState, Modal};
use crate::guard::TerminalGuard;
use crate::intent::Intent;

// ---------------------------------------------------------------------------
// Logging setup
// ---------------------------------------------------------------------------

/// Install a file-only tracing subscriber.  Returns the `WorkerGuard` that must
/// be kept alive for the duration of the process (dropping it flushes + closes
/// the log file).
fn setup_logging() -> Result<WorkerGuard> {
    let dirs = ProjectDirs::from("", "", "kwire").context("cannot determine XDG state dir")?;
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
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // (1) Logging — file only; must be set up BEFORE we enter the alternate screen.
    let _log_guard = setup_logging().unwrap_or_else(|e| {
        // If we can't open the log file, swallow the error so the TUI still
        // starts.  We'll just lose log output.
        eprintln!("warning: could not set up file logging: {e}");
        // Return a dummy guard by using the tracing_appender sink.
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

    // (4) Build initial AppState.
    let mut app = AppState::new();

    // (5) Stage 2 bootstrap: load the Jeremy public-domain fixture from the
    //     fixtures/ dir bundled in the repo, project a ViewModel, and set it.
    let fixture_md = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("jeremy_public_domain_list.md");

    if let Ok(content) = std::fs::read_to_string(&fixture_md) {
        match parse_markdown(&content) {
            Ok(list) => {
                info!("loaded fixture: {}", fixture_md.display());
                let vm = build_with_id("fixture".into(), &list);
                app.set_view(vm);
            }
            Err(e) => {
                tracing::warn!("failed to parse fixture: {e}");
            }
        }
    } else {
        tracing::warn!("fixture not found at {}", fixture_md.display());
    }

    // (6) Event loop.
    run_loop(&mut terminal, &mut app).await?;

    Ok(())
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
) -> Result<()> {
    let mut input = EventStream::new();
    let mut tick_interval = time::interval(Duration::from_millis(120));

    // Initial draw.
    terminal.draw(|f| ui::render(f, app))?;

    loop {
        tokio::select! {
            // Terminal input
            Some(Ok(ev)) = input.next() => {
                let intent = app.on_input(ev);
                match intent {
                    Intent::Quit => break,
                    Intent::Command(line) => {
                        if handle_command(&line, app) {
                            break;
                        }
                    }
                    Intent::OpenDetail { flat_index } => {
                        app.modal = Some(Modal::Detail { book_flat_index: flat_index });
                    }
                    Intent::OpenPicker { flat_index } => {
                        app.modal = Some(Modal::Picker { book_flat_index: flat_index, selected: 0 });
                    }
                    Intent::OpenHelp => {
                        app.modal = Some(Modal::Help);
                    }
                    Intent::OpenFile(path) => {
                        let _ = std::process::Command::new("open")
                            .arg(&path)
                            .spawn();
                    }
                    Intent::RevealFile(path) => {
                        let parent = Path::new(&path)
                            .parent()
                            .unwrap_or(Path::new("."))
                            .to_owned();
                        let _ = std::process::Command::new("open")
                            .arg("-R")
                            .arg(&path)
                            .spawn()
                            .or_else(|_| {
                                std::process::Command::new("open")
                                    .arg(&parent)
                                    .spawn()
                            });
                    }
                    Intent::Pause { .. } => {
                        // Stage 3 stub: engine call not yet wired.
                    }
                    Intent::Cancel { .. } => {
                        // Stage 3 stub: engine call not yet wired.
                    }
                    Intent::Redraw
                    | Intent::Select { .. }
                    | Intent::Retry { .. } => {
                        // Stage 2: engine calls for Select/Retry are Stage 3;
                        // for now just redraw.
                    }
                }
            }

            // Redraw tick (spinner + progress bars)
            _ = tick_interval.tick() => {
                app.on_tick();
            }
        }

        terminal.draw(|f| ui::render(f, app))?;
    }

    info!("libgen-tui exiting");
    Ok(())
}

/// Command-line dispatcher (`:` commands per TUI.md §6). Returns `true` when the
/// command requests a graceful exit (`:quit`/`:q`).
///
/// The engine-backed commands (`:import`, `:add`, `:open`, `:requery`,
/// `:pause-all`) are recognized and logged here; their orchestrator wiring lands
/// when the multi-list engine driver is mounted. The pure UI commands
/// (`:settings`, `:help`, `:quit`) take effect immediately.
fn handle_command(line: &str, app: &mut AppState) -> bool {
    let line = line.trim();
    info!("command: {:?}", line);
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    match cmd {
        "quit" | "q" => return true,
        "settings" => app.modal = Some(Modal::Settings),
        "help" => app.modal = Some(Modal::Help),
        "import" | "add" | "open" | "requery" | "pause-all" => {
            // Recognized; engine wiring lands with the multi-list driver.
            info!("command {:?} recognized (engine wiring pending)", cmd);
        }
        "" => {}
        other => tracing::warn!("unknown command: {:?}", other),
    }
    false
}
