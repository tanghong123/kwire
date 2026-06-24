// Hide the extra console window on Windows in release; harmless elsewhere.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Where rolling log files live: `<app data>/logs` (next to the library DB), so
/// the user (and post-hoc debugging) can read what the app actually did across
/// restarts — stderr alone is lost when the app isn't launched from a terminal.
fn log_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Kwire")
            .join("logs");
    }
    PathBuf::from("logs")
}

fn main() {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);

    // Daily-rolling file appender + a non-blocking writer. The guard must outlive
    // the program or buffered lines are dropped on exit, so we leak it.
    let file = tracing_appender::rolling::daily(&dir, "kwire.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file);
    std::mem::forget(guard);

    // One filter, two sinks: human-readable stderr + a plain (no-ANSI) log file.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().with_ansi(false).with_writer(file_writer))
        .init();

    // Build stamp — the first line of every run identifies exactly which build
    // this is, so "am I on the latest?" is a log lookup, not a guess.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        git_sha = env!("GIT_SHA"),
        build_time = env!("BUILD_TIME"),
        log_dir = %dir.display(),
        "Kwire starting"
    );

    libgen_app_lib::run();
}
