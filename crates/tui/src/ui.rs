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

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState},
    Frame,
};

use crate::app::{AppState, Focus, Modal, StatusFilter};
use crate::theme::{
    self, style_dim, style_header, style_hint, style_normal, style_selected, style_title, C_DIM,
    C_FAINT, C_NEEDS_YOU, C_TEXT,
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

    let chunks = Layout::vertical([
        Constraint::Min(0),    // spacer + content
        Constraint::Length(1), // hint bar
    ])
    .split(area);

    app.last_rects.hint_bar = chunks[1];

    let text = "Welcome to kwire\n\nNo reading list loaded.\n\nPress : and type 'import <file.md>' to get started.\n\nPress ? for help.";
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(style_dim()),
        chunks[0],
    );

    render_hint_bar(frame, app, chunks[1]);

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
    let counts = app.status_counts();
    let done = counts.done;
    let total = counts.total;

    let (title, roll_up) = if let Some(v) = &app.view {
        let t = format!(" kwire — {}  ★ {} ", v.title, v.title);
        let r = format!(" {}/{} done ", done, total);
        (t, r)
    } else {
        (" kwire ".to_string(), String::new())
    };

    let line = Line::from(vec![
        Span::styled(title, style_title()),
        Span::styled(roll_up, style_dim()),
    ]);
    frame.render_widget(Paragraph::new(line).style(style_normal()), area);
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
            .style(style_dim())
            .block(Block::default().borders(Borders::ALL).title(" Books "));
        frame.render_widget(para, area);
        return;
    }

    let header = Row::new([
        Cell::from("#").style(style_header()),
        Cell::from("Title").style(style_header()),
        Cell::from("Author").style(style_header()),
        Cell::from("Fmt").style(style_header()),
        Cell::from("Size").style(style_header()),
        Cell::from("State").style(style_header()),
        Cell::from("Progress").style(style_header()),
    ])
    .height(1)
    .style(style_header());

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
                    "not_found" => "✗ not found",
                    "needs_selection" => "● choose",
                    "queuing" | "querying" => "⠋ querying",
                    _ => "queued",
                };
                ("???".to_string(), "—".to_string(), disc.to_string(), 0u32)
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
                    "done" => "✓ done".to_string(),
                    "downloading" => format!("{} {}%", theme::spinner(app.tick), best.progress),
                    "failed" | "cancelled" => "✗ failed".to_string(),
                    "queued" => "· queued".to_string(),
                    "paused" => "⏸ paused".to_string(),
                    _ => best.state.clone(),
                };

                let size_label = if best.size > 0 {
                    format!("{} MB", best.size)
                } else {
                    "—".to_string()
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

        // Store book row rect: border(1) + header(1) = offset 2 from area.y, then
        // the visual row index (group headers shift book rows down).
        let row_rect = Rect::new(area.x, area.y + 2 + visual_row, area.width, 1);
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
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" Books "))
    .row_highlight_style(style_selected());

    frame.render_stateful_widget(table, area, &mut table_state);
}

// ---------------------------------------------------------------------------
// 3  Docked Activity pane
// ---------------------------------------------------------------------------

