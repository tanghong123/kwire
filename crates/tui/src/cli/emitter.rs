//! CLI emitter — implements [`EngineEmitter`] for printing engine lifecycle
//! events to stdout/stderr as they arrive.
//!
//! Used by `kwire search` (activity lines) and `kwire get` (lifecycle + download
//! progress).  Stdout stays clean and pipe-friendly: the live `\r` progress
//! line is only written when stdout is a TTY; non-TTY paths get periodic
//! newlines instead.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use libgen_core::download::current_edge;
use libgen_core::model::DownloadList;
use libgen_core::orchestrator::Event;
use libgen_core::queue::Progress;
use libgen_core::search::{SearchEvent, SearchObserver};
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
    /// Uppercased format label (e.g. `"EPUB"`) for the desktop-style download
    /// chronicle lines, when known.  `None` (bare-md5 path) omits the label.
    pub format_label: Option<String>,
    /// First host a leg resolved on — the "started on" host.  Used to decide
    /// when the actual serving CDN edge differs (→ a "serving from" line).
    /// `Mutex`/`Atomic` (not `Cell`/`RefCell`) so the type stays `Sync` for the
    /// `EngineEmitter` trait.
    start_host: Mutex<Option<String>>,
    /// Whether the one-shot "serving from <edge>" line has already been emitted.
    serving_emitted: AtomicBool,
}

impl CliEmitter {
    /// Construct an emitter, detecting TTY from the real stdout and carrying an
    /// uppercased format label (e.g. `"EPUB"`) for the chronicle lines when known
    /// (`None` for the bare-md5 / search paths, which omit the label).
    pub fn with_label(format_label: Option<String>) -> Self {
        CliEmitter {
            is_tty: io::stdout().is_terminal(),
            format_label,
            start_host: Mutex::new(None),
            serving_emitted: AtomicBool::new(false),
        }
    }

    /// Test-only constructor with an explicit `is_tty` (real stdout detection is
    /// unreliable under the test harness).
    #[cfg(test)]
    pub fn for_test(is_tty: bool) -> Self {
        CliEmitter {
            is_tty,
            format_label: None,
            start_host: Mutex::new(None),
            serving_emitted: AtomicBool::new(false),
        }
    }

    /// Render the chronicle label prefix: `"EPUB "` when known, else empty.
    fn label_prefix(&self) -> String {
        match &self.format_label {
            Some(l) => format!("{l} "),
            None => String::new(),
        }
    }

    /// Wipe the in-place `\r` progress bar from the current line so a chronicle
    /// event prints on a CLEAN line instead of jamming onto the bar's leftover
    /// cells. No-op when stdout isn't a TTY (piped output has no live bar). The
    /// next `Bytes` render redraws the bar with its own `\r` on a fresh line.
    fn clear_progress_line(&self) {
        if self.is_tty {
            // \r → column 0, then \x1b[K → erase to end of line.
            print!("\r\u{1b}[K");
            io::stdout().flush().ok();
        }
    }

