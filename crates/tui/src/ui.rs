//! ratatui render pass — derives everything from [`AppState`].
//!
//! The layout follows §4 exactly:
//! ```
//! Length(1)       — list strip
//! Length(1)       — status-filter row
//! Min(8)          — book Table
//! Length(N)       — docked Activity pane  (N=1 collapsed, N=5 expanded)
//! Length(1)       — key-hint bar / command line
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

use crate::app::{AppState, Focus, Modal, StatusFilter};
use crate::theme::{
    self, style_dim, style_header, style_hint, style_normal, style_selected, style_title, C_BG,
    C_BRIGHT, C_DIM, C_DONE, C_FAINT, C_NEEDS_YOU, C_PANEL, C_TEXT,
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

    let chunks = Layout::vertical([
        Constraint::Length(1),          // 0 list strip
        Constraint::Length(1),          // 1 status-filter row
        Constraint::Min(8),             // 2 book Table
        Constraint::Length(activity_h), // 3 docked Activity pane
        Constraint::Length(1),          // 4 hint bar / command line
    ])
    .split(frame.area());

    // Store panel rects.
    app.last_rects.list_strip = chunks[0];
    app.last_rects.filter_row = chunks[1];
    app.last_rects.book_table = chunks[2];
    app.last_rects.activity = chunks[3];
    app.last_rects.hint_bar = chunks[4];

    render_list_strip(frame, app, chunks[0]);
    render_filter_row(frame, app, chunks[1]);
    render_book_table(frame, app, chunks[2]);
    render_activity(frame, app, chunks[3]);
    render_hint_bar(frame, app, chunks[4]);

    // Wildmenu: one line directly above the command-line hint bar.
    if !app.completion_candidates.is_empty() && chunks[4].y > 0 {
        render_wildmenu(frame, app, chunks[4]);
    }

    // Overlay modal if one is open.
    if let Some(modal) = app.modal.clone() {
        match modal {
            Modal::Picker {
                book_flat_index,
                selected,
            } => render_picker_modal(frame, app, book_flat_index, selected),
            Modal::Detail { book_flat_index } => render_detail_modal(frame, app, book_flat_index),
            Modal::Settings => render_settings_modal(frame, app),
            Modal::Help => render_help_modal(frame, frame.area()),
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
    //   1 — logo box (3-line box: border + content + border)
    //   1 — blank
    //   1 — wordmark
    //   3 — tagline (3 lines)
    //   1 — blank
    //   1 — NO READING LISTS YET
    //   1 — blank
    //   4 — command hints
    // Total = 3 + 1 + 1 + 3 + 1 + 1 + 1 + 4 = 15 lines
    let content_h: u16 = 15;
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
        Constraint::Length(1), // wordmark
        Constraint::Length(3), // tagline
        Constraint::Length(1), // blank
        Constraint::Length(1), // NO READING LISTS YET
        Constraint::Length(1), // blank
        Constraint::Length(4), // command hints
    ])
    .split(content_area);

    // 1. Logo glyph — a bordered box containing "· · ·"
    let logo_block = Block::default()
        .borders(Borders::ALL)
        .border_style(style_dim());
    // Make the logo box small (~9 wide), centered
    let logo_inner_w: u16 = 7;
    let logo_box_w: u16 = logo_inner_w + 2; // +2 for borders
    let logo_area = centered_rect(logo_box_w, 3, parts[0]);
    frame.render_widget(
        Paragraph::new("· · ·")
            .alignment(Alignment::Center)
            .style(style_dim())
            .block(logo_block),
        logo_area,
    );

    // 2. Wordmark — bold, bright, centered
    frame.render_widget(
        Paragraph::new("kwire")
            .alignment(Alignment::Center)
            .style(Style::default().fg(C_BRIGHT).add_modifier(Modifier::BOLD)),
        parts[2],
    );

    // 3. Tagline — 3 lines, "quire" emphasized
    //    "A quire gathers folded sheets into one section"
    //    "of a book — kwire gathers a scattered reading"
    //    "list into one tidy, downloaded collection."
    let line1 = Line::from(vec![
        Span::styled("A ", style_dim()),
        Span::styled(
            "quire",
            Style::default().fg(C_DONE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" gathers folded sheets into one section", style_dim()),
    ]);
    let line2 = Line::from(Span::styled(
        "of a book \u{2014} kwire gathers a scattered reading",
        style_dim(),
    ));
    let line3 = Line::from(Span::styled(
        "list into one tidy, downloaded collection.",
        style_dim(),
    ));
    frame.render_widget(
        Paragraph::new(vec![line1, line2, line3]).alignment(Alignment::Center),
        parts[3],
    );

    // 4. NO READING LISTS YET — dim, centered
    frame.render_widget(
        Paragraph::new("NO READING LISTS YET")
            .alignment(Alignment::Center)
            .style(style_dim()),
        parts[5],
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
    let hint_area = centered_rect(hint_row_w.min(area.width), 4, parts[7]);
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
        match modal {
            Modal::Help => render_help_modal(frame, area),
            Modal::Settings => render_settings_modal(frame, app),
            _ => {}
        }
    }
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

    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::styled(
        format!(" All {}/{}", all_done, all_total),
        style_dim(),
    ));

    for (i, list) in app.all_lists.iter().enumerate() {
        let is_active = i == app.active_list_idx;
        if is_active {
            // Active list: star prefix, bold/bright
            spans.push(Span::styled(
                format!("   \u{2605} {} {}/{}", list.title, list.done, list.total),
                style_title(),
            ));
        } else {
            // Inactive lists: dim, no star
            spans.push(Span::styled(
                format!("   {} {}/{}", list.title, list.done, list.total),
                style_dim(),
            ));
        }
    }

    // Right-edge navigation hint
    spans.push(Span::styled("   \u{2190} \u{2192}", style_dim()));

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(style_normal()),
        area,
    );
}

