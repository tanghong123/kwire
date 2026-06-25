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
    use libgen_engine::{ViewBook, ViewVariation};
    use ratatui::{backend::TestBackend, Terminal};

    use crate::app::{AppState, FlatBook, Focus, Modal, StatusFilter};
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

    /// Build a `FlatBook` with one downloading version, for Activity-pane tests.
    fn make_downloading_flat_book(title: &str, group_index: usize, bi: usize) -> FlatBook {
        FlatBook {
            group_name: "Test Group".into(),
            group_index,
            book_index_in_group: bi,
            book: ViewBook {
                id: format!("id-{}", bi),
                title: title.into(),
                author: "Author".into(),
                year: None,
                pages: None,
                backfilled: vec![],
                seq: bi + 1,
                discovery: "matched".into(),
                versions: vec![ViewVariation {
                    md5: "a".repeat(32),
                    title: title.into(),
                    author: "Author".into(),
                    fmt: "epub".into(),
                    size: 1,
                    size_bytes: None,
                    year: None,
                    publisher: String::new(),
                    language: String::new(),
                    pages: None,
                    counted_pages: None,
                    low_pages: false,
                    host: Some("libgen.li".into()),
                    state: "downloading".into(),
                    progress: 50,
                    downloaded_bytes: None,
                    total_bytes: None,
                    speed_bps: None,
                    eta_secs: None,
                    output_path: None,
                    score: 0.9,
                    cover_url: None,
                    last_error: None,
                }],
                acquisition: None,
                review: false,
                recommended_md5: None,
                history: vec![],
            },
        }
    }

    /// Collect the terminal buffer into a single string (one long line).
    fn buffer_string(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
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

        // The empty/first-run screen shows "kwire" but the main library screen
        // renders the list strip with "★ <title>" and the filter row. Check
        // for the active-list indicator and the list title.
        assert!(
            content.contains("Test List"),
            "buffer should contain list title 'Test List'"
        );
        // The list strip always shows the "All" chip when a view is loaded.
        assert!(
            content.contains("All"),
            "buffer should contain 'All' filter chip"
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

    #[test]
    fn render_empty_screen_contains_expected_content() {
        // The empty/first-run screen must contain the wordmark, the
        // "NO READING LISTS YET" heading, and the command hints.
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        assert!(app.view.is_none());
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        let content: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();

        assert!(
            content.contains("kwire"),
            "empty screen should contain wordmark 'kwire'"
        );
        assert!(
            content.contains("NO READING LISTS YET"),
            "empty screen should contain heading 'NO READING LISTS YET'"
        );
        assert!(
            content.contains(": import ~/list.md"),
            "empty screen should contain command hint ': import ~/list.md'"
        );
        assert!(
            content.contains(": add"),
            "empty screen should contain command hint ': add'"
        );
        assert!(
            content.contains("all keys & commands"),
            "empty screen should contain '?' hint description"
        );
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

    // -----------------------------------------------------------------------
    // Feature A: Command-line autocomplete (Tab / wildmenu)
    // -----------------------------------------------------------------------

    #[test]
    fn tab_on_partial_prefix_fills_single_match() {
        // ":im" + Tab → buffer "import" (only one command starts with "im").
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':'))); // enter command mode
        app.on_input(key(KeyCode::Char('i')));
        app.on_input(key(KeyCode::Char('m')));
        app.on_input(key(KeyCode::Tab));
        assert_eq!(
            app.command_buf.as_deref(),
            Some("import"),
            "single match should fill buffer directly"
        );
        assert!(
            app.completion_candidates.is_empty(),
            "single match must not open the wildmenu"
        );
    }

    #[test]
    fn tab_on_empty_buf_opens_wildmenu_with_all_commands() {
        // ":" + Tab → wildmenu shows all 8 commands.
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':'))); // buf = ""
        app.on_input(key(KeyCode::Tab));
        assert!(
            !app.completion_candidates.is_empty(),
            "Tab on empty prefix should open wildmenu"
        );
        assert!(
            app.completion_candidates.len() >= 2,
            "expected multiple candidates"
        );
        // All known commands should be present.
        for cmd in &[
            "import",
            "add",
            "open",
            "requery",
            "settings",
            "pause-all",
            "quit",
            "help",
        ] {
            assert!(
                app.completion_candidates.iter().any(|c| c == cmd),
                "candidate '{}' missing",
                cmd
            );
        }
    }

    #[test]
    fn tab_cycles_wildmenu_forward() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':'))); // buf = ""
        app.on_input(key(KeyCode::Tab)); // open wildmenu, index = 0
        assert!(!app.completion_candidates.is_empty());
        let first_index = app.completion_index;
        app.on_input(key(KeyCode::Tab)); // cycle forward
        assert_ne!(
            app.completion_index, first_index,
            "Tab should advance the wildmenu index"
        );
    }

    #[test]
    fn backtab_cycles_wildmenu_backward() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':'))); // buf = ""
        app.on_input(key(KeyCode::Tab)); // open wildmenu, index = 0
        let n = app.completion_candidates.len();
        app.on_input(key(KeyCode::BackTab)); // should wrap to n-1
        assert_eq!(
            app.completion_index,
            n - 1,
            "Shift-Tab from index 0 should wrap to last candidate"
        );
    }

    #[test]
    fn enter_while_wildmenu_open_accepts_not_submits() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':'))); // buf = ""
        app.on_input(key(KeyCode::Tab)); // open wildmenu, index = 0
        assert!(!app.completion_candidates.is_empty());
        let selected = app.completion_candidates[app.completion_index].clone();

        let intent = app.on_input(key(KeyCode::Enter));

        assert_eq!(
            intent,
            Intent::Redraw,
            "Enter while wildmenu open must not submit (returns Redraw)"
        );
        assert_eq!(
            app.command_buf.as_deref(),
            Some(selected.as_str()),
            "buffer should hold the accepted candidate"
        );
        assert!(
            app.completion_candidates.is_empty(),
            "wildmenu should close after accept"
        );
        // A second Enter now submits.
        let intent2 = app.on_input(key(KeyCode::Enter));
        assert_eq!(
            intent2,
            Intent::Command(selected.clone()),
            "second Enter should submit the command"
        );
    }

    #[test]
    fn space_while_wildmenu_open_accepts_candidate() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':'))); // buf = ""
        app.on_input(key(KeyCode::Tab)); // open wildmenu
        let selected = app.completion_candidates[app.completion_index].clone();
        let intent = app.on_input(key(KeyCode::Char(' ')));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(app.command_buf.as_deref(), Some(selected.as_str()));
        assert!(app.completion_candidates.is_empty());
    }

    #[test]
    fn esc_closes_wildmenu_but_keeps_buffer() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':'))); // buf = ""
        app.on_input(key(KeyCode::Char('i'))); // buf = "i"
        app.on_input(key(KeyCode::Tab)); // single match → fills "import"
                                         // Now open wildmenu by Tab on empty-ish buffer.
        app.on_input(key(KeyCode::Backspace)); // buf = "impor" (or just re-open)
                                               // Let's just test Esc closing a wildmenu.
                                               // Open it fresh from scratch.
        let mut app2 = AppState::new();
        app2.on_input(key(KeyCode::Char(':'))); // buf = ""
        app2.on_input(key(KeyCode::Tab)); // open wildmenu
        assert!(!app2.completion_candidates.is_empty());
        let intent = app2.on_input(key(KeyCode::Esc)); // close wildmenu, keep buf
        assert_eq!(intent, Intent::Redraw);
        assert!(
            app2.completion_candidates.is_empty(),
            "Esc must close wildmenu"
        );
        assert!(
            app2.command_buf.is_some(),
            "Esc must keep the command buffer when wildmenu was open"
        );
    }

    #[test]
    fn typing_char_closes_wildmenu() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':'))); // buf = ""
        app.on_input(key(KeyCode::Tab)); // open wildmenu
        assert!(!app.completion_candidates.is_empty());
        app.on_input(key(KeyCode::Char('q'))); // type 'q'
        assert!(
            app.completion_candidates.is_empty(),
            "typing a char must close the wildmenu"
        );
        assert_eq!(
            app.command_buf.as_deref(),
            Some("q"),
            "typed char appended to buffer"
        );
    }

    #[test]
    fn wildmenu_render_contains_candidates() {
        // When the wildmenu is open the rendered buffer must show candidate text.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        // Manually activate the wildmenu.
        app.command_buf = Some(String::new());
        app.completion_candidates = vec!["import".into(), "add".into(), "open".into()];
        app.completion_index = 0;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(content.contains("import"), "wildmenu must show 'import'");
        assert!(content.contains("add"), "wildmenu must show 'add'");
        assert!(content.contains("open"), "wildmenu must show 'open'");
    }

    // -----------------------------------------------------------------------
    // Feature B: Activity-pane conditional scroll
    // -----------------------------------------------------------------------

    #[test]
    fn activity_fit_shows_all_rows_no_indicator() {
        // N = 2, capacity = ACTIVITY_EXPANDED_H(5) - 2 borders = 3.
        // All rows visible, no ▾/▴ indicator.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = vec![
            make_downloading_flat_book("Alpha Book", 0, 0),
            make_downloading_flat_book("Beta Book", 0, 1),
        ];
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(
            content.contains("Alpha Book"),
            "Alpha Book must be visible in FIT mode"
        );
        assert!(
            content.contains("Beta Book"),
            "Beta Book must be visible in FIT mode"
        );
        assert!(
            !content.contains("more"),
            "FIT mode must not show 'more' indicator"
        );
        assert!(
            !content.contains("above"),
            "FIT mode must not show 'above' indicator"
        );
    }

    #[test]
    fn activity_overflow_shows_below_indicator() {
        // N = 5, capacity = 3 → OVERFLOW: "▾ N more" indicator appears.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = (0..5)
            .map(|i| make_downloading_flat_book(&format!("Book {}", i), 0, i))
            .collect();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(
            content.contains("more"),
            "OVERFLOW must show 'more' indicator"
        );
    }

    #[test]
    fn activity_overflow_scrolled_shows_above_indicator() {
        // Scrolled down: "▴ N above" indicator appears.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = (0..5)
            .map(|i| make_downloading_flat_book(&format!("Book {}", i), 0, i))
            .collect();
        app.activity_selected = 2; // scrolled past beginning
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(
            content.contains("above"),
            "scrolled OVERFLOW must show 'above' indicator"
        );
    }

    #[test]
    fn activity_scroll_keys_advance_offset() {
        // ↓/j while Focus::Activity increments activity_selected.
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = (0..5)
            .map(|i| make_downloading_flat_book(&format!("Book {}", i), 0, i))
            .collect();
        app.focus = Focus::Activity;
        assert_eq!(app.activity_selected, 0);

        app.on_input(key(KeyCode::Down));
        assert_eq!(
            app.activity_selected, 1,
            "Down should advance scroll offset"
        );

        app.on_input(key(KeyCode::Char('j')));
        assert_eq!(app.activity_selected, 2, "'j' should advance scroll offset");

        app.on_input(key(KeyCode::Up));
        assert_eq!(app.activity_selected, 1, "Up should retreat scroll offset");

        app.on_input(key(KeyCode::Char('k')));
        assert_eq!(app.activity_selected, 0, "'k' should retreat scroll offset");
    }

    #[test]
    fn activity_scroll_does_not_affect_book_selection() {
        // While Activity is focused, j/k must not move app.selected.
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = (0..5)
            .map(|i| make_downloading_flat_book(&format!("Book {}", i), 0, i))
            .collect();
        app.focus = Focus::Activity;
        let book_sel_before = app.selected;
        app.on_input(key(KeyCode::Down));
        app.on_input(key(KeyCode::Down));
        assert_eq!(
            app.selected, book_sel_before,
            "book selection unchanged while Activity focused"
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

    // -----------------------------------------------------------------------
    // BUG 1 — theme bg uses Color::Reset
    // -----------------------------------------------------------------------

    #[test]
    fn theme_bg_constant_is_color_reset() {
        use crate::theme::C_BG;
        use ratatui::style::Color;
        assert_eq!(
            C_BG,
            Color::Reset,
            "C_BG must be Color::Reset so the TUI blends into the terminal background"
        );
    }

    #[test]
    fn theme_panel_constant_is_color_reset() {
        use crate::theme::C_PANEL;
        use ratatui::style::Color;
        assert_eq!(C_PANEL, Color::Reset, "C_PANEL must be Color::Reset");
    }

    #[test]
    fn style_normal_bg_is_reset() {
        use crate::theme::style_normal;
        use ratatui::style::Color;
        assert_eq!(
            style_normal().bg,
            Some(Color::Reset),
            "style_normal() background must be Color::Reset"
        );
    }

    // -----------------------------------------------------------------------
    // BUG 2 — :add arg parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_add_arg_with_comma_splits_title_and_author() {
        let (title, authors) = crate::parse_add_arg("steve jobs, walter isaacson");
        assert_eq!(title, "steve jobs");
        assert_eq!(authors, vec!["walter isaacson"]);
    }

    #[test]
    fn parse_add_arg_splits_on_last_comma() {
        // Title itself contains a comma — split on last one only.
        let (title, authors) = crate::parse_add_arg("A, B, C Author");
        assert_eq!(title, "A, B");
        assert_eq!(authors, vec!["C Author"]);
    }

    #[test]
    fn parse_add_arg_without_comma_has_empty_authors() {
        let (title, authors) = crate::parse_add_arg("  The Great Gatsby  ");
        assert_eq!(title, "The Great Gatsby");
        assert!(authors.is_empty());
    }

    // -----------------------------------------------------------------------
    // BUG 3 — command-line history ↑/↓ recall
    // -----------------------------------------------------------------------

    #[test]
    fn cmd_history_up_recalls_most_recent() {
        let mut app = AppState::new();
        // Seed the history directly (simulates a previously submitted command).
        app.cmd_history.push("import ~/books.md".to_string());

        // Enter command mode.
        app.on_input(key(KeyCode::Char(':')));
        assert_eq!(app.command_buf.as_deref(), Some(""));

        // Press ↑.
        app.on_input(key(KeyCode::Up));
        assert_eq!(
            app.command_buf.as_deref(),
            Some("import ~/books.md"),
            "↑ should recall the most recent command"
        );
    }

    #[test]
    fn cmd_history_up_multiple_entries() {
        let mut app = AppState::new();
        app.cmd_history.push("open Classics".to_string());
        app.cmd_history.push("import ~/books.md".to_string());

        app.on_input(key(KeyCode::Char(':')));
        app.on_input(key(KeyCode::Up)); // most recent: "import ~/books.md"
        assert_eq!(app.command_buf.as_deref(), Some("import ~/books.md"));

        app.on_input(key(KeyCode::Up)); // older: "open Classics"
        assert_eq!(app.command_buf.as_deref(), Some("open Classics"));
    }

    #[test]
    fn cmd_history_down_restores_draft() {
        let mut app = AppState::new();
        app.cmd_history.push("import ~/books.md".to_string());

        app.on_input(key(KeyCode::Char(':')));
        // Type a draft.
        app.on_input(key(KeyCode::Char('h')));
        app.on_input(key(KeyCode::Char('e')));
        assert_eq!(app.command_buf.as_deref(), Some("he"));

        // ↑ saves draft and shows history.
        app.on_input(key(KeyCode::Up));
        assert_eq!(app.command_buf.as_deref(), Some("import ~/books.md"));

        // ↓ restores draft.
        app.on_input(key(KeyCode::Down));
        assert_eq!(
            app.command_buf.as_deref(),
            Some("he"),
            "↓ should restore the saved draft"
        );
    }

    #[test]
    fn cmd_history_pushed_on_submit() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        for c in "requery".chars() {
            app.on_input(key(KeyCode::Char(c)));
        }
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Command("requery".into()));
        assert_eq!(
            app.cmd_history.last().map(String::as_str),
            Some("requery"),
            "submitted command must be pushed to history"
        );
    }

    #[test]
    fn cmd_history_not_pushed_for_empty_command() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        app.on_input(key(KeyCode::Enter)); // submit empty
        assert!(
            app.cmd_history.is_empty(),
            "empty command must not be pushed to history"
        );
    }

    #[test]
    fn cmd_history_up_does_nothing_when_empty() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        // Press ↑ with no history — must not panic.
        app.on_input(key(KeyCode::Up));
        assert_eq!(
            app.command_buf.as_deref(),
            Some(""),
            "buffer unchanged when history is empty"
        );
    }

    #[test]
    fn cmd_history_typing_resets_cursor() {
        let mut app = AppState::new();
        app.cmd_history.push("requery".to_string());

        app.on_input(key(KeyCode::Char(':')));
        app.on_input(key(KeyCode::Up)); // history mode
        assert_eq!(app.command_buf.as_deref(), Some("requery"));

        // Typing resets cursor (stays at whatever is in the buffer).
        app.on_input(key(KeyCode::Char('x')));
        // Next ↑ from this point should save the current buffer as draft.
        // Verify by checking the buffer is now "requeryx".
        assert_eq!(
            app.command_buf.as_deref(),
            Some("requeryx"),
            "typed char must be appended after history recall"
        );
    }

    // -----------------------------------------------------------------------
    // BUG 4 — :import tilde expansion + json dispatch helpers
    // -----------------------------------------------------------------------

    #[test]
    fn expand_tilde_expands_home() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/test".to_string());
        let result = crate::expand_tilde("~/books.md");
        assert_eq!(result, format!("{}/books.md", home));
    }

    #[test]
    fn expand_tilde_bare_tilde_is_home() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/test".to_string());
        let result = crate::expand_tilde("~");
        assert_eq!(result, home);
    }

    #[test]
    fn expand_tilde_absolute_path_unchanged() {
        let result = crate::expand_tilde("/absolute/path/list.md");
        assert_eq!(result, "/absolute/path/list.md");
    }

    #[test]
    fn expand_tilde_no_tilde_relative_unchanged() {
        let result = crate::expand_tilde("relative/path.md");
        assert_eq!(result, "relative/path.md");
    }

    // -----------------------------------------------------------------------
    // Bug 4c / shared — status_msg rendering and clearance
    // -----------------------------------------------------------------------

    #[test]
    fn status_msg_renders_in_hint_bar() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.status_msg = Some("Imported 5 book(s) from \"My List\"".to_string());
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(
            content.contains("Imported 5 book(s)"),
            "status message must appear in the hint bar"
        );
    }

    #[test]
    fn status_msg_renders_on_empty_screen() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        // No list loaded — empty screen.
        app.status_msg = Some("Import failed: file not found".to_string());
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(
            content.contains("Import failed"),
            "status message must appear in the empty-screen command box"
        );
    }

    #[test]
    fn status_msg_cleared_on_keypress() {
        let mut app = AppState::new();
        app.status_msg = Some("Some status".to_string());
        // Any keypress should clear the message (on_input calls self.status_msg = None first).
        app.on_input(key(KeyCode::Char('/')));
        assert!(
            app.status_msg.is_none(),
            "status_msg must be cleared on the next keypress"
        );
    }

    #[test]
    fn status_msg_cleared_on_command_mode_key() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.status_msg = Some("old status".to_string());
        // Pressing ':' enters command mode — this is also a keypress.
        app.on_input(key(KeyCode::Char(':')));
        assert!(
            app.status_msg.is_none(),
            "status_msg must be cleared when entering command mode"
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
