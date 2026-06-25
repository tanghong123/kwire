//! "Quiet" palette — 24-bit truecolor; falls back to ANSI 16 when `COLORTERM`
//! is unset. All render code references these constants instead of hardcoding.

use ratatui::style::{Color, Modifier, Style};

// ---------------------------------------------------------------------------
// Palette (§7)
// ---------------------------------------------------------------------------

pub const C_DONE: Color = Color::Rgb(0xa3, 0xbe, 0x8c); // accent / done
pub const C_NEEDS_YOU: Color = Color::Rgb(0xd8, 0xa6, 0x57); // needs-you / paused
pub const C_DOWNLOADING: Color = Color::Rgb(0x83, 0xa5, 0x98); // downloading
pub const C_FAILED: Color = Color::Rgb(0xcf, 0x6f, 0x5a); // failed / cannot
pub const C_TEXT: Color = Color::Rgb(0xd6, 0xd3, 0xcc); // normal text
pub const C_BRIGHT: Color = Color::Rgb(0xf0, 0xec, 0xe4); // bright / title
pub const C_DIM: Color = Color::Rgb(0x8a, 0x85, 0x7c); // dim metadata
pub const C_FAINT: Color = Color::Rgb(0x5c, 0x57, 0x50); // faint / separators
pub const C_BG: Color = Color::Reset; // background (terminal default)
pub const C_PANEL: Color = Color::Reset; // panel background (terminal default)
pub const C_SELECTED: Color = Color::Rgb(0x1b, 0x1a, 0x14); // selected row
pub const C_BACKDROP: Color = Color::Rgb(0x12, 0x11, 0x0f); // modal dim overlay

// ---------------------------------------------------------------------------
// Convenience style constructors
// ---------------------------------------------------------------------------

pub fn style_normal() -> Style {
    Style::default().fg(C_TEXT).bg(C_BG)
}

pub fn style_dim() -> Style {
    Style::default().fg(C_DIM).bg(C_BG)
}

pub fn style_title() -> Style {
    Style::default()
        .fg(C_BRIGHT)
        .bg(C_BG)
        .add_modifier(Modifier::BOLD)
}

pub fn style_selected() -> Style {
    Style::default()
        .fg(C_BRIGHT)
        .bg(C_SELECTED)
        .add_modifier(Modifier::BOLD)
}

pub fn style_hint() -> Style {
    Style::default().fg(C_DIM).bg(C_PANEL)
}

pub fn style_header() -> Style {
    Style::default().fg(C_FAINT).bg(C_BG)
}

pub fn style_for_state(state: &str) -> Style {
    let color = match state {
        "done" => C_DONE,
        "downloading" => C_DOWNLOADING,
        "failed" | "cancelled" => C_FAILED,
        "paused" | "available" => C_NEEDS_YOU,
        _ => C_DIM, // queued / unknown
    };
    Style::default().fg(color)
}

/// Render a `▰▱` block-character progress bar in `width` chars (min 2).
pub fn progress_bar(pct: u32, width: usize) -> String {
    let width = width.max(2);
    let filled = ((pct as usize) * width / 100).min(width);
    let empty = width - filled;
    format!("{}{}", "▰".repeat(filled), "▱".repeat(empty))
}

/// Color-code a MATCH score: green → amber → red by value.
pub fn score_color(score: f64) -> Color {
    if score >= 0.85 {
        C_DONE
    } else if score >= 0.60 {
        C_NEEDS_YOU
    } else {
        C_FAILED
    }
}

/// Color for a history-event `kind` string, keyed by common substrings.
pub fn history_kind_color(kind: &str) -> Color {
    let k = kind.to_ascii_lowercase();
    if k.contains("done") || k.contains("download") || k.contains("match") {
        C_DONE
    } else if k.contains("fail") || k.contains("error") || k.contains("cancel") {
        C_FAILED
    } else if k.contains("start") || k.contains("resolv") || k.contains("queue") {
        C_DOWNLOADING
    } else {
        C_DIM
    }
}

/// Braille spinner frames, advanced by the tick counter.
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner(tick: u64) -> &'static str {
    SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()]
}