// ---------------------------------------------------------------------------
// 1  Status-filter row
// ---------------------------------------------------------------------------

fn render_filter_row(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let counts = app.status_counts();
    let active_filter = app.filter;

    // Clear filter_chips so we can rebuild.
    app.last_rects.filter_chips.clear();

    // Build chips and track their rects for mouse hit-testing.
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

    let mut x_offset = area.x;
    let mut spans: Vec<Span> = Vec::new();
    for (filter, label) in &chip_data {
        let style = if *filter == active_filter {
            Style::default()
                .fg(C_TEXT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(C_DIM)
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
        let is_selected = i == app.selected && app.focus == Focus::List;

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

        let seq_label = format!("{:>3}", book.seq);
        let title_label = book.title.clone();
        let author_label = book.author.clone();

        let row = Row::new([
            Cell::from(seq_label).style(row_style),
            Cell::from(title_label).style(if is_selected {
                style_selected()
            } else {
                style_title()
            }),
            Cell::from(author_label).style(if is_selected {
                style_selected()
            } else {
                Style::default().fg(C_NEEDS_YOU)
            }),
            Cell::from(display_fmt).style(style_dim()),
            Cell::from(display_size).style(style_dim()),
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
        style_dim()
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

    // Build content lines (to be windowed by scroll offset).
    let mut all_content: Vec<Line> = Vec::new();

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
                all_content.push(Line::from(Span::styled(host_line, style_normal())));

                for (title, pct, fmt, eta_secs) in versions {
                    let bar = theme::progress_bar((*pct).into(), 6);
                    let eta = eta_secs.map(|s| format!("  {}s", s)).unwrap_or_default();
                    all_content.push(Line::from(vec![
                        Span::styled(format!("  {} ", theme::spinner(app.tick)), style_dim()),
                        Span::styled(title.clone(), style_normal()),
                        Span::styled(format!("  {}  {}%  {}{}", fmt, pct, bar, eta), style_dim()),
                    ]));
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
            all_content.push(Line::from(Span::styled(host_line, style_normal())));
            for (title, pct, _) in transfers {
                let bar = theme::progress_bar((*pct).into(), 6);
                all_content.push(Line::from(vec![
                    Span::styled(format!("  {} ", theme::spinner(app.tick)), style_dim()),
                    Span::styled(title.clone(), style_normal()),
                    Span::styled(format!("  {}%  {}", pct, bar), style_dim()),
                ]));
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
                style_dim(),
            )));
        }
        for line in &all_content[offset..end] {
            display.push(line.clone());
        }
        if has_below {
            display.push(Line::from(Span::styled(
                format!("  \u{25be} {} more", n - end),
                style_dim(),
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
// 4  Hint bar / command line
// ---------------------------------------------------------------------------

fn render_hint_bar(frame: &mut Frame, app: &AppState, area: Rect) {
    let content = if let Some(ref buf) = app.command_buf {
        Line::from(vec![
            Span::styled(":", style_hint()),
            Span::styled(buf.as_str(), style_hint()),
            Span::styled("\u{2588}", Style::default().fg(C_TEXT)), // cursor
        ])
    } else if let Some(ref msg) = app.status_msg {
        // Transient status message — shown until the next keypress.
        Line::from(Span::styled(msg.as_str(), Style::default().fg(C_BRIGHT)))
    } else {
        let hint = match app.focus {
            Focus::List => {
                "\u{2191}\u{2193} move  \u{2190}\u{2192} list  \u{23ce} open  d detail  / filter  : command  tab downloads  ? help  q"
            }
            Focus::Activity => "\u{2191}\u{2193} scroll  tab list  q quit",
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

// ---------------------------------------------------------------------------
// 4a  Picker modal ("choose a copy")
// ---------------------------------------------------------------------------

fn render_picker_modal(
    frame: &mut Frame,
    app: &AppState,
    book_flat_index: usize,
    picker_selected: usize,
) {
    let area = centered_rect(88, 22, frame.area());
    frame.render_widget(Clear, area);

    let Some(fb) = app.flat.get(book_flat_index) else {
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(" Choose a copy ")
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
        .title(format!(" {} \u{2014} choose a copy ", fb.book.title))
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Layout: subheader (1) + column header row (1) + table rows + hint (1)
    let split = Layout::vertical([
        Constraint::Length(1), // subheader
        Constraint::Min(1),    // table (header + rows)
        Constraint::Length(1), // hint
    ])
    .split(inner);

    // Subheader line
    let subhead = format!(
        "{} candidates \u{00b7} auto-download needs a single copy \u{2265} {:.2} confidence",
        n_candidates, threshold
    );
    frame.render_widget(Paragraph::new(Span::styled(subhead, style_dim())), split[0]);

    // Table columns: FMT · TITLE · AUTHOR | SIZE | YEAR | PAGES | MATCH
    // The "·" separators in the header labels are decorative (like the mock)
    let header = Row::new([
        Cell::from("FMT").style(style_header()),
        Cell::from("TITLE \u{00b7} AUTHOR").style(style_header()),
        Cell::from("SIZE").style(style_header()),
        Cell::from("YEAR").style(style_header()),
        Cell::from("PAGES").style(style_header()),
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
            let sel_indicator = if is_sel { "\u{25b6} " } else { "  " };
            let fmt_cell = format!("{}{}", sel_indicator, v.fmt);
            // Title · Author combined
            let title_author = format!("{} {}", v.title, v.author);
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
                Cell::from(title_author).style(if is_sel {
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
                    Style::default().fg(C_DONE)
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

    // Hint bar at bottom
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("\u{2191}\u{2193} pick", style_hint()),
            Span::styled("  space mark  ", style_hint()),
            Span::styled("\u{23ce} download", Style::default().fg(C_DONE)),
            Span::styled(
                "  a all preferred formats  v metadata  esc cancel",
                style_hint(),
            ),
        ])),
        split[2],
    );
}

// ---------------------------------------------------------------------------
// 4b  Detail modal
// ---------------------------------------------------------------------------

fn render_detail_modal(frame: &mut Frame, app: &AppState, book_flat_index: usize) {
    let area = centered_rect(90, 24, frame.area());
    frame.render_widget(Clear, area);

    let Some(fb) = app.flat.get(book_flat_index) else {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Book detail ")
            .style(style_normal());
        frame.render_widget(block, area);
        return;
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Book detail ")
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);

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
    let history_h = inner
        .height
        .saturating_sub(1 + 1 + 1 + 1 + var_rows_h + 1 + 1 + 1); // = available for history

    let split = Layout::vertical([
        Constraint::Length(1),          // title line
        Constraint::Length(1),          // subtitle line
        Constraint::Length(1),          // blank
        Constraint::Length(1),          // VARIATIONS label
        Constraint::Length(var_rows_h), // variation rows
        Constraint::Length(1),          // blank
        Constraint::Length(1),          // HISTORY label
        Constraint::Min(history_h),     // history rows
        Constraint::Length(1),          // hint
    ])
    .split(inner);

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
    let subtitle = format!(
        "{} \u{00b7} {}.  \u{25cf} {} requested \u{00b7} {} done \u{00b7} {} active",
        fb.group_name, book.seq, n_requested, n_done, n_active
    );
    frame.render_widget(
        Paragraph::new(Span::styled(subtitle, style_dim())),
        split[1],
    );

    // VARIATIONS header
    let var_summary = format!(
        "\u{25be} VARIATIONS  {} requested \u{00b7} {} done \u{00b7} {} active",
        n_requested, n_done, n_active
    );
    frame.render_widget(
        Paragraph::new(Span::styled(var_summary, style_dim())),
        split[3],
    );

    // Variation rows as a table (no outer border — inline with block)
    let var_header = Row::new([
        Cell::from("").style(style_header()), // checkmark col
        Cell::from("Fmt").style(style_header()),
        Cell::from("Size").style(style_header()),
        Cell::from("Source").style(style_header()),
        Cell::from("State").style(style_header()),
        Cell::from("Progress").style(style_header()),
    ])
    .height(1)
    .style(style_header());

    let var_rows: Vec<Row> = fb
        .book
        .versions
        .iter()
        .map(|v| {
            let check = if v.state == "done" {
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
            Row::new([
                Cell::from(check).style(theme::style_for_state(&v.state)),
                Cell::from(v.fmt.clone()).style(style_dim()),
                Cell::from(if v.size > 0 {
                    format!("{} MB", v.size)
                } else {
                    "\u{2014}".into()
                })
                .style(style_dim()),
                Cell::from(host.to_string()).style(style_dim()),
                Cell::from(state_cell).style(theme::style_for_state(&v.state)),
                Cell::from(bar).style(theme::style_for_state(&v.state)),
            ])
            .height(1)
        })
        .collect();

    let var_table = Table::new(
        var_rows,
        [
            Constraint::Length(2),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Length(12),
            Constraint::Min(20),
            Constraint::Length(10),
        ],
    )
    .header(var_header);

    frame.render_widget(var_table, split[4]);

    // Output path for done variations shown below (if any)
    // HISTORY header
    frame.render_widget(
        Paragraph::new(Span::styled("\u{25be} HISTORY", style_dim())),
        split[6],
    );

    // History list — rows: time · kind · detail
    let history_items: Vec<Line> = fb
        .book
        .history
        .iter()
        .rev()
        .take(split[7].height as usize)
        .map(|e| {
            // Format time as HH:MM:SS from ms timestamp
            let secs = e.at_ms / 1000;
            let time_str = format!(
                "{:02}:{:02}:{:02}",
                (secs / 3600) % 24,
                (secs / 60) % 60,
                secs % 60
            );
            Line::from(vec![
                Span::styled(format!("{:<10}  ", time_str), style_dim()),
                Span::styled(
                    format!("{:<14}  ", e.kind),
                    Style::default().fg(C_DONE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(e.detail.clone(), style_dim()),
            ])
        })
        .collect();

    frame.render_widget(List::new(history_items), split[7]);

    // Hint
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("\u{2191}\u{2193} variation", style_hint()),
            Span::styled(
                "  o open file  R reveal  r re-download  esc back",
                style_hint(),
            ),
        ])),
        split[8],
    );
}

// ---------------------------------------------------------------------------
// 4c  Settings modal
// ---------------------------------------------------------------------------

fn render_settings_modal(frame: &mut Frame, app: &AppState) {
    let area = centered_rect(72, 22, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(if let Some(v) = &app.view {
            format!(" settings \u{00b7} {} ", v.title)
        } else {
            " settings ".to_string()
        })
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let split = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

    // Structured settings sections matching the mock:
    // FORMATS, MATCHING, FILES, DOWNLOADS & MIRRORS
    #[derive(Clone)]
    enum SettingsRow {
        SectionHeader(&'static str),
        Field {
            label: &'static str,
            value: String,
            index: usize,
        },
    }

    let (
        fmt_pref,
        language,
        auto_thresh,
        near_thresh,
        keep_top,
        naming,
        sub_group,
        max_conc,
        per_host,
        hedged,
        search_mirrors,
        dl_sites,
    ) = if let Some(v) = &app.view {
        let s = &v.settings;
        (
            s.format_pref.join(", "),
            s.language.clone(),
            format!("{:.2}", s.auto_threshold),
            format!("{:.2}", s.near_threshold),
            s.keep_top.to_string(),
            s.naming_template.clone(),
            "on".to_string(),
            "8".to_string(),
            "4".to_string(),
            "off".to_string(),
            "libgen.li  libgen.is  libgen.rs".to_string(),
            "libgen.li  libgen.pw  ipfs".to_string(),
        )
    } else {
        (
            "epub, pdf".into(),
            "any".into(),
            "0.85".into(),
            "0.40".into(),
            "3".into(),
            "{seq:02} - {authors} - {title}.{ext}".into(),
            "on".into(),
            "8".into(),
            "4".into(),
            "off".into(),
            "libgen.li  libgen.is  libgen.rs".into(),
            "libgen.li  libgen.pw  ipfs".into(),
        )
    };

    let mut field_index = 0usize;
    let mut make_field = |label: &'static str, value: String| -> SettingsRow {
        let row = SettingsRow::Field {
            label,
            value,
            index: field_index,
        };
        field_index += 1;
        row
    };

    let rows: Vec<SettingsRow> = vec![
        SettingsRow::SectionHeader("FORMATS"),
        make_field("Preferred formats", fmt_pref),
        make_field("Language", language),
        SettingsRow::SectionHeader("MATCHING"),
        make_field("Auto-download at \u{2265}", auto_thresh),
        make_field("Treat as not-found below", near_thresh),
        make_field("Keep top copies", keep_top),
        SettingsRow::SectionHeader("FILES"),
        make_field("Download folder", "~/Books/Kwire".into()),
        make_field("Naming template", naming),
        make_field("Sub-grouping", sub_group),
        SettingsRow::SectionHeader("DOWNLOADS & MIRRORS"),
        make_field(
            "Max concurrent",
            format!(
                "{} per-host attempts {}  hedged {}",
                max_conc, per_host, hedged
            ),
        ),
        make_field("Search mirrors", search_mirrors),
        make_field("Download sites", dl_sites),
    ];

    let items: Vec<ListItem> = rows
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
                let value_display = if is_sel {
                    if let Some(ref edit) = app.settings_edit {
                        edit.clone()
                    } else {
                        value.clone()
                    }
                } else {
                    value.clone()
                };
                let row_style = if is_sel {
                    style_selected()
                } else {
                    style_normal()
                };
                let edit_indicator = if is_sel { "  edit |" } else { "" };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("  {:<26}", label), style_dim()),
                    Span::styled(value_display, row_style),
                    Span::styled(edit_indicator.to_string(), style_dim()),
                ]))
            }
        })
        .collect();

    let list = List::new(items).block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, split[0]);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("\u{2191}\u{2193} field", style_hint()),
            Span::styled(
                "  \u{23ce} edit  space toggle  esc save & close",
                style_hint(),
            ),
        ])),
        split[1],
    );
}

