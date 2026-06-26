//! CLI emitter — implements [`EngineEmitter`] for printing engine lifecycle
//! events to stdout/stderr as they arrive.
//!
//! Used by `kwire search` (activity lines) and `kwire get` (lifecycle + download
//! progress).  Stdout stays clean and pipe-friendly: the live `\r` progress
//! line is only written when stdout is a TTY; non-TTY paths get periodic
//! newlines instead.

use std::io::{self, IsTerminal, Write};

use libgen_core::model::DownloadList;
use libgen_core::orchestrator::Event;
use libgen_core::queue::Progress;
use libgen_engine::{BookStatePayload, EngineEmitter};

// ---------------------------------------------------------------------------
// CliEmitter
// ---------------------------------------------------------------------------

/// Prints engine lifecycle + download events to stdout/stderr.
///
/// Lifecycle activity (searching, planned, status changes) → **stderr**.
/// Download progress → **stdout** with `\r` on TTY, periodic newlines otherwise.
pub struct CliEmitter {
    /// Whether stdout is a real terminal (TTY).  Drives `\r` vs newline for
    /// the `Bytes` progress line.
    pub is_tty: bool,
}

impl CliEmitter {
    /// Detect TTY from the real stdout.
    pub fn new() -> Self {
        CliEmitter {
            is_tty: io::stdout().is_terminal(),
        }
    }

    /// Print one download `Progress` event.  Called both by `impl
    /// EngineEmitter::emit_event` (when driven through the engine) and
    /// directly from `cmd_get` which drives the scheduler without the full
    /// engine.
    pub fn print_progress(&self, p: &Progress) {
        match p {
            Progress::Resolved {
                host, total_bytes, ..
            } => {
                let size_str = total_bytes
                    .map(super::cmd_search::human_size)
                    .unwrap_or_else(|| "?".to_string());
                eprintln!("connecting  {host}  ({size_str})");
            }
            Progress::Bytes {
                bytes_done,
                total_bytes,
                speed_bps,
                eta_secs,
                ..
            } => {
                let bd = *bytes_done;
                let tb = *total_bytes;
                let spd = *speed_bps;
                let eta = *eta_secs;
                let line = format_progress_line(bd, tb, spd, eta);
                if self.is_tty {
                    print!("\r{line}");
                    io::stdout().flush().ok();
                } else {
                    // Non-TTY: emit at 0 %, every 10 % increment, and at 100 %
                    // to avoid flooding piped consumers with thousands of lines.
                    if let Some(total) = tb {
                        let pct = bd * 100 / total.max(1);
                        if pct % 10 == 0 {
                            println!("{line}");
                        }
                    }
                }
            }
            Progress::Done {
                path,
                bytes_written,
                ..
            } => {
                if self.is_tty {
                    // End the in-place progress line with a real newline.
                    println!();
                }
                println!("saved  {}  ({bytes_written} bytes)", path.display());
            }
            Progress::Failed { error, .. } => {
                if self.is_tty {
                    println!();
                }
                eprintln!("failed  {error}");
            }
            Progress::Retrying {
                attempt,
                host,
                error,
                ..
            } => {
                eprintln!("retry #{attempt}  {host}  {error}");
            }
            Progress::FailingOver {
                from_host, error, ..
            } => {
                eprintln!("failover  from {from_host}  {error}");
            }
            _ => {}
        }
    }
}

impl EngineEmitter for CliEmitter {
    fn emit_event(&self, _list_id: &str, _shape: &DownloadList, ev: &Event) {
        match ev {
            Event::QueryStage { title, stage, .. } => {
                // E.g. "querying: The Hobbit"  or "matched: The Hobbit"
                eprintln!("{stage}: {title}");
            }
            Event::StatusChanged { title, status, .. } => {
                use libgen_core::model::RequestStatus;
                let label = match status {
                    RequestStatus::Queued => return,
                    RequestStatus::Downloading => "downloading",
                    RequestStatus::Done => "done",
                    RequestStatus::Failed { .. } => "failed",
                    _ => return,
                };
                eprintln!("{label}: {title}");
            }
            Event::Planned {
                title, destination, ..
            } => {
                eprintln!("planned  {}  →  {}", title, destination.display());
            }
            Event::Download(p) => {
                self.print_progress(p);
            }
            Event::Done => {}
        }
    }

    fn emit_book_state(&self, _payload: BookStatePayload) {}
}

// ---------------------------------------------------------------------------
// Progress-line formatter
// ---------------------------------------------------------------------------

/// Format a download-progress status line:
/// `⬇  47%  1.4 MB/s  eta 1m04s  ▰▰▰▰░░░░░░`
///
/// When `total_bytes` is `None` (content-length not known), the percentage and
/// bar are replaced with `?`.
pub fn format_progress_line(
    bytes_done: u64,
    total_bytes: Option<u64>,
    speed_bps: Option<u64>,
    eta_secs: Option<u64>,
) -> String {
    let (pct_str, bar) = match total_bytes.filter(|&t| t > 0) {
        Some(total) => {
            let pct = (bytes_done * 100 / total).min(100);
            (format!("{pct:3}%"), format_bar(pct, 10))
        }
        None => ("  ?%".to_string(), "??????????".to_string()),
    };

    let speed_str = speed_bps
        .map(format_speed)
        .unwrap_or_else(|| "?".to_string());

    let eta_str = eta_secs.map(format_eta).unwrap_or_else(|| "?".to_string());

    // ⬇ = U+2B07 DOWNWARDS BLACK ARROW
    format!("\u{2B07} {pct_str}  {speed_str}  eta {eta_str}  {bar}")
}

