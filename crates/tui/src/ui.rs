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
    settings_field_kind, AppState, DetailSubFocus, EditBookField, Focus, Modal, SettingsEditor,
    SettingsFieldKind, StatusFilter, FORMAT_EDITOR_FORMATS,
};
use crate::theme::{
    self, history_kind_color, score_color, style_dim, style_header, style_hint, style_muted,
    style_normal, style_selected, style_title, C_BACKDROP, C_BG, C_BRIGHT, C_DONE, C_FAINT,
    C_MUTED, C_NEEDS_YOU, C_PANEL, C_SELECTED, C_TEXT, C_WARM,
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

    // Build layout constraints dynamically.  When `:` is active the
    // command-line gets its own row plus an extra rule below it.
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
        constraints.push(Constraint::Length(1)); // 7  :command-line row
        constraints.push(Constraint::Length(1)); // 8  rule — cmd-line → hint
    }
    constraints.push(Constraint::Length(1)); // 7/9  hint bar

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
        render_command_line(frame, app, chunks[7]);
        render_rule(frame, chunks[8]);
        render_hint_bar(frame, app, chunks[9]);
    } else {
        render_hint_bar(frame, app, chunks[7]);
    }

    // Wildmenu: one line directly above the command-line (cmd mode) or
    // hint bar (normal mode).  In both cases that is chunks[7].
    if !app.completion_candidates.is_empty() && chunks[7].y > 0 {
        render_wildmenu(frame, app, chunks[7]);
    }

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

    // 6. Bordered command-input box at the bottom
    let (cmd_content, show_cursor) = if let Some(ref buf) = app.command_buf {
        (format!(":{}", buf), true)
    } else if let Some(ref msg) = app.status_msg {
        (msg.clone(), false)
    } else {
        (String::new(), false)
    };

    let cmd_spans = if show_cursor {
        vec![
            Span::styled(&cmd_content, style_hint()),
            Span::styled("\u{2588}", Style::default().fg(C_TEXT)), // block cursor
        ]
    } else {
        vec![Span::styled(&cmd_content, style_hint())]
    };

    let cmd_border_style = style_dim();
    frame.render_widget(
        Paragraph::new(Line::from(cmd_spans))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(cmd_border_style),
            )
            .style(style_hint()),
        outer[1],
    );

    // Wildmenu: one line above the command-input box.
    if !app.completion_candidates.is_empty() && outer[1].y > 0 {
        render_wildmenu(frame, app, outer[1]);
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

fn render_list_strip(frame: &mut Frame, app: &AppState, area: Rect) {
    // When no lists are loaded fall back to the bare wordmark.
    if app.all_lists.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(" kwire", style_title())).style(style_normal()),
            area,
        );
        return;
    }

    // Compute "All" aggregate across every list.
    let all_done: usize = app.all_lists.iter().map(|l| l.done).sum();
    let all_total: usize = app.all_lists.iter().map(|l| l.total).sum();

    // Build each segment as (text, style).  Track the column offset of each
    // list so we can compute a horizontal scroll that keeps the active list
    // always fully visible.
    struct Seg {
        text: String,
        style: ratatui::style::Style,
    }
    let mut segs: Vec<Seg> = Vec::new();

    let prefix = format!(" All {}/{}", all_done, all_total);
    let mut cumulative: usize = prefix.chars().count();
    segs.push(Seg {
        text: prefix,
        style: style_muted(),
    });

    // Store the [start, end) column range of each list segment (excluding
    // the "All" prefix and trailing nav hint).
    let mut list_col_ranges: Vec<(usize, usize)> = Vec::new();

    for (i, list) in app.all_lists.iter().enumerate() {
        let is_active = i == app.active_list_idx;
        let text = if is_active {
            format!("   \u{2605} {} {}/{}", list.title, list.done, list.total)
        } else {
            format!("   {} {}/{}", list.title, list.done, list.total)
        };
        let start = cumulative;
        let end = start + text.chars().count();
        list_col_ranges.push((start, end));
        cumulative = end;
        let style = if is_active {
            style_title()
        } else {
            style_muted()
        };
        segs.push(Seg { text, style });
    }

    let nav = "   [ ]";
    let nav_len = nav.chars().count();
    let total_width = cumulative + nav_len;
    segs.push(Seg {
        text: nav.into(),
        style: style_muted(),
    });

    let area_w = area.width as usize;

    // Compute scroll_x so the active list is fully visible.
    let (active_start, active_end) = list_col_ranges
        .get(app.active_list_idx)
        .copied()
        .unwrap_or((0, 0));

    let scroll_x: usize = if total_width <= area_w {
        0 // everything fits — no scroll
    } else {
        // We want [scroll_x, scroll_x + area_w) to contain [active_start, active_end).
        // Try scrolling just far enough to show the start of the active list.
        let want_start = active_start.saturating_sub(1);
        // Clamp so we never show blank space at the right.
        let max_scroll = total_width.saturating_sub(area_w);
        let mut sx = want_start.min(max_scroll);
        // Ensure the end of the active list is also visible (scroll right if needed).
        if sx + area_w < active_end {
            sx = active_end.saturating_sub(area_w).min(max_scroll);
        }
        sx
    };

    let has_left = scroll_x > 0;
    let has_right = scroll_x + area_w < total_width;

    // Build visible spans by slicing each segment to the visible column window
    // [scroll_x, scroll_x + area_w).
    let mut spans: Vec<Span> = Vec::new();
    let mut pos: usize = 0;

    for seg in &segs {
        let seg_len = seg.text.chars().count();
        let seg_end = pos + seg_len;

        if seg_end <= scroll_x {
            // Entirely before the visible window — skip.
            pos = seg_end;
            continue;
        }
        if pos >= scroll_x + area_w {
            // Entirely after the visible window — stop.
            break;
        }

        // Clip to the visible window.
        let char_skip = scroll_x.saturating_sub(pos);
        let chars_available = area_w.saturating_sub(pos.saturating_sub(scroll_x));
        let visible_chars = seg_len.saturating_sub(char_skip).min(chars_available);
        if visible_chars > 0 {
            let visible: String = seg
                .text
                .chars()
                .skip(char_skip)
                .take(visible_chars)
                .collect();
            if !visible.is_empty() {
                spans.push(Span::styled(visible, seg.style));
            }
        }

        pos = seg_end;
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

    let mut x_offset = area.x + 1; // +1 for pane accent char
    let mut spans: Vec<Span> = vec![pane_accent];
    for (filter, label) in &chip_data {
        let is_active_chip = *filter == active_filter;
        let style = if is_active_chip && header_focused {
            // Header focused + active chip: bright bold underlined (cursor here)
            Style::default()
                .fg(C_BRIGHT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else if is_active_chip {
            // Active chip, Header not focused: normal bold underlined
            Style::default()
                .fg(C_TEXT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(C_MUTED)
        };
        let chip_width = label.len() as u16;
        // Store chip rect for mouse hit-testing.
        app.last_rects
            .filter_chips
            .push((Rect::new(x_offset, area.y, chip_width, 1), *filter));
        x_offset += chip_width + 2; // +2 for the separator
        spans.push(Span::styled(label.clone(), style));
        spans.push(Span::styled("  ", Style::default().fg(C_FAINT)));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(style_normal()),
        area,
    );
}

// ---------------------------------------------------------------------------
// 2  Book table
// ---------------------------------------------------------------------------

fn render_book_table(frame: &mut Frame, app: &mut AppState, area: Rect) {
    // Clear previous book_rows.
    app.last_rects.book_rows.clear();

    if app.view.is_none() {
        let para = Paragraph::new("No list loaded. Press : and type 'import <file>' to load.")
            .style(style_dim());
        frame.render_widget(para, area);
        return;
    }

    let mut rows: Vec<Row> = Vec::new();
    // Visual row index within the table body (group headers + book rows), used
    // both for mouse hit-test rects and to keep the selected book scrolled in.
    let mut visual_row: u16 = 0;
    let mut last_group: Option<usize> = None;
    let mut selected_visual: usize = 0;

    for (i, fb) in app.flat.iter().enumerate() {
        let book = &fb.book;
        // Active selection only when the List pane has focus.
        let is_selected = i == app.selected && app.focus == Focus::List;
        // Dim selection marker when List pane is inactive (any other pane focused).
        let is_inactive_selected = i == app.selected && app.focus != Focus::List;

        // Emit a group-header row whenever the owning group changes (matches the
        // wireframe's "LIFT-OFF  4/12" section bands).
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
            rows.push(
                Row::new([
                    Cell::from(""),
                    Cell::from(fb.group_name.clone()).style(style_header()),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(format!("{done}/{total}")).style(style_header()),
                ])
                .height(1)
                .style(style_header()),
            );
            visual_row += 1;
        }

        if i == app.selected {
            selected_visual = visual_row as usize;
        }

        // Determine display state from the first non-available variation (or show
        // discovery state when nothing is queued).
        let (display_fmt, display_size, display_state, display_progress) =
            if book.versions.is_empty() {
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
                // Pick the "best" variation to display: prefer an active one,
                // else the first done, else the first kept.
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

        let bar = theme::progress_bar(display_progress, 10);

        let state_style = theme::style_for_state(
            // Map the display_state string back to a core state key.
            if display_state.contains("done") {
                "done"
            } else if display_state.contains('%') {
                "downloading"
            } else if display_state.contains("failed") {
                "failed"
            } else if display_state.contains("paused") {
                "paused"
            } else {
                "queued"
            },
        );

        let row_style = if is_selected {
            style_selected()
        } else {
            style_normal()
        };

        // Left accent cell: ▌ in accent green on selected bg (active); dim ▌ (inactive); seq # otherwise.
        let seq_cell = if is_selected {
            Cell::from("\u{258c}").style(Style::default().fg(C_DONE).bg(C_SELECTED))
        } else if is_inactive_selected {
            // Dim accent when List pane is inactive — reminds user of the remembered position.
            Cell::from("\u{258c}").style(Style::default().fg(C_FAINT))
        } else {
            Cell::from(format!("{:>3}", book.seq)).style(Style::default().fg(C_FAINT))
        };
        let title_label = book.title.clone();
        let author_label = book.author.clone();

        let row = Row::new([
            seq_cell,
            Cell::from(title_label).style(if is_selected {
                style_selected()
            } else {
                style_title()
            }),
            Cell::from(author_label).style(if is_selected {
                style_selected()
            } else {
                Style::default().fg(C_MUTED)
            }),
            Cell::from(display_fmt).style(if is_selected {
                style_selected()
            } else {
                Style::default().fg(C_MUTED)
            }),
            Cell::from(display_size).style(if is_selected {
                style_selected()
            } else {
                Style::default().fg(C_MUTED)
            }),
            Cell::from(display_state).style(state_style),
            Cell::from(bar).style(state_style),
        ])
        .height(1)
        .style(row_style);
        rows.push(row);

        // Store book row rect: no border, no header — offset is just visual_row.
        let row_rect = Rect::new(area.x, area.y + visual_row, area.width, 1);
        app.last_rects.book_rows.push((row_rect, i));
        visual_row += 1;
    }

    let mut table_state = TableState::default();
    if !app.flat.is_empty() {
        table_state.select(Some(selected_visual));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),  // #
            Constraint::Min(20),    // Title
            Constraint::Min(14),    // Author
            Constraint::Length(5),  // Fmt
            Constraint::Length(8),  // Size
            Constraint::Length(14), // State
            Constraint::Length(12), // Progress bar
        ],
    )
    .row_highlight_style(style_selected());

    frame.render_stateful_widget(table, area, &mut table_state);
}

// ---------------------------------------------------------------------------
// 3  Docked Activity pane  (BORDERLESS — plain line rendering)
// ---------------------------------------------------------------------------

fn render_activity(frame: &mut Frame, app: &AppState, area: Rect) {
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
    let header_text = if app.activity_expanded {
        format!(
            "{} ACTIVITY  {} downloading \u{00b7} {} connecting \u{00b7} {} queued{}  tab to focus",
            arrow, downloading_count, connecting_count, queued_count, speed_str
        )
    } else {
        format!(
            "{} ACTIVITY  {} downloading \u{00b7} {} queued{}  tab to expand",
            arrow, downloading_count, queued_count, speed_str
        )
    };
    let header_style = if app.focus == Focus::Activity {
        style_normal()
    } else {
        style_muted()
    };
    let header_line = Line::from(Span::styled(header_text, header_style));

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

    // Build content lines (to be windowed by scroll offset).
    // leg_idx counts download-book rows so activity_selected maps to them.
    // (#66: legs are ALWAYS selectable — no overflow condition.)
    let mut all_content: Vec<Line> = Vec::new();
    let mut leg_idx: usize = 0;

    if !use_telemetry {
        if host_groups.is_empty() {
            all_content.push(Line::from(Span::styled(
                "  No active transfers.",
                style_dim(),
            )));
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

                for (title, pct, fmt, eta_secs) in versions {
                    let is_leg_sel = activity_active && leg_idx == app.activity_selected;
                    let is_leg_dim = !activity_active && leg_idx == app.activity_selected;
                    let bar = theme::progress_bar((*pct).into(), 6);
                    let eta = eta_secs.map(|s| format!("  {}s", s)).unwrap_or_default();
                    if is_leg_sel {
                        // Active selection: bright highlight
                        all_content.push(Line::from(vec![
                            Span::styled(
                                format!("  \u{25b8} {} ", theme::spinner(app.tick)),
                                Style::default().fg(C_DONE).bg(C_SELECTED),
                            ),
                            Span::styled(
                                format!("{}  {}  {}%  {}{}", title, fmt, pct, bar, eta),
                                style_selected(),
                            ),
                        ]));
                    } else if is_leg_dim {
                        // Inactive selection: dim marker
                        all_content.push(Line::from(vec![
                            Span::styled(
                                format!("  \u{25b8} {} ", theme::spinner(app.tick)),
                                style_dim(),
                            ),
                            Span::styled(title.clone(), style_muted()),
                            Span::styled(
                                format!("  {}  {}%  {}{}", fmt, pct, bar, eta),
                                style_dim(),
                            ),
                        ]));
                    } else {
                        all_content.push(Line::from(vec![
                            Span::styled(format!("  {} ", theme::spinner(app.tick)), style_muted()),
                            Span::styled(title.clone(), style_normal()),
                            Span::styled(
                                format!("  {}  {}%  {}{}", fmt, pct, bar, eta),
                                style_muted(),
                            ),
                        ]));
                    }
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
            for (title, pct, _) in transfers {
                let is_leg_sel = activity_active && leg_idx == app.activity_selected;
                let is_leg_dim = !activity_active && leg_idx == app.activity_selected;
                let bar = theme::progress_bar((*pct).into(), 6);
                if is_leg_sel {
                    all_content.push(Line::from(vec![
                        Span::styled(
                            format!("  \u{25b8} {} ", theme::spinner(app.tick)),
                            Style::default().fg(C_DONE).bg(C_SELECTED),
                        ),
                        Span::styled(format!("{}  {}%  {}", title, pct, bar), style_selected()),
                    ]));
                } else if is_leg_dim {
                    all_content.push(Line::from(vec![
                        Span::styled(
                            format!("  \u{25b8} {} ", theme::spinner(app.tick)),
                            style_dim(),
                        ),
                        Span::styled(title.clone(), style_muted()),
                        Span::styled(format!("  {}%  {}", pct, bar), style_dim()),
                    ]));
                } else {
                    all_content.push(Line::from(vec![
                        Span::styled(format!("  {} ", theme::spinner(app.tick)), style_muted()),
                        Span::styled(title.clone(), style_normal()),
                        Span::styled(format!("  {}%  {}", pct, bar), style_muted()),
                    ]));
                }
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
        Line::from(vec![
            Span::styled(":", style_hint()),
            Span::styled(buf.as_str(), style_hint()),
            Span::styled("\u{2588}", Style::default().fg(C_TEXT)), // block cursor
        ])
    } else {
        Line::default()
    };
    frame.render_widget(Paragraph::new(content).style(style_hint()), area);
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

fn render_hint_bar(frame: &mut Frame, app: &AppState, area: Rect) {
    // The command-line input is rendered in its own row by render_command_line;
    // this bar shows a transient status message or the focus-appropriate hint keys.
    let content = if let Some(ref msg) = app.status_msg {
        // Transient status message — shown until the next keypress.
        Line::from(Span::styled(msg.as_str(), Style::default().fg(C_BRIGHT)))
    } else {
        // ⏎ is universal (shown only in the Help screen per #70) — never in the hint bar.
        const GLOBALS: &str = "  : command \u{00b7} ? help \u{00b7} q quit";
        let hint: String = match app.focus {
            Focus::Header => {
                format!("\u{2190}\u{2192} filter  [ ] list{GLOBALS}")
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
                format!("p pause \u{00b7} c cancel \u{00b7} r retry{GLOBALS}")
            }
        };
        Line::from(Span::styled(hint, style_hint()))
    };

    frame.render_widget(Paragraph::new(content).style(style_hint()), area);
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
    app: &AppState,
    book_flat_index: usize,
    picker_selected: usize,
) {
    let area = centered_rect(96, 26, frame.area());
    frame.render_widget(Clear, area);

    let Some(fb) = app.flat.get(book_flat_index) else {
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

    // Count versions for the subheader.
    let n_candidates = fb.book.versions.len();
    let threshold = if let Some(v) = &app.view {
        v.settings.auto_threshold
    } else {
        0.85
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_dim())
        .title(Span::styled(
            format!(" {} \u{2014} choose a copy ", fb.book.title),
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

    // Layout: subheader (1) + column header row (1) + table rows + rule (1) + hint (1)
    let split = Layout::vertical([
        Constraint::Length(1), // subheader
        Constraint::Min(1),    // table (header + rows)
        Constraint::Length(1), // dim rule
        Constraint::Length(1), // hint
    ])
    .split(padded);

    // Subheader line
    let subhead = format!(
        "{} candidates \u{00b7} auto needs one copy \u{2265} {:.2} \u{2014} none was clear, so pick.",
        n_candidates, threshold
    );
    frame.render_widget(Paragraph::new(Span::styled(subhead, style_dim())), split[0]);

    // Table columns: FMT · TITLE · SOURCE | SIZE | YEAR | PG | MATCH
    // The "·" separators in the header labels are decorative (like the mock)
    let header = Row::new([
        Cell::from("FMT").style(style_header()),
        Cell::from("TITLE \u{00b7} SOURCE").style(style_header()),
        Cell::from("SIZE").style(style_header()),
        Cell::from("YEAR").style(style_header()),
        Cell::from("PG").style(style_header()),
        Cell::from("MATCH").style(style_header()),
    ])
    .height(1)
    .style(style_header());

    let rows: Vec<Row> = fb
        .book
        .versions
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let is_sel = i == picker_selected;
            let sel_indicator = if is_sel { "\u{25b8} " } else { "  " };
            let fmt_cell = format!("{}{}", sel_indicator, v.fmt);
            // Title · Author · Source combined (publisher·language)
            let source = {
                let pub_ = v.publisher.as_str();
                let lang = v.language.as_str();
                match (pub_.is_empty(), lang.is_empty()) {
                    (true, true) => String::new(),
                    (true, false) => lang.to_string(),
                    (false, true) => pub_.to_string(),
                    (false, false) => format!("{}\u{00b7}{}", pub_, lang),
                }
            };
            let author_part = if v.author.is_empty() {
                String::new()
            } else {
                format!(" \u{00b7} {}", v.author)
            };
            let title_source = if source.is_empty() {
                format!("{}{}", v.title, author_part)
            } else {
                format!("{}{} {}", v.title, author_part, source)
            };
            let style_row = if is_sel {
                style_selected()
            } else {
                style_normal()
            };
            Row::new([
                Cell::from(fmt_cell).style(if is_sel {
                    style_selected()
                } else {
                    style_dim()
                }),
                Cell::from(title_source).style(if is_sel {
                    style_selected()
                } else {
                    style_title()
                }),
                Cell::from(if v.size > 0 {
                    format!("{} MB", v.size)
                } else {
                    "\u{2014}".into()
                })
                .style(style_dim()),
                Cell::from(
                    v.year
                        .map(|y| y.to_string())
                        .unwrap_or_else(|| "\u{2014}".into()),
                )
                .style(style_dim()),
                Cell::from(
                    v.pages
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "\u{2014}".into()),
                )
                .style(style_dim()),
                Cell::from(format!("{:.2}", v.score)).style(if is_sel {
                    style_selected()
                } else {
                    Style::default().fg(score_color(v.score.into()))
                }),
            ])
            .height(1)
            .style(style_row)
        })
        .collect();

    let mut table_state = TableState::default();
    if !fb.book.versions.is_empty() {
        table_state.select(Some(picker_selected));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(7), // FMT (with indicator)
            Constraint::Min(24),   // TITLE · AUTHOR
            Constraint::Length(8), // SIZE
            Constraint::Length(6), // YEAR
            Constraint::Length(6), // PAGES
            Constraint::Length(6), // MATCH
        ],
    )
    .header(header)
    .row_highlight_style(style_selected());

    frame.render_stateful_widget(table, split[1], &mut table_state);

    // Dim rule + hint bar (⏎ omitted — see Help screen).
    render_rule(frame, split[2]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            "\u{2191}\u{2193} pick  space mark  a all formats  v meta  esc cancel",
            style_hint(),
        )),
        split[3],
    );
}

// ---------------------------------------------------------------------------
// 4b  Detail modal
// ---------------------------------------------------------------------------

fn render_detail_modal(
    frame: &mut Frame,
    app: &AppState,
    book_flat_index: usize,
    detail_selected: usize,
    sub_focus: &DetailSubFocus,
    history_selected: usize,
) {
    let area = centered_rect(92, 30, frame.area());
    frame.render_widget(Clear, area);

    let Some(fb) = app.flat.get(book_flat_index) else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(style_dim())
            .title(Span::styled(" book detail ", style_dim()))
            .style(style_normal());
        frame.render_widget(block, area);
        return;
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_dim())
        .title(Span::styled(" book detail ", style_dim()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Internal gutter: 2-cell horizontal padding, 1-cell vertical padding.
    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

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
    let var_rows_h = (n_versions.max(1) + 1) as u16; // +1 for header row
                                                     // Subtract all fixed rows: title(1)+subtitle(1)+blank(1)+VARIATIONS(1)+var_rows(n)+blank(1)+HISTORY(1)+rule(1)+hint(1).
    let history_h = padded
        .height
        .saturating_sub(1 + 1 + 1 + 1 + var_rows_h + 1 + 1 + 1 + 1); // = available for history

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
        Constraint::Length(1),          // hint
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

    let title_line = Line::from(vec![
        Span::styled(book.title.clone(), style_title()),
        Span::styled("  ", style_dim()),
        Span::styled(book.author.clone(), style_dim()),
    ]);
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
    let var_header_style = if *sub_focus == DetailSubFocus::Variations {
        style_title()
    } else {
        style_dim()
    };
    frame.render_widget(
        Paragraph::new(Span::styled(var_summary, var_header_style)),
        split[3],
    );

    // Variation rows as a table (no outer border — inline with block)
    let var_header = Row::new([
        Cell::from("").style(style_header()), // checkmark col
        Cell::from("Title \u{00b7} Author").style(style_header()),
        Cell::from("Fmt").style(style_header()),
        Cell::from("Size").style(style_header()),
        Cell::from("Src").style(style_header()),
        Cell::from("Match").style(style_header()),
        Cell::from("State").style(style_header()),
        Cell::from("Progress").style(style_header()),
    ])
    .height(1)
    .style(style_header());

    let var_rows: Vec<Row> = fb
        .book
        .versions
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let is_sel = i == detail_selected;
            // ▶ for the selected variation; ✓ for done; spinner for downloading.
            let check = if is_sel {
                "\u{25b6}" // ▶ selected marker
            } else if v.state == "done" {
                "\u{2713}"
            } else if v.state == "downloading" {
                theme::spinner(app.tick)
            } else {
                " "
            };
            let bar = theme::progress_bar(v.progress, 8);
            let state_cell = match v.state.as_str() {
                "done" => {
                    let md5_short = v.md5.chars().take(3).collect::<String>();
                    format!("done \u{00b7} {} \u{00b7} \u{2713}", md5_short)
                }
                "downloading" => {
                    let spd = v
                        .eta_secs
                        .map(|s| format!(" \u{00b7} eta {}s", s))
                        .unwrap_or_default();
                    format!("downloading {}%{}", v.progress, spd)
                }
                other => other.to_string(),
            };
            let host = v.host.as_deref().unwrap_or("\u{2014}");
            let title_author = if v.author.is_empty() {
                v.title.clone()
            } else {
                format!("{} \u{00b7} {}", v.title, v.author)
            };
            let row_style = if is_sel {
                style_selected()
            } else {
                style_normal()
            };
            Row::new([
                Cell::from(check).style(if is_sel {
                    style_selected()
                } else {
                    theme::style_for_state(&v.state)
                }),
                Cell::from(title_author).style(if is_sel {
                    style_selected()
                } else {
                    style_title()
                }),
                Cell::from(v.fmt.clone()).style(if is_sel {
                    style_selected()
                } else {
                    style_dim()
                }),
                Cell::from(if v.size > 0 {
                    format!("{} MB", v.size)
                } else {
                    "\u{2014}".into()
                })
                .style(if is_sel {
                    style_selected()
                } else {
                    style_dim()
                }),
                Cell::from(host.to_string()).style(if is_sel {
                    style_selected()
                } else {
                    style_dim()
                }),
                Cell::from(format!("{:.2}", v.score)).style(if is_sel {
                    style_selected()
                } else {
                    Style::default().fg(score_color(v.score.into()))
                }),
                Cell::from(state_cell).style(if is_sel {
                    style_selected()
                } else {
                    theme::style_for_state(&v.state)
                }),
                Cell::from(bar).style(if is_sel {
                    style_selected()
                } else {
                    theme::style_for_state(&v.state)
                }),
            ])
            .height(1)
            .style(row_style)
        })
        .collect();

    let mut var_table_state = TableState::default();
    if !fb.book.versions.is_empty() {
        var_table_state.select(Some(detail_selected));
    }

    let var_table = Table::new(
        var_rows,
        [
            Constraint::Length(2),  // check
            Constraint::Min(20),    // Title · Author
            Constraint::Length(6),  // Fmt
            Constraint::Length(8),  // Size
            Constraint::Length(10), // Src (host)
            Constraint::Length(6),  // Match (score)
            Constraint::Min(14),    // State
            Constraint::Length(9),  // Progress
        ],
    )
    .header(var_header)
    .row_highlight_style(style_selected());

    frame.render_stateful_widget(var_table, split[4], &mut var_table_state);

    // Output path for done variations shown below (if any)
    // HISTORY header — accent when History is focused.
    let hist_header_style = if *sub_focus == DetailSubFocus::History {
        style_title()
    } else {
        style_dim()
    };
    frame.render_widget(
        Paragraph::new(Span::styled("\u{25be} HISTORY", hist_header_style)),
        split[6],
    );

    // History list — windowed so it can scroll; highlight the selected row.
    let n_hist = fb.book.history.len();
    let win_h = split[7].height as usize;
    // history_selected is 0 = most-recent (top of the reversed list).
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
        .rev()
        .enumerate()
        .skip(scroll_offset)
        .take(win_h)
        .map(|(i, e)| {
            let is_hist_sel = *sub_focus == DetailSubFocus::History && i == hist_sel;
            // Format time as HH:MM:SS from ms timestamp
            let secs = e.at_ms / 1000;
            let time_str = format!(
                "{:02}:{:02}:{:02}",
                (secs / 3600) % 24,
                (secs / 60) % 60,
                secs % 60
            );
            let kind_color = history_kind_color(&e.kind);
            let base_style = if is_hist_sel {
                style_selected()
            } else {
                style_dim()
            };
            Line::from(vec![
                Span::styled(format!("   {:<8}  \u{25cf} ", time_str), base_style),
                Span::styled(
                    format!("{:<12}  ", e.kind),
                    if is_hist_sel {
                        style_selected()
                    } else {
                        Style::default().fg(kind_color).add_modifier(Modifier::BOLD)
                    },
                ),
                Span::styled(e.detail.clone(), base_style),
            ])
        })
        .collect();

    frame.render_widget(List::new(history_items), split[7]);

    // Dim rule above hint (⏎/↑↓/tab omitted per #64).
    render_rule(frame, split[8]);

    // Context-aware hint: varies by sub-focus and selected variation state.
    let detail_hint: &str = match sub_focus {
        DetailSubFocus::History => "esc back",
        DetailSubFocus::Variations => {
            let var_state = fb
                .book
                .versions
                .get(detail_selected)
                .map(|v| v.state.as_str())
                .unwrap_or("available");
            match var_state {
                "done" => {
                    "o open \u{00b7} R reveal \u{00b7} r re-download  e edit \u{00b7} x remove \u{00b7} m not-found \u{00b7} esc back"
                }
                "downloading" => {
                    "p pause \u{00b7} c cancel  e edit \u{00b7} x remove \u{00b7} m not-found \u{00b7} esc back"
                }
                "failed" | "cancelled" => {
                    "r retry  e edit \u{00b7} x remove \u{00b7} m not-found \u{00b7} esc back"
                }
                _ => {
                    // available / queued
                    "download  e edit \u{00b7} x remove \u{00b7} m not-found \u{00b7} esc back"
                }
            }
        }
    };
    frame.render_widget(
        Paragraph::new(Span::styled(detail_hint, style_hint())),
        split[9],
    );
}

// ---------------------------------------------------------------------------
// 4c  Settings modal
// ---------------------------------------------------------------------------

fn render_settings_modal(frame: &mut Frame, app: &AppState) {
    let area = centered_rect(80, 30, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_dim())
        .title(Span::styled(
            if let Some(v) = &app.view {
                format!(" Settings \u{00b7} {} ", v.title)
            } else {
                " Settings ".to_string()
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

    // Min(field list) + rule(1) + hint(1)
    let split = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(padded);

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
        .map(|row| match row {
            SettingsRow::SectionHeader(title) => ListItem::new(Line::from(vec![
                Span::styled("\n", style_dim()),
                Span::styled(*title, style_header()),
            ])),
            SettingsRow::Field {
                label,
                value,
                index,
            } => {
                let is_sel = *index == app.settings_selected;
                let is_editing = editing_idx.map(|e| e == *index).unwrap_or(false);

                // For format-pref (idx 0): render [epub] [pdf]  + mobi · azw3 (off)
                // For threshold fields (idx 2/3): append a ▰▱ bar.
                // Only applies when NOT actively being edited.
                let display_value: String = if !is_editing && *index == 0 {
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
                    if included.is_empty() {
                        "\u{2014}".into()
                    } else if excluded.is_empty() {
                        included.join(" ")
                    } else {
                        format!(
                            "{}  + {} (off)",
                            included.join(" "),
                            excluded.join(" \u{00b7} ")
                        )
                    }
                } else if !is_editing && (*index == 2 || *index == 3) {
                    if let Ok(v) = value.parse::<f32>() {
                        let pct = (v * 100.0) as u32;
                        let bar = theme::progress_bar(pct, 10);
                        format!("{value}   {bar}")
                    } else {
                        value.clone()
                    }
                } else {
                    value.clone()
                };

                let value_style = if is_editing {
                    Style::default()
                        .fg(C_BG)
                        .bg(C_NEEDS_YOU)
                        .add_modifier(Modifier::BOLD)
                } else if is_sel {
                    style_selected()
                } else {
                    Style::default().fg(C_WARM)
                };

                let indicator = if is_editing {
                    ""
                } else if is_sel {
                    "  \u{2502}"
                } else {
                    ""
                };

                ListItem::new(Line::from(vec![
                    Span::styled(format!("  {:<28}", label), style_dim()),
                    Span::styled(display_value, value_style),
                    Span::styled(indicator, style_dim()),
                ]))
            }
        })
        .collect();

    let list = List::new(items).block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, split[0]);

    // Dim rule + context-sensitive hint (⏎ omitted per #70).
    render_rule(frame, split[1]);
    let hint_text: String = match &draft.editor {
        SettingsEditor::Editing(_) => "type \u{00b7} esc cancel".into(),
        SettingsEditor::FormatEditor { .. } => {
            "space toggle \u{00b7} J/K reorder \u{00b7} esc done".into()
        }
        SettingsEditor::LangPicker { .. } => "\u{2191}\u{2193} \u{00b7} esc".into(),
        SettingsEditor::Viewing => {
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
            if field_hint.is_empty() {
                "s save \u{00b7} esc cancel".into()
            } else {
                format!("{field_hint}  s save \u{00b7} esc cancel")
            }
        }
    };
    frame.render_widget(
        Paragraph::new(Span::styled(hint_text, style_hint())),
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
                Style::default().fg(C_NEEDS_YOU)
            } else {
                style_dim()
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
        Paragraph::new(Span::styled(
            "spc toggle  J/K reorder  esc done",
            style_hint(),
        )),
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
    let area = centered_rect(88, 30, parent);
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_dim())
        .title(Span::styled(" keys & commands ", style_dim()))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Internal gutter: 2-cell horizontal padding, 1-cell vertical padding.
    let padded = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    // Two-column layout matching the mock:
    // Left: NAVIGATE + FILTER sections
    // Right: ACT ON SELECTION + COMMAND LINE sections
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(padded);

    let split_left = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(cols[0]);

    // Left column
    let left_lines: Vec<Line> = vec![
        Line::from(Span::styled("NAVIGATE", style_header())),
        make_key_line(
            "\u{2191} \u{2193} / j k",
            "move \u{00b7} activity cross at edge",
        ),
        make_key_line("[ ]", "prev / next reading list  (any pane)"),
        make_key_line(
            "tab / S-tab",
            "cycle panes: Header \u{2192} List \u{2192} Activity",
        ),
        make_key_line("\u{23ce}", "open \u{00b7} choose a copy  (List pane)"),
        make_key_line("d", "book detail & history"),
        make_key_line("esc \u{00b7} q", "back \u{00b7} quit"),
        Line::from(""),
        Line::from(Span::styled(
            "FILTER  (Header pane \u{2014} Tab to focus)",
            style_header(),
        )),
        make_key_line("\u{2190} \u{2192}", "move filter chip  (Header pane only)"),
        make_key_line("/", "cycle filter  (any pane)"),
        make_key_line(
            "1\u{2013}6",
            "all \u{00b7} needs \u{00b7} check \u{00b7} cannot \u{00b7} progress \u{00b7} done",
        ),
    ];

    let left_list: Vec<ListItem> = left_lines.into_iter().map(|l| ListItem::new(l)).collect();
    frame.render_widget(List::new(left_list), split_left[0]);

    // Right column
    let right_lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "ACT ON SELECTION  (List pane)",
            style_header(),
        )),
        make_key_line("space", "mark a copy"),
        make_key_line("a", "fetch all preferred formats"),
        make_key_line("r", "retry \u{00b7} re-download"),
        make_key_line("p \u{00b7} c", "pause \u{00b7} cancel"),
        make_key_line("o \u{00b7} R", "open file \u{00b7} reveal in Finder"),
        Line::from(""),
        Line::from(Span::styled(
            "ACTIVITY PANE  (Tab to focus)",
            style_header(),
        )),
        make_key_line("\u{2191}\u{2193}", "select transfer leg"),
        make_key_line(
            "p \u{00b7} c \u{00b7} r",
            "pause \u{00b7} cancel \u{00b7} resume leg",
        ),
        Line::from(""),
        Line::from(Span::styled("COMMAND LINE  (press :)", style_header())),
        make_key_line(":import <file>", "add a list"),
        make_key_line(":add", "add one book"),
        make_key_line(":open <list>", "switch list"),
        make_key_line(":requery", "re-search & re-verify"),
        make_key_line(":settings", "open settings"),
        make_key_line(":pause-all / :pause", "pause downloads"),
        make_key_line(":start / :start-all", "resume downloads"),
        make_key_line(":delete", "remove active list"),
        make_key_line(":add-md5 <md5>", "inject MD5 for selection"),
        make_key_line(":download-series", "add selection's series siblings"),
        make_key_line(":refresh-mirrors", "refresh mirror status"),
        make_key_line(":cleanup", "remove .part files"),
        make_key_line(":reorganize", "move files to current layout"),
    ];

    let right_list: Vec<ListItem> = right_lines.into_iter().map(|l| ListItem::new(l)).collect();
    frame.render_widget(List::new(right_list), cols[1]);

    // Dim rule + hint.
    render_rule(frame, split_left[1]);
    frame.render_widget(
        Paragraph::new(Span::styled("? or esc  to close", style_hint())),
        split_left[2],
    );
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

/// Render the Tab-completion wildmenu as a single line directly above
/// `hint_rect` (i.e. at `hint_rect.y - 1`).  The currently highlighted
/// candidate is drawn reversed (dark bg, accent fg); others are dim.
fn render_wildmenu(frame: &mut Frame, app: &AppState, hint_rect: Rect) {
    if hint_rect.y == 0 {
        return;
    }
    let menu_area = Rect::new(hint_rect.x, hint_rect.y - 1, hint_rect.width, 1);

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
        spans.push(Span::styled(format!(" {} ", cand), style));
        if i + 1 < app.completion_candidates.len() {
            // Thin separator between candidates.
            spans.push(Span::styled("  ", Style::default().fg(C_FAINT).bg(C_PANEL)));
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(C_PANEL)),
        menu_area,
    );
}

fn make_key_line<'a>(key: &'a str, action: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{:<10}", key), Style::default().fg(C_DONE)),
        Span::styled(action, style_normal()),
    ])
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
