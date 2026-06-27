//! Display-width-aware text fitting helpers.
//!
//! Terminal cells are not Unicode scalar values: a CJK ideograph or a wide
//! emoji occupies **two** columns, a combining mark occupies **zero**, and a
//! control character occupies none.  Layout/clipping math that counts
//! `chars()` (scalar values) therefore mis-sizes any non-ASCII text — CJK
//! titles get clipped too late and marquees slice a wide glyph in half.
//!
//! Everything here measures in **display columns** via `unicode-width`, and is
//! the single foundation the per-view clipping fixes (and the one marquee) sit
//! on.  See `docs/tui-clipping-plan.md` (#10/#14 + the CROSS-CUTTING RULE).

use std::ops::Range;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of a single `char` in terminal columns (control/None → 0).
#[inline]
fn char_cols(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Terminal display width of `s` in columns.
///
/// CJK wide glyphs count as 2, combining marks as 0, regular ASCII as 1.
#[inline]
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate `s` to at most `max_cols` **display columns**, appending `…`
/// (U+2026, width 1) when truncation occurs.
///
/// Semantics:
/// - The trailing `…` counts toward `max_cols` (output never exceeds it).
/// - A wide glyph straddling the cut boundary is dropped whole, never split.
/// - `max_cols == 0` → empty string.
/// - If `s` already fits in `max_cols`, it is returned unchanged.
// Consumed by the per-view clipping fixes: #1 detail variations + #11 picker
// (flex rows, border title), with more to land.
pub fn ellipsize(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    // Reserve one column for the ellipsis glyph.
    let budget = max_cols - 1;
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_cols(c);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('\u{2026}');
    out
}

/// Char-index range of `s` visible in a marquee window `window_cols` columns
/// wide, scrolled `offset` display columns from the left.
///
/// Shared windowing core for both [`marquee_window`] and the styled
/// title·author marquee in `ui.rs` — there is exactly one implementation of the
/// column-skip math.  Guarantees:
/// - If `s` fits in `window_cols`, the full range is returned (offset ignored).
/// - The window never starts or ends inside a wide glyph: an offset that lands
///   mid-glyph skips that glyph whole (the window may begin one column late).
/// - The taken slice never exceeds `window_cols` columns.
pub fn marquee_char_range(s: &str, window_cols: usize, offset: usize) -> Range<usize> {
    if window_cols == 0 {
        return 0..0;
    }
    let chars: Vec<char> = s.chars().collect();
    let total: usize = chars.iter().map(|&c| char_cols(c)).sum();
    if total <= window_cols {
        return 0..chars.len();
    }
    let mut col = 0; // running column position while skipping the offset
    let mut taken = 0; // columns accumulated inside the window
    let mut start: Option<usize> = None;
    for (i, &c) in chars.iter().enumerate() {
        let w = char_cols(c);
        if start.is_none() {
            // Still skipping the offset region. A wide glyph whose first column
            // is below `offset` is skipped whole (avoids a half-glyph start).
            if col < offset {
                col += w;
                continue;
            }
            start = Some(i);
        }
        if taken + w > window_cols {
            return start.unwrap()..i;
        }
        taken += w;
    }
    start.unwrap_or(chars.len())..chars.len()
}

/// Visible display-width slice of `s` for a marquee window `window_cols` wide,
/// scrolled `offset` columns from the left (ping-pong scrolling).
///
/// Thin wrapper over [`marquee_char_range`]; never splits a wide glyph and
/// never exceeds `window_cols` columns.
// Plain (unstyled) marquee window: #1/#11 Mode B packed line; #9/#15 to land.
// The styled detail-title marquee uses `marquee_char_range` directly.
pub fn marquee_window(s: &str, window_cols: usize, offset: usize) -> String {
    let r = marquee_char_range(s, window_cols, offset);
    s.chars().skip(r.start).take(r.end - r.start).collect()
}

/// Visible slice + edge-clip flags for a cursor-following editor window,
/// returned by [`scroll_to_cursor`].
///
/// `visible` is the text shown in the window; `clipped_left` / `clipped_right`
/// say whether any text is hidden past each edge so the caller can draw the
/// `‹` / `›` affordances.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorView {
    pub visible: String,
    pub clipped_left: bool,
    pub clipped_right: bool,
}

