//! CLI emitter — implements [`EngineEmitter`] for printing engine lifecycle
//! events to stdout/stderr as they arrive.
//!
//! Used by `kwire search` (activity lines) and `kwire get` (lifecycle + download
//! progress).  Stdout stays clean and pipe-friendly: the live `\r` progress
//! line is only written when stdout is a TTY; non-TTY paths get periodic
//! newlines instead.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use libgen_core::download::current_edge;
use libgen_core::model::DownloadList;
use libgen_core::orchestrator::Event;
use libgen_core::queue::Progress;
use libgen_core::search::{SearchEvent, SearchObserver};
use libgen_engine::{BookStatePayload, EngineEmitter};

/// Braille-dots spinner frames, advanced one step per progress render. The
/// classic 10-frame rotating-dot cycle reads as a smooth spin in a single cell
/// and animates even in the indeterminate "connecting / ?% / ? eta" phase, where
/// it's the only sign of life. (A bar-shimmer set like `⣷⣯⣟⡿⢿⣻⣽⣾` also reads
/// well, but the rotating dots are the most universally recognised.)
const SPINNER: [&str; 10] = [
    "\u{280B}", // ⠋
    "\u{2819}", // ⠙
    "\u{2839}", // ⠹
    "\u{2838}", // ⠸
    "\u{283C}", // ⠼
    "\u{2834}", // ⠴
    "\u{2826}", // ⠦
    "\u{2827}", // ⠧
    "\u{2807}", // ⠇
    "\u{280F}", // ⠏
];

// ---------------------------------------------------------------------------
// CursorGuard
// ---------------------------------------------------------------------------

/// Hides the terminal cursor for the lifetime of a download and restores it on
/// `Drop` — so the cursor reappears on normal completion, on an early `?` error
/// return, and on a panic unwind. (Ctrl-C, which by default kills the process
/// without running destructors, is restored by a small signal handler in
/// `cmd_get`.) A no-op when stdout isn't a TTY (piped output has no cursor to
/// hide and we mustn't pollute the pipe with escape codes).
pub struct CursorGuard {
    active: bool,
}

