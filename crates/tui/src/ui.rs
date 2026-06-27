//! ratatui render pass — derives everything from [`AppState`].
//!
//! The layout (top → bottom):
//! ```
//! Length(1)       — list strip
//! Length(1)       — status-filter row
//! Length(1)       — dim ─── separator rule
//! Min(8)          — book Table
//! Length(1)       — dim ─── separator rule
//! Length(N)       — docked Activity pane  (N=1 collapsed, N=5 expanded)
//! Length(1)       — dim ─── separator rule  (always present)
//! [ Length(1)     — WILDMENU row             (only when : active + wildmenu open) ]
//! [ Length(1)     — :command-line row         (only when : active) ]
//! [ Length(1)     — dim ─── separator rule    (only when : active) ]
//! Length(1)       — key-hint bar
//! ```

use std::collections::BTreeMap;

use ratatui::{
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState,
    },
    Frame,
};

use crate::app::{
    armed_variations, settings_field_kind, AppState, DetailSubFocus, EditBookField, Focus, Modal,
    RowRef, SettingsEditor, SettingsFieldKind, StatusFilter, FORMAT_EDITOR_FORMATS,
};
use crate::theme::{
    self, filter_chip_color, history_kind_color, score_color, style_dim, style_header, style_hint,
    style_muted, style_normal, style_sel_accent, style_sel_accent_dim, style_selected, style_title,
    C_BACKDROP, C_BG, C_BRIGHT, C_DONE, C_FAINT, C_MUTED, C_NEEDS_YOU, C_PANEL, C_SELECTED,
    C_SOFTER, C_TEXT, C_WARM,
};

const ACTIVITY_EXPANDED_H: u16 = 5;
const ACTIVITY_COLLAPSED_H: u16 = 1;