/// Scroll `s` horizontally inside a `window_cols`-wide field so the cell at
/// display column `cursor_col` stays visible, sliding the text as the cursor
/// moves past the right edge.
///
/// This is the shared "editing" primitive for the cross-cutting rule (#2
/// settings, #3 command line / `:import`, and groups d/e reuse it): given the
/// buffer, the cursor column, and the field width, it returns the visible
/// display-width slice that keeps the cursor in view plus the left/right-clip
/// flags for the `‹`/`›` indicators.
///
/// `cursor_col` is the cursor's display-column position within `s`
/// (`0..=display_width(s)`; equal to the full width means the cursor sits one
/// cell past the last glyph — the usual end-of-line insertion point). Once the
/// text overflows, the window is pinned so the cursor rests on the right edge.
///
/// Guarantees:
/// - If `s` fits in `window_cols`, the whole string is returned, both flags false.
/// - The visible slice never exceeds `window_cols` columns and never splits a
///   wide glyph (a glyph straddling the left boundary is dropped whole, so the
///   window may begin one column late).
/// - `clipped_left` ⇔ text is hidden to the left of the window;
///   `clipped_right` ⇔ text is hidden to the right.
///
/// The caller reserves any columns it needs for the `‹`/`›` indicators by
/// passing a correspondingly smaller `window_cols`.
pub fn scroll_to_cursor(s: &str, cursor_col: usize, window_cols: usize) -> CursorView {
    if window_cols == 0 {
        return CursorView::default();
    }
    let total = display_width(s);
    if total <= window_cols {
        return CursorView {
            visible: s.to_string(),
            clipped_left: false,
            clipped_right: false,
        };
    }
    // Text overflows the field. Pin the window so the cursor cell rests on the
    // right edge; when the cursor is still near the start, keep offset at 0.
    let offset = (cursor_col + 1).saturating_sub(window_cols);
    // Take glyphs whose columns fall in [offset, offset + window_cols).
    let mut visible = String::new();
    let mut col = 0usize;
    let mut taken = 0usize;
    for c in s.chars() {
        let w = char_cols(c);
        if col < offset {
            // Still skipping the offset region; a glyph straddling the boundary
            // is skipped whole (the window may begin one column late).
            col += w;
            continue;
        }
        if taken + w > window_cols {
            break;
        }
        visible.push(c);
        taken += w;
        col += w;
    }
    CursorView {
        visible,
        clipped_left: offset > 0,
        clipped_right: offset + window_cols < total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── display_width ──────────────────────────────────────────────────────
    #[test]
    fn width_ascii() {
        assert_eq!(display_width("hello"), 5);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn width_cjk_is_double() {
        // Two CJK ideographs → 4 columns.
        assert_eq!(display_width("量子"), 4);
        assert_eq!(display_width("a量"), 3);
    }

    #[test]
    fn width_emoji() {
        // A wide emoji occupies two columns.
        assert_eq!(display_width("★"), 1); // narrow symbol
        assert_eq!(display_width("🚀"), 2); // wide emoji
    }

    #[test]
    fn width_combining_is_zero() {
        // 'e' + combining acute accent (U+0301) → one column, not two.
        assert_eq!(display_width("e\u{0301}"), 1);
        // base 'a' (1) + combining (0) + 'b' (1) = 2
        assert_eq!(display_width("a\u{0301}b"), 2);
    }

    // ── ellipsize ──────────────────────────────────────────────────────────
    #[test]
    fn ellipsize_fits_unchanged() {
        assert_eq!(ellipsize("hello", 5), "hello");
        assert_eq!(ellipsize("hello", 10), "hello");
        assert_eq!(ellipsize("量子", 4), "量子");
    }

    #[test]
    fn ellipsize_one_over_truncates() {
        // "hello" is 5 cols, max 4 → keep 3 + '…' = width 4.
        let out = ellipsize("hello", 4);
        assert_eq!(out, "hel\u{2026}");
        assert_eq!(display_width(&out), 4);
    }

    #[test]
    fn ellipsize_never_exceeds_max() {
        for max in 1..=6 {
            let out = ellipsize("abcdefgh", max);
            assert!(display_width(&out) <= max, "max={max} out={out:?}");
        }
    }

    #[test]
    fn ellipsize_max_zero_is_empty() {
        assert_eq!(ellipsize("anything", 0), "");
    }

    #[test]
    fn ellipsize_max_one_is_just_ellipsis() {
        let out = ellipsize("hello", 1);
        assert_eq!(out, "\u{2026}");
        assert_eq!(display_width(&out), 1);
    }

    #[test]
    fn ellipsize_wide_char_not_split_at_boundary() {
        // "量子" is 4 cols. max=3 → budget 2 for content + '…'.
        // First glyph fits (2 cols), second would overflow → keep "量…".
        let out = ellipsize("量子", 3);
        assert_eq!(out, "量\u{2026}");
        assert_eq!(display_width(&out), 3);

        // max=2 → budget 1, no whole wide glyph fits → just "…".
        let out2 = ellipsize("量子", 2);
        assert_eq!(out2, "\u{2026}");
        assert!(display_width(&out2) <= 2);
    }

    // ── marquee_window ─────────────────────────────────────────────────────
    #[test]
    fn marquee_fits_returns_whole() {
        assert_eq!(marquee_window("hello", 10, 0), "hello");
        // Offset ignored when it fits.
        assert_eq!(marquee_window("hi", 5, 3), "hi");
    }

    #[test]
    fn marquee_ascii_scrolls() {
        // "abcdefgh" (8 cols) in a 4-col window.
        assert_eq!(marquee_window("abcdefgh", 4, 0), "abcd");
        assert_eq!(marquee_window("abcdefgh", 4, 2), "cdef");
        // End of ping-pong: max offset = 8 - 4 = 4.
        assert_eq!(marquee_window("abcdefgh", 4, 4), "efgh");
    }

    #[test]
    fn marquee_cjk_no_half_glyph() {
        // "量子化学" = 4 glyphs × 2 cols = 8 cols, window 4 cols.
        let s = "量子化学";
        // offset 0 → first two glyphs exactly fill 4 cols.
        let w0 = marquee_window(s, 4, 0);
        assert_eq!(w0, "量子");
        assert_eq!(display_width(&w0), 4);

        // offset 1 lands mid-first-glyph → skip it whole, start at "子".
        let w1 = marquee_window(s, 4, 1);
        assert_eq!(w1, "子化");
        assert!(display_width(&w1) <= 4);

        // offset 2 → start exactly at second glyph.
        assert_eq!(marquee_window(s, 4, 2), "子化");

        // End of scroll (max offset 4) → last two glyphs.
        assert_eq!(marquee_window(s, 4, 4), "化学");
    }

    #[test]
    fn marquee_odd_window_never_splits_wide_glyph() {
        // 3-col window over CJK: only one whole glyph fits (2 cols), the third
        // column stays empty rather than splitting the next glyph.
        let w = marquee_window("量子化", 3, 0);
        assert_eq!(w, "量");
        assert!(display_width(&w) <= 3);
    }

    #[test]
    fn marquee_window_zero_is_empty() {
        assert_eq!(marquee_window("abc", 0, 0), "");
    }

    // ── scroll_to_cursor ───────────────────────────────────────────────────
    #[test]
    fn scroll_fits_returns_whole_no_flags() {
        let cv = scroll_to_cursor("hello", 5, 10);
        assert_eq!(cv.visible, "hello");
        assert!(!cv.clipped_left);
        assert!(!cv.clipped_right);
        // Exactly filling the window still counts as fitting.
        let cv = scroll_to_cursor("hello", 5, 5);
        assert_eq!(cv.visible, "hello");
        assert!(!cv.clipped_left && !cv.clipped_right);
    }

    #[test]
    fn scroll_cursor_near_start_clips_right_only() {
        // Cursor at column 0 of an 8-col string in a 4-col window.
        let cv = scroll_to_cursor("abcdefgh", 0, 4);
        assert_eq!(cv.visible, "abcd");
        assert!(!cv.clipped_left, "nothing hidden left");
        assert!(cv.clipped_right, "efgh hidden right");
    }

    #[test]
    fn scroll_cursor_in_middle_clips_both() {
        // Cursor at column 4 → window slides to keep it on the right edge.
        let cv = scroll_to_cursor("abcdefgh", 4, 4);
        assert_eq!(cv.visible, "bcde");
        assert!(cv.clipped_left);
        assert!(cv.clipped_right);
        assert!(display_width(&cv.visible) <= 4);
    }

    #[test]
    fn scroll_cursor_at_end_clips_left_only() {
        // Cursor one past the last glyph (insertion point) — typical editing.
        let cv = scroll_to_cursor("abcdefgh", 8, 4);
        // Window [5,9): only "fgh" exists; the 4th column is the cursor cell.
        assert_eq!(cv.visible, "fgh");
        assert!(cv.clipped_left, "head hidden left");
        assert!(!cv.clipped_right, "cursor sits at the right edge");
        assert!(display_width(&cv.visible) <= 4);
    }

    #[test]
    fn scroll_settings_editor_keeps_block_cursor_visible() {
        // Mirrors the settings inline editor: buffer + trailing block cursor.
        let buf = "/Users/someone/very/long/download/path";
        let with_cursor = format!("{buf}\u{258f}");
        let cur = display_width(&with_cursor).saturating_sub(1); // col of the cursor glyph
        let cv = scroll_to_cursor(&with_cursor, cur, 12);
        // The block cursor must be the last visible glyph.
        assert!(cv.visible.ends_with('\u{258f}'), "cursor visible: {cv:?}");
        assert!(cv.clipped_left, "long path scrolled → ‹ shown");
        assert!(!cv.clipped_right, "cursor at end → no ›");
        assert!(display_width(&cv.visible) <= 12);
    }

    #[test]
    fn scroll_cjk_never_splits_wide_glyph() {
        // "量子化学" = 8 cols; cursor at end in a 4-col window.
        let cv = scroll_to_cursor("量子化学", 8, 4);
        assert!(display_width(&cv.visible) <= 4);
        assert!(cv.clipped_left);
        // Every visible glyph is whole (width stays even for pure-CJK).
        assert_eq!(display_width(&cv.visible) % 2, 0);
    }

    #[test]
    fn scroll_window_zero_is_empty() {
        let cv = scroll_to_cursor("abc", 1, 0);
        assert_eq!(cv, CursorView::default());
    }
}
