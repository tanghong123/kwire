//! ratatui render pass — stateless; derives everything from [`AppState`].
//!
//! The layout follows §4 exactly:
//! ```
//! Length(1)       — list strip
//! Length(1)       — status-filter row
//! Min(8)          — book Table
//! Length(N)       — docked Activity pane  (N=1 collapsed, N=5 expanded)
//! Length(1)       — key-hint bar / command line
//! ```
//!
//! Stage 2 uses placeholders for the list strip and Activity pane; the book
//! Table renders live from the ViewModel.

use ratatui::{
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame,
};

use crate::app::{AppState, Focus};
use crate::theme::{
    self, style_dim, style_header, style_hint, style_normal, style_selected, style_title, C_DIM,
    C_FAINT, C_NEEDS_YOU, C_TEXT,
};

const ACTIVITY_EXPANDED_H: u16 = 5;
const ACTIVITY_COLLAPSED_H: u16 = 1;

/// Single entry point: render the full UI from `app` into `frame`.
pub fn render(frame: &mut Frame, app: &AppState) {
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

    render_list_strip(frame, app, chunks[0]);
    render_filter_row(frame, app, chunks[1]);
    render_book_table(frame, app, chunks[2]);
    render_activity(frame, app, chunks[3]);
    render_hint_bar(frame, app, chunks[4]);
}

// ---------------------------------------------------------------------------
// 0  List strip
// ---------------------------------------------------------------------------

fn render_list_strip(frame: &mut Frame, app: &AppState, area: ratatui::layout::Rect) {
    let title = app
        .view
        .as_ref()
        .map(|v| format!(" kwire — {} ", v.title))
        .unwrap_or_else(|| " kwire ".to_string());

    let line = Line::from(vec![
        Span::styled(title, style_title()),
        Span::styled(" (no lists loaded) ", style_dim()),
    ]);
    frame.render_widget(Paragraph::new(line).style(style_normal()), area);
}

// ---------------------------------------------------------------------------
// 1  Status-filter row
// ---------------------------------------------------------------------------

fn render_filter_row(frame: &mut Frame, app: &AppState, area: ratatui::layout::Rect) {
    let counts = app.status_counts();
    let active_filter = app.filter;

    let chips: Vec<Span> = [
        (
            crate::app::StatusFilter::All,
            format!("All {}", counts.total),
        ),
        (
            crate::app::StatusFilter::NeedsYou,
            format!("Needs you {}", counts.needs_you),
        ),
        (
            crate::app::StatusFilter::Check,
            format!("Check {}", counts.check),
        ),
        (
            crate::app::StatusFilter::Cannot,
            format!("Cannot {}", counts.cannot),
        ),
        (
            crate::app::StatusFilter::InProgress,
            format!("In progress {}", counts.in_progress),
        ),
        (
            crate::app::StatusFilter::Done,
            format!("Done {}", counts.done),
        ),
    ]
    .into_iter()
    .flat_map(|(filter, label)| {
        let style = if filter == active_filter {
            Style::default()
                .fg(C_TEXT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(C_DIM)
        };
        vec![
            Span::styled(format!(" {} ", label), style),
            Span::styled("  ", Style::default().fg(C_FAINT)),
        ]
    })
    .collect();

    frame.render_widget(
        Paragraph::new(Line::from(chips)).style(style_normal()),
        area,
    );
}

// ---------------------------------------------------------------------------
// 2  Book table
// ---------------------------------------------------------------------------

fn render_book_table(frame: &mut Frame, app: &AppState, area: ratatui::layout::Rect) {
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

    for (i, fb) in app.flat.iter().enumerate() {
        let book = &fb.book;
        let is_selected = i == app.selected && app.focus == Focus::List;

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
    }

    let mut table_state = TableState::default();
    if !app.flat.is_empty() {
        table_state.select(Some(app.selected));
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

fn render_activity(frame: &mut Frame, app: &AppState, area: ratatui::layout::Rect) {
    let focused = app.focus == Focus::Activity;
    let border_style = if focused {
        Style::default().fg(C_TEXT)
    } else {
        Style::default().fg(C_FAINT)
    };

    let summary = if app.activity_expanded {
        "▾ ACTIVITY  (tab to focus)"
    } else {
        "▸ ACTIVITY  (tab to expand)"
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(summary)
        .border_style(border_style);

    if app.activity_expanded {
        // Stage 2 placeholder: list any in-progress books from the flat list.
        let in_progress: Vec<Line> = app
            .flat
            .iter()
            .filter(|fb| fb.book.versions.iter().any(|v| v.state == "downloading"))
            .take(3) // show up to 3 active transfers
            .map(|fb| {
                let v = fb
                    .book
                    .versions
                    .iter()
                    .find(|v| v.state == "downloading")
                    .unwrap();
                let bar = theme::progress_bar(v.progress, 6);
                Line::from(vec![
                    Span::styled(format!("  {} ", theme::spinner(app.tick)), style_dim()),
                    Span::styled(fb.book.title.clone(), style_normal()),
                    Span::styled(
                        format!("  {}  {}%  {}", v.fmt, v.progress, bar),
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

fn render_hint_bar(frame: &mut Frame, app: &AppState, area: ratatui::layout::Rect) {
    let content = if let Some(ref buf) = app.command_buf {
        Line::from(vec![
            Span::styled(":", style_hint()),
            Span::styled(buf.as_str(), style_hint()),
            Span::styled("█", Style::default().fg(C_TEXT)), // cursor
        ])
    } else {
        let hint = match app.focus {
            Focus::List => "↑↓ move  / filter  1-6 filter  : command  tab downloads  q quit",
            Focus::Activity => "↑↓ scroll  tab list  q quit",
        };
        Line::from(Span::styled(hint, style_hint()))
    };

    frame.render_widget(Paragraph::new(content).style(style_hint()), area);
}