/// Single entry point: render the full UI from `app` into `frame`.
/// Takes `&mut AppState` so it can write back `last_rects` for mouse
/// hit-testing.
pub fn render(frame: &mut Frame, app: &mut AppState) {
    // Empty / first-run screen when no view is loaded and no modal open.
    if app.view.is_none() && app.modal.is_none() {
        render_empty(frame, app);
        return;
    }

    let activity_h = if app.activity_expanded {
        ACTIVITY_EXPANDED_H
    } else {
        ACTIVITY_COLLAPSED_H
    };

    let cmd_active = app.command_buf.is_some();
    // When `:` is active and the wildmenu is open it gets its own row in the
    // layout, sitting between the rule[6] and the command-line row.
    let wildmenu_in_layout = cmd_active && !app.completion_candidates.is_empty();

    // Build layout constraints dynamically.  When `:` is active the
    // command-line gets its own row plus an extra rule below it.
    // When the wildmenu is also open it inserts one more row above the
    // command-line so it never overwrites the dim rule.
    let mut constraints = vec![
        Constraint::Length(1),          // 0  list strip
        Constraint::Length(1),          // 1  status-filter row
        Constraint::Length(1),          // 2  rule — header → list
        Constraint::Min(8),             // 3  book table
        Constraint::Length(1),          // 4  rule — list → activity
        Constraint::Length(activity_h), // 5  docked activity pane
        Constraint::Length(1),          // 6  rule — always present before bottom
    ];
    if cmd_active {
        if wildmenu_in_layout {
            constraints.push(Constraint::Length(1)); // 7  WILDMENU row
        }
        constraints.push(Constraint::Length(1)); // 7/8  :command-line row
        constraints.push(Constraint::Length(1)); // 8/9  rule — cmd-line → hint
    }
    // #7: the hint bar GROWS to fit its wrapped lines at the current width so no
    // hint is ever dropped; the book table (Min(8)) gives back the rows.
    let hint_h = hint_bar_lines(app, frame.area().width).len().max(1) as u16;
    constraints.push(Constraint::Length(hint_h)); // hint bar (always last)

    let chunks = Layout::vertical(constraints).split(frame.area());
    let hint_idx = chunks.len() - 1;

    // Store panel rects for mouse hit-testing.
    app.last_rects.list_strip = chunks[0];
    app.last_rects.filter_row = chunks[1];
    app.last_rects.book_table = chunks[3];
    app.last_rects.activity = chunks[5];
    app.last_rects.hint_bar = chunks[hint_idx];

    render_list_strip(frame, app, chunks[0]);
    render_filter_row(frame, app, chunks[1]);
    render_rule(frame, chunks[2]);
    render_book_table(frame, app, chunks[3]);
    render_rule(frame, chunks[4]);
    render_activity(frame, app, chunks[5]);
    render_rule(frame, chunks[6]);

    if cmd_active {
        if wildmenu_in_layout {
            // Wildmenu gets its own allocated row; the dim rule[6] stays intact.
            render_wildmenu(frame, app, chunks[7]);
            render_command_line(frame, app, chunks[8]);
            render_rule(frame, chunks[9]);
        } else {
            render_command_line(frame, app, chunks[7]);
            render_rule(frame, chunks[8]);
        }
    }
    render_hint_bar(frame, app, chunks[hint_idx]);

    // Overlay modal if one is open.
    if let Some(modal) = app.modal.clone() {
        render_backdrop(frame);
        match modal {
            Modal::Picker {
                book_flat_index,
                selected,
            } => render_picker_modal(frame, app, book_flat_index, selected),
            Modal::Detail {
                book_flat_index,
                selected,
                sub_focus,
                history_selected,
            } => render_detail_modal(
                frame,
                app,
                book_flat_index,
                selected,
                &sub_focus,
                history_selected,
            ),
            Modal::Settings => render_settings_modal(frame, app),
            Modal::Help => render_help_modal(frame, frame.area()),
            Modal::Confirm {
                title,
                n_books,
                target_id: _,
            } => render_confirm_modal(frame, &title, n_books),
            Modal::ReQuery {
                book_flat_index,
                buf,
            } => render_requery_modal(frame, app, book_flat_index, &buf),
            Modal::EditBook {
                book_flat_index,
                title_buf,
                author_buf,
                field,
            } => {
                render_edit_book_modal(frame, app, book_flat_index, &title_buf, &author_buf, &field)
            }
            Modal::ConfirmBookRemove { book_flat_index } => {
                render_confirm_book_remove_modal(frame, app, book_flat_index)
            }
            Modal::Reorganize { diff, selected } => render_reorganize_modal(frame, &diff, selected),
            Modal::Snapshot { title, lines, .. } => {
                render_snapshot_modal(frame, &title, &lines);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Empty / first-run screen
// ---------------------------------------------------------------------------

fn render_empty(frame: &mut Frame, app: &mut AppState) {
    let area = frame.area();

    // Reserve 3 lines at the bottom: 1 for the command-input box border top,
    // 1 for the command-input content, 1 for the command-input border bottom.
    // We actually use a bordered 3-line block at the very bottom.
    let outer = Layout::vertical([
        Constraint::Min(0),    // content area
        Constraint::Length(3), // command-input box
    ])
    .split(area);

    app.last_rects.hint_bar = outer[1];

    // Vertically center the content block.
    // Content lines:
    //   3 — logo box (bordered: top border + content + bottom border)
    //   1 — blank
    //   6 — wordmark (ASCII-art banner, 6 rows)
    //   1 — blank
    //   2 — tagline (2 lines)
    //   1 — blank
    //   1 — NO READING LISTS YET
    //   1 — blank
    //   4 — command hints
    // Total = 3 + 1 + 6 + 1 + 2 + 1 + 1 + 1 + 4 = 20 lines
    let content_h: u16 = 20;
    let top_pad = outer[0].height.saturating_sub(content_h) / 2;

    let content_area = Layout::vertical([
        Constraint::Length(top_pad),   // top padding
        Constraint::Length(content_h), // content
        Constraint::Min(0),            // bottom padding
    ])
    .split(outer[0])[1];

    // Split content_area into its pieces.
    let parts = Layout::vertical([
        Constraint::Length(3), // logo box (bordered)
        Constraint::Length(1), // blank
        Constraint::Length(6), // wordmark (ASCII-art banner)
        Constraint::Length(1), // blank
        Constraint::Length(2), // tagline (2 lines)
        Constraint::Length(1), // blank
        Constraint::Length(1), // NO READING LISTS YET
        Constraint::Length(1), // blank
        Constraint::Length(4), // command hints
    ])
    .split(content_area);

    // 1. Logo glyph — a bordered box containing "▤ ▤ ▤"
    let logo_block = Block::default()
        .borders(Borders::ALL)
        .border_style(style_dim());
    // Make the logo box small (~9 wide), centered
    let logo_inner_w: u16 = 7;
    let logo_box_w: u16 = logo_inner_w + 2; // +2 for borders
    let logo_area = centered_rect(logo_box_w, 3, parts[0]);
    frame.render_widget(
        Paragraph::new("\u{25a4} \u{25a4} \u{25a4}")
            .alignment(Alignment::Center)
            .style(style_dim())
            .block(logo_block),
        logo_area,
    );

    // 2. Wordmark — ASCII-art block-letter banner, centered, in the bright color.
    //    Generated from the ANSI Shadow figlet font for "KWIRE" (6 rows × 41 cols).
    let banner: &[&str] = &[
        "██╗  ██╗ ██╗    ██╗ ██╗ ██████╗  ███████╗",
        "██║ ██╔╝ ██║    ██║ ██║ ██╔══██╗ ██╔════╝",
        "█████╔╝  ██║ █╗ ██║ ██║ ██████╔╝ █████╗  ",
        "██╔═██╗  ██║███╗██║ ██║ ██╔══██╗ ██╔══╝  ",
        "██║  ██╗ ╚███╔███╔╝ ██║ ██║  ██║ ███████╗",
        "╚═╝  ╚═╝  ╚══╝╚══╝  ╚═╝ ╚═╝  ╚═╝ ╚══════╝",
    ];
    let banner_style = Style::default().fg(C_BRIGHT).add_modifier(Modifier::BOLD);
    let banner_lines: Vec<Line> = banner
        .iter()
        .map(|row| Line::from(Span::styled(*row, banner_style)))
        .collect();
    frame.render_widget(
        Paragraph::new(banner_lines).alignment(Alignment::Center),
        parts[2],
    );

    // 3. Tagline — 2 lines, "quire" emphasized
    //    "A quire gathers folded sheets into one section of a book —"
    //    "kwire gathers a scattered reading list into one tidy collection."
    let line1 = Line::from(vec![
        Span::styled("A ", Style::default().fg(C_WARM)),
        Span::styled(
            "quire",
            Style::default().fg(C_WARM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " gathers folded sheets into one section of a book \u{2014}",
            Style::default().fg(C_WARM),
        ),
    ]);
    let line2 = Line::from(Span::styled(
        "kwire gathers a scattered reading list into one tidy collection.",
        Style::default().fg(C_WARM),
    ));
    frame.render_widget(
        Paragraph::new(vec![line1, line2]).alignment(Alignment::Center),
        parts[4],
    );

    // 4. NO READING LISTS YET — muted, centered
    frame.render_widget(
        Paragraph::new("NO READING LISTS YET")
            .alignment(Alignment::Center)
            .style(style_muted()),
        parts[6],
    );

    // 5. Command hints — left-aligned group, group centered
    //    Each row: accent command + dim description
    let hint_rows: &[(&str, &str)] = &[
        (": import ~/list.md", "add a Markdown or JSON reading list"),
        (": add", "add a single book by hand"),
        (": open <name>", "switch between lists"),
        ("?", "all keys & commands"),
    ];

    // Calculate column widths: max command width, fixed gap, description.
    let cmd_col_w = hint_rows
        .iter()
        .map(|(cmd, _)| cmd.len())
        .max()
        .unwrap_or(0);

    let hint_lines: Vec<Line> = hint_rows
        .iter()
        .map(|(cmd, desc)| {
            Line::from(vec![
                Span::styled(
                    format!("{:<width$}", cmd, width = cmd_col_w),
                    Style::default().fg(C_DONE),
                ),
                Span::styled("  ", style_dim()),
                Span::styled(*desc, style_dim()),
            ])
        })
        .collect();

    // Center the block: find total width of a hint row
    let hint_row_w =
        (cmd_col_w + 2 + hint_rows.iter().map(|(_, d)| d.len()).max().unwrap_or(0)) as u16;
    let hint_area = centered_rect(hint_row_w.min(area.width), 4, parts[8]);
    frame.render_widget(Paragraph::new(hint_lines), hint_area);

    // 6. Bordered command-input box at the bottom. The input scrolls to keep the
    //    cursor visible (#3) — same mechanism as the main command row. The box
    //    has a 1-col border on each side, so the inner field is width − 2.
    let cmd_line: Line = if let Some(ref buf) = app.command_buf {
        command_input_line(buf, outer[1].width.saturating_sub(2))
    } else if let Some(ref msg) = app.status_msg {
        Line::from(Span::styled(crate::i18n::decode(msg), style_hint()))
    } else {
        Line::default()
    };

    let cmd_border_style = style_dim();
    frame.render_widget(
        Paragraph::new(cmd_line)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(cmd_border_style),
            )
            .style(style_hint()),
        outer[1],
    );

    // Wildmenu: one line above the command-input box (empty screen has no
    // separate layout row for it, so we paint directly above the box).
    if !app.completion_candidates.is_empty() && outer[1].y > 0 {
        render_wildmenu(
            frame,
            app,
            Rect::new(outer[1].x, outer[1].y - 1, outer[1].width, 1),
        );
    }

    // Overlay modal (e.g. Help opened from empty state).
    if let Some(modal) = app.modal.clone() {
        render_backdrop(frame);
        match modal {
            Modal::Help => render_help_modal(frame, area),
            Modal::Settings => render_settings_modal(frame, app),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Rule helper — dim full-width single-line separator
// ---------------------------------------------------------------------------

/// Render a dim horizontal rule (`─` repeated to fill `area.width`) into `area`.
fn render_rule(frame: &mut Frame, area: Rect) {
    let line = "\u{2500}".repeat(area.width as usize);
    frame.render_widget(Paragraph::new(Span::styled(line, style_dim())), area);
}

// ---------------------------------------------------------------------------
// 0  List strip
// ---------------------------------------------------------------------------

fn render_list_strip(frame: &mut Frame, app: &mut AppState, area: Rect) {
    // Clear previous list chips so we can rebuild for mouse hit-testing.
    app.last_rects.list_chips.clear();

    // When no lists are loaded fall back to the bare wordmark.
    if app.all_lists.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(" kwire", style_title())).style(style_normal()),
            area,
        );
        return;
    }

    let area_w = area.width as usize;

    // The aggregate "All" stop — a fixed-width prefix (not a sized list column).
    let all_done: usize = app.all_lists.iter().map(|l| l.done).sum();
    let all_total: usize = app.all_lists.iter().map(|l| l.total).sum();
    let prefix = if app.all_active {
        format!(" \u{2605} All {}/{}", all_done, all_total)
    } else {
        format!(" All {}/{}", all_done, all_total)
    };
    let prefix_style = if app.all_active {
        style_title()
    } else {
        style_muted()
    };
    let prefix_w = crate::textfit::display_width(&prefix);

    // ── Phase 1: per-list labels + natural widths (owned → releases the borrow). ─
    struct Lbl {
        text: String,
        active: bool,
    }
    let labels: Vec<Lbl> = app
        .all_lists
        .iter()
        .enumerate()
        .map(|(i, list)| {
            let active = !app.all_active && i == app.active_list_idx;
            let text = if active {
                format!("   \u{2605} {} {}/{}", list.title, list.done, list.total)
            } else {
                format!("   {} {}/{}", list.title, list.done, list.total)
            };
            Lbl { text, active }
        })
        .collect();
    let natural_widths: Vec<usize> = labels
        .iter()
        .map(|l| crate::textfit::display_width(&l.text))
        .collect();

    // #15: per-list column widths (≤4 even split / >4 cap N/4, floor 30).
    let col_widths = list_strip_layout(area_w, &natural_widths);
    let total_natural: usize = natural_widths.iter().sum();
    // Equal padded columns only in the ≤4 even-split (capped) case.
    let even_mode = total_natural > area_w && labels.len() <= 4;

    // ── Phase 2: advance only the ACTIVE list's in-column marquee. ────────────
    let active_idx = if app.all_active {
        None
    } else {
        Some(app.active_list_idx)
    };
    if let Some(ai) = active_idx {
        app.reset_list_marquee_if_changed(ai);
        app.advance_list_marquee(
            natural_widths.get(ai).copied().unwrap_or(0),
            col_widths.get(ai).copied().unwrap_or(0),
        );
    } else {
        app.reset_list_marquee_if_changed(usize::MAX);
        app.advance_list_marquee(0, 1); // "All" active → park
    }
    let marquee_off = app.list_marquee_offset;

    // ── Phase 3: build per-column segments (clipped/marqueed) + column ranges. ─
    struct Seg {
        text: String,
        width: usize,
        style: ratatui::style::Style,
    }
    let mut segs: Vec<Seg> = vec![Seg {
        text: prefix,
        width: prefix_w,
        style: prefix_style,
    }];
    // [start, end) strip-column range of each list segment (for scroll + chips).
    let mut list_col_ranges: Vec<(usize, usize)> = Vec::new();
    let mut cumulative = prefix_w;
    for (i, l) in labels.iter().enumerate() {
        let col = col_widths[i];
        // Active list marquees within its column; inactive lists ellipsize.
        let mut text = if l.active {
            crate::textfit::marquee_window(&l.text, col, marquee_off)
        } else {
            crate::textfit::ellipsize(&l.text, col)
        };
        // Even split → pad to an equal column; packed/natural stay tight.
        if even_mode {
            text = pad_cell(&text, col);
        }
        let style = if l.active {
            style_title()
        } else {
            style_muted()
        };
        let start = cumulative;
        let end = start + col;
        list_col_ranges.push((start, end));
        cumulative = end;
        segs.push(Seg {
            text,
            width: col,
            style,
        });
    }

    let nav = "   [ ]";
    let nav_w = crate::textfit::display_width(nav);
    let total_width = cumulative + nav_w;
    segs.push(Seg {
        text: nav.into(),
        width: nav_w,
        style: style_muted(),
    });

    // Compute scroll_x so the active list column is fully visible.
    let (active_start, active_end) = active_idx
        .and_then(|ai| list_col_ranges.get(ai).copied())
        .unwrap_or((0, 0));
    let scroll_x: usize = if total_width <= area_w || app.all_active {
        0 // everything fits (or "All" is active, anchored at column 0) — no scroll
    } else {
        let want_start = active_start.saturating_sub(1);
        let max_scroll = total_width.saturating_sub(area_w);
        let mut sx = want_start.min(max_scroll);
        if sx + area_w < active_end {
            sx = active_end.saturating_sub(area_w).min(max_scroll);
        }
        sx
    };

    let has_left = scroll_x > 0;
    let has_right = scroll_x + area_w < total_width;

    // Populate list_chips for mouse hit-testing at the scrolled positions.
    for (i, (start, end)) in list_col_ranges.iter().enumerate() {
        let screen_start = start.saturating_sub(scroll_x);
        let screen_end = end.saturating_sub(scroll_x).min(area_w);
        if screen_end > screen_start {
            app.last_rects.list_chips.push((
                Rect::new(
                    area.x + screen_start as u16,
                    area.y,
                    (screen_end - screen_start) as u16,
                    1,
                ),
                i,
            ));
        }
    }

    // Slice each segment to the visible column window [scroll_x, scroll_x+area_w),
    // display-width aware (#14) so CJK list titles never split mid-glyph.
    let win_start = scroll_x;
    let win_end = scroll_x + area_w;
    let mut spans: Vec<Span> = Vec::new();
    let mut pos: usize = 0;
    for seg in &segs {
        let s = pos;
        let e = pos + seg.width;
        pos = e;
        if e <= win_start || s >= win_end {
            continue;
        }
        let vis_start = s.max(win_start);
        let vis_end = e.min(win_end);
        let skip = vis_start - s;
        let take = vis_end - vis_start;
        let sliced = crate::textfit::marquee_window(&seg.text, take, skip);
        if !sliced.is_empty() {
            spans.push(Span::styled(sliced, seg.style));
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(style_normal()),
        area,
    );

    // Overlay ‹ / › affordances on top of the rendered line.
    if has_left && area.width >= 1 {
        frame.render_widget(
            Paragraph::new(Span::styled("\u{2039}", style_dim())),
            Rect::new(area.x, area.y, 1, 1),
        );
    }
    if has_right && area.width >= 2 {
        frame.render_widget(
            Paragraph::new(Span::styled("\u{203a}", style_dim())),
            Rect::new(area.x + area.width - 1, area.y, 1, 1),
        );
    }
}

// ---------------------------------------------------------------------------
// 1  Status-filter row
// ---------------------------------------------------------------------------

fn render_filter_row(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let counts = app.status_counts();
    let active_filter = app.filter;
    let header_focused = app.focus == Focus::Header;

    // Clear filter_chips so we can rebuild.
    app.last_rects.filter_chips.clear();

    // Build chips and track their rects for mouse hit-testing.
    // When the Header pane is active, prepend a bright ▌ accent as a pane indicator.
    // We approximate chip positions based on cumulative text width.
    let chip_data: Vec<(StatusFilter, String)> = [
        (StatusFilter::All, counts.total),
        (StatusFilter::NeedsYou, counts.needs_you),
        (StatusFilter::Check, counts.check),
        (StatusFilter::Cannot, counts.cannot),
        (StatusFilter::InProgress, counts.in_progress),
        (StatusFilter::Done, counts.done),
    ]
    .into_iter()
    .map(|(filter, count)| (filter, format!(" {} {} ", filter.label(), count)))
    .collect();

    // Pane accent: ▌ when Header is active, space otherwise.
    let pane_accent = if header_focused {
        Span::styled("\u{258c}", Style::default().fg(C_DONE))
    } else {
        Span::styled(" ", style_dim())
    };

    // Style helper for a single chip's label.
    let chip_style = |filter: &StatusFilter| -> Style {
        if *filter == active_filter {
            // Active/selected chip: always bright + underlined (cursor here).
            Style::default()
                .fg(C_BRIGHT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            // Idle chip: colored by its status family (amber/red/teal/green/dim).
            Style::default().fg(filter_chip_color(filter.label_key()))
        }
    };

    // Distribute the chips evenly across the row instead of left-packing them.
    // The pane accent occupies the first column, leaving `usable` columns for
    // the chips and the gaps between/around them. We spread the leftover space
    // as `n + 1` equal gaps (one before each chip + one trailing margin), so the
    // chips fan out across the full width. The trailing gap is implicit (the
    // background to the right of the last chip), so we only render the n leading
    // gaps. Any remainder columns are handed out one-per-slot from the left.
    let n = chip_data.len() as u16;
    let sum_chips: u16 = chip_data.iter().map(|(_, l)| l.len() as u16).sum();
    let usable = area.width.saturating_sub(1); // minus the pane-accent column
    let n_slots = n + 1; // leading gaps (n) + one trailing margin

    let mut x_offset = area.x + 1; // +1 for pane accent char
    let mut spans: Vec<Span> = vec![pane_accent];

    if usable >= sum_chips + n_slots {
        // Enough room to spread evenly.
        let total_gap = usable - sum_chips;
        let base = total_gap / n_slots;
        let mut rem = total_gap % n_slots;
        for (filter, label) in &chip_data {
            // Leading gap before this chip (distribute remainder from the left).
            let mut gap = base;
            if rem > 0 {
                gap += 1;
                rem -= 1;
            }
            spans.push(Span::styled(
                " ".repeat(gap as usize),
                Style::default().fg(C_FAINT),
            ));
            x_offset += gap;

            let chip_width = label.len() as u16;
            // Store chip rect (at its spread position) for mouse hit-testing.
            app.last_rects
                .filter_chips
                .push((Rect::new(x_offset, area.y, chip_width, 1), *filter));
            x_offset += chip_width;
            spans.push(Span::styled(label.clone(), chip_style(filter)));
        }
    } else {
        // Narrow row: fall back to the original left-packed layout so chips
        // never overlap (they may run off the edge / be truncated by the
        // Paragraph, which is preferable to overlapping hit-rects).
        for (filter, label) in &chip_data {
            let chip_width = label.len() as u16;
            app.last_rects
                .filter_chips
                .push((Rect::new(x_offset, area.y, chip_width, 1), *filter));
            x_offset += chip_width + 2; // +2 for the separator
            spans.push(Span::styled(label.clone(), chip_style(filter)));
            spans.push(Span::styled("  ", Style::default().fg(C_FAINT)));
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(style_normal()),
        area,
    );
}

// ---------------------------------------------------------------------------
// 2  Book table
// ---------------------------------------------------------------------------

/// Per-variation display cells for a SINGLE armed copy (used for the primary
/// row of a stacked book and for each "↳ alt. copy" sub-row). Returns
/// `(fmt, size, state_label, progress)`.
fn variation_display(v: &libgen_engine::ViewVariation, tick: u64) -> (String, String, String, u32) {
    let state_label = match v.state.as_str() {
        "done" => "\u{2713} done".to_string(),
        "downloading" => format!("{} {}%", theme::spinner(tick), v.progress),
        "queued" => "\u{00b7} queued".to_string(),
        "paused" => "\u{23f8} paused".to_string(),
        "failed" | "cancelled" => "\u{2717} failed".to_string(),
        other => other.to_string(),
    };
    let size = if v.size > 0 {
        format!("{} MB", v.size)
    } else {
        "\u{2014}".to_string()
    };
    (v.fmt.clone(), size, state_label, v.progress)
}

/// Map a rendered state label back to a core state key for `style_for_state`.
fn state_key_for_label(label: &str) -> &'static str {
    if label.contains("done") {
        "done"
    } else if label.contains('%') {
        "downloading"
    } else if label.contains("failed") || label.contains("not found") {
        "failed"
    } else if label.contains("choose") {
        "available"
    } else if label.contains("paused") {
        "paused"
    } else {
        "queued"
    }
}

/// One pre-collected library table row (group divider or a body book/sub-row),
/// built before the marquee `&mut` borrow and the final layout decision.
enum BookItem {
    /// A non-focusable group divider — name ellipsized (#12), `done/total` frac.
    Group { name: String, frac: String },
    /// A book primary row or an indented "↳ alt. copy" sub-row.
    Body {
        rref: RowRef,
        seq_cell: Cell<'static>,
        title: String,
        author: String,
        title_style: Style,
        author_style: Style,
        /// (text, style) for the rest fields: Fmt, Size, State.
        rest: Vec<(String, Style)>,
        base_style: Style,
        /// Is this the focused (selected + List-focused, no modal) row? Only it
        /// marquees; everyone else ellipsizes.
        focused: bool,
        /// Is this the selected row at all (regardless of pane focus)? Drives the
        /// table scroll so the selection stays in view.
        selected: bool,
    },
}

fn render_book_table(frame: &mut Frame, app: &mut AppState, area: Rect) {
    // Clear previous book_rows.
    app.last_rects.book_rows.clear();

    if app.view.is_none() {
        let para = Paragraph::new("No list loaded. Press : and type 'import <file>' to load.")
            .style(style_dim());
        frame.render_widget(para, area);
        return;
    }

    // Fixed seq/accent column; everything else is the flexing 3-region content.
    const SEQ_W: u16 = 4;
    let content_w = (area.width as usize).saturating_sub(SEQ_W as usize);

    // The focused row only marquees when the List pane owns focus and no modal
    // covers the table (the table is drawn behind every modal each frame, so the
    // book-row marquee must NOT advance `var_marquee_*` while Detail/Picker also
    // animate it).
    let marquee_active = app.modal.is_none() && app.focus == Focus::List;

    // ── Phase 1: collect every row, the rest-field natural widths, and the
    //    focused row's metrics — all under the shared `&app.flat` borrow. ──────
    let mut items: Vec<BookItem> = Vec::new();
    let mut last_group: Option<usize> = None;
    let mut rest_widths = [0usize; 3];
    // Focused row's (title, author, rest strings) for the marquee math below.
    let mut focused_meta: Option<(String, String, Vec<String>)> = None;

    for (i, fb) in app.flat.iter().enumerate() {
        let book = &fb.book;
        let book_focused = i == app.selected && app.selected_var.is_none();
        let is_selected = book_focused && app.focus == Focus::List;
        let is_inactive_selected = book_focused && app.focus != Focus::List;

        // Emit a group-header divider whenever the owning group changes.
        if last_group != Some(fb.group_index) {
            last_group = Some(fb.group_index);
            let total = app
                .flat
                .iter()
                .filter(|o| o.group_index == fb.group_index)
                .count();
            let done = app
                .flat
                .iter()
                .filter(|o| o.group_index == fb.group_index)
                .filter(|o| {
                    o.book
                        .acquisition
                        .as_ref()
                        .map(|a| a.done >= 1 && a.active == 0)
                        .unwrap_or(false)
                })
                .count();
            items.push(BookItem::Group {
                name: fb.group_name.clone(),
                frac: format!("{done}/{total}"),
            });
        }

        let armed = armed_variations(book);
        let stacked = armed.len() >= 2;

        // Determine the PRIMARY row's rest-field cells.
        let (display_fmt, display_size, display_state, _progress) = if stacked {
            variation_display(armed[0], app.tick)
        } else if book.versions.is_empty() {
            let disc = match book.discovery.as_str() {
                "not_found" => "\u{2717} not found",
                "needs_selection" => "\u{25cf} choose",
                "queuing" | "querying" => "\u{280b} querying",
                _ => "queued",
            };
            (
                "???".to_string(),
                "\u{2014}".to_string(),
                disc.to_string(),
                0u32,
            )
        } else {
            let best = book
                .versions
                .iter()
                .find(|v| v.state == "downloading")
                .or_else(|| book.versions.iter().find(|v| v.state == "done"))
                .or_else(|| book.versions.first())
                .unwrap();
            let state_label = match best.state.as_str() {
                "done" => "\u{2713} done".to_string(),
                "downloading" => format!("{} {}%", theme::spinner(app.tick), best.progress),
                "failed" | "cancelled" => "\u{2717} failed".to_string(),
                "queued" => "\u{00b7} queued".to_string(),
                "paused" => "\u{23f8} paused".to_string(),
                _ => best.state.clone(),
            };
            let size_label = if best.size > 0 {
                format!("{} MB", best.size)
            } else {
                "\u{2014}".to_string()
            };
            (best.fmt.clone(), size_label, state_label, best.progress)
        };

        let state_style = theme::style_for_state(state_key_for_label(&display_state));
        let base_style = if is_selected {
            style_selected()
        } else {
            style_normal()
        };
        let title_style = if is_selected {
            style_selected()
        } else {
            style_title()
        };
        // On the selected row the author is ENHANCED to warm beige; else dim.
        let author_style = if is_selected {
            Style::default()
                .fg(C_WARM)
                .bg(C_SELECTED)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_MUTED)
        };
        let meta_style = if is_selected {
            style_selected()
        } else {
            Style::default().fg(C_MUTED)
        };

        // Left accent cell: ▌ accent (active), dim ▌ (inactive), else seq #.
        let seq_cell = if is_selected {
            Cell::from("\u{258c}").style(Style::default().fg(C_DONE).bg(C_SELECTED))
        } else if is_inactive_selected {
            Cell::from("\u{258c}").style(style_sel_accent_dim())
        } else {
            Cell::from(format!("{:>3}", book.seq)).style(Style::default().fg(C_FAINT))
        };

        let rest = vec![
            (display_fmt, meta_style),
            (display_size, meta_style),
            (display_state, state_style),
        ];
        for (k, (s, _)) in rest.iter().enumerate() {
            rest_widths[k] = rest_widths[k].max(crate::textfit::display_width(s));
        }

        let focused = is_selected && marquee_active;
        if focused {
            focused_meta = Some((
                book.title.clone(),
                book.author.clone(),
                rest.iter().map(|(s, _)| s.clone()).collect(),
            ));
        }

        items.push(BookItem::Body {
            rref: RowRef::Book(i),
            seq_cell,
            title: book.title.clone(),
            author: book.author.clone(),
            title_style,
            author_style,
            rest,
            base_style,
            focused,
            selected: book_focused,
        });

        // ── Indented "↳ alt. copy" sub-rows for each ADDITIONAL armed copy ──
        if stacked {
            for v in &armed[1..] {
                let var_focused =
                    i == app.selected && app.selected_var.as_deref() == Some(v.md5.as_str());
                let sub_selected = var_focused && app.focus == Focus::List;
                let sub_inactive = var_focused && app.focus != Focus::List;

                let (vfmt, vsize, vstate, _vprog) = variation_display(v, app.tick);
                let vstate_style = theme::style_for_state(state_key_for_label(&vstate));
                let vbase_style = if sub_selected {
                    style_selected()
                } else {
                    style_normal()
                };
                let vseq_cell = if sub_selected {
                    Cell::from("\u{258c}").style(Style::default().fg(C_DONE).bg(C_SELECTED))
                } else if sub_inactive {
                    Cell::from("\u{258c}").style(style_sel_accent_dim())
                } else {
                    Cell::from("").style(Style::default().fg(C_FAINT))
                };
                let vmeta_style = if sub_selected {
                    style_selected()
                } else {
                    Style::default().fg(C_MUTED)
                };
                let vtitle_style = vmeta_style;

                let host = v.host.as_deref().unwrap_or("\u{2014}");
                let label = format!("  \u{21b3} alt. copy \u{00b7} {host}");

                let vrest = vec![
                    (vfmt, vmeta_style),
                    (vsize, vmeta_style),
                    (vstate, vstate_style),
                ];
                for (k, (s, _)) in vrest.iter().enumerate() {
                    rest_widths[k] = rest_widths[k].max(crate::textfit::display_width(s));
                }

                let vfocused = sub_selected && marquee_active;
                if vfocused {
                    focused_meta = Some((
                        label.clone(),
                        String::new(),
                        vrest.iter().map(|(s, _)| s.clone()).collect(),
                    ));
                }

                items.push(BookItem::Body {
                    rref: RowRef::Variation(i, v.md5.clone()),
                    seq_cell: vseq_cell,
                    title: label,
                    author: String::new(),
                    title_style: vtitle_style,
                    author_style: vmeta_style,
                    rest: vrest,
                    base_style: vbase_style,
                    focused: vfocused,
                    selected: var_focused,
                });
            }
        }
    }

    // ── Phase 2: decide the 3-region split, then advance the focused marquee. ──
    let layout = book_row_layout(content_w, &rest_widths);

    if marquee_active {
        // Identity = selected book + (disambiguated) focused sub-row, so the
        // marquee resets when you move between rows.
        let sel_id = app
            .selected
            .wrapping_mul(97)
            .wrapping_add(if app.selected_var.is_some() {
                app.detail_variation_index(app.selected) + 1
            } else {
                0
            });
        app.reset_var_marquee_if_changed(sel_id);
        if let Some((t, a, rest_strs)) = &focused_meta {
            match &layout {
                BookRowLayout::Fixed {
                    title_w, author_w, ..
                } => {
                    let ta_w = *title_w + BOOK_SEP + *author_w;
                    let text_w = crate::textfit::display_width(t)
                        + if a.is_empty() {
                            0
                        } else {
                            2 + crate::textfit::display_width(a)
                        };
                    app.advance_var_marquee(text_w, ta_w);
                }
                BookRowLayout::Packed { width } => {
                    let mut packed = combine_title_author(t, a);
                    for s in rest_strs {
                        if !s.is_empty() {
                            packed.push_str(", ");
                            packed.push_str(s);
                        }
                    }
                    app.advance_var_marquee(crate::textfit::display_width(&packed), *width);
                }
            }
        } else {
            app.advance_var_marquee(0, 1); // no focused row → park at zero
        }
    }
    let marquee_off = app.var_marquee_offset;

    // ── Phase 3: build the table rows + hit-test rects (visual_row = item idx). ─
    let mut rows: Vec<Row> = Vec::new();
    let mut selected_visual: usize = 0;
    for (idx, item) in items.iter().enumerate() {
        match item {
            BookItem::Group { name, frac } => {
                let frac_w = crate::textfit::display_width(frac);
                let name_w = content_w.saturating_sub(frac_w + 1);
                let line = Line::from(vec![
                    Span::styled(
                        pad_cell(&crate::textfit::ellipsize(name, name_w), name_w),
                        style_header(),
                    ),
                    Span::styled(" ", style_header()),
                    Span::styled(frac.clone(), style_header()),
                ]);
                rows.push(
                    Row::new([Cell::from(""), Cell::from(line)])
                        .height(1)
                        .style(style_header()),
                );
            }
            BookItem::Body {
                rref,
                seq_cell,
                title,
                author,
                title_style,
                author_style,
                rest,
                base_style,
                focused,
                selected,
            } => {
                if *selected {
                    selected_visual = idx;
                }
                let line = book_row_line(
                    &layout,
                    title,
                    author,
                    *title_style,
                    *author_style,
                    rest,
                    *focused,
                    marquee_off,
                    *base_style,
                );
                rows.push(
                    Row::new([seq_cell.clone(), Cell::from(line)])
                        .height(1)
                        .style(*base_style),
                );
                let rect = Rect::new(area.x, area.y + idx as u16, area.width, 1);
                app.last_rects.book_rows.push((rect, rref.clone()));
            }
        }
    }

    let mut table_state = TableState::default();
    if !app.flat.is_empty() {
        table_state.select(Some(selected_visual));
    }

    let table = Table::new(rows, [Constraint::Length(SEQ_W), Constraint::Min(0)])
        .column_spacing(0)
        .row_highlight_style(Style::default().bg(C_SELECTED));

    frame.render_stateful_widget(table, area, &mut table_state);
}

// ---------------------------------------------------------------------------
// 3  Docked Activity pane  (BORDERLESS — plain line rendering)
// ---------------------------------------------------------------------------

/// Build one Activity transfer-leg line with the transfer STATUS pinned to the
/// right and the leg TITLE flexing in the middle (#9).
///
/// Layout across `row_w` columns:
/// ```text
///  marker | title (flex) | …gap… | status (fmt · % · bar · eta) →pinned right
/// ```
/// The `status` field (its display width) is reserved on the right so it is
/// ALWAYS visible; the title gets the remaining width (minus a 2-col gap) and is
/// marquee-scrolled when `focused`, else `…`-ellipsized.
#[allow(clippy::too_many_arguments)]
fn activity_leg_line(
    marker: String,
    marker_style: Style,
    title: &str,
    title_style: Style,
    status: String,
    status_style: Style,
    row_w: usize,
    focused: bool,
    marquee_off: usize,
) -> Line<'static> {
    let marker_w = crate::textfit::display_width(&marker);
    let status_w = crate::textfit::display_width(&status);
    // Region left of the pinned status for the title (+ a 2-col gap before it).
    let avail = row_w.saturating_sub(marker_w + status_w);
    let title_budget = avail.saturating_sub(2);
    let title_fitted = if focused {
        crate::textfit::marquee_window(title, title_budget, marquee_off)
    } else {
        crate::textfit::ellipsize(title, title_budget)
    };
    let pad = avail.saturating_sub(crate::textfit::display_width(&title_fitted));
    Line::from(vec![
        Span::styled(marker, marker_style),
        Span::styled(title_fitted, title_style),
        Span::styled(" ".repeat(pad), title_style),
        Span::styled(status, status_style),
    ])
}

fn render_activity(frame: &mut Frame, app: &mut AppState, area: Rect) {
    // Count download states.
    let downloading_count = app
        .flat
        .iter()
        .flat_map(|fb| fb.book.versions.iter())
        .filter(|v| v.state == "downloading")
        .count();
    let queued_count = app
        .flat
        .iter()
        .flat_map(|fb| fb.book.versions.iter())
        .filter(|v| v.state == "queued")
        .count();
    let connecting_count = app
        .transfers
        .values()
        .filter(|t| {
            // transfers that are resolving (no bytes yet)
            t.bytes_done == 0
        })
        .count();

    // Aggregate speed across all live transfers
    let total_speed_bps: u64 = app.transfers.values().filter_map(|t| t.speed_bps).sum();
    let speed_str = if total_speed_bps >= 1_000_000 {
        format!(" \u{2193} {:.1}MB/s ", total_speed_bps as f64 / 1_000_000.0)
    } else if total_speed_bps >= 1_000 {
        format!(" \u{2193} {}KB/s ", total_speed_bps / 1_000)
    } else {
        String::new()
    };

    // Build header line
    let arrow = if app.activity_expanded {
        "\u{25be}"
    } else {
        "\u{25b8}"
    };
    let toggle_hint = if app.focus == Focus::Activity {
        if app.activity_expanded {
            "space collapse"
        } else {
            "space expand"
        }
    } else {
        "tab to focus"
    };
    // The stats that follow the green "ACTIVITY" word.
    let header_stats = if app.activity_expanded {
        format!(
            "  {} downloading \u{00b7} {} connecting \u{00b7} {} queued{}  {}",
            downloading_count, connecting_count, queued_count, speed_str, toggle_hint
        )
    } else {
        format!(
            "  {} downloading \u{00b7} {} queued{}  {}",
            downloading_count, queued_count, speed_str, toggle_hint
        )
    };
    let header_style = if app.focus == Focus::Activity {
        style_normal()
    } else {
        style_muted()
    };
    // "ACTIVITY" is always green; the arrow + stats follow the focus style.
    let header_line = Line::from(vec![
        Span::styled(format!("{} ", arrow), header_style),
        Span::styled(
            "ACTIVITY",
            Style::default().fg(C_DONE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(header_stats, header_style),
    ]);

    if !app.activity_expanded {
        frame.render_widget(Paragraph::new(header_line).style(style_normal()), area);
        return;
    }

    // Build per-host transfer lines by grouping downloading versions by host.
    // 1. Collect from flat ViewModel (has progress / fmt info from engine).
    let mut host_groups: BTreeMap<String, Vec<(String, u32, String, Option<u64>)>> =
        BTreeMap::new();
    for fb in &app.flat {
        for v in &fb.book.versions {
            if v.state == "downloading" {
                let host = v.host.as_deref().unwrap_or("unknown").to_string();
                host_groups.entry(host).or_default().push((
                    fb.book.title.clone(),
                    v.progress,
                    v.fmt.clone(),
                    v.eta_secs,
                ));
            }
        }
    }

    // 2. Fallback to live telemetry if ViewModel has nothing.
    let use_telemetry = host_groups.is_empty() && !app.transfers.is_empty();
    let mut telemetry_groups: BTreeMap<String, Vec<(String, u8, u64)>> = BTreeMap::new();
    if use_telemetry {
        for t in app.transfers.values() {
            let pct = match (t.bytes_done, t.total_bytes) {
                (done, Some(total)) if total > 0 => {
                    ((done as f64 / total as f64) * 100.0).min(100.0) as u8
                }
                _ => 0,
            };
            let title = if t.title.is_empty() {
                t.md5.chars().take(8).collect::<String>()
            } else {
                t.title.clone()
            };
            let speed = t.speed_bps.unwrap_or(0);
            telemetry_groups
                .entry(t.host.clone())
                .or_default()
                .push((title, pct, speed));
        }
    }

    // capacity: area.height - 1 (header) lines available for transfer rows.
    let capacity = area.height.saturating_sub(1) as usize;

    // Whether Activity pane currently holds focus — used for selection styling.
    let activity_active = app.focus == Focus::Activity;

    // #9: full-width row budget + reset the focused-leg title marquee when the
    // selection moves (the focused leg's title marquees; status is pinned right).
    let row_w = area.width as usize;
    let sel_leg = app.activity_selected;
    app.reset_activity_marquee_if_changed(sel_leg);

    // Build content lines (to be windowed by scroll offset).
    // leg_idx counts download-book rows so activity_selected maps to them.
    // (#66: legs are ALWAYS selectable — no overflow condition.)
    let mut all_content: Vec<Line> = Vec::new();
    // Parallel to all_content: Some(leg_idx) for transfer-leg rows, None for others.
    let mut leg_map: Vec<Option<usize>> = Vec::new();
    let mut leg_idx: usize = 0;

    if !use_telemetry {
        if host_groups.is_empty() {
            all_content.push(Line::from(Span::styled(
                "  No active transfers.",
                style_dim(),
            )));
            leg_map.push(None);
        } else {
            for (host, versions) in &host_groups {
                // Per-host aggregate speed
                let host_speed: u64 = app
                    .transfers
                    .values()
                    .filter(|t| &t.host == host)
                    .filter_map(|t| t.speed_bps)
                    .sum();
                let host_speed_str = if host_speed >= 1_000_000 {
                    format!("{:.1}MB/s", host_speed as f64 / 1_000_000.0)
                } else if host_speed >= 1_000 {
                    format!("{}KB/s", host_speed / 1_000)
                } else {
                    String::new()
                };
                let host_line = format!(
                    "\u{25cf} {}{}",
                    host,
                    if host_speed_str.is_empty() {
                        String::new()
                    } else {
                        format!("   {}↓ \u{00b7} {}", versions.len(), host_speed_str)
                    }
                );
                all_content.push(Line::from(Span::styled(
                    host_line,
                    Style::default().fg(C_WARM),
                )));
                leg_map.push(None); // host label — not a selectable leg

                for (title, pct, fmt, eta_secs) in versions {
                    let is_leg_sel = activity_active && leg_idx == sel_leg;
                    let is_leg_dim = !activity_active && leg_idx == sel_leg;
                    let bar = theme::progress_bar((*pct).into(), 6);
                    let eta = eta_secs.map(|s| format!("  {}s", s)).unwrap_or_default();
                    // Pinned-right STATUS: fmt · % · bar · eta (always visible).
                    let status = format!("{}  {}%  {}{}", fmt, pct, bar, eta);
                    let (marker, marker_style, title_style, status_style) = if is_leg_sel {
                        (
                            format!("  \u{25b8} {} ", theme::spinner(app.tick)),
                            Style::default().fg(C_DONE).bg(C_SELECTED),
                            style_selected(),
                            style_selected(),
                        )
                    } else if is_leg_dim {
                        (
                            format!("  \u{25b8} {} ", theme::spinner(app.tick)),
                            style_dim(),
                            style_muted(),
                            style_dim(),
                        )
                    } else {
                        (
                            format!("  {} ", theme::spinner(app.tick)),
                            style_muted(),
                            style_normal(),
                            style_muted(),
                        )
                    };
                    // Focused leg: advance + use its title marquee; else `…`.
                    let marquee_off = if is_leg_sel {
                        let budget = row_w
                            .saturating_sub(
                                crate::textfit::display_width(&marker)
                                    + crate::textfit::display_width(&status),
                            )
                            .saturating_sub(2);
                        app.advance_activity_marquee(crate::textfit::display_width(title), budget);
                        app.activity_marquee_offset
                    } else {
                        0
                    };
                    all_content.push(activity_leg_line(
                        marker,
                        marker_style,
                        title,
                        title_style,
                        status,
                        status_style,
                        row_w,
                        is_leg_sel,
                        marquee_off,
                    ));
                    leg_map.push(Some(leg_idx)); // transfer-leg row — selectable
                    leg_idx += 1;
                }
            }
        }
    } else {
        for (host, transfers) in &telemetry_groups {
            let host_speed: u64 = transfers.iter().map(|(_, _, s)| s).sum();
            let speed_s = if host_speed >= 1_000_000 {
                format!("{:.1}MB/s", host_speed as f64 / 1_000_000.0)
            } else if host_speed >= 1_000 {
                format!("{}KB/s", host_speed / 1_000)
            } else {
                String::new()
            };
            let host_line = format!(
                "\u{25cf} {}   {}↓{}",
                host,
                transfers.len(),
                if speed_s.is_empty() {
                    String::new()
                } else {
                    format!(" \u{00b7} {}", speed_s)
                }
            );
            all_content.push(Line::from(Span::styled(
                host_line,
                Style::default().fg(C_WARM),
            )));
            leg_map.push(None); // host label — not a selectable leg
            for (title, pct, _) in transfers {
                let is_leg_sel = activity_active && leg_idx == sel_leg;
                let is_leg_dim = !activity_active && leg_idx == sel_leg;
                let bar = theme::progress_bar((*pct).into(), 6);
                // Pinned-right STATUS: % · bar (always visible).
                let status = format!("{}%  {}", pct, bar);
                let (marker, marker_style, title_style, status_style) = if is_leg_sel {
                    (
                        format!("  \u{25b8} {} ", theme::spinner(app.tick)),
                        Style::default().fg(C_DONE).bg(C_SELECTED),
                        style_selected(),
                        style_selected(),
                    )
                } else if is_leg_dim {
                    (
                        format!("  \u{25b8} {} ", theme::spinner(app.tick)),
                        style_dim(),
                        style_muted(),
                        style_dim(),
                    )
                } else {
                    (
                        format!("  {} ", theme::spinner(app.tick)),
                        style_muted(),
                        style_normal(),
                        style_muted(),
                    )
                };
                let marquee_off = if is_leg_sel {
                    let budget = row_w
                        .saturating_sub(
                            crate::textfit::display_width(&marker)
                                + crate::textfit::display_width(&status),
                        )
                        .saturating_sub(2);
                    app.advance_activity_marquee(crate::textfit::display_width(title), budget);
                    app.activity_marquee_offset
                } else {
                    0
                };
                all_content.push(activity_leg_line(
                    marker,
                    marker_style,
                    title,
                    title_style,
                    status,
                    status_style,
                    row_w,
                    is_leg_sel,
                    marquee_off,
                ));
                leg_map.push(Some(leg_idx)); // transfer-leg row — selectable
                leg_idx += 1;
            }
        }
    }

    // Apply scroll windowing to content lines.
    let n = all_content.len();
    let windowed: Vec<Line> = if n == 0 || n <= capacity {
        all_content
    } else {
        let offset = app.activity_selected.min(n.saturating_sub(1));
        let has_above = offset > 0;
        let above_slots = usize::from(has_above);
        let mut tfer_slots = capacity.saturating_sub(above_slots + 1).max(1);
        let end = (offset + tfer_slots).min(n);
        let has_below = end < n;
        if !has_below {
            tfer_slots = capacity.saturating_sub(above_slots).max(1);
        }
        let end = (offset + tfer_slots).min(n);
        let mut display: Vec<Line> = Vec::with_capacity(capacity);
        if has_above {
            display.push(Line::from(Span::styled(
                format!("  \u{25b4} {} above", offset),
                style_muted(),
            )));
        }
        for line in &all_content[offset..end] {
            display.push(line.clone());
        }
        if has_below {
            display.push(Line::from(Span::styled(
                format!("  \u{25be} {} more", n - end),
                style_muted(),
            )));
        }
        display
    };

    // Populate activity_rows for mouse hit-testing.
    // Row 0 of the pane is the header (area.y); content starts at area.y + 1.
    app.last_rects.activity_rows.clear();
    let base_y = area.y + 1;
    if n == 0 || n <= capacity {
        // All content visible in order.
        for (ci, lo) in leg_map.iter().enumerate() {
            if let Some(li) = *lo {
                let row_y = base_y + ci as u16;
                if row_y < area.y + area.height {
                    app.last_rects
                        .activity_rows
                        .push((Rect::new(area.x, row_y, area.width, 1), li));
                }
            }
        }
    } else {
        let offset = app.activity_selected.min(n.saturating_sub(1));
        let has_above = offset > 0;
        let above_slots = usize::from(has_above);
        let mut tfer_slots = capacity.saturating_sub(above_slots + 1).max(1);
        let end_i = (offset + tfer_slots).min(n);
        let has_below = end_i < n;
        if !has_below {
            tfer_slots = capacity.saturating_sub(above_slots).max(1);
        }
        let end_i = (offset + tfer_slots).min(n);
        let content_base_y = base_y + above_slots as u16;
        for (pos, ci) in (offset..end_i).enumerate() {
            if let Some(li) = leg_map[ci] {
                let row_y = content_base_y + pos as u16;
                if row_y < area.y + area.height {
                    app.last_rects
                        .activity_rows
                        .push((Rect::new(area.x, row_y, area.width, 1), li));
                }
            }
        }
    }

    // Combine header + windowed content and render as a plain Paragraph.
    let mut all_lines: Vec<Line> = Vec::with_capacity(1 + windowed.len());
    all_lines.push(header_line);
    all_lines.extend(windowed);

    frame.render_widget(Paragraph::new(all_lines).style(style_normal()), area);
}

// ---------------------------------------------------------------------------
// 4  Command-line row (only rendered when : is active)
// ---------------------------------------------------------------------------

fn render_command_line(frame: &mut Frame, app: &AppState, area: Rect) {
    let content = if let Some(ref buf) = app.command_buf {
        command_input_line(buf, area.width)
    } else {
        Line::default()
    };
    frame.render_widget(Paragraph::new(content).style(style_hint()), area);
}

/// Build the scrolling `:` command-line input line for an `inner_w`-wide field.
///
/// Shared by the main command row ([`render_command_line`]) and the empty-screen
/// import box ([`render_empty`]) so both inputs scroll identically (#3). The `:`
/// prefix is pinned; the buffer + trailing block cursor scroll-to-cursor via
/// [`crate::textfit::scroll_to_cursor`] so the cursor never falls off the right
/// edge. One column is reserved on each side of the buffer region for the
/// `‹`/`›` edge indicators (shown only when text is hidden that way).
pub(crate) fn command_input_line(buf: &str, inner_w: u16) -> Line<'static> {
    // The block cursor sits one cell past the buffer (end-of-line insertion).
    let content = format!("{buf}\u{2588}");
    let cursor_col = crate::textfit::display_width(&content).saturating_sub(1);
    // Reserve: 1 col for the pinned ":" + 1 col each side for the ‹/› markers.
    let window = (inner_w as usize).saturating_sub(3).max(1);
    let cv = crate::textfit::scroll_to_cursor(&content, cursor_col, window);
    let left = if cv.clipped_left { "\u{2039}" } else { " " };
    let right = if cv.clipped_right { "\u{203a}" } else { " " };
    // Peel the trailing block cursor off the visible slice so it keeps its own
    // bright style (the cursor is always the last visible glyph here).
    let mut visible = cv.visible;
    let cursor_span = if visible.ends_with('\u{2588}') {
        visible.pop();
        Some(Span::styled(
            "\u{2588}".to_string(),
            Style::default().fg(C_TEXT),
        ))
    } else {
        None
    };
    let mut spans = vec![
        Span::styled(":", style_hint()),
        Span::styled(left, style_dim()),
        Span::styled(visible, style_hint()),
    ];
    if let Some(c) = cursor_span {
        spans.push(c);
    }
    spans.push(Span::styled(right, style_dim()));
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// 5  Hint bar (always the very last row — shows hint keys or status message)
// ---------------------------------------------------------------------------

/// Returns a coarse hint-state token for the currently-selected book in the
/// List pane.  Used by `render_hint_bar` to pick context-specific action chips.
fn selected_book_hint_state(app: &AppState) -> &'static str {
    let Some(fb) = app.flat.get(app.selected) else {
        return "unknown";
    };
    let book = &fb.book;
    if book.discovery == "needs_selection" {
        return "needs_selection";
    }
    if book.versions.iter().any(|v| v.state == "downloading") {
        return "downloading";
    }
    if book.versions.iter().any(|v| v.state == "done") {
        return "done";
    }
    if book
        .versions
        .iter()
        .any(|v| v.state == "failed" || v.state == "cancelled")
    {
        return "failed";
    }
    "unknown"
}

/// Build the focus/state-appropriate hint string for the global bottom bar
/// (everything except `:` command mode and transient status messages, which
/// are single, non-wrapping lines). The returned string uses ` · ` / double
/// space hint boundaries that [`wrap_hint`] breaks on.
fn global_hint_text(app: &AppState) -> String {
    // ⏎ is universal (shown only in the Help screen per #70) — never in the hint bar.
    const GLOBALS: &str = "  : command \u{00b7} ? help \u{00b7} q quit";
    match app.focus {
        Focus::Header => {
            // Header focus owns the LIST ops (re-search / pause / start / delete).
            format!(
                "\u{2190}\u{2192} filter  r re-search \u{00b7} p pause \u{00b7} s start \u{00b7} D delete  [ ] list{GLOBALS}"
            )
        }
        Focus::List => {
            let state = selected_book_hint_state(app);
            match state {
                "needs_selection" => format!("choose  d detail{GLOBALS}"),
                "failed" => format!("r retry  d detail{GLOBALS}"),
                "done" => format!("d detail \u{00b7} o open{GLOBALS}"),
                "downloading" => format!("p pause \u{00b7} c cancel  d detail{GLOBALS}"),
                _ => format!("d detail{GLOBALS}"),
            }
        }
        Focus::Activity => {
            // p/c/r act on the focused download leg — only show them when a leg
            // exists; otherwise just the pane/collapse keys.
            if app.activity_has_legs() {
                format!(
                    "p pause \u{00b7} c cancel \u{00b7} r retry \u{00b7} space collapse{GLOBALS}"
                )
            } else {
                format!("tab pane \u{00b7} space collapse{GLOBALS}")
            }
        }
    }
}

/// Build the global bottom-bar lines, WRAPPING the focus hints to as many lines
/// as the width needs so no hint is dropped (#7). The `:` command prompt and
/// transient status message stay single-line. The caller sizes the bar area to
/// `.len()` rows.
fn hint_bar_lines(app: &AppState, width: u16) -> Vec<Line<'static>> {
    // ── `:` command-line mode → a PROMPT plus only the keys live here. ──
    // The `:add` argument sub-mode shows the book-entry prompt; the bare
    // command line shows a generic "type a command" prompt.
    if let Some(buf) = app.command_buf.as_deref() {
        let in_add = {
            let t = buf.trim_start();
            t == "add" || t.starts_with("add ")
        };
        let prompt = if in_add {
            "enter a book title (and author) or the MD5"
        } else {
            "type a command"
        };
        let key = Style::default().fg(C_DONE).bg(C_PANEL);
        let mut spans: Vec<Span> = vec![Span::styled(prompt.to_string(), style_hint())];
        spans.push(Span::styled("  \u{00b7} ", style_hint()));
        // Tab is only meaningful while the completion wildmenu is open.
        if !app.completion_candidates.is_empty() {
            spans.push(Span::styled("Tab", key));
            spans.push(Span::styled(" complete \u{00b7} ", style_hint()));
        }
        spans.push(Span::styled("esc", key));
        spans.push(Span::styled(" cancel", style_hint()));
        return vec![Line::from(spans)];
    }

    if let Some(ref msg) = app.status_msg {
        // Transient status message — shown until the next keypress.
        return vec![Line::from(Span::styled(
            crate::i18n::decode(msg),
            Style::default().fg(C_BRIGHT),
        ))];
    }

    wrap_hint_lines(&global_hint_text(app), width)
}

/// Context-aware Detail-modal footer hint, varying by sub-focus and the
/// selected variation's state. `m` reads "mark unavailable"; `S`
/// (download-series) is live across the whole detail context. Uses ` · ` hint
/// boundaries so [`wrap_hint`] can wrap it (#8).
fn detail_hint_text(
    sub_focus: &DetailSubFocus,
    fb: &crate::app::FlatBook,
    detail_selected: usize,
) -> &'static str {
    match sub_focus {
        DetailSubFocus::History => "S series \u{00b7} esc back",
        DetailSubFocus::Variations => {
            let var_state = fb
                .book
                .versions
                .get(detail_selected)
                .map(|v| v.state.as_str())
                .unwrap_or("available");
            match var_state {
                "done" => {
                    "o open \u{00b7} R reveal \u{00b7} r re-download  e edit \u{00b7} x remove \u{00b7} m mark unavailable \u{00b7} S series \u{00b7} esc back"
                }
                "downloading" => {
                    "e edit \u{00b7} x remove \u{00b7} m mark unavailable \u{00b7} S series \u{00b7} esc back"
                }
                "failed" | "cancelled" => {
                    "r retry  e edit \u{00b7} x remove \u{00b7} m mark unavailable \u{00b7} S series \u{00b7} esc back"
                }
                _ => {
                    // available / queued
                    "d download  e edit \u{00b7} x remove \u{00b7} m mark unavailable \u{00b7} S series \u{00b7} esc back"
                }
            }
        }
    }
}

/// Context-sensitive Settings-modal footer hint (⏎ omitted per #70). Pulled out
/// of `render_settings_modal` so its WRAPPED height can be measured before the
/// modal layout reserves the footer rows (#8). `None`/`Viewing` share a branch.
fn settings_hint_text(app: &AppState) -> String {
    let editor = app.settings_draft.as_ref().map(|d| &d.editor);
    match editor {
        Some(SettingsEditor::Editing(_)) => "type \u{00b7} esc cancel".into(),
        Some(SettingsEditor::FormatEditor { .. }) => {
            "space toggle \u{00b7} J/K reorder \u{00b7} esc done".into()
        }
        Some(SettingsEditor::LangPicker { .. }) => "\u{2191}\u{2193} \u{00b7} esc".into(),
        _ => {
            // Viewing (or no draft yet).
            let field_hint = match settings_field_kind(app.settings_selected) {
                SettingsFieldKind::FormatPref => "format editor",
                SettingsFieldKind::Language => "pick",
                SettingsFieldKind::F32 | SettingsFieldKind::Usize | SettingsFieldKind::U32 => {
                    "\u{2190}\u{2192} nudge"
                }
                SettingsFieldKind::Bool => "space toggle",
                SettingsFieldKind::Text => "type",
                SettingsFieldKind::ReadOnly => "",
            };
            // Maintenance hot keys live in the Viewing sub-mode.
            const MAINT: &str = "r mirrors \u{00b7} o reorganize \u{00b7} c cleanup";
            if field_hint.is_empty() {
                format!("{MAINT}  s save \u{00b7} esc cancel")
            } else {
                format!("{field_hint}  {MAINT}  s save \u{00b7} esc cancel")
            }
        }
    }
}

fn render_hint_bar(frame: &mut Frame, app: &AppState, area: Rect) {
    // The command-line input is rendered in its own row by render_command_line;
    // this bar shows a transient status message, a `:`-mode prompt, or the
    // focus-appropriate hint keys (wrapped over multiple lines if needed).
    let lines = hint_bar_lines(app, area.width);
    frame.render_widget(Paragraph::new(lines).style(style_hint()), area);
}

// ---------------------------------------------------------------------------
// Modal helpers
// ---------------------------------------------------------------------------

/// Center a rect of the given width/height within `parent`.
fn centered_rect(width: u16, height: u16, parent: Rect) -> Rect {
    let x = parent.x + parent.width.saturating_sub(width) / 2;
    let y = parent.y + parent.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(parent.width), height.min(parent.height))
}

/// Render a full-frame dimming overlay so the active modal pops visually.
/// Must be called before `Clear`+modal rendering.
fn render_backdrop(frame: &mut Frame) {
    frame.render_widget(
        Block::default().style(Style::default().bg(C_BACKDROP).add_modifier(Modifier::DIM)),
        frame.area(),
    );
}

// ---------------------------------------------------------------------------
// 4a  Picker modal ("choose a copy")
// ---------------------------------------------------------------------------

fn render_picker_modal(
    frame: &mut Frame,
    app: &mut AppState,
    book_flat_index: usize,
    picker_selected: usize,
) {
    app.reset_var_marquee_if_changed(picker_selected);

    // #72: widen to ~80% of 132 cols (clamped to the terminal by centered_rect).
    let area = centered_rect(105, 26, frame.area());
    frame.render_widget(Clear, area);

    // Clone so we can mutably borrow `app` (row marquee) while holding book data.
    let Some(fb) = app.flat.get(book_flat_index).cloned() else {
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(style_dim())
                .title(Span::styled(" choose a copy ", style_dim()))
                .style(style_normal()),
            area,
        );
        return;
    };

    let n_candidates = fb.book.versions.len();
    let threshold = if let Some(v) = &app.view {
        v.settings.auto_threshold
    } else {
        0.85
    };

    // #11: the book title baked into the border is ELLIPSIZED (no marquee).
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_dim())
        .title(Span::styled(
            picker_border_title(&fb.book.title, area.width),
            style_dim(),
        ))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    // Layout: subheader (1) + column header row (1) + table rows + rule (1) + hint (wraps)
    const PICKER_HINT: &str =
        "\u{2191}\u{2193} pick  \u{23ce} this copy  a all formats  v meta  esc cancel";
    let picker_hint_h = hint_wrap_height(PICKER_HINT, padded.width);
    let split = Layout::vertical([
        Constraint::Length(1),             // subheader
        Constraint::Min(1),                // header + rows
        Constraint::Length(1),             // dim rule
        Constraint::Length(picker_hint_h), // hint (wraps at narrow widths)
    ])
    .split(padded);

    let subhead = format!(
        "{} candidates \u{00b7} auto needs one copy \u{2265} {:.2} \u{2014} none was clear, so pick.",
        n_candidates, threshold
    );
    frame.render_widget(Paragraph::new(Span::styled(subhead, style_dim())), split[0]);

    let rows_area = split[1];
    let row_w = rows_area.width as usize;

    // #11: candidate rows = #1 flex treatment. Rest fields = Fmt, Size, Year, Pg,
    // Match; title region carries Title + (author · source).
    let fields: Vec<(String, String, String, String, String)> = fb
        .book
        .versions
        .iter()
        .map(|v| {
            let fmt = v.fmt.clone();
            let size = if v.size > 0 {
                format!("{} MB", v.size)
            } else {
                "\u{2014}".into()
            };
            let year = v
                .year
                .map(|y| y.to_string())
                .unwrap_or_else(|| "\u{2014}".into());
            let pg = v
                .pages
                .map(|p| p.to_string())
                .unwrap_or_else(|| "\u{2014}".into());
            let mat = format!("{:.2}", v.score);
            (fmt, size, year, pg, mat)
        })
        .collect();

    // Title-region text = author + source (publisher·language) under the title.
    let author_source = |v: &libgen_engine::ViewVariation| -> String {
        let source = match (v.publisher.is_empty(), v.language.is_empty()) {
            (true, true) => String::new(),
            (true, false) => v.language.clone(),
            (false, true) => v.publisher.clone(),
            (false, false) => format!("{}\u{00b7}{}", v.publisher, v.language),
        };
        match (v.author.is_empty(), source.is_empty()) {
            (true, true) => String::new(),
            (true, false) => source,
            (false, true) => v.author.clone(),
            (false, false) => format!("{} \u{00b7} {}", v.author, source),
        }
    };

    let labels = ["FMT", "SIZE", "YEAR", "PG", "MATCH"];
    let mut rest_w = [
        crate::textfit::display_width(labels[0]),
        crate::textfit::display_width(labels[1]),
        crate::textfit::display_width(labels[2]),
        crate::textfit::display_width(labels[3]),
        crate::textfit::display_width(labels[4]),
    ];
    for (f, s, y, p, m) in &fields {
        rest_w[0] = rest_w[0].max(crate::textfit::display_width(f));
        rest_w[1] = rest_w[1].max(crate::textfit::display_width(s));
        rest_w[2] = rest_w[2].max(crate::textfit::display_width(y));
        rest_w[3] = rest_w[3].max(crate::textfit::display_width(p));
        rest_w[4] = rest_w[4].max(crate::textfit::display_width(m));
    }
    let rest_widths = rest_w.to_vec();
    let layout = flex_row_layout(row_w, &rest_widths);

    // Advance the focused (selected) row's marquee.
    if let Some(v) = fb.book.versions.get(picker_selected) {
        let auth = author_source(v);
        match layout {
            FlexLayout::Fixed { title_w } => {
                let combined = combine_title_author(&v.title, &auth);
                app.advance_var_marquee(crate::textfit::display_width(&combined), title_w);
            }
            FlexLayout::Packed { width } => {
                let (f, s, y, p, m) = &fields[picker_selected];
                let mut packed = combine_title_author(&v.title, &auth);
                for x in [f, s, y, p, m] {
                    if !x.is_empty() {
                        packed.push_str(", ");
                        packed.push_str(x);
                    }
                }
                app.advance_var_marquee(crate::textfit::display_width(&packed), width);
            }
        }
    }
    let marquee_off = app.var_marquee_offset;

    // Header row.
    let header_rest: Vec<(String, Style)> = labels
        .iter()
        .map(|l| (l.to_string(), style_header()))
        .collect();
    render_flex_row(
        frame,
        Rect::new(rows_area.x, rows_area.y, rows_area.width, 1),
        layout,
        " ",
        style_header(),
        "TITLE \u{00b7} SOURCE",
        "",
        style_header(),
        style_header(),
        &header_rest,
        &rest_widths,
        false,
        0,
        style_header(),
    );

    // Candidate rows (uniform 1-line height — derive y from the index).
    let bottom = rows_area.y + rows_area.height;
    for (i, v) in fb.book.versions.iter().enumerate() {
        let y = rows_area.y + 1 + i as u16;
        if y >= bottom {
            break;
        }
        let is_sel = i == picker_selected;
        let accent = if is_sel { "\u{258c}" } else { " " };
        let accent_style = if is_sel {
            style_sel_accent()
        } else {
            style_dim()
        };
        let base_style = if is_sel {
            Style::default().bg(C_SELECTED)
        } else {
            style_normal()
        };
        let title_style = if is_sel {
            style_selected()
        } else {
            style_title()
        };
        let author_style = if is_sel {
            style_selected()
        } else {
            style_dim()
        };
        let dim_or_sel = if is_sel {
            style_selected()
        } else {
            style_dim()
        };
        let match_style = if is_sel {
            style_selected()
        } else {
            Style::default().fg(score_color(v.score.into()))
        };
        let (f, s, yr, p, m) = &fields[i];
        let rest = vec![
            (f.clone(), dim_or_sel),
            (s.clone(), dim_or_sel),
            (yr.clone(), dim_or_sel),
            (p.clone(), dim_or_sel),
            (m.clone(), match_style),
        ];
        render_flex_row(
            frame,
            Rect::new(rows_area.x, y, rows_area.width, 1),
            layout,
            accent,
            accent_style,
            &v.title,
            &author_source(v),
            title_style,
            author_style,
            &rest,
            &rest_widths,
            is_sel, // focused row marquees
            marquee_off,
            base_style,
        );
    }

    // Dim rule + hint bar (⏎ omitted — see Help screen).
    render_rule(frame, split[2]);
    frame.render_widget(
        Paragraph::new(wrap_hint_lines(PICKER_HINT, split[3].width)).style(style_hint()),
        split[3],
    );
}