impl CursorGuard {
    /// Hide the cursor (`\x1b[?25l`) when `is_tty`; otherwise a no-op guard.
    pub fn hide(is_tty: bool) -> Self {
        if is_tty {
            print!("\u{1b}[?25l");
            io::stdout().flush().ok();
        }
        CursorGuard { active: is_tty }
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        if self.active {
            // \x1b[?25h → show cursor.
            print!("\u{1b}[?25h");
            io::stdout().flush().ok();
        }
    }
}

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
    /// Current animated-spinner frame index. Advances one step on every progress
    /// render so the leading braille dot appears to spin. `Atomic` (not `Cell`)
    /// to keep the emitter `Sync` for the `EngineEmitter` trait.
    spinner_frame: AtomicUsize,
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
            spinner_frame: AtomicUsize::new(0),
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
            spinner_frame: AtomicUsize::new(0),
        }
    }

    /// Advance and return the next animated-spinner frame. Each call moves one
    /// step through [`SPINNER`] (wrapping), so successive progress renders show a
    /// spinning braille dot.
    fn next_spinner(&self) -> &'static str {
        let i = self.spinner_frame.fetch_add(1, Ordering::Relaxed) % SPINNER.len();
        SPINNER[i]
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
                let spinner = self.next_spinner();
                let line = format_progress_line(spinner, bd, tb, spd, eta, self.term_width());
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
/// Layout: the animated `spinner` leads, the `▰▱` bar fills the middle, and a
/// FIXED-WIDTH, right-aligned stats block (pct / speed / eta) sits at the END so
/// the values never shift the bar as they change:
///
/// ```text
/// {spinner} {bar…………}   47%   1.4 MB/s  eta 1m04s
/// ```
///
/// When `total_bytes` is known (and non-zero) the bar tracks the real percentage
/// and pct/eta show concrete values. When `total_bytes` is `None` (or 0) — common
/// for libgen CDN mirrors that omit Content-Length — the layout is *identical*
/// (so nothing jumps width): the spinner + an empty bar still render, pct shows a
/// dash, eta shows `eta —`, and the speed is still shown when known. The spinner
/// is the sign of life in that indeterminate phase.
///
/// `term_width` is the terminal width in columns; the bar grows to fill whatever
/// space is left between the leading spinner and the trailing fixed stats block.
pub fn format_progress_line(
    spinner: &str,
    bytes_done: u64,
    total_bytes: Option<u64>,
    speed_bps: Option<u64>,
    eta_secs: Option<u64>,
    term_width: usize,
) -> String {
    // Speed is shown identically in both phases — concrete when known, a dash
    // (NOT "?") when the rate isn't available yet.
    let speed_field = speed_bps
        .map(format_speed)
        .unwrap_or_else(|| "\u{2014}".to_string());

    let (bar_pct, pct_field, eta_field) = match total_bytes.filter(|&t| t > 0) {
        Some(total) => {
            let pct = (bytes_done * 100 / total).min(100);
            let eta_field = match eta_secs {
                Some(s) => format!("eta {}", format_eta(s)),
                None => "eta \u{2014}".to_string(),
            };
            (pct, format!("{pct}%"), eta_field)
        }
        // Indeterminate: empty bar, dash for pct/eta — same fixed-width layout.
        None => (0, "\u{2014}".to_string(), "eta \u{2014}".to_string()),
    };

    // FIXED-WIDTH, right-aligned stats block at the END so changing values never
    // shift the bar. pct in 4 cols ("  5%"/" 47%"/"100%"), speed in 9, eta in 9.
    let stats = format!("{pct_field:>4}  {speed_field:>9}  {eta_field:>9}");
    // `{spinner} ` leads; `  {stats}` trails. Measure both by display width, then
    // let the ▰▱ bar fill the middle.
    let head = format!("{spinner} ");
    let tail = format!("  {stats}");
    let used = crate::textfit::display_width(&head) + crate::textfit::display_width(&tail);
    // Leave one trailing column so the line never wraps to the next row; floor at
    // a small width so a very narrow terminal still shows a bar.
    let bar_w = term_width.saturating_sub(used + 1).clamp(4, 200);
    let bar = format_bar(bar_pct, bar_w);
    format!("{head}{bar}{tail}")
}

