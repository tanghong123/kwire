//! TUI tests (§8): drive pure reducers + render into a `TestBackend`.
//!
//! No I/O is involved: we build an `AppState` directly from a ViewModel
//! constructed from fixture data, then call `on_input` with synthetic events
//! and render into a ratatui `TestBackend` buffer.

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use libgen_core::model::{BookInput, BookRequest, DownloadList, Group};
    use libgen_engine::viewmodel::build_with_id;
    use ratatui::{backend::TestBackend, Terminal};

    use crate::app::{AppState, Focus, StatusFilter};
    use crate::intent::Intent;
    use crate::ui;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Build a minimal ViewModel with two books in one group.
    fn fixture_vm() -> libgen_engine::ViewModel {
        let mut g = Group::new("Test Group");
        g.books.push(BookRequest::new(BookInput {
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            ..Default::default()
        }));
        g.books.push(BookRequest::new(BookInput {
            title: "Anne of Green Gables".into(),
            authors: vec!["L. M. Montgomery".into()],
            ..Default::default()
        }));
        let list = DownloadList {
            title: "Test List".into(),
            settings: libgen_core::model::ListSettings::default(),
            groups: vec![g],
        };
        build_with_id("test".into(), &list)
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    // -----------------------------------------------------------------------
    // Reducer tests
    // -----------------------------------------------------------------------

    #[test]
    fn q_returns_quit() {
        let mut app = AppState::new();
        let intent = app.on_input(key(KeyCode::Char('q')));
        assert_eq!(intent, Intent::Quit);
    }

    #[test]
    fn esc_returns_quit() {
        let mut app = AppState::new();
        let intent = app.on_input(key(KeyCode::Esc));
        assert_eq!(intent, Intent::Quit);
    }

    #[test]
    fn down_moves_selection_and_returns_redraw() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        assert_eq!(app.selected, 0);

        let intent = app.on_input(key(KeyCode::Down));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(app.selected, 1, "selection should have moved down");
    }

    #[test]
    fn j_moves_selection_down() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        let _intent = app.on_input(key(KeyCode::Char('j')));
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn up_does_not_underflow() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        assert_eq!(app.selected, 0);
        let intent = app.on_input(key(KeyCode::Up));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(app.selected, 0, "selection must not go below 0");
    }

    #[test]
    fn down_clamps_at_bottom() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        // Two books in the fixture — move down twice.
        app.on_input(key(KeyCode::Down));
        app.on_input(key(KeyCode::Down));
        // A third move should clamp.
        app.on_input(key(KeyCode::Down));
        assert_eq!(app.selected, 1, "selection must not exceed last row");
    }

    #[test]
    fn tab_toggles_focus() {
        let mut app = AppState::new();
        assert_eq!(app.focus, Focus::List);
        app.on_input(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Activity);
        app.on_input(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn slash_cycles_filter() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        assert_eq!(app.filter, StatusFilter::All);
        app.on_input(key(KeyCode::Char('/')));
        assert_eq!(app.filter, StatusFilter::NeedsYou);
        app.on_input(key(KeyCode::Char('/')));
        assert_eq!(app.filter, StatusFilter::Check);
    }

    #[test]
    fn number_keys_set_filter() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.on_input(key(KeyCode::Char('5')));
        assert_eq!(app.filter, StatusFilter::InProgress);
        app.on_input(key(KeyCode::Char('1')));
        assert_eq!(app.filter, StatusFilter::All);
    }

    #[test]
    fn colon_enters_command_mode() {
        let mut app = AppState::new();
        let intent = app.on_input(key(KeyCode::Char(':')));
        assert_eq!(intent, Intent::Redraw);
        assert!(app.command_buf.is_some(), "command buf should be active");
    }

    #[test]
    fn command_mode_esc_cancels() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        let intent = app.on_input(key(KeyCode::Esc));
        assert_eq!(intent, Intent::Redraw);
        assert!(app.command_buf.is_none(), "command buf should be cleared");
    }

    #[test]
    fn command_mode_enter_emits_command_intent() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        app.on_input(key(KeyCode::Char('q')));
        app.on_input(key(KeyCode::Char('u')));
        app.on_input(key(KeyCode::Char('i')));
        app.on_input(key(KeyCode::Char('t')));
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Command("quit".into()));
        assert!(app.command_buf.is_none());
    }

    // -----------------------------------------------------------------------
    // TestBackend render tests
    // -----------------------------------------------------------------------

    #[test]
    fn render_does_not_panic_with_empty_state() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = AppState::new();
        terminal.draw(|f| ui::render(f, &app)).unwrap();
    }

    #[test]
    fn render_shows_book_title_in_buffer() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = AppState::new();
        app.set_view(fixture_vm());

        terminal.draw(|f| ui::render(f, &app)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        let content: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();

        assert!(
            content.contains("Treasure Island"),
            "buffer should contain 'Treasure Island'; got:\n{}",
            content
                .chars()
                .collect::<Vec<_>>()
                .chunks(120)
                .map(|row| row.iter().collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn render_shows_filter_label() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = AppState::new();
        app.set_view(fixture_vm());

        terminal.draw(|f| ui::render(f, &app)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        let content: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();

        assert!(
            content.contains("All"),
            "buffer should contain filter label 'All'"
        );
    }

    #[test]
    fn render_shows_list_title() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = AppState::new();
        app.set_view(fixture_vm());

        terminal.draw(|f| ui::render(f, &app)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        let content: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();

        assert!(
            content.contains("kwire"),
            "buffer should contain app name 'kwire'"
        );
        assert!(
            content.contains("Test List"),
            "buffer should contain list title 'Test List'"
        );
    }

    #[test]
    fn status_counts_correct_for_fixture() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        let counts = app.status_counts();
        // Two books, neither downloaded: total=2, all zeroes except total.
        assert_eq!(counts.total, 2);
        assert_eq!(counts.needs_you, 0);
        assert_eq!(counts.done, 0);
    }
}