// ---------------------------------------------------------------------------
// 4b  Detail modal
// ---------------------------------------------------------------------------

fn render_detail_modal(
    frame: &mut Frame,
    app: &mut AppState,
    book_flat_index: usize,
    detail_selected: usize,
    sub_focus: &DetailSubFocus,
    history_selected: usize,
) {
    // #62: width = 80% of the actual window; reset marquee if variation selection changed.
    app.reset_marquee_if_selection_changed(detail_selected);
    app.reset_var_marquee_if_changed(detail_selected);
    let fa = frame.area();
    let detail_w = (fa.width as u32 * 80 / 100) as u16;
    let area = centered_rect(detail_w, 30.min(fa.height), fa);
    frame.render_widget(Clear, area);

    // Clone so we can mutably borrow `app` (for marquee) while still
    // holding the book data.  FlatBook derives Clone.
    let Some(fb) = app.flat.get(book_flat_index).cloned() else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(style_dim())
            .title(Span::styled(" Book detail ", style_dim()))
            .style(style_normal());
        frame.render_widget(block, area);
        return;
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_dim())
        .title(Span::styled(" Book detail ", style_dim()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Internal gutter: 2-cell horizontal padding, 1-cell vertical padding.
    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    // #62 Marquee: the prominent "Title · Author" header line ping-pong scrolls
    // when it overflows the modal.  Reserve room on the right for the
    // year · pages readout, then advance the offset once per render tick.
    let head_title = fb.book.title.clone();
    let head_author = fb.book.author.clone();
    // #10/#14: measure in terminal DISPLAY WIDTH (CJK=2, combining=0), not
    // `chars().count()`, so wide-glyph titles trigger + scroll correctly.
    let head_full_len = crate::textfit::display_width(&head_title)
        + if head_author.is_empty() {
            0
        } else {
            2 + crate::textfit::display_width(&head_author)
        };
    let head_avail = (padded.width as usize).saturating_sub(14).max(10);
    app.advance_marquee(head_full_len, head_avail);

    // Inner layout:
    //   title line (1)
    //   subtitle/breadcrumb line (1)
    //   blank (1)
    //   VARIATIONS section header (1)
    //   variation rows (one per version, min 1)
    //   blank (1)
    //   HISTORY section header (1)
    //   history rows (rest)
    //   hint (1)

    let n_versions = fb.book.versions.len();
    // #1: actively-downloading variations get an EXTRA line below them for the
    // progress bar, so the section grows by the number of downloading copies.
    let n_downloading = fb
        .book
        .versions
        .iter()
        .filter(|v| v.state == "downloading")
        .count();
    let var_rows_h = (n_versions.max(1) + 1 + n_downloading) as u16; // header + rows + progress lines

    // Context-aware hint footer (#8): compute it up front so the footer area can
    // GROW to the number of WRAPPED lines at this width — no hint is dropped.
    let detail_hint = detail_hint_text(sub_focus, &fb, detail_selected);
    let hint_h = hint_wrap_height(detail_hint, padded.width);

    // Subtract all fixed rows: title(1)+subtitle(1)+blank(1)+VARIATIONS(1)+var_rows(n)+blank(1)+HISTORY(1)+rule(1)+hint(hint_h).
    let history_h = padded
        .height
        .saturating_sub(1 + 1 + 1 + 1 + var_rows_h + 1 + 1 + 1 + hint_h); // = available for history

    let split = Layout::vertical([
        Constraint::Length(1),          // title line
        Constraint::Length(1),          // subtitle line
        Constraint::Length(1),          // blank
        Constraint::Length(1),          // VARIATIONS label
        Constraint::Length(var_rows_h), // variation rows
        Constraint::Length(1),          // blank
        Constraint::Length(1),          // HISTORY label
        Constraint::Min(history_h),     // history rows
        Constraint::Length(1),          // dim rule
        Constraint::Length(hint_h),     // hint (wraps at narrow widths)
    ])
    .split(padded);

    // Title line: bold title + dim author + right-aligned year · pages
    let book = &fb.book;
    let year_pages = book
        .versions
        .first()
        .map(|v| {
            let y = v.year.map(|y| y.to_string()).unwrap_or_else(|| "?".into());
            let p = v.pages.map(|p| format!("{} pages", p)).unwrap_or_default();
            if p.is_empty() {
                y
            } else {
                format!("{} \u{00b7} {}", y, p)
            }
        })
        .unwrap_or_default();

    // Build the "Title · Author" header as styled spans, then window it through
    // the ping-pong marquee offset when it overflows the line.
    let title_line = marquee_title_author(
        &book.title,
        &book.author,
        style_title(),
        style_dim(),
        if head_full_len > head_avail {
            app.marquee_offset
        } else {
            0
        },
        head_avail,
    );
    frame.render_widget(Paragraph::new(title_line), split[0]);

    // Right-align year · pages
    if !year_pages.is_empty() {
        let yp_w = year_pages.len() as u16;
        let yp_area = Rect::new(
            split[0].x + split[0].width.saturating_sub(yp_w),
            split[0].y,
            yp_w,
            1,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(year_pages, style_dim())),
            yp_area,
        );
    }

    // Subtitle line: breadcrumb-style
    let n_requested = fb.book.versions.len();
    let n_done = fb
        .book
        .versions
        .iter()
        .filter(|v| v.state == "done")
        .count();
    let n_active = fb
        .book
        .versions
        .iter()
        .filter(|v| v.state == "downloading")
        .count();
    let backfill_note = if book.backfilled.is_empty() {
        format!(
            "{} req \u{00b7} {} done \u{00b7} {} active",
            n_requested, n_done, n_active
        )
    } else {
        format!("{} auto-filled from match", book.backfilled.join(" & "))
    };
    let subtitle = format!(
        "{} \u{00b7} seq {:02}   \u{25cf} {}",
        fb.group_name, book.seq, backfill_note
    );
    frame.render_widget(
        Paragraph::new(Span::styled(subtitle, style_dim())),
        split[1],
    );

    // VARIATIONS header — accent when Variations is focused.
    let var_summary = format!(
        "\u{25be} VARIATIONS  {} requested \u{00b7} {} done \u{00b7} {} active",
        n_requested, n_done, n_active
    );
    // Lower-contrast section header: dim when focused, mid-dim otherwise.
    let var_header_style = if *sub_focus == DetailSubFocus::Variations {
        style_dim()
    } else {
        style_muted()
    };
    frame.render_widget(
        Paragraph::new(Span::styled(var_summary, var_header_style)),
        split[3],
    );

    // ── Variation rows (#1): manual flex layout, NOT a fixed-column Table ─────
    // Dropped the Src column + MD5; State "available"→"avail"; progress bar moved
    // to its own line below each actively-downloading row. Title·Author flexes
    // (≥60%); the rest fields are capped at 40% (Mode A) else everything packs
    // into one comma string (Mode B). Focused row marquees, others ellipsize.
    let var_focused = *sub_focus == DetailSubFocus::Variations;
    let var_area = split[4];
    let row_w = var_area.width as usize;

    // Per-version rest-field strings (Fmt, Size, Match, State).
    let fields: Vec<(String, String, String, String)> = fb
        .book
        .versions
        .iter()
        .map(|v| {
            let fmt = v.fmt.clone();
            let size = if v.size > 0 {
                format!("{} MB", v.size)
            } else {
                "\u{2014}".into()
            };
            let mat = format!("{:.2}", v.score);
            let state = match v.state.as_str() {
                "available" => "avail".into(),
                // MD5 removed from this table (stays in the picker + `v` snapshot).
                "done" => "done \u{00b7} \u{2713}".into(),
                "downloading" => format!("downloading {}%", v.progress),
                other => other.to_string(),
            };
            (fmt, size, mat, state)
        })
        .collect();

    // Natural column widths (max of header label + data), used for the 40% cap
    // test and the Mode A fixed positions.
    let labels = ["Fmt", "Size", "Match", "State"];
    let mut rest_w = [
        crate::textfit::display_width(labels[0]),
        crate::textfit::display_width(labels[1]),
        crate::textfit::display_width(labels[2]),
        crate::textfit::display_width(labels[3]),
    ];
    for (f, s, m, st) in &fields {
        rest_w[0] = rest_w[0].max(crate::textfit::display_width(f));
        rest_w[1] = rest_w[1].max(crate::textfit::display_width(s));
        rest_w[2] = rest_w[2].max(crate::textfit::display_width(m));
        rest_w[3] = rest_w[3].max(crate::textfit::display_width(st));
    }
    let rest_widths = rest_w.to_vec();
    let layout = flex_row_layout(row_w, &rest_widths);

    // Advance the marquee for the FOCUSED row only (one row animates at a time).
    if var_focused {
        if let Some(v) = fb.book.versions.get(detail_selected) {
            match layout {
                FlexLayout::Fixed { title_w } => {
                    let combined = combine_title_author(&v.title, &v.author);
                    app.advance_var_marquee(crate::textfit::display_width(&combined), title_w);
                }
                FlexLayout::Packed { width } => {
                    let (f, s, m, st) = &fields[detail_selected];
                    let mut packed = combine_title_author(&v.title, &v.author);
                    for x in [f, s, m, st] {
                        if !x.is_empty() {
                            packed.push_str(", ");
                            packed.push_str(x);
                        }
                    }
                    app.advance_var_marquee(crate::textfit::display_width(&packed), width);
                }
            }
        }
    } else {
        app.advance_var_marquee(0, 1); // park while unfocused
    }
    let marquee_off = app.var_marquee_offset;

    // Header row (same flex positions as data rows).
    let header_rest: Vec<(String, Style)> = labels
        .iter()
        .map(|l| (l.to_string(), style_header()))
        .collect();
    render_flex_row(
        frame,
        Rect::new(var_area.x, var_area.y, var_area.width, 1),
        layout,
        " ",
        style_header(),
        "Title \u{00b7} Author",
        "",
        style_header(),
        style_header(),
        &header_rest,
        &rest_widths,
        false,
        0,
        style_header(),
    );

    // Data rows + per-row progress lines.
    app.last_rects.detail_var_area = var_area;
    app.last_rects.detail_var_rows.clear();
    let mut y = var_area.y + 1;
    let bottom = var_area.y + var_area.height;
    for (i, v) in fb.book.versions.iter().enumerate() {
        if y >= bottom {
            break;
        }
        let is_sel = i == detail_selected;
        let sel_active = is_sel && var_focused;
        let downloading = v.state == "downloading";
        let accent = if is_sel {
            "\u{258c}" // ▌ green accent bar
        } else if v.state == "done" {
            "\u{2713}"
        } else if downloading {
            theme::spinner(app.tick)
        } else {
            " "
        };
        let accent_style = if is_sel {
            if var_focused {
                style_sel_accent()
            } else {
                style_sel_accent_dim()
            }
        } else {
            theme::style_for_state(&v.state)
        };
        let base_style = if sel_active {
            Style::default().bg(C_SELECTED)
        } else {
            style_normal()
        };
        let title_style = if sel_active {
            style_selected()
        } else {
            style_title()
        };
        let author_style = if sel_active {
            style_selected()
        } else {
            style_dim()
        };
        let (f, s, m, st) = &fields[i];
        let dim_or_sel = if sel_active {
            style_selected()
        } else {
            style_dim()
        };
        let match_style = if sel_active {
            style_selected()
        } else {
            Style::default().fg(score_color(v.score.into()))
        };
        let state_style = if sel_active {
            style_selected()
        } else {
            theme::style_for_state(&v.state)
        };
        let rest = vec![
            (f.clone(), dim_or_sel),
            (s.clone(), dim_or_sel),
            (m.clone(), match_style),
            (st.clone(), state_style),
        ];
        let row_rect = Rect::new(var_area.x, y, var_area.width, 1);
        render_flex_row(
            frame,
            row_rect,
            layout,
            accent,
            accent_style,
            &v.title,
            &v.author,
            title_style,
            author_style,
            &rest,
            &rest_widths,
            sel_active, // marquee only the focused (selected + focused) row
            marquee_off,
            base_style,
        );
        app.last_rects.detail_var_rows.push((row_rect, i));
        y += 1;

        // #1: progress bar on its OWN line below — only while downloading.
        if downloading && y < bottom {
            let bar = theme::progress_bar(v.progress, 16);
            let eta = v
                .eta_secs
                .map(|s| format!(" \u{00b7} eta {}s", s))
                .unwrap_or_default();
            let txt = format!(
                "{}{} {}%{}",
                " ".repeat(FLEX_ACCENT_W),
                bar,
                v.progress,
                eta
            );
            frame.render_widget(
                Paragraph::new(Span::styled(txt, theme::style_for_state("downloading"))),
                Rect::new(var_area.x, y, var_area.width, 1),
            );
            y += 1;
        }
    }

    // Output path for done variations shown below (if any)
    // HISTORY header — accent when History is focused.
    let hist_focused = *sub_focus == DetailSubFocus::History;
    let hist_header_style = if *sub_focus == DetailSubFocus::History {
        style_dim()
    } else {
        style_muted()
    };
    frame.render_widget(
        Paragraph::new(Span::styled("\u{25be} HISTORY", hist_header_style)),
        split[6],
    );

    // History list — windowed so it can scroll; highlight the selected row.
    let n_hist = fb.book.history.len();
    let win_h = split[7].height as usize;
    // Chronological order: index 0 = oldest (top), newest at the bottom.
    let hist_sel = history_selected.min(n_hist.saturating_sub(1));
    // Compute scroll offset so hist_sel stays inside the visible window.
    let scroll_offset = if win_h == 0 || n_hist == 0 {
        0
    } else {
        let raw = hist_sel.saturating_sub(win_h.saturating_sub(1));
        raw.min(n_hist.saturating_sub(win_h))
    };
    let history_items: Vec<Line> = fb
        .book
        .history
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(win_h)
        .map(|(i, e)| {
            // The row is the selected history event regardless of focus; the
            // selection bar only goes "full" while History is focused, else it
            // stays a DIMMED accent (visible, never blank — survives Tab-away).
            let is_hist_row_sel = i == hist_sel;
            let sel_active = is_hist_row_sel && hist_focused;
            // Format time as HH:MM:SS from ms timestamp
            let secs = e.at_ms / 1000;
            let time_str = format!(
                "{:02}:{:02}:{:02}",
                (secs / 3600) % 24,
                (secs / 60) % 60,
                secs % 60
            );
            let kind_color = history_kind_color(&e.kind);
            let base_style = if sel_active {
                style_selected()
            } else {
                style_dim()
            };
            // Shared selected-line accent: green ▌ left bar (dimmed when the list
            // isn't focused); the leading event dot is removed (per feedback).
            let accent = if is_hist_row_sel {
                if hist_focused {
                    Span::styled("\u{258c} ", style_sel_accent())
                } else {
                    Span::styled("\u{258c} ", style_sel_accent_dim())
                }
            } else {
                Span::styled("  ", base_style)
            };
            Line::from(vec![
                accent,
                Span::styled(format!("{:<8}  ", time_str), base_style),
                Span::styled(
                    format!("{:<12}  ", e.kind),
                    if sel_active {
                        style_selected()
                    } else {
                        Style::default().fg(kind_color).add_modifier(Modifier::BOLD)
                    },
                ),
                Span::styled(crate::i18n::decode(&e.detail), base_style),
            ])
        })
        .collect();

    frame.render_widget(List::new(history_items), split[7]);

    // Register each visible history row rect for mouse hit-testing. Visible item
    // at real index `i` renders at `split[7].y + (i - scroll_offset)`.
    app.last_rects.detail_hist_area = split[7];
    app.last_rects.detail_hist_rows.clear();
    for i in scroll_offset..(scroll_offset + win_h).min(n_hist) {
        let y = split[7].y + (i - scroll_offset) as u16;
        app.last_rects
            .detail_hist_rows
            .push((Rect::new(split[7].x, y, split[7].width, 1), i));
    }

    // Dim rule above hint (⏎/↑↓/tab omitted per #64).
    render_rule(frame, split[8]);

    // Context-aware hint footer — wraps to as many lines as `hint_h` reserved.
    frame.render_widget(
        Paragraph::new(wrap_hint_lines(detail_hint, split[9].width)).style(style_hint()),
        split[9],
    );
}