fn render_activity(frame: &mut Frame, app: &AppState, area: Rect) {
    let focused = app.focus == Focus::Activity;
    let border_style = if focused {
        Style::default().fg(C_TEXT)
    } else {
        Style::default().fg(C_FAINT)
    };

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
        .flat
        .iter()
        .flat_map(|fb| fb.book.versions.iter())
        .filter(|v| v.state == "queued")
        .count();

    let summary = if app.activity_expanded {
        format!(
            "▾ ACTIVITY  {} downloading · {} connecting · {} queued   tab to focus",
            downloading_count, connecting_count, queued_count
        )
    } else {
        format!(
            "▸ ACTIVITY  {} downloading · {} queued   tab to expand",
            downloading_count, queued_count
        )
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(summary)
        .border_style(border_style);

    if app.activity_expanded {
        // Group in-progress variations by host, show up to 3 active transfers.
        let in_progress: Vec<Line> = app
            .flat
            .iter()
            .filter(|fb| fb.book.versions.iter().any(|v| v.state == "downloading"))
            .take(3)
            .map(|fb| {
                let v = fb
                    .book
                    .versions
                    .iter()
                    .find(|v| v.state == "downloading")
                    .unwrap();
                let bar = theme::progress_bar(v.progress, 6);
                let host = v.host.as_deref().unwrap_or("unknown");
                let eta = v
                    .eta_secs
                    .map(|s| format!(" ETA {}s", s))
                    .unwrap_or_default();
                Line::from(vec![
                    Span::styled(format!("  {} ", theme::spinner(app.tick)), style_dim()),
                    Span::styled(fb.book.title.clone(), style_normal()),
                    Span::styled(
                        format!("  {} · {}  {}%  {}{}", v.fmt, host, v.progress, bar, eta),
                        style_dim(),
                    ),
                ])
            })
            .collect();

        let content = if in_progress.is_empty() {
            vec![Line::from(Span::styled(
                "  No active transfers.",
                style_dim(),
            ))]
        } else {
            in_progress
        };

        frame.render_widget(
            Paragraph::new(content).block(block).style(style_normal()),
            area,
        );
    } else {
        frame.render_widget(Paragraph::new("").block(block), area);
    }
}

// ---------------------------------------------------------------------------
// 4  Hint bar / command line
// ---------------------------------------------------------------------------

fn render_hint_bar(frame: &mut Frame, app: &AppState, area: Rect) {
    let content = if let Some(ref buf) = app.command_buf {
        Line::from(vec![
            Span::styled(":", style_hint()),
            Span::styled(buf.as_str(), style_hint()),
            Span::styled("█", Style::default().fg(C_TEXT)), // cursor
        ])
    } else {
        let hint = match app.focus {
            Focus::List => {
                "↑↓ move  Enter open  d detail  r retry  p pause  c cancel  o open  R reveal  ? help  : cmd  q quit"
            }
            Focus::Activity => "↑↓ scroll  tab list  q quit",
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
// 4a  Picker modal
// ---------------------------------------------------------------------------

fn render_picker_modal(
    frame: &mut Frame,
    app: &AppState,
    book_flat_index: usize,
    picker_selected: usize,
) {
    let area = centered_rect(80, 20, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Choose a copy ")
        .style(style_normal());

    let Some(fb) = app.flat.get(book_flat_index) else {
        frame.render_widget(block, area);
        return;
    };

    let inner = block.inner(area);

    // Table columns: Fmt | Title | Author | Size | Year | Pages | Score
    let header = Row::new([
        Cell::from("Fmt").style(style_header()),
        Cell::from("Title").style(style_header()),
        Cell::from("Author").style(style_header()),
        Cell::from("Size").style(style_header()),
        Cell::from("Year").style(style_header()),
        Cell::from("Pages").style(style_header()),
        Cell::from("Score").style(style_header()),
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
            let style = if is_sel {
                style_selected()
            } else {
                style_normal()
            };
            Row::new([
                Cell::from(v.fmt.clone()).style(style_dim()),
                Cell::from(v.title.clone()).style(if is_sel {
                    style_selected()
                } else {
                    style_title()
                }),
                Cell::from(v.author.clone()).style(style),
                Cell::from(if v.size > 0 {
                    format!("{} MB", v.size)
                } else {
                    "—".into()
                })
                .style(style_dim()),
                Cell::from(v.year.map(|y| y.to_string()).unwrap_or_else(|| "—".into()))
                    .style(style_dim()),
                Cell::from(v.pages.map(|p| p.to_string()).unwrap_or_else(|| "—".into()))
                    .style(style_dim()),
                Cell::from(format!("{:.2}", v.score)).style(style_dim()),
            ])
            .height(1)
            .style(style)
        })
        .collect();

    let mut table_state = TableState::default();
    if !fb.book.versions.is_empty() {
        table_state.select(Some(picker_selected));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(5), // Fmt
            Constraint::Min(16),   // Title
            Constraint::Min(12),   // Author
            Constraint::Length(8), // Size
            Constraint::Length(6), // Year
            Constraint::Length(6), // Pages
            Constraint::Length(6), // Score
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(style_selected());

    frame.render_stateful_widget(table, area, &mut table_state);

    // Hint bar at bottom of modal.
    let hint_area = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    frame.render_widget(
        Paragraph::new("⏎ download  a all preferred  esc cancel").style(style_hint()),
        hint_area,
    );
}

// ---------------------------------------------------------------------------
// 4b  Detail modal
// ---------------------------------------------------------------------------

fn render_detail_modal(frame: &mut Frame, app: &AppState, book_flat_index: usize) {
    let area = centered_rect(90, 22, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Book detail ")
        .style(style_normal());

    let Some(fb) = app.flat.get(book_flat_index) else {
        frame.render_widget(block, area);
        return;
    };

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split inner into top (variations table) and bottom (history list).
    let split = Layout::vertical([
        Constraint::Min(5),    // variations
        Constraint::Min(4),    // history
        Constraint::Length(1), // hint
    ])
    .split(inner);

    // Variations table.
    let var_header = Row::new([
        Cell::from("Fmt").style(style_header()),
        Cell::from("State").style(style_header()),
        Cell::from("Progress").style(style_header()),
        Cell::from("Path").style(style_header()),
    ])
    .height(1)
    .style(style_header());

    let var_rows: Vec<Row> = fb
        .book
        .versions
        .iter()
        .map(|v| {
            let bar = theme::progress_bar(v.progress, 8);
            Row::new([
                Cell::from(v.fmt.clone()).style(style_dim()),
                Cell::from(v.state.clone()).style(theme::style_for_state(&v.state)),
                Cell::from(format!("{}  {}", v.progress, bar)).style(style_dim()),
                Cell::from(v.output_path.clone().unwrap_or_else(|| "—".into())).style(style_dim()),
            ])
            .height(1)
        })
        .collect();

    let var_table = Table::new(
        var_rows,
        [
            Constraint::Length(5),
            Constraint::Length(12),
            Constraint::Length(14),
            Constraint::Min(20),
        ],
    )
    .header(var_header)
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .title(" Variations "),
    );

    frame.render_widget(var_table, split[0]);

    // History list.
    let history_items: Vec<ListItem> = fb
        .book
        .history
        .iter()
        .rev()
        .take(split[1].height.saturating_sub(2) as usize)
        .map(|e| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>12}ms  ", e.at_ms), style_dim()),
                Span::styled(format!("{:<12}  ", e.kind), style_normal()),
                Span::styled(e.detail.clone(), style_dim()),
            ]))
        })
        .collect();

    let history_list =
        List::new(history_items).block(Block::default().borders(Borders::NONE).title(" History "));

    frame.render_widget(history_list, split[1]);

    // Hint.
    frame.render_widget(
        Paragraph::new("o open  R reveal  r retry  esc back").style(style_hint()),
        split[2],
    );
}

// ---------------------------------------------------------------------------
// 4c  Settings modal
// ---------------------------------------------------------------------------

fn render_settings_modal(frame: &mut Frame, app: &AppState) {
    let area = centered_rect(70, 18, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Settings ")
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let settings_data: Vec<(&str, String)> = if let Some(v) = &app.view {
        let s = &v.settings;
        vec![
            ("format_pref", s.format_pref.join(", ")),
            ("language", s.language.clone()),
            ("naming_template", s.naming_template.clone()),
            ("auto_threshold", format!("{:.2}", s.auto_threshold)),
            ("near_threshold", format!("{:.2}", s.near_threshold)),
            ("seq_per_group", s.seq_per_group.to_string()),
            ("keep_top", s.keep_top.to_string()),
        ]
    } else {
        vec![("(no list loaded)", String::new())]
    };

    let split = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

    let items: Vec<ListItem> = settings_data
        .iter()
        .enumerate()
        .map(|(i, (key, val))| {
            let is_sel = i == app.settings_selected;
            let style = if is_sel {
                style_selected()
            } else {
                style_normal()
            };
            let value_display = if is_sel {
                if let Some(ref edit) = app.settings_edit {
                    format!("{}", edit)
                } else {
                    val.clone()
                }
            } else {
                val.clone()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("  {:<20}  ", key), style_dim()),
                Span::styled(value_display, style),
            ]))
        })
        .collect();

    let list = List::new(items).block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, split[0]);

    frame.render_widget(
        Paragraph::new("⏎ commit  esc close").style(style_hint()),
        split[1],
    );
}