/// Render a progress bar with `width` cells using filled (`▰`) / empty (`░`).
fn format_bar(pct: u64, width: usize) -> String {
    let filled = (pct as usize * width / 100).min(width);
    let empty = width - filled;
    // ▰ = U+25B0 BLACK PARALLELOGRAM   ░ = U+2591 LIGHT SHADE
    "\u{25B0}".repeat(filled) + &"\u{2591}".repeat(empty)
}

/// Format a speed in bytes/sec as a human-readable string (e.g. `"1.4 MB/s"`).
fn format_speed(bps: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;
    if bps >= MB {
        format!("{:.1} MB/s", bps as f64 / MB as f64)
    } else if bps >= KB {
        format!("{:.0} KB/s", bps as f64 / KB as f64)
    } else {
        format!("{bps} B/s")
    }
}

/// Format an ETA in seconds as a compact human-readable string.
///
/// Examples: `"30s"`, `"1m04s"`, `"1h02m"`.
pub fn format_eta(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_progress_line ────────────────────────────────────────────────

    #[test]
    fn progress_line_known_total() {
        // 470 / 1000 = 47 %; speed 1.5 MB/s → rounds to "1.5 MB/s"; eta 64 s = 1m04s
        // 1_500_000 / 1_048_576 ≈ 1.43 → "{:.1}" = "1.4 MB/s"
        let line = format_progress_line(470, Some(1000), Some(1_500_000), Some(64));
        assert!(line.contains("47%"), "pct: {line:?}");
        assert!(line.contains("1.4 MB/s"), "speed: {line:?}");
        assert!(line.contains("1m04s"), "eta: {line:?}");
        assert!(line.contains('\u{2B07}'), "⬇ missing: {line:?}");
        // bar: 47 * 10 / 100 = 4 filled, 6 empty
        assert_eq!(
            line.chars().filter(|&c| c == '\u{25B0}').count(),
            4,
            "filled cells: {line:?}"
        );
        assert_eq!(
            line.chars().filter(|&c| c == '\u{2591}').count(),
            6,
            "empty cells: {line:?}"
        );
    }

    #[test]
    fn progress_line_unknown_total() {
        let line = format_progress_line(100, None, None, None);
        assert!(line.contains("?%"), "pct unknown: {line:?}");
        assert!(line.contains("??????????"), "bar unknown: {line:?}");
    }

    #[test]
    fn progress_line_complete() {
        let line = format_progress_line(1000, Some(1000), Some(2 * 1024 * 1024), Some(0));
        assert!(line.contains("100%"), "100%: {line:?}");
        // bar must be all filled
        assert_eq!(
            line.chars().filter(|&c| c == '\u{25B0}').count(),
            10,
            "all filled: {line:?}"
        );
    }

    // ── format_eta ──────────────────────────────────────────────────────────

    #[test]
    fn eta_seconds_only() {
        assert_eq!(format_eta(0), "0s");
        assert_eq!(format_eta(30), "30s");
        assert_eq!(format_eta(59), "59s");
    }

    #[test]
    fn eta_minutes_and_seconds() {
        assert_eq!(format_eta(60), "1m00s");
        assert_eq!(format_eta(64), "1m04s");
        assert_eq!(format_eta(90), "1m30s");
        assert_eq!(format_eta(3599), "59m59s");
    }

    #[test]
    fn eta_hours() {
        assert_eq!(format_eta(3600), "1h00m");
        assert_eq!(format_eta(3661), "1h01m");
        assert_eq!(format_eta(7200), "2h00m");
    }

    // ── format_bar ──────────────────────────────────────────────────────────

    #[test]
    fn bar_empty() {
        let bar = format_bar(0, 10);
        assert!(bar.chars().all(|c| c == '\u{2591}'), "all empty: {bar:?}");
        assert_eq!(bar.chars().count(), 10);
    }

    #[test]
    fn bar_full() {
        let bar = format_bar(100, 10);
        assert!(bar.chars().all(|c| c == '\u{25B0}'), "all filled: {bar:?}");
    }

    #[test]
    fn bar_half() {
        // 50% of 10 = 5 filled
        let bar = format_bar(50, 10);
        assert_eq!(bar.chars().filter(|&c| c == '\u{25B0}').count(), 5);
        assert_eq!(bar.chars().filter(|&c| c == '\u{2591}').count(), 5);
    }

    // ── emitter receives events ──────────────────────────────────────────────

    /// Verify that `CliEmitter::emit_event` matches on Download(Progress::Done)
    /// without panicking — the actual I/O is not observable in a unit test, but
    /// we can confirm no arm is missed.
    #[test]
    fn emitter_handles_download_done_without_panic() {
        let emitter = CliEmitter { is_tty: false };
        let shape = DownloadList {
            title: "test".into(),
            settings: Default::default(),
            groups: vec![],
        };
        let ev = Event::Download(Progress::Done {
            md5: "a".repeat(32),
            host: "libgen.li".into(),
            path: std::path::PathBuf::from("/tmp/test.epub"),
            bytes_written: 1024,
        });
        // Must not panic.
        emitter.emit_event("list1", &shape, &ev);
    }

    #[test]
    fn emitter_handles_query_stage_without_panic() {
        let emitter = CliEmitter { is_tty: false };
        let shape = DownloadList {
            title: "test".into(),
            settings: Default::default(),
            groups: vec![],
        };
        let ev = Event::QueryStage {
            group_path: vec![0],
            book_index: 0,
            title: "The Hobbit".into(),
            stage: "querying".into(),
        };
        emitter.emit_event("list1", &shape, &ev);
    }
}