// ---------------------------------------------------------------------------
// 4c  Settings modal
// ---------------------------------------------------------------------------

/// Settings modal width (#2): `min(80, floor(0.9 × total_width), total_width − 10)`.
///
/// Stays pinned at 80 on wide terminals, but shrinks on narrow ones so it never
/// exceeds 90 % of the screen and always leaves a ≥10-column margin.
pub(crate) fn settings_modal_width(total_width: u16) -> u16 {
    let ninety = (total_width as u32 * 9 / 10) as u16; // floor(0.9 × width)
    let margin = total_width.saturating_sub(10);
    80.min(ninety).min(margin)
}

/// Pick the display string for a plain (non-editing) settings value following
/// the cross-cutting rule (#2): focused row → marquee-scroll, unfocused → `…`
/// ellipsis. (The editing case is handled separately via
/// [`crate::textfit::scroll_to_cursor`].)
pub(crate) fn settings_value_display(
    value: &str,
    width: usize,
    focused: bool,
    marquee_off: usize,
) -> String {
    if focused {
        crate::textfit::marquee_window(value, width, marquee_off)
    } else {
        crate::textfit::ellipsize(value, width)
    }
}

fn render_settings_modal(frame: &mut Frame, app: &mut AppState) {
    let area = centered_rect(settings_modal_width(frame.area().width), 30, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_dim())
        .title(Span::styled(
            // Show the active list name ONLY when more than one list exists;
            // with a single list it is redundant noise.
            match &app.view {
                Some(v) if app.all_lists.len() > 1 => format!(" Settings \u{00b7} {} ", v.title),
                _ => " Settings ".to_string(),
            },
            style_dim(),
        ))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Internal gutter: 2-cell horizontal padding, 1-cell vertical padding.
    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    // Min(field list) + rule(1) + hint(wraps). Compute the wrapped footer height
    // up front (read-only) so the field list gives back exactly the rows it needs.
    let settings_hint = settings_hint_text(app);
    let settings_hint_h = hint_wrap_height(&settings_hint, padded.width);
    let split = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(settings_hint_h),
    ])
    .split(padded);

    // Value column width: the line is accent(2) + label(28) + value, spanning
    // the padded width. Long values flex/scroll inside what remains.
    let value_w = (padded.width as usize).saturating_sub(30);

    // ── Marquee bookkeeping (#2) — needs &mut, so do it before the long
    // immutable borrow of the draft below. Reset on row change; advance the
    // focused value's marquee, but park it (`0, 1`) while a field is editing.
    let focused_idx = app.settings_selected;
    let focused_editing = matches!(
        app.settings_draft.as_ref().map(|d| &d.editor),
        Some(SettingsEditor::Editing(_))
    );
    let focused_val = app
        .settings_draft
        .as_ref()
        .map(|d| d.field_value(focused_idx))
        .unwrap_or_default();
    app.reset_settings_marquee_if_changed(focused_idx);
    if focused_editing {
        app.advance_settings_marquee(0, 1);
    } else {
        app.advance_settings_marquee(crate::textfit::display_width(&focused_val), value_w);
    }
    let settings_marquee_off = app.settings_marquee_offset;

    // Pull values from the staged draft (always present when modal is open).
    let draft = match &app.settings_draft {
        Some(d) => d,
        None => return, // guard: shouldn't render without a draft
    };

    // Determine the display value for each field — show the edit buffer when
    // this field is actively being edited.
    let editing_idx: Option<usize> = match &draft.editor {
        SettingsEditor::Editing(_) => Some(app.settings_selected),
        _ => None,
    };
    let editing_buf: String = match &draft.editor {
        SettingsEditor::Editing(buf) => format!("{buf}\u{258f}"), // block cursor at end
        _ => String::new(),
    };

    let field_value = |idx: usize| -> String {
        if editing_idx == Some(idx) {
            editing_buf.clone()
        } else {
            draft.field_value(idx)
        }
    };

    // Structured settings sections.
    #[derive(Clone)]
    enum SettingsRow {
        SectionHeader(&'static str),
        Field {
            label: &'static str,
            value: String,
            index: usize,
        },
    }

    let mut fi = 0usize;
    let mut make_field = |label: &'static str, value: String| -> SettingsRow {
        let row = SettingsRow::Field {
            label,
            value,
            index: fi,
        };
        fi += 1;
        row
    };

    let rows: Vec<SettingsRow> = vec![
        SettingsRow::SectionHeader("FORMATS"),
        make_field("Preferred formats", field_value(0)),
        make_field("Language", field_value(1)),
        SettingsRow::SectionHeader("MATCHING"),
        make_field("Auto-download at \u{2265}", field_value(2)),
        make_field("Treat as not-found below", field_value(3)),
        make_field("Keep top copies", field_value(4)),
        SettingsRow::SectionHeader("FILES"),
        make_field("Download folder", field_value(5)),
        make_field("Naming template", field_value(6)),
        make_field("Sub-grouping", field_value(7)),
        SettingsRow::SectionHeader("DOWNLOADS & MIRRORS"),
        make_field("Max concurrent", field_value(8)),
        make_field("Per-host attempts", field_value(9)),
        make_field("Hedged", field_value(10)),
        // Display-only (no field index — not navigable):
        // We stop calling make_field here to keep them outside the navigation range.
        SettingsRow::SectionHeader(""),
    ];
    // Append the display-only mirror rows as part of DOWNLOADS & MIRRORS.
    // (We use a plain SettingsRow::Field with index = usize::MAX so they never
    // match `settings_selected`.)
    let display_only: Vec<SettingsRow> = vec![
        SettingsRow::Field {
            label: "Search mirrors",
            value: "libgen.li \u{25cf} libgen.is \u{25cf} libgen.rs \u{25cf}".into(),
            index: usize::MAX,
        },
        SettingsRow::Field {
            label: "Download sites",
            value: "libgen.li \u{25cf} libgen.pw \u{25cf} ipfs \u{25cf}".into(),
            index: usize::MAX,
        },
    ];
    let all_rows: Vec<SettingsRow> = rows
        .into_iter()
        .filter(|r| !matches!(r, SettingsRow::SectionHeader("")))
        .chain(display_only)
        .collect();

    let items: Vec<ListItem> = all_rows
        .iter()
        .enumerate()
        .map(|(row_idx, row)| match row {
            SettingsRow::SectionHeader(title) => {
                // Green section header, preceded by one blank line (except the
                // first section, which sits at the top of the modal).
                let header = Line::from(Span::styled(
                    *title,
                    Style::default().fg(C_DONE).add_modifier(Modifier::BOLD),
                ));
                if row_idx == 0 {
                    ListItem::new(header)
                } else {
                    ListItem::new(vec![Line::from(""), header])
                }
            }
            SettingsRow::Field {
                label,
                value,
                index,
            } => {
                let is_sel = *index == app.settings_selected;
                let is_editing = editing_idx.map(|e| e == *index).unwrap_or(false);

                // Shared selected-line style: green ▌ accent + faint green row bg.
                let accent = if is_sel {
                    Span::styled("\u{258c} ", style_sel_accent())
                } else {
                    Span::styled("  ", style_dim())
                };
                let label_style = if is_sel {
                    Style::default()
                        .fg(C_TEXT)
                        .bg(C_SELECTED)
                        .add_modifier(Modifier::BOLD)
                } else {
                    style_dim()
                };

                let mut spans: Vec<Span> =
                    vec![accent, Span::styled(format!("{:<28}", label), label_style)];

                if !is_editing && *index == 0 {
                    // Preferred formats: SELECTED green, UNSELECTED lower contrast.
                    let included: Vec<String> = draft
                        .format_pref
                        .iter()
                        .filter(|f| FORMAT_EDITOR_FORMATS.contains(&f.as_str()))
                        .map(|f| format!("[{}]", f))
                        .collect();
                    let excluded: Vec<&str> = FORMAT_EDITOR_FORMATS
                        .iter()
                        .filter(|&&f| !draft.format_pref.iter().any(|p| p.as_str() == f))
                        .copied()
                        .collect();
                    let inc_style = if is_sel {
                        style_selected()
                    } else {
                        Style::default().fg(C_DONE)
                    };
                    let exc_style = if is_sel {
                        style_selected()
                    } else {
                        style_dim()
                    };
                    if included.is_empty() {
                        spans.push(Span::styled("\u{2014}".to_string(), exc_style));
                    } else {
                        spans.push(Span::styled(included.join(" "), inc_style));
                        if !excluded.is_empty() {
                            spans.push(Span::styled(
                                format!("  + {} (off)", excluded.join(" \u{00b7} ")),
                                exc_style,
                            ));
                        }
                    }
                } else if is_editing {
                    // EDITING (cross-cutting rule): scroll the buffer to keep the
                    // block cursor `▏` visible, with `‹`/`›` indicators when text
                    // is hidden left/right. `value` already carries the trailing
                    // cursor glyph (appended in `editing_buf`); reserve one column
                    // on each side of the value region for the indicators.
                    let content_w = value_w.saturating_sub(2);
                    let cursor_col = crate::textfit::display_width(value).saturating_sub(1);
                    let cv = crate::textfit::scroll_to_cursor(value, cursor_col, content_w);
                    let left = if cv.clipped_left { "\u{2039}" } else { " " };
                    let right = if cv.clipped_right { "\u{203a}" } else { " " };
                    // Active inline editor: light text on the faint-green selected
                    // row bg. No bright-orange — unreadable on the bg.
                    let edit_style = Style::default()
                        .fg(C_BRIGHT)
                        .bg(C_SELECTED)
                        .add_modifier(Modifier::BOLD);
                    spans.push(Span::styled(left, style_dim()));
                    spans.push(Span::styled(cv.visible, edit_style));
                    spans.push(Span::styled(right, style_dim()));
                } else {
                    // Non-editing value. Threshold fields (idx 2/3) keep their
                    // trailing ▰▱ bar; every other long value follows the
                    // cross-cutting rule (marquee when focused, `…` otherwise).
                    let display_value: String = if *index == 2 || *index == 3 {
                        if let Ok(v) = value.parse::<f32>() {
                            let pct = (v * 100.0) as u32;
                            format!("{value}   {}", theme::progress_bar(pct, 10))
                        } else {
                            value.clone()
                        }
                    } else {
                        settings_value_display(value, value_w, is_sel, settings_marquee_off)
                    };
                    let value_style = if is_sel {
                        style_selected()
                    } else if *index == 6 {
                        // Naming template — a more contrasty beige.
                        Style::default().fg(C_WARM).add_modifier(Modifier::BOLD)
                    } else {
                        // Other values — higher-contrast neutral.
                        Style::default().fg(C_SOFTER)
                    };
                    spans.push(Span::styled(display_value, value_style));
                }

                let item = ListItem::new(Line::from(spans));
                if is_sel {
                    item.style(Style::default().bg(C_SELECTED))
                } else {
                    item
                }
            }
        })
        .collect();

    let list = List::new(items).block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, split[0]);

    // Dim rule + context-sensitive hint (⏎ omitted per #70). Wraps at narrow widths.
    render_rule(frame, split[1]);
    frame.render_widget(
        Paragraph::new(wrap_hint_lines(&settings_hint, split[2].width)).style(style_hint()),
        split[2],
    );

    // ── Overlay: Format Editor sub-modal ─────────────────────────────────────
    if let SettingsEditor::FormatEditor {
        rows: fmt_rows,
        cursor,
    } = &draft.editor
    {
        render_format_editor(frame, area, fmt_rows, *cursor);
    }

    // ── Overlay: Language picker popup ───────────────────────────────────────
    if let SettingsEditor::LangPicker { options, selected } = &draft.editor {
        render_lang_picker(frame, area, options, *selected);
    }
}