/// Render a progress bar with `width` cells using filled (`▰`) / empty (`▱`),
/// matching the TUI's ▰▱ bar — no shaded fill on the undownloaded part.
fn format_bar(pct: u64, width: usize) -> String {
    let filled = (pct as usize * width / 100).min(width);
    let empty = width - filled;
    // ▰ = U+25B0 BLACK PARALLELOGRAM   ▱ = U+25B1 WHITE PARALLELOGRAM
    "\u{25B0}".repeat(filled) + &"\u{25B1}".repeat(empty)
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
        let line = format_progress_line(SPINNER[0], 470, Some(1000), Some(1_500_000), Some(64), 80);
        assert!(line.contains("47%"), "pct: {line:?}");
        assert!(line.contains("1.4 MB/s"), "speed: {line:?}");
        assert!(line.contains("eta 1m04s"), "eta: {line:?}");
        // The leading braille spinner frame replaces the old ⬇ and the arrow is gone.
        assert!(line.starts_with(SPINNER[0]), "spinner leads: {line:?}");
        assert!(!line.contains('\u{2B07}'), "old ⬇ arrow removed: {line:?}");
        // The stats block sits at the END — the bar precedes pct/speed/eta.
        let last_bar = line.rfind(['\u{25B0}', '\u{25B1}']).unwrap();
        let pct_at = line.find("47%").unwrap();
        assert!(last_bar < pct_at, "bar before stats: {line:?}");
        // The bar fills the remaining terminal width: filled + empty cells together
        // are far wider than the old fixed 10-cell stub, and the filled fraction
        // tracks the percentage.
        let filled = line.chars().filter(|&c| c == '\u{25B0}').count();
        let empty = line.chars().filter(|&c| c == '\u{25B1}').count();
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
    fn progress_line_stats_fixed_width() {
        // Stats block is right-aligned at a FIXED width, so the bar does not shift
        // as pct/speed/eta change: a 5 % line and a 100 % line have the SAME stats
        // block width (and the same total line width on the same terminal).
        let a = format_progress_line(SPINNER[0], 50, Some(1000), Some(1024), Some(5), 80);
        let b = format_progress_line(
            SPINNER[0],
            1000,
            Some(1000),
            Some(2_000_000),
            Some(3600),
            80,
        );
        assert_eq!(
            crate::textfit::display_width(&a),
            crate::textfit::display_width(&b),
            "lines stay the same width: {a:?} vs {b:?}"
        );
        // pct field right-aligned in 4 cols: "  5%" and "100%".
        assert!(a.contains("  5%"), "pct right-aligned: {a:?}");
        assert!(b.contains("100%"), "pct right-aligned: {b:?}");
    }

    #[test]
    fn progress_line_fills_terminal_width() {
        // A wider terminal → a wider bar (the bar is responsive, not fixed).
        let narrow = format_progress_line(SPINNER[0], 500, Some(1000), Some(1024), Some(10), 40);
        let wide = format_progress_line(SPINNER[0], 500, Some(1000), Some(1024), Some(10), 120);
        let cells = |s: &str| {
            s.chars()
                .filter(|&c| c == '\u{25B0}' || c == '\u{25B1}')
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
        // No Content-Length: identical fixed-width layout — spinner + empty bar +
        // dash pct/eta, with the speed still shown. No bogus "?%"/"eta ?".
        let line = format_progress_line(SPINNER[0], 5_452_595, None, Some(317_440), None, 80);
        assert!(line.contains("310 KB/s"), "speed still shown: {line:?}");
        assert!(line.contains('\u{2014}'), "dash pct/eta: {line:?}"); // —
        assert!(line.contains("eta \u{2014}"), "eta dash: {line:?}");
        assert!(line.starts_with(SPINNER[0]), "spinner leads: {line:?}");
        // No bogus placeholders, no old arrow.
        assert!(!line.contains("?%"), "no ?%: {line:?}");
        assert!(!line.contains('?'), "no '?': {line:?}");
        assert!(!line.contains('\u{2B07}'), "no ⬇: {line:?}");
        // The bar still renders (empty), so the line stays the full width.
        assert!(
            line.contains('\u{25B1}'),
            "indeterminate bar still renders: {line:?}"
        );
        assert!(crate::textfit::display_width(&line) <= 80);
    }

    #[test]
    fn progress_line_complete() {
        let line = format_progress_line(
            SPINNER[0],
            1000,
            Some(1000),
            Some(2 * 1024 * 1024),
            Some(0),
            80,
        );
        assert!(line.contains("100%"), "100%: {line:?}");
        // bar must be all filled — no empty cells at 100 %.
        assert_eq!(
            line.chars().filter(|&c| c == '\u{25B1}').count(),
            0,
            "no empty cells at 100%: {line:?}"
        );
        assert!(
            line.chars().filter(|&c| c == '\u{25B0}').count() > 10,
            "full-width filled bar: {line:?}"
        );
    }

    // ── spinner animation ────────────────────────────────────────────────────

    #[test]
    fn spinner_advances_each_render() {
        // Each `next_spinner` call advances one frame through SPINNER and wraps.
        let emitter = CliEmitter::for_test(true);
        assert_eq!(emitter.next_spinner(), SPINNER[0]);
        assert_eq!(emitter.next_spinner(), SPINNER[1]);
        assert_eq!(emitter.next_spinner(), SPINNER[2]);
        // Advance to just before the wrap, then confirm it wraps to frame 0.
        for _ in 3..SPINNER.len() {
            emitter.next_spinner();
        }
        assert_eq!(emitter.next_spinner(), SPINNER[0], "wraps after one cycle");
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
        assert!(bar.chars().all(|c| c == '\u{25B1}'), "all empty: {bar:?}");
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
        assert_eq!(bar.chars().filter(|&c| c == '\u{25B1}').count(), 5);
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