// ---------------------------------------------------------------------------
// 4d  Help screen
// ---------------------------------------------------------------------------

fn render_help_modal(frame: &mut Frame, parent: Rect) {
    let area = centered_rect(82, 26, parent);
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Keys & Commands ")
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Two-column layout matching the mock:
    // Left: NAVIGATE + FILTER sections
    // Right: ACT ON SELECTION + COMMAND LINE sections
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);

    let split_left = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(cols[0]);

    // Left column
    let left_lines: Vec<Line> = vec![
        Line::from(Span::styled("NAVIGATE", style_header())),
        make_key_line("\u{2191} \u{2193}", "move selection"),
        make_key_line("\u{2190} \u{2192}", "switch reading list"),
        make_key_line("\u{23ce}", "open \u{00b7} choose a copy"),
        make_key_line("d", "book detail & history"),
        make_key_line("tab", "focus the downloads pane"),
        make_key_line("esc \u{00b7} q", "back \u{00b7} quit"),
        Line::from(""),
        Line::from(Span::styled("FILTER", style_header())),
        make_key_line("/", "cycle status filter"),
        make_key_line(
            "1\u{2013}6",
            "all \u{00b7} needs \u{00b7} check \u{00b7} cannot \u{00b7} progress \u{00b7} done",
        ),
    ];

    let left_list: Vec<ListItem> = left_lines.into_iter().map(|l| ListItem::new(l)).collect();
    frame.render_widget(List::new(left_list), split_left[0]);

    // Right column
    let right_lines: Vec<Line> = vec![
        Line::from(Span::styled("ACT ON SELECTION", style_header())),
        make_key_line("space", "mark a copy"),
        make_key_line("a", "fetch all preferred formats"),
        make_key_line("r", "retry \u{00b7} re-download"),
        make_key_line("p \u{00b7} c", "pause \u{00b7} cancel"),
        make_key_line("o \u{00b7} R", "open file \u{00b7} reveal in Finder"),
        Line::from(""),
        Line::from(Span::styled("COMMAND LINE  (press :)", style_header())),
        make_key_line(":import <file>", "add a list"),
        make_key_line(":add", "add one book"),
        make_key_line(":open <list>", "switch list"),
        make_key_line(":requery", "re-search & re-verify"),
        make_key_line(":settings", "open settings"),
        make_key_line(":pause-all", "pause every download"),
    ];

    let right_list: Vec<ListItem> = right_lines.into_iter().map(|l| ListItem::new(l)).collect();
    frame.render_widget(List::new(right_list), cols[1]);

    // Bottom hint
    frame.render_widget(
        Paragraph::new(Span::styled("? or esc  to close", style_hint())),
        split_left[1],
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