/// Render the Format Editor sub-modal centred inside `parent`.
fn render_format_editor(frame: &mut Frame, parent: Rect, rows: &[(bool, String)], cursor: usize) {
    let area = centered_rect(42, 14, parent);
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BRIGHT))
        .title(Span::styled(" format editor ", style_header()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    let padded = inner.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });

    let split = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(padded);

    let mut rank = 0usize;
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, (included, name))| {
            let is_cur = i == cursor;
            let rank_str = if *included {
                rank += 1;
                format!("{rank}")
            } else {
                " ".to_string()
            };
            let checkbox = if *included { "[x]" } else { "[ ]" };
            let line_style = if is_cur {
                style_selected()
            } else if *included {
                Style::default().fg(C_DONE) // selected formats — green
            } else {
                style_dim() // unselected — lower contrast
            };
            ListItem::new(Line::from(Span::styled(
                format!("{checkbox} {rank_str:<2} {name}"),
                line_style,
            )))
        })
        .collect();

    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::NONE)),
        split[0],
    );
    frame.render_widget(
        Paragraph::new(hint_line("spc toggle  J/K reorder  esc done")).style(style_hint()),
        split[1],
    );
}

/// Render the Language picker popup anchored near `parent`.
fn render_lang_picker(frame: &mut Frame, parent: Rect, options: &[String], selected: usize) {
    let h = (options.len() as u16 + 2).min(14);
    let area = centered_rect(28, h, parent);
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BRIGHT))
        .title(Span::styled(" language ", style_header()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            let is_sel = i == selected;
            let style = if is_sel {
                style_selected()
            } else {
                style_dim()
            };
            ListItem::new(Line::from(Span::styled(format!("  {opt}"), style)))
        })
        .collect();

    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::NONE)),
        inner,
    );
}