// ---------------------------------------------------------------------------
// 4d  Help screen
// ---------------------------------------------------------------------------

fn render_help_modal(frame: &mut Frame, parent: Rect) {
    let area = centered_rect(72, 24, parent);
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help — Kwire ")
        .style(style_normal());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let keymap: &[(&str, &str)] = &[
        ("↑ / k", "Move selection up"),
        ("↓ / j", "Move selection down"),
        ("Enter", "Open detail / choose copy"),
        ("d", "Open book detail"),
        ("r", "Retry failed/not-found book"),
        ("p", "Pause download"),
        ("c", "Cancel download"),
        ("o", "Open file with system app"),
        ("R", "Reveal file in Finder"),
        ("a", "Request all preferred formats"),
        ("Tab", "Toggle focus List ↔ Activity"),
        ("/", "Cycle filter"),
        ("1–6", "Set filter directly"),
        (":", "Enter command mode"),
        ("?", "Toggle this help screen"),
        ("Esc", "Close modal / cancel command"),
        ("q", "Quit"),
        ("Ctrl-C", "Quit (unconditional)"),
    ];

    let split_h = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

    // Two-column table.
    let col_rows: Vec<Row> = keymap
        .iter()
        .map(|(key, action)| {
            Row::new([
                Cell::from(*key).style(style_title()),
                Cell::from(*action).style(style_normal()),
            ])
            .height(1)
        })
        .collect();

    let help_table = Table::new(col_rows, [Constraint::Length(12), Constraint::Min(30)]);
    frame.render_widget(help_table, split_h[0]);

    frame.render_widget(
        Paragraph::new("? or esc  close").style(style_hint()),
        split_h[1],
    );
}
