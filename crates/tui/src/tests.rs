//! TUI tests (§8): drive pure reducers + render into a `TestBackend`.
//!
//! No I/O is involved: we build an `AppState` directly from a ViewModel
//! constructed from fixture data, then call `on_input` with synthetic events
//! and render into a ratatui `TestBackend` buffer.

#[cfg(test)]
mod tests {
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton,
        MouseEvent, MouseEventKind,
    };
    use libgen_core::model::{BookInput, BookRequest, DownloadList, Group};
    use libgen_engine::viewmodel::build_with_id;
    use ratatui::{backend::TestBackend, Terminal};

    use crate::app::{AppState, Focus, Modal, StatusFilter};
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

    /// Build a ViewModel where the first book has `needs_selection` discovery.
    fn fixture_vm_needs_selection() -> libgen_engine::ViewModel {
        use libgen_core::model::{Candidate, Format, RequestStatus};
        let mut g = Group::new("Test Group");
        let mut req = BookRequest::new(BookInput {
            title: "Ambiguous Book".into(),
            authors: vec!["Unknown".into()],
            ..Default::default()
        });
        req.status = RequestStatus::NeedsSelection;
        // Add two candidate variations so the picker has rows.
        req.candidates = vec![
            Candidate {
                md5: "a".repeat(32),
                title: "Ambiguous Book".into(),
                authors: vec!["Unknown".into()],
                year: Some(2000),
                publisher: Some("Pub A".into()),
                language: Some("English".into()),
                pages: Some(100),
                extension: Some(Format::Epub),
                size_bytes: Some(1024 * 1024),
                source_host: Some("libgen.li".into()),
                cover_url: None,
                score: 0.9,
                job: None,
            },
            Candidate {
                md5: "b".repeat(32),
                title: "Ambiguous Book (alt)".into(),
                authors: vec!["Unknown".into()],
                year: Some(2001),
                publisher: Some("Pub B".into()),
                language: Some("English".into()),
                pages: Some(200),
                extension: Some(Format::Pdf),
                size_bytes: Some(2 * 1024 * 1024),
                source_host: Some("libgen.li".into()),
                cover_url: None,
                score: 0.8,
                job: None,
            },
        ];
        g.books.push(req);
        g.books.push(BookRequest::new(BookInput {
            title: "Normal Book".into(),
            authors: vec!["Author B".into()],
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

    fn mouse_left_click(column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    // -----------------------------------------------------------------------
    // Reducer tests — Stage 2
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
    // Stage 3 reducer tests
    // -----------------------------------------------------------------------

    #[test]
    fn enter_on_needs_selection_opens_picker() {
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        // First book has needs_selection discovery.
        assert_eq!(app.flat[0].book.discovery, "needs_selection");
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::OpenPicker { flat_index: 0 });
    }

    #[test]
    fn enter_on_normal_book_opens_detail() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        // Normal books have "queued" discovery (not needs_selection).
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::OpenDetail { flat_index: 0 });
    }

    #[test]
    fn esc_closes_modal() {
        let mut app = AppState::new();
        app.modal = Some(Modal::Help);
        let intent = app.on_input(key(KeyCode::Esc));
        assert_eq!(intent, Intent::Redraw);
        assert!(app.modal.is_none(), "modal should be closed");
    }

    #[test]
    fn d_opens_detail() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        let intent = app.on_input(key(KeyCode::Char('d')));
        assert_eq!(intent, Intent::OpenDetail { flat_index: 0 });
    }

    #[test]
    fn question_mark_returns_open_help() {
        // `?` is a pure intent: the event loop opens the modal. Asserting the
        // intent keeps `on_input` side-effect-light and matches `:help`.
        let mut app = AppState::new();
        let intent = app.on_input(key(KeyCode::Char('?')));
        assert_eq!(intent, Intent::OpenHelp);
    }

    #[test]
    fn activity_scroll_uses_focus() {
        // When the Activity pane is focused, j/k scroll it instead of moving the
        // book-list selection. With no in-flight transfers it stays clamped at 0.
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Activity;
        app.on_input(key(KeyCode::Char('j')));
        assert_eq!(
            app.selected, 0,
            "book selection unchanged while Activity focused"
        );
        assert_eq!(app.activity_selected, 0, "no transfers → clamped at 0");
    }

    #[test]
    fn r_returns_retry_intent() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        let intent = app.on_input(key(KeyCode::Char('r')));
        assert_eq!(
            intent,
            Intent::Retry {
                group_path: vec![0],
                book_index: 0,
            }
        );
    }

    // -----------------------------------------------------------------------
    // TestBackend render tests — Stage 2
    // -----------------------------------------------------------------------

    #[test]
    fn render_does_not_panic_with_empty_state() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn render_shows_book_title_in_buffer() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = AppState::new();
        app.set_view(fixture_vm());

        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

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

        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

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

        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

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

    // -----------------------------------------------------------------------
    // Stage 3 render tests
    // -----------------------------------------------------------------------

    #[test]
    fn render_picker_modal_does_not_panic() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.modal = Some(Modal::Picker {
            book_flat_index: 0,
            selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn render_detail_modal_does_not_panic() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Detail { book_flat_index: 0 });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn render_help_modal_does_not_panic() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Help);
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn render_settings_modal_does_not_panic() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Settings);
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn render_empty_screen_does_not_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        // No view loaded — should render the empty/first-run screen.
        assert!(app.view.is_none());
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    // -----------------------------------------------------------------------
    // Stage 4: engine wiring tests
    // -----------------------------------------------------------------------

    #[test]
    fn apply_progress_bytes_populates_transfer() {
        use libgen_core::queue::Progress;
        let mut app = AppState::new();
        app.apply_progress(&Progress::Bytes {
            md5: "a".repeat(32),
            leg_id: 0,
            is_hedge: false,
            host: "libgen.li".into(),
            bytes_done: 1024,
            total_bytes: Some(2048),
            speed_bps: Some(512),
            eta_secs: Some(2),
        });
        let t = app
            .transfers
            .get(&"a".repeat(32))
            .expect("transfer should be present");
        assert_eq!(t.host, "libgen.li");
        assert_eq!(t.bytes_done, 1024);
        assert_eq!(t.eta_secs, Some(2));
    }

    #[test]
    fn apply_progress_done_removes_transfer() {
        use libgen_core::queue::Progress;
        let mut app = AppState::new();
        let md5 = "b".repeat(32);
        app.apply_progress(&Progress::Bytes {
            md5: md5.clone(),
            leg_id: 0,
            is_hedge: false,
            host: "host".into(),
            bytes_done: 100,
            total_bytes: None,
            speed_bps: None,
            eta_secs: None,
        });
        assert!(app.transfers.contains_key(&md5));
        app.apply_progress(&Progress::Done {
            md5: md5.clone(),
            host: "host".into(),
            path: std::path::PathBuf::from("/tmp/test.epub"),
            bytes_written: 100,
        });
        assert!(
            !app.transfers.contains_key(&md5),
            "Done should remove transfer"
        );
    }

    #[test]
    fn engine_wiring_select_intent_from_picker() {
        // Verify that the Picker modal produces a Select intent when Enter is pressed.
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
        };
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        // Open the picker.
        app.modal = Some(Modal::Picker {
            book_flat_index: 0,
            selected: 0,
        });
        let intent = app.on_input(Event::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }));
        // Should produce Select with the first variation's md5.
        assert!(
            matches!(intent, Intent::Select { .. }),
            "Enter in picker should produce Select intent, got: {:?}",
            intent
        );
    }

    #[test]
    fn mouse_click_selects_book_row() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());

        // Move to second book first.
        app.selected = 1;

        // Do a render to populate last_rects.book_rows.
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        // The first book row should be at approximately y=4 in our layout:
        // row 0=list strip, 1=filter, 2=border, 3=header, 4=first data row.
        // The book_rows rects are stored during render_book_table.
        // Click on the first book row.
        if let Some((rect, _)) = app.last_rects.book_rows.first().cloned() {
            let intent = app.on_input(mouse_left_click(rect.x + 5, rect.y));
            assert_eq!(intent, Intent::Redraw);
            assert_eq!(app.selected, 0, "clicking first book row should select it");
        } else {
            // If no rows were stored (e.g. all books filtered), skip.
            assert!(
                !app.flat.is_empty(),
                "flat list should not be empty in this test"
            );
        }
    }
}