// ---------------------------------------------------------------------------
// 4d  Help screen
// ---------------------------------------------------------------------------

fn render_help_modal(frame: &mut Frame, parent: Rect) {
    // Widen so the longest command line fits two real columns with a gutter.
    let help_w = (parent.width.saturating_sub(4)).clamp(60, 104);
    let area = centered_rect(help_w, 30.min(parent.height), parent);
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_dim())
        .title(Span::styled(" Keys & Commands ", style_dim()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Internal gutter: 2-cell horizontal padding, 1-cell vertical padding.
    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    // Two-column layout matching the mock, with a 2-cell gutter between groups.
    // Left: NAVIGATE + FILTER · Right: ACT ON SELECTION + ACTIVITY + COMMAND LINE.
    let cols = Layout::horizontal([
        Constraint::Percentage(50),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .split(padded);

    let split_left = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(cols[0]);

    // Each column owns its key-column width (longest key + 2-cell gutter) so the
    // description can never collide with the key token (audit P1).
    let left_rows: &[HelpRow] = &[
        HelpRow::Head("NAVIGATE"),
        HelpRow::Key("\u{2191} \u{2193} / j k", "move \u{00b7} cross at edge"),
        HelpRow::Key("[ ]", "prev / next reading list"),
        HelpRow::Key("tab / S-tab", "cycle panes"),
        HelpRow::Key("\u{23ce}", "open \u{00b7} choose a copy"),
        HelpRow::Key("d", "book detail & history"),
        HelpRow::Key("esc \u{00b7} q", "back \u{00b7} quit"),
        HelpRow::Blank,
        HelpRow::Head("FILTER  (Header pane \u{2014} Tab to focus)"),
        HelpRow::Key("\u{2190} \u{2192}", "move filter chip"),
        HelpRow::Key("/", "cycle filter"),
        HelpRow::Key("1\u{2013}6", "all/needs/check/cannot/progress/done"),
    ];

    let right_rows: &[HelpRow] = &[
        HelpRow::Head("ACT ON SELECTION  (List pane)"),
        HelpRow::Key("\u{23ce}", "choose a copy (picker)"),
        HelpRow::Key("a", "fetch all preferred formats"),
        HelpRow::Key("r", "retry \u{00b7} re-download"),
        HelpRow::Key("p \u{00b7} c", "pause \u{00b7} cancel"),
        HelpRow::Key("o \u{00b7} R", "open file \u{00b7} reveal in Finder"),
        HelpRow::Blank,
        HelpRow::Head("ACTIVITY PANE  (Tab to focus)"),
        HelpRow::Key("\u{2191}\u{2193}", "select transfer leg"),
        HelpRow::Key(
            "p \u{00b7} c \u{00b7} r",
            "pause \u{00b7} cancel \u{00b7} resume",
        ),
        HelpRow::Blank,
        HelpRow::Head("COMMAND LINE  (press :)"),
        HelpRow::Key(":settings", "open settings"),
        HelpRow::Key(":import <file>", "add a list"),
        HelpRow::Key(":add <title|md5>", "add one book"),
        HelpRow::Key(":start-all", "resume downloads"),
        HelpRow::Key(":pause-all", "pause every download"),
    ];

    frame.render_widget(
        List::new(help_column(left_rows, split_left[0].width as usize)),
        split_left[0],
    );
    frame.render_widget(
        List::new(help_column(right_rows, cols[2].width as usize)),
        cols[2],
    );

    // Dim rule + hint.
    render_rule(frame, split_left[1]);
    frame.render_widget(
        Paragraph::new(hint_line("? or esc  to close")).style(style_hint()),
        split_left[2],
    );
}

/// One row in a Help column.
enum HelpRow {
    Head(&'static str),
    Key(&'static str, &'static str),
    Blank,
}

/// Lay out a Help column: section headers flush-left, key tokens green and
/// padded to the column's longest key + 2-cell gutter, descriptions clipped
/// (with `…`) to the remaining width so a column can never collide.
fn help_column(rows: &[HelpRow], col_w: usize) -> Vec<ListItem<'static>> {
    let key_w = rows
        .iter()
        .filter_map(|r| match r {
            HelpRow::Key(k, _) => Some(k.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        + 2;
    let desc_w = col_w.saturating_sub(key_w).max(1);
    rows.iter()
        .map(|r| match r {
            HelpRow::Head(t) => ListItem::new(Line::from(Span::styled(*t, style_header()))),
            HelpRow::Blank => ListItem::new(Line::from("")),
            HelpRow::Key(k, d) => {
                let desc = truncate_ellipsis(d, desc_w);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<key_w$}", k), Style::default().fg(C_DONE)),
                    Span::styled(desc, style_normal()),
                ]))
            }
        })
        .collect()
}

/// Clip `s` to `w` columns, appending `…` when truncated.
fn truncate_ellipsis(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else if w == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

// ---------------------------------------------------------------------------
// Confirm modal — `:delete` confirmation dialog
// ---------------------------------------------------------------------------

fn render_confirm_modal(frame: &mut Frame, title: &str, n_books: usize) {
    let area = centered_rect(60, 8, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_NEEDS_YOU))
        .title(Span::styled(" Delete list? ", style_title()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    // body (greedy) + rule (1) + hint (1)
    let split = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(padded);

    let body_text = format!("Delete \"{title}\" and its {n_books} book(s)?");
    let para = Paragraph::new(body_text)
        .style(style_normal())
        .alignment(Alignment::Left);
    frame.render_widget(para, split[0]);

    render_rule(frame, split[1]);
    frame.render_widget(
        Paragraph::new(Span::styled("y confirm  n / esc cancel", style_hint())),
        split[2],
    );
}

// ---------------------------------------------------------------------------
// #52 Reorganize preview modal — old → new path moves, [y] apply / [n] cancel
// ---------------------------------------------------------------------------

/// Render the reorganize preview: a scrollable list of `old/path → new/path`
/// pairs, a count, and the apply/cancel hint. `selected` highlights one row and
/// anchors the visible window so the list can scroll past the modal height.
fn render_reorganize_modal(frame: &mut Frame, diff: &[(String, String)], selected: usize) {
    let area = centered_rect(100, 28, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_NEEDS_YOU))
        .title(Span::styled(" Reorganize downloaded files ", style_title()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    // subheader (1) + list (Min) + rule (1) + hint (1)
    let split = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(padded);

    let subhead = format!(
        "{} file(s) would move into the current naming / folder layout",
        diff.len()
    );
    frame.render_widget(Paragraph::new(Span::styled(subhead, style_dim())), split[0]);

    // Scroll so the highlighted row stays visible. Each pair takes 2 lines
    // (old path, then the indented arrow → new path).
    let rows_visible = (split[1].height as usize / 2).max(1);
    let first = selected.saturating_sub(rows_visible.saturating_sub(1));
    let items: Vec<ListItem> = diff
        .iter()
        .enumerate()
        .skip(first)
        .take(rows_visible)
        .map(|(i, (old, new))| {
            let is_sel = i == selected;
            let marker = if is_sel { "\u{25b6} " } else { "  " };
            let old_style = if is_sel {
                style_selected()
            } else {
                style_dim()
            };
            let new_style = if is_sel {
                style_selected()
            } else {
                Style::default().fg(C_DONE)
            };
            ListItem::new(vec![
                Line::from(Span::styled(format!("{marker}{old}"), old_style)),
                Line::from(Span::styled(format!("    \u{2192} {new}"), new_style)),
            ])
        })
        .collect();
    frame.render_widget(List::new(items), split[1]);

    render_rule(frame, split[2]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            "\u{2191}\u{2193} scroll  y apply  n / esc cancel",
            style_hint(),
        )),
        split[3],
    );
}

// ---------------------------------------------------------------------------
// #49 Re-query inline input modal
// ---------------------------------------------------------------------------

fn render_requery_modal(frame: &mut Frame, app: &AppState, book_flat_index: usize, buf: &str) {
    let area = centered_rect(72, 7, frame.area());
    frame.render_widget(Clear, area);

    let title_label = app
        .flat
        .get(book_flat_index)
        .map(|fb| format!(" re-query: {} ", fb.book.title))
        .unwrap_or_else(|| " re-query ".into());

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_NEEDS_YOU))
        .title(Span::styled(title_label, style_title()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    let display_buf = format!("{buf}\u{258f}"); // block cursor at end
    let split = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(padded);

    frame.render_widget(
        Paragraph::new(Span::styled("Search title:", style_dim())),
        split[0],
    );
    frame.render_widget(
        Paragraph::new(Span::styled(display_buf, style_normal())),
        split[1],
    );
    frame.render_widget(
        Paragraph::new(Span::styled("  type  esc cancel", style_hint())),
        split[2],
    );
}

// ---------------------------------------------------------------------------
// #50 Edit-book inline input modal
// ---------------------------------------------------------------------------

fn render_edit_book_modal(
    frame: &mut Frame,
    _app: &AppState,
    _book_flat_index: usize,
    title_buf: &str,
    author_buf: &str,
    field: &EditBookField,
) {
    let area = centered_rect(72, 10, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_NEEDS_YOU))
        .title(Span::styled(" edit book ", style_title()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    let split = Layout::vertical([
        Constraint::Length(1), // "Title:" label
        Constraint::Length(1), // title field
        Constraint::Length(1), // blank
        Constraint::Length(1), // "Author:" label
        Constraint::Length(1), // author field
        Constraint::Length(1), // blank
        Constraint::Min(0),    // hint
    ])
    .split(padded);

    let title_display = if *field == EditBookField::Title {
        format!("{title_buf}\u{258f}")
    } else {
        title_buf.to_string()
    };
    let author_display = if *field == EditBookField::Author {
        format!("{author_buf}\u{258f}")
    } else {
        author_buf.to_string()
    };

    let title_label_style = if *field == EditBookField::Title {
        style_title()
    } else {
        style_dim()
    };
    let author_label_style = if *field == EditBookField::Author {
        style_title()
    } else {
        style_dim()
    };

    frame.render_widget(
        Paragraph::new(Span::styled("Title:", title_label_style)),
        split[0],
    );
    frame.render_widget(
        Paragraph::new(Span::styled(title_display, style_normal())),
        split[1],
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            "Author(s) — comma-separated:",
            author_label_style,
        )),
        split[3],
    );
    frame.render_widget(
        Paragraph::new(Span::styled(author_display, style_normal())),
        split[4],
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            "tab switch field  s save  esc cancel",
            style_hint(),
        )),
        split[6],
    );
}