    /// Current terminal width in columns (for the full-width progress bar),
    /// falling back to 80 when it can't be queried (e.g. piped output).
    fn term_width(&self) -> usize {
        crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80)
    }

    /// Print one download `Progress` event.  Called both by `impl
    /// EngineEmitter::emit_event` (when driven through the engine) and
    /// directly from `cmd_get` which drives the scheduler without the full
    /// engine.
    pub fn print_progress(&self, p: &Progress) {
        let prefix = self.label_prefix();
        match p {
            Progress::Resolved { host, .. } => {
                // Desktop-style chronicle: announce the start ONCE, on the first
                // leg to resolve (the "started on" host).
                if let Ok(mut start) = self.start_host.lock() {
                    if start.is_none() {
                        *start = Some(host.clone());
                        self.clear_progress_line();
                        eprintln!("{prefix}started on {host}");
                    }
                }
            }
            Progress::Bytes {
                md5,
                bytes_done,
                total_bytes,
                speed_bps,
                eta_secs,
                ..
            } => {
                // Surface the real CDN edge ONCE, when it differs from the host we
                // started on (the mirror front-door) — "serving from <edge>".
                if !self.serving_emitted.load(Ordering::Relaxed) {
                    if let Some(edge) = current_edge(md5) {
                        let started = self.start_host.lock().ok().and_then(|s| s.clone());
                        if started.as_deref() != Some(edge.as_str()) {
                            self.clear_progress_line();
                            eprintln!("{prefix}serving from {edge}");
                            self.serving_emitted.store(true, Ordering::Relaxed);
                        }
                    }
                }
                let bd = *bytes_done;
                let tb = *total_bytes;
                let spd = *speed_bps;
                let eta = *eta_secs;
                let line = format_progress_line(bd, tb, spd, eta, self.term_width());
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
                host,
                path,
                bytes_written,
                ..
            } => {
                // Wipe the in-place progress bar so the completion lines print on
                // their own clean line (not jammed onto the bar's last frame).
                self.clear_progress_line();
                // Chronicle (stderr) + the saved file location (stdout, pipe-friendly).
                let size = super::cmd_search::human_size(*bytes_written);
                eprintln!("{prefix}completed on {host} ({size})");
                println!("saved  {}", path.display());
            }
            Progress::Failed { error, .. } => {
                self.clear_progress_line();
                eprintln!("failed  {error}");
            }
            Progress::Retrying {
                attempt,
                host,
                error,
                ..
            } => {
                self.clear_progress_line();
                eprintln!("retry #{attempt}  {host}  {error}");
            }
            Progress::FailingOver {
                from_host, error, ..
            } => {
                self.clear_progress_line();
                eprintln!("failover  from {from_host}  {error}");
            }
            // Lead-up activity: a resume from an existing `.part` — one concise line
            // so the user sees the transfer is continuing, not starting fresh.
            Progress::Resuming { host, offset, .. } => {
                self.clear_progress_line();
                let off = super::cmd_search::human_size(*offset);
                eprintln!("{prefix}resuming on {host} (from {off})");
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// CliSearchObserver
// ---------------------------------------------------------------------------

/// Streams search-stage activity to **stderr** as the client works through
/// mirrors — mirroring the download chronicle — so the user sees what is being
/// tried *before* the final verdict. Also records the distinct hosts queried, in
/// order, so the caller can build a precise "no candidates on any mirror
/// (tried: …)" message on a true miss instead of a bare "no candidates found".
pub struct CliSearchObserver {
    /// Distinct hosts queried, in first-seen order.
    tried: Mutex<Vec<String>>,
}

impl CliSearchObserver {
    pub fn new() -> Self {
        CliSearchObserver {
            tried: Mutex::new(Vec::new()),
        }
    }

    /// The distinct hosts queried so far, in order — for the exhaustion message.
    pub fn tried_hosts(&self) -> Vec<String> {
        self.tried.lock().map(|t| t.clone()).unwrap_or_default()
    }

    fn note_host(&self, host: &str) {
        if let Ok(mut t) = self.tried.lock() {
            if !t.iter().any(|h| h == host) {
                t.push(host.to_string());
            }
        }
    }
}

impl Default for CliSearchObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchObserver for CliSearchObserver {
    fn on_event(&self, ev: &SearchEvent<'_>) {
        match ev {
            SearchEvent::Querying { host, .. } => {
                self.note_host(host);
                eprintln!("  searching {host}…");
            }
            SearchEvent::MirrorResult { host, count } => {
                if *count == 0 {
                    eprintln!("  {host}: 0 results");
                } else {
                    eprintln!("  {host}: {count} candidates");
                }
            }
            SearchEvent::MirrorError { host, error } => {
                eprintln!("  {host}: error ({error})");
            }
        }
    }
}

impl EngineEmitter for CliEmitter {
    fn emit_event(&self, _list_id: &str, _shape: &DownloadList, ev: &Event) {
        match ev {
            Event::QueryStage { title, stage, .. } => {
                // E.g. "querying: The Hobbit"  or "matched: The Hobbit"
                self.clear_progress_line();
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
                self.clear_progress_line();
                eprintln!("{label}: {title}");
            }
            Event::Planned {
                title, destination, ..
            } => {
                self.clear_progress_line();
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

/// Format a download-progress status line.
///
/// When `total_bytes` is known (and non-zero) the full readout is rendered:
/// `⬇  47%  1.4 MB/s  eta 1m04s  ▰▰▰▰░░░░░░`.
///
/// When `total_bytes` is `None` (or 0) — common for libgen CDN mirrors that omit
/// Content-Length — an *indeterminate* readout is rendered instead of a bogus
/// `?%`/`??????????`/`eta ?`: the bytes downloaded so far plus speed, e.g.
/// `⬇ 5.2 MB · 310 KB/s`.
///
/// `term_width` is the terminal width in columns; the progress bar grows to fill
/// whatever space is left after the `⬇ pct speed eta ` prefix, so it spans the
/// full width instead of a fixed short stub.
pub fn format_progress_line(
    bytes_done: u64,
    total_bytes: Option<u64>,
    speed_bps: Option<u64>,
    eta_secs: Option<u64>,
    term_width: usize,
) -> String {
    let speed_str = speed_bps
        .map(format_speed)
        .unwrap_or_else(|| "?".to_string());

    match total_bytes.filter(|&t| t > 0) {
        Some(total) => {
            let pct = (bytes_done * 100 / total).min(100);
            let eta_str = eta_secs.map(format_eta).unwrap_or_else(|| "?".to_string());
            // ⬇ = U+2B07 DOWNWARDS BLACK ARROW. Render the prefix first, measure its
            // display width, then let the bar fill the rest of the terminal width.
            let prefix = format!("\u{2B07} {pct:3}%  {speed_str}  eta {eta_str}  ");
            let prefix_w = crate::textfit::display_width(&prefix);
            // Leave one trailing column so the bar never wraps to the next row;
            // floor at a small width so a very narrow terminal still shows a bar.
            let bar_w = term_width.saturating_sub(prefix_w + 1).clamp(4, 200);
            let bar = format_bar(pct, bar_w);
            format!("{prefix}{bar}")
        }
        None => {
            // Indeterminate: show progress as bytes-so-far + speed.
            // · = U+00B7 MIDDLE DOT
            let done_str = super::cmd_search::human_size(bytes_done);
            format!("\u{2B07} {done_str} \u{00B7} {speed_str}")
        }
    }
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
        let line = format_progress_line(470, Some(1000), Some(1_500_000), Some(64), 80);
        assert!(line.contains("47%"), "pct: {line:?}");
        assert!(line.contains("1.4 MB/s"), "speed: {line:?}");
        assert!(line.contains("1m04s"), "eta: {line:?}");
        assert!(line.contains('\u{2B07}'), "⬇ missing: {line:?}");
        // The bar fills the remaining terminal width: filled + empty cells together
        // are far wider than the old fixed 10-cell stub, and the filled fraction
        // tracks the percentage.
        let filled = line.chars().filter(|&c| c == '\u{25B0}').count();
        let empty = line.chars().filter(|&c| c == '\u{2591}').count();
        let cells = filled + empty;
        assert!(
            cells > 30,
            "bar should span the width (got {cells} cells): {line:?}"
        );
        assert_eq!(filled, 47 * cells / 100, "filled tracks pct: {line:?}");
        // The whole line fits within the terminal width.
        assert!(
            crate::textfit::display_width(&line) <= 80,
            "line width {} > 80: {line:?}",
            crate::textfit::display_width(&line)
        );
    }

    #[test]
    fn progress_line_fills_terminal_width() {
        // A wider terminal → a wider bar (the bar is responsive, not fixed).
        let narrow = format_progress_line(500, Some(1000), Some(1024), Some(10), 40);
        let wide = format_progress_line(500, Some(1000), Some(1024), Some(10), 120);
        let cells = |s: &str| {
            s.chars()
                .filter(|&c| c == '\u{25B0}' || c == '\u{2591}')
                .count()
        };
        assert!(
            cells(&wide) > cells(&narrow),
            "wider terminal → wider bar: {} vs {}",
            cells(&wide),
            cells(&narrow)
        );
        assert!(crate::textfit::display_width(&wide) <= 120);
    }

    #[test]
    fn progress_line_unknown_total() {
        // No Content-Length: render bytes-so-far + speed, NOT a bogus %/bar/eta.
        // 5_452_595 bytes ≈ 5.2 MB; 317_440 B/s ≈ 310 KB/s.
        let line = format_progress_line(5_452_595, None, Some(317_440), None, 80);
        assert!(line.contains("5.2 MB"), "bytes: {line:?}");
        assert!(line.contains("310 KB/s"), "speed: {line:?}");
        assert!(line.contains('\u{2B07}'), "⬇ missing: {line:?}");
        // Must NOT show the old indeterminate placeholders.
        assert!(!line.contains("?%"), "should not show ?%: {line:?}");
        assert!(
            !line.contains('\u{2591}') && !line.contains('?'),
            "should not show a ?-bar: {line:?}"
        );
    }

    #[test]
    fn progress_line_complete() {
        let line = format_progress_line(1000, Some(1000), Some(2 * 1024 * 1024), Some(0), 80);
        assert!(line.contains("100%"), "100%: {line:?}");
        // bar must be all filled — no empty cells at 100 %.
        assert_eq!(
            line.chars().filter(|&c| c == '\u{2591}').count(),
            0,
            "no empty cells at 100%: {line:?}"
        );
        assert!(
            line.chars().filter(|&c| c == '\u{25B0}').count() > 10,
            "full-width filled bar: {line:?}"
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
        let emitter = CliEmitter::for_test(false);
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
        let emitter = CliEmitter::for_test(false);
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
