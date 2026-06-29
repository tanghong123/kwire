//! "Quiet" palette — 24-bit truecolor; falls back to ANSI 16 when `COLORTERM`
//! is unset. All render code references these constants instead of hardcoding.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

// ---------------------------------------------------------------------------
// Palette (§7)
// ---------------------------------------------------------------------------

pub const C_DONE: Color = Color::Rgb(0xa3, 0xbe, 0x8c); // accent / done
pub const C_NEEDS_YOU: Color = Color::Rgb(0xd8, 0xa6, 0x57); // needs-you / paused
pub const C_DOWNLOADING: Color = Color::Rgb(0x83, 0xa5, 0x98); // downloading
pub const C_FAILED: Color = Color::Rgb(0xcf, 0x6f, 0x5a); // failed / cannot
pub const C_TEXT: Color = Color::Rgb(0xd6, 0xd3, 0xcc); // normal text
pub const C_BRIGHT: Color = Color::Rgb(0xf0, 0xec, 0xe4); // bright / title
pub const C_DIM: Color = Color::Rgb(0x8a, 0x85, 0x7c); // light-dim metadata
pub const C_MUTED: Color = Color::Rgb(0x6f, 0x6a, 0x5f); // mid-dim — workhorse secondary text
pub const C_FAINT: Color = Color::Rgb(0x5c, 0x57, 0x50); // faint / separators
pub const C_NEAR_DIM: Color = Color::Rgb(0x9a, 0x95, 0x8c); // near-dim (connecting / resolving)
pub const C_SOFTER: Color = Color::Rgb(0xcf, 0xca, 0xbf); // softer dim — embedded modal contexts
pub const C_WARM: Color = Color::Rgb(0xcd, 0xbf, 0x9c); // warm amber — hosts, field values, tagline
pub const C_BG: Color = Color::Reset; // background (terminal default)
pub const C_PANEL: Color = Color::Reset; // panel background (terminal default)
                                         // Shared "selected line" treatment: a FAINT GREEN background (not reverse-video)
                                         // paired with a GREEN vertical left accent bar (`▌` in `C_SEL_ACCENT`).
pub const C_SELECTED: Color = Color::Rgb(0x18, 0x20, 0x13); // selected row — faint green tint
pub const C_SEL_ACCENT: Color = C_DONE; // green left accent bar on selected rows
pub const C_SEL_ACCENT_DIM: Color = Color::Rgb(0x5e, 0x70, 0x4f); // muted green accent for the
                                                                  // selection of a NON-focused list (stays visible, dimmer than C_SEL_ACCENT)
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

pub fn style_muted() -> Style {
    Style::default().fg(C_MUTED).bg(C_BG)
}

/// High-contrast modal title style — green accent + bold, matching the Help
/// modal's active-page title. Used for every modal's border title so they read
/// consistently (Detail / Settings / Picker were dim and inconsistent).
pub fn style_modal_title() -> Style {
    Style::default().fg(C_DONE).add_modifier(Modifier::BOLD)
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

/// The shared green left accent bar (`▌`) style for a selected list row —
/// green glyph on the faint-green selected background. Used by every selectable
/// list (main book table, detail variations, detail history, settings fields).
pub fn style_sel_accent() -> Style {
    Style::default().fg(C_SEL_ACCENT).bg(C_SELECTED)
}

/// Dimmed variant of [`style_sel_accent`] for the selected row of a list that is
/// NOT focused: a MUTED-green `▌` accent on the plain background (no faint-green
/// tint). The selection stays visible (never blank) but clearly reads as
/// inactive next to a focused list's full-green accent + tinted background.
pub fn style_sel_accent_dim() -> Style {
    Style::default().fg(C_SEL_ACCENT_DIM).bg(C_BG)
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
        "failed" | "cancelled" | "not_found" => C_FAILED,
        // A downloaded-but-too-short copy reads as "needs your attention" (amber),
        // not the reassuring green of a good `done`.
        "paused" | "available" | "too_few_pages" => C_NEEDS_YOU,
        _ => C_DIM, // queued / unknown
    };
    Style::default().fg(color)
}

/// Color for a status-filter chip, keyed by status family (audit palette).
/// Keyed off the catalog label key (locale-stable) rather than the translated
/// text. The active/selected chip is rendered bright + underlined by the caller
/// and does not go through this function.
pub fn filter_chip_color(label_key: &str) -> Color {
    match label_key {
        "filter.needs" => C_NEEDS_YOU,    // amber
        "filter.cantdl" => C_FAILED,      // red
        "filter.queued" => C_NEAR_DIM,    // near-dim beige — waiting / resolving
        "filter.active" => C_DOWNLOADING, // teal / blue
        "filter.done" => C_DONE,          // green
        // filter.all / filter.review and anything else: dim metadata.
        _ => C_DIM,
    }
}

/// Render a `▰▱` block-character progress bar in `width` chars (min 2).
pub fn progress_bar(pct: u32, width: usize) -> String {
    let width = width.max(2);
    let filled = ((pct as usize) * width / 100).min(width);
    let empty = width - filled;
    format!("{}{}", "▰".repeat(filled), "▱".repeat(empty))
}

/// Styled variant of [`progress_bar`]: the filled `▰` portion in the download
/// color, the remaining `▱` portion faint. Shared by the Activity pane and the
/// Detail view so the bar style can't drift between them. Total display width is
/// exactly `width.max(2)` cells.
pub fn progress_bar_spans(pct: u32, width: usize) -> Vec<Span<'static>> {
    let width = width.max(2);
    let filled = ((pct as usize) * width / 100).min(width);
    let empty = width - filled;
    vec![
        Span::styled("▰".repeat(filled), Style::default().fg(C_DOWNLOADING)),
        Span::styled("▱".repeat(empty), Style::default().fg(C_FAINT)),
    ]
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
/// Palette: done = green, downloading = blue, failed = red, retry = yellow,
/// everything else = a neutral badge color.
pub fn history_kind_color(kind: &str) -> Color {
    let k = kind.to_ascii_lowercase();
    if k.contains("done") || k.contains("verif") || k.contains("match") {
        C_DONE // green — success family
    } else if k.contains("download") {
        C_DOWNLOADING // blue — in flight
    } else if k.contains("fail") || k.contains("error") || k.contains("cancel") {
        C_FAILED // red
    } else if k.contains("retry") || k.contains("rotat") || k.contains("failover") {
        C_NEEDS_YOU // yellow — retry / rotation
    } else {
        C_MUTED // neutral badge (born / discovered / selected / …)
    }
}

/// Braille spinner frames, advanced by the tick counter.
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner(tick: u64) -> &'static str {
    SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()]
}