// ---------------------------------------------------------------------------
// #50 Confirm book-remove modal
// ---------------------------------------------------------------------------

fn render_confirm_book_remove_modal(frame: &mut Frame, app: &AppState, book_flat_index: usize) {
    let area = centered_rect(60, 8, frame.area());
    frame.render_widget(Clear, area);

    let book_title = app
        .flat
        .get(book_flat_index)
        .map(|fb| fb.book.title.as_str())
        .unwrap_or("this book");

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_NEEDS_YOU))
        .title(Span::styled(" Remove book? ", style_title()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    // body (greedy) + rule (1) + hint (1)
    let split = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(padded);

    let body_text = format!("Remove \"{book_title}\" from the list?");
    let para = Paragraph::new(body_text)
        .style(style_normal())
        .alignment(Alignment::Left);
    frame.render_widget(para, split[0]);

    render_rule(frame, split[1]);
    frame.render_widget(
        Paragraph::new(Span::styled("y confirm  n / esc cancel", style_hint())),
        split[2],
    );
}

// ---------------------------------------------------------------------------
// Wildmenu — Tab-completion strip shown above the command line
// ---------------------------------------------------------------------------

/// Split a `:import` path completion into `(parent, name, is_dir)`.
///
/// The candidate is the full typed path (e.g. `~/docs/deep/file.md` or a
/// directory `~/docs/sub/`). Directory candidates end in `/`. `parent` is the
/// portion before the final component (without its trailing slash), `name` the
/// final component, and `is_dir` whether the candidate is a directory.
fn split_import_candidate(cand: &str) -> (&str, &str, bool) {
    let is_dir = cand.ends_with('/');
    let core = cand.strip_suffix('/').unwrap_or(cand);
    match core.rfind('/') {
        Some(i) => (&core[..i], &core[i + 1..], is_dir),
        None => ("", core, is_dir),
    }
}

/// Last `max` display columns of `s` (whole glyphs only, never split).
fn last_n_cols(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut start = chars.len();
    let mut cols = 0;
    for i in (0..chars.len()).rev() {
        let w = crate::textfit::display_width(&chars[i].to_string());
        if cols + w > max {
            break;
        }
        cols += w;
        start = i;
    }
    chars[start..].iter().collect()
}

/// DISPLAYED label for a `:import` path completion (#3).
///
/// - Full `<parent>/<name>` when it fits in ~25 display columns.
/// - Otherwise `…<parent-last-10>/<name>`, the parent capped to its last 10
///   display columns with a leading `…`; a parent ≤10 cols is shown whole.
/// - Directory candidates keep a trailing `/`.
///
/// Only the *display* changes — the value inserted into the buffer stays the
/// full path.
pub(crate) fn format_import_candidate_label(parent: &str, name: &str, is_dir: bool) -> String {
    let slash = if is_dir { "/" } else { "" };
    let full = if parent.is_empty() {
        format!("{name}{slash}")
    } else {
        format!("{parent}/{name}{slash}")
    };
    if crate::textfit::display_width(&full) <= 25 {
        return full;
    }
    if parent.is_empty() {
        // Nothing to shorten — only a (long) name.
        return full;
    }
    let parent_disp = if crate::textfit::display_width(parent) <= 10 {
        parent.to_string()
    } else {
        format!("\u{2026}{}", last_n_cols(parent, 10))
    };
    format!("{parent_disp}/{name}{slash}")
}

/// Render the Tab-completion wildmenu into `area`.
///
/// The currently highlighted candidate is drawn reversed (dark bg, accent fg);
/// others are dim.  The caller is responsible for allocating `area` — either
/// a dedicated layout row (main render, #71) or a manually computed rect
/// (empty screen).
fn render_wildmenu(frame: &mut Frame, app: &AppState, area: Rect) {
    // `:import <path>` candidates carry a full (possibly long) path; the wildmenu
    // shows a shortened label (#3) while the buffer keeps the full value.
    let is_import = app
        .command_buf
        .as_deref()
        .map(|b| b.trim_start().starts_with("import "))
        .unwrap_or(false);
    let mut spans: Vec<Span> = Vec::new();
    for (i, cand) in app.completion_candidates.iter().enumerate() {
        let is_active = i == app.completion_index;
        let style = if is_active {
            Style::default()
                .fg(C_BG)
                .bg(C_DONE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_TEXT).bg(C_PANEL)
        };
        let label = if is_import {
            let (parent, name, is_dir) = split_import_candidate(cand);
            format_import_candidate_label(parent, name, is_dir)
        } else {
            cand.clone()
        };
        spans.push(Span::styled(format!(" {} ", label), style));
        if i + 1 < app.completion_candidates.len() {
            // Thin separator between candidates.
            spans.push(Span::styled("  ", Style::default().fg(C_FAINT).bg(C_PANEL)));
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(C_PANEL)),
        area,
    );
}

/// Build a "Title  Author" header line, windowed by a ping-pong marquee
/// `offset` (DISPLAY COLUMNS scrolled from the left) when it overflows `avail`
/// columns.  The title and author keep their own styles across the scroll.
///
/// #10/#14: windowing is display-width aware via `textfit::marquee_char_range`
/// — the single marquee windowing core — so CJK/emoji are never split mid-glyph.
fn marquee_title_author(
    title: &str,
    author: &str,
    title_style: Style,
    author_style: Style,
    offset: usize,
    avail: usize,
) -> Line<'static> {
    // Flatten to (char, style) so we can window across the title/author boundary.
    let mut chars: Vec<(char, Style)> = title.chars().map(|c| (c, title_style)).collect();
    if !author.is_empty() {
        chars.push((' ', author_style));
        chars.push((' ', author_style));
        chars.extend(author.chars().map(|c| (c, author_style)));
    }
    // Window by display columns. The combined string's char indices align 1:1
    // with `chars`, so the range maps straight back onto the styled vec.
    let combined: String = chars.iter().map(|(c, _)| *c).collect();
    let range = crate::textfit::marquee_char_range(&combined, avail, offset);
    let windowed: &[(char, Style)] = &chars[range];
    // Coalesce consecutive same-style chars into spans.
    let mut spans: Vec<Span> = Vec::new();
    let mut cur = String::new();
    let mut cur_style: Option<Style> = None;
    for (c, st) in windowed {
        if cur_style == Some(*st) {
            cur.push(*c);
        } else {
            if let Some(s) = cur_style {
                spans.push(Span::styled(std::mem::take(&mut cur), s));
            }
            cur.push(*c);
            cur_style = Some(*st);
        }
    }
    if let Some(s) = cur_style {
        spans.push(Span::styled(cur, s));
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Flexing "title·author + rest fields" row (Detail variations #1 · Picker #11)
// ---------------------------------------------------------------------------

/// Accent column width (selection glyph + trailing space).
const FLEX_ACCENT_W: usize = 2;
/// Breathing space between the title·author region and the rest-field block.
const FLEX_SEP: usize = 2;
/// Gap between adjacent rest fields.
const FLEX_GAP: usize = 1;
/// Never shrink the title region below this in Mode A.
const FLEX_MIN_TITLE: usize = 8;

/// Layout decision for one flex row.
///
/// `Mode A` (`Fixed`) — the rest fields fit within the 40% cap: render them at
/// their natural fixed widths/positions and give ALL the remaining width
/// (≥60%) to title·author.  `Mode B` (`Packed`) — the rest fields would need
/// >40%: concatenate title, author AND the rest into one comma string spanning
/// the whole content width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlexLayout {
    Fixed { title_w: usize },
    Packed { width: usize },
}

/// Decide Mode A vs Mode B for a row `row_w` columns wide whose rest fields have
/// the given natural widths.
///
/// **The 40% cap is computed here:** `cap = row_w * 40 / 100`.  The rest block
/// width is `Σ field widths + gaps`.  Mode A only when that block fits the cap
/// *and* leaves at least [`FLEX_MIN_TITLE`] for the title; otherwise Mode B.
pub(crate) fn flex_row_layout(row_w: usize, rest_field_widths: &[usize]) -> FlexLayout {
    let n = rest_field_widths.len();
    let gaps = n.saturating_sub(1) * FLEX_GAP;
    let rest_w: usize = rest_field_widths.iter().sum::<usize>() + gaps;
    let cap = row_w * 40 / 100; // 40% CAP on the whole row width
    let content_w = row_w.saturating_sub(FLEX_ACCENT_W);
    if rest_w <= cap && content_w >= rest_w + FLEX_SEP + FLEX_MIN_TITLE {
        FlexLayout::Fixed {
            title_w: content_w - FLEX_SEP - rest_w,
        }
    } else {
        FlexLayout::Packed { width: content_w }
    }
}

// ---------------------------------------------------------------------------
// Book list row — three never-starved regions (#4)
// ---------------------------------------------------------------------------
//
// Unlike the Detail variations row (#1, which merges title+author), the library
// list reserves a SEPARATE author region so you can scan authors. After the
// fixed seq/accent column the content splits into:
//   [ Title 60% ][SEP][ Author 10% + slack ][SEP][ rest ≤30% ]
// The rest fields (Fmt, Size, State) take their NATURAL width capped at 30%; the
// 30% the rest leaves unused is the *slack* that flows to the author (author =
// 10% + slack). The title stays at 60%. When the rest fields can't fit the 30%
// cap (narrow terminals) the whole line packs into one comma string (Packed).

/// Gutter between the title/author regions and before the rest block.
const BOOK_SEP: usize = 2;
/// Gap between adjacent rest fields (Fmt · Size · State).
const BOOK_GAP: usize = 1;
/// Never enter Fixed mode unless the title region keeps at least this width.
const BOOK_MIN_TITLE: usize = 8;

/// Layout of one library book row (#4), computed from the content width (the
/// row width minus the fixed seq/accent column) and the rest fields' natural
/// widths.
///
/// `Fixed` (situation a) — the rest fields fit the 30% cap: title 60%, author
/// 10% + slack, rest at natural fixed widths. `Packed` (situation b) — the rest
/// fields would overflow 30% (or no title room is left): everything joins one
/// comma string spanning the whole content width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BookRowLayout {
    Fixed {
        title_w: usize,
        author_w: usize,
        rest_widths: Vec<usize>,
    },
    Packed {
        width: usize,
    },
}

/// Decide the three-region split for a book row `content_w` columns wide (i.e.
/// the row width already minus the seq/accent column) whose rest fields have the
/// given natural widths.
///
/// The 30% cap and the 10%/60% reservations are computed here; the unused part
/// of the 30% (`slack = rest_cap − rest_natural`) is added to the author region,
/// so the **author always keeps a visible slice** while the title holds 60%.
pub(crate) fn book_row_layout(content_w: usize, rest_field_widths: &[usize]) -> BookRowLayout {
    let n = rest_field_widths.len();
    let gaps = n.saturating_sub(1) * BOOK_GAP;
    let rest_natural = rest_field_widths.iter().sum::<usize>() + gaps;
    let rest_cap = content_w * 30 / 100;
    let author_base = content_w * 10 / 100;
    if rest_natural <= rest_cap {
        // Slack (the 30% the rest fields didn't use) flows to the author.
        let slack = rest_cap - rest_natural;
        let author_w = author_base + slack;
        // Title gets whatever's left after author, the rest block and the two
        // inter-region gutters — i.e. ~60% (the two separators are overhead).
        let used = author_w + rest_natural + 2 * BOOK_SEP;
        if content_w >= used + BOOK_MIN_TITLE {
            return BookRowLayout::Fixed {
                title_w: content_w - used,
                author_w,
                rest_widths: rest_field_widths.to_vec(),
            };
        }
    }
    BookRowLayout::Packed { width: content_w }
}

/// Per-list column widths for the top reading-list strip (#15).
///
/// `strip_w` is the whole strip width (N); `natural_widths` each list's natural
/// label width. Returns one width per list.
/// - All lists fit at natural width within the strip → natural widths (no cap).
/// - Otherwise per-list base = `max(30, strip_w / min(#lists, 4))`:
///   - **≤4 lists** → every list gets that base (EVEN equal columns; floor 30 so
///     the strip overflows/scrolls when `strip_w/#lists < 30`).
///   - **>4 lists** → each list capped at the base (`min(natural, base)`), packed
///     tight; the strip overflows and scrolls horizontally.
pub(crate) fn list_strip_layout(strip_w: usize, natural_widths: &[usize]) -> Vec<usize> {
    let n = natural_widths.len();
    if n == 0 {
        return Vec::new();
    }
    let total_natural: usize = natural_widths.iter().sum();
    if total_natural <= strip_w {
        // Everything fits at natural width — no capping.
        return natural_widths.to_vec();
    }
    let base = (strip_w / n.min(4)).max(30);
    if n <= 4 {
        // Even split: each list owns an equal column of width `base`.
        vec![base; n]
    } else {
        // Cap each at `base` (≤ a quarter of the strip), packed tight.
        natural_widths.iter().map(|&w| w.min(base)).collect()
    }
}

/// "Title · Author" combined into one string (author omitted when empty).
fn combine_title_author(title: &str, author: &str) -> String {
    if author.is_empty() {
        title.to_string()
    } else {
        format!("{} \u{00b7} {}", title, author)
    }
}

/// Window a single-style string for a flex region: marquee when `focused`
/// (offset-scrolled, no ellipsis), `…`-ellipsize when not.  This is the
/// focused-vs-unfocused selection used by Mode B and unfocused Mode A.
pub(crate) fn flex_text(s: &str, window: usize, offset: usize, focused: bool) -> String {
    if focused {
        crate::textfit::marquee_window(s, window, offset)
    } else {
        crate::textfit::ellipsize(s, window)
    }
}

/// Right-pad `s` with spaces to exactly `w` display columns (ellipsizing first
/// if it overflows).
fn pad_cell(s: &str, w: usize) -> String {
    let fitted = crate::textfit::ellipsize(s, w);
    let used = crate::textfit::display_width(&fitted);
    if used < w {
        format!("{}{}", fitted, " ".repeat(w - used))
    } else {
        fitted
    }
}

/// Total display width of an already-built styled line.
fn line_disp_width(line: &Line) -> usize {
    line.spans
        .iter()
        .map(|s| crate::textfit::display_width(&s.content))
        .sum()
}

/// Build the border title for the picker modal, ellipsizing the book title so
/// the " — choose a copy " suffix is never clipped (#11: border = ellipsize, no
/// marquee).
pub(crate) fn picker_border_title(title: &str, area_w: u16) -> String {
    const SUFFIX: &str = " \u{2014} choose a copy ";
    // Leading space + suffix + the two rounded corners worth of slack.
    let reserved = crate::textfit::display_width(SUFFIX) + 3;
    let max_title = (area_w as usize).saturating_sub(reserved).max(4);
    format!(" {}{}", crate::textfit::ellipsize(title, max_title), SUFFIX)
}

/// Render one flexing "title·author + rest" row into a 1-high `rect`.
///
/// Mode A: accent · title·author (marquee if `focused`, else `…`) · fixed rest
/// fields.  Mode B: accent · one comma-joined packed line (marquee/`…`).  The
/// caller must have advanced the row marquee (`advance_var_marquee`) for the
/// focused row before calling so `marquee_offset` is current.
#[allow(clippy::too_many_arguments)]
fn render_flex_row(
    frame: &mut Frame,
    rect: Rect,
    layout: FlexLayout,
    accent: &str,
    accent_style: Style,
    title: &str,
    author: &str,
    title_style: Style,
    author_style: Style,
    rest: &[(String, Style)],
    rest_widths: &[usize],
    focused: bool,
    marquee_offset: usize,
    base_style: Style,
) {
    let mut spans: Vec<Span> = Vec::new();
    // Accent column, padded to a fixed width.
    spans.push(Span::styled(pad_cell(accent, FLEX_ACCENT_W), accent_style));

    match layout {
        FlexLayout::Fixed { title_w } => {
            if focused {
                // Two-tone marquee window across the title/author boundary.
                let line = marquee_title_author(
                    title,
                    author,
                    title_style,
                    author_style,
                    marquee_offset,
                    title_w,
                );
                let used = line_disp_width(&line);
                spans.extend(line.spans);
                if used < title_w {
                    spans.push(Span::styled(" ".repeat(title_w - used), base_style));
                }
            } else {
                let combined = combine_title_author(title, author);
                spans.push(Span::styled(
                    pad_cell(&crate::textfit::ellipsize(&combined, title_w), title_w),
                    title_style,
                ));
            }
            // Separator, then the fixed rest fields.
            spans.push(Span::styled(" ".repeat(FLEX_SEP), base_style));
            for (i, ((s, st), &w)) in rest.iter().zip(rest_widths).enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" ".repeat(FLEX_GAP), base_style));
                }
                spans.push(Span::styled(pad_cell(s, w), *st));
            }
        }
        FlexLayout::Packed { width } => {
            let mut packed = combine_title_author(title, author);
            for (s, _) in rest {
                if !s.is_empty() {
                    packed.push_str(", ");
                    packed.push_str(s);
                }
            }
            spans.push(Span::styled(
                flex_text(&packed, width, marquee_offset, focused),
                title_style,
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(base_style), rect);
}

/// Build the content `Line` for one library book row (#4 three regions), to be
/// placed in the single flex cell after the seq/accent column.
///
/// `Fixed`: `[title][gutter][author][gutter][rest fixed]` — focused marquees
/// title+author together (rest stays put), unfocused ellipsizes each region
/// independently so the author is never dropped. `Packed`: one comma line over
/// the whole width — focused marquees, unfocused ellipsizes.
#[allow(clippy::too_many_arguments)]
fn book_row_line(
    layout: &BookRowLayout,
    title: &str,
    author: &str,
    title_style: Style,
    author_style: Style,
    rest: &[(String, Style)],
    focused: bool,
    marquee_offset: usize,
    base_style: Style,
) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    match layout {
        BookRowLayout::Fixed {
            title_w,
            author_w,
            rest_widths,
        } => {
            let ta_w = *title_w + BOOK_SEP + *author_w;
            if focused {
                // One marquee window across the title→author boundary.
                let line = marquee_title_author(
                    title,
                    author,
                    title_style,
                    author_style,
                    marquee_offset,
                    ta_w,
                );
                let used = line_disp_width(&line);
                spans.extend(line.spans);
                if used < ta_w {
                    spans.push(Span::styled(" ".repeat(ta_w - used), base_style));
                }
            } else {
                // Each region ellipsizes on its own — author keeps its slice.
                spans.push(Span::styled(
                    pad_cell(&crate::textfit::ellipsize(title, *title_w), *title_w),
                    title_style,
                ));
                spans.push(Span::styled(" ".repeat(BOOK_SEP), base_style));
                spans.push(Span::styled(
                    pad_cell(&crate::textfit::ellipsize(author, *author_w), *author_w),
                    author_style,
                ));
            }
            // Gutter, then the fixed rest fields.
            spans.push(Span::styled(" ".repeat(BOOK_SEP), base_style));
            for (i, ((s, st), &w)) in rest.iter().zip(rest_widths).enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" ".repeat(BOOK_GAP), base_style));
                }
                spans.push(Span::styled(pad_cell(s, w), *st));
            }
        }
        BookRowLayout::Packed { width } => {
            let mut packed = combine_title_author(title, author);
            for (s, _) in rest {
                if !s.is_empty() {
                    packed.push_str(", ");
                    packed.push_str(s);
                }
            }
            spans.push(Span::styled(
                flex_text(&packed, *width, marquee_offset, focused),
                title_style,
            ));
        }
    }
    Line::from(spans)
}

/// True when `tok` should render as a hotkey KEY (green) in a hint row.
///
/// A token is a KEY when it is a recognised key word, a `:`-prefixed command, a
/// single visible character, a pure-glyph cluster (`↑↓`, `←→`, …) or a short
/// slash-combo (`J/K`).  The `·` separator and ordinary description words are not.
fn is_key_token(tok: &str) -> bool {
    const KEYWORDS: &[&str] = &["esc", "tab", "S-tab", "space", "spc", "ctrl", "alt", "del"];
    if tok == "\u{00b7}" || tok == "\u{2014}" {
        return false; // · / — separators stay dim
    }
    if KEYWORDS.contains(&tok) || tok.starts_with(':') {
        return true;
    }
    let cc = tok.chars().count();
    if cc == 1 {
        return true; // single char: letter / digit / glyph / [ ] / ? / /
    }
    let has_alnum = tok.chars().any(|c| c.is_ascii_alphanumeric());
    if !has_alnum {
        return true; // pure glyph cluster (↑↓, ←→)
    }
    tok.contains('/') && cc <= 4 // J/K, p/c
}

/// Split a hint string into `(unit, separator_after)` pairs, breaking only at
/// **hint boundaries** — a ` · ` middot separator or a double-space group gap.
/// Single spaces inside a unit (the `key desc` gap) are preserved. The last
/// unit carries an empty separator.
fn split_hint_units(s: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = s.chars().collect();
    let mut units: Vec<(String, String)> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        // " · " — space, middot, space.
        if chars[i] == ' '
            && i + 2 < chars.len()
            && chars[i + 1] == '\u{00b7}'
            && chars[i + 2] == ' '
        {
            units.push((std::mem::take(&mut cur), " \u{00b7} ".to_string()));
            i += 3;
            continue;
        }
        // "  " — double-space group gap.
        if chars[i] == ' ' && i + 1 < chars.len() && chars[i + 1] == ' ' {
            units.push((std::mem::take(&mut cur), "  ".to_string()));
            i += 2;
            continue;
        }
        cur.push(chars[i]);
        i += 1;
    }
    units.push((cur, String::new()));
    units
}

/// Wrap a hint string into as many lines as needed so it fits `width` display
/// columns, breaking **only** at hint boundaries (` · ` / group gaps) so no hint
/// token is ever dropped or ellipsized. A single hint wider than `width` keeps
/// its own line (it overflows rather than being split). Always ≥1 line.
fn wrap_hint(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let units = split_hint_units(s);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for (idx, (unit, _)) in units.iter().enumerate() {
        let uw = crate::textfit::display_width(unit);
        let sep = if idx == 0 {
            ""
        } else {
            units[idx - 1].1.as_str()
        };
        let sep_w = crate::textfit::display_width(sep);
        if cur.is_empty() {
            cur.push_str(unit);
            cur_w = uw;
        } else if cur_w + sep_w + uw <= width {
            cur.push_str(sep);
            cur.push_str(unit);
            cur_w += sep_w + uw;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(unit);
            cur_w = uw;
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Number of lines [`wrap_hint`] yields for `s` at `width` columns (≥1).
fn hint_wrap_height(s: &str, width: u16) -> u16 {
    wrap_hint(s, width as usize).len().max(1) as u16
}

/// Wrap a hint string and style each line with [`hint_line`], ready to drop into
/// a (grown) hint/footer area as a `Paragraph`.
fn wrap_hint_lines(s: &str, width: u16) -> Vec<Line<'static>> {
    wrap_hint(s, width as usize)
        .into_iter()
        .map(|l| hint_line(&l))
        .collect()
}

/// Build a hint/footer line with hotkey KEY tokens in green and descriptions
/// dim — the shared treatment for the bottom bar and every modal footer.
fn hint_line(s: &str) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    for (i, tok) in s.split(' ').enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ", style_hint()));
        }
        if tok.is_empty() {
            continue;
        }
        let style = if is_key_token(tok) {
            Style::default().fg(C_DONE).bg(C_PANEL)
        } else {
            style_hint()
        };
        spans.push(Span::styled(tok.to_string(), style));
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Snapshot popup (variation / history-event / leg)
// ---------------------------------------------------------------------------

/// Render a generic label/value snapshot popup.
///
/// Layout (inside the rounded border):
/// ```
/// 2-cell margin ─┐
///   Label        Value
///   ...
///   ──────────────  ← dim rule
///   esc  close      ← hint
/// └─ 2-cell margin
/// ```
fn render_snapshot_modal(frame: &mut Frame, title: &str, lines: &[(String, String)]) {
    let n = lines.len() as u16;
    // border(2) + top-margin(1) + content rows + rule(1) + hint(1) + bottom-margin(1)
    let height = (n + 6).max(8).min(28);
    let width = 72u16;
    let area = centered_rect(width, height, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_dim())
        .title(Span::styled(title, style_dim()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    // Layout: content rows (greedy) + dim rule (1) + hint (1).
    let split = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(padded);

    // Label / value rows.
    let content_lines: Vec<Line> = lines
        .iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(format!("{:<18}", label), style_dim()),
                Span::styled(value.clone(), style_normal()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(content_lines), split[0]);

    // Dim horizontal rule above hint.
    render_rule(frame, split[1]);

    // Hint row.
    frame.render_widget(
        Paragraph::new(Span::styled("esc  close", style_hint())),
        split[2],
    );
}

#[cfg(test)]
mod hint_wrap_tests {
    use super::*;
    use crate::textfit::display_width;

    /// Collect the non-separator hint tokens (e.g. "q quit", "esc back") from a
    /// hint string, so a wrap can be checked for losing none of them.
    fn hint_tokens(s: &str) -> Vec<String> {
        split_hint_units(s)
            .into_iter()
            .map(|(u, _)| u)
            .filter(|u| !u.trim().is_empty())
            .collect()
    }

    #[test]
    fn wrap_wide_keeps_everything_on_one_line() {
        // A hint that fits stays a single line.
        let s = "d detail  : command \u{00b7} ? help \u{00b7} q quit";
        let lines = wrap_hint(s, 200);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], s);
    }

    #[test]
    fn wrap_narrow_grows_to_multiple_lines() {
        let s = "\u{2190}\u{2192} filter  r re-search \u{00b7} p pause \u{00b7} s start \u{00b7} D delete  [ ] list  : command \u{00b7} ? help \u{00b7} q quit";
        // Width 80 must not fit this whole header hint on one line.
        let lines = wrap_hint(s, 80);
        assert!(lines.len() >= 2, "expected wrap at 80, got {lines:?}");
        // hint_wrap_height agrees with wrap_hint length.
        assert_eq!(hint_wrap_height(s, 80) as usize, lines.len());
        // Every wrapped line fits within the width.
        for l in &lines {
            assert!(display_width(l) <= 80, "line over width: {l:?}");
        }
    }

    #[test]
    fn wrap_loses_no_hint_token() {
        // The longest footer (detail "done" row) at a narrow modal width.
        let s = "o open \u{00b7} R reveal \u{00b7} r re-download  e edit \u{00b7} x remove \u{00b7} m mark unavailable \u{00b7} S series \u{00b7} esc back";
        let before = hint_tokens(s);
        let lines = wrap_hint(s, 40);
        assert!(lines.len() >= 2, "expected wrap at 40");
        // Re-tokenize every wrapped line and confirm the multiset is unchanged.
        let mut after: Vec<String> = lines.iter().flat_map(|l| hint_tokens(l)).collect();
        let mut before_sorted = before.clone();
        before_sorted.sort();
        after.sort();
        assert_eq!(before_sorted, after, "a hint token was dropped or altered");
        // Critical tail tokens are present.
        assert!(after.iter().any(|t| t == "esc back"));
        assert!(after.iter().any(|t| t == "o open"));
    }

    #[test]
    fn wrap_single_oversized_hint_stays_intact() {
        // A single hint wider than the width keeps its own line (never split).
        let s = "enter a very long single hint token here";
        let lines = wrap_hint(s, 10);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], s);
    }

    #[test]
    fn activity_leg_pins_status_and_flexes_title() {
        let row_w = 50;
        let title = "A Very Long Book Title That Will Not Fit In The Row At All";
        let status = "EPUB  42%  \u{2593}\u{2593}\u{2593}\u{2591}\u{2591}\u{2591}  12s";
        let line = activity_leg_line(
            "  \u{25b8} . ".to_string(),
            Style::default(),
            title,
            Style::default(),
            status.to_string(),
            Style::default(),
            row_w,
            false, // not focused → ellipsize
            0,
        );
        // The whole row never exceeds row_w columns.
        let total: usize = line
            .spans
            .iter()
            .map(|sp| display_width(sp.content.as_ref()))
            .sum();
        assert!(total <= row_w, "row over width: {total} > {row_w}");
        // The STATUS is the last span, pinned and intact (fully visible).
        let last = line.spans.last().unwrap();
        assert_eq!(last.content.as_ref(), status);
        // The TITLE (2nd span) was ellipsized (does not contain the full title).
        let title_span = line.spans[1].content.as_ref();
        assert!(
            title_span.ends_with('\u{2026}'),
            "title not ellipsized: {title_span:?}"
        );
        assert!(display_width(title_span) < display_width(title));
    }

    #[test]
    fn activity_leg_focused_marquees_title() {
        let row_w = 40;
        let title = "0123456789abcdefghijklmnopqrstuvwxyz";
        let status = "50%  \u{2593}\u{2593}\u{2593}";
        // offset 0 vs a later offset must show different title windows (scrolling).
        let l0 = activity_leg_line(
            "  ".to_string(),
            Style::default(),
            title,
            Style::default(),
            status.to_string(),
            Style::default(),
            row_w,
            true,
            0,
        );
        let l3 = activity_leg_line(
            "  ".to_string(),
            Style::default(),
            title,
            Style::default(),
            status.to_string(),
            Style::default(),
            row_w,
            true,
            3,
        );
        // Status still pinned + intact in both.
        assert_eq!(l0.spans.last().unwrap().content.as_ref(), status);
        assert_eq!(l3.spans.last().unwrap().content.as_ref(), status);
        // The marquee window slid → the visible title differs.
        assert_ne!(
            l0.spans[1].content.as_ref(),
            l3.spans[1].content.as_ref(),
            "marquee offset had no effect"
        );
    }
}
