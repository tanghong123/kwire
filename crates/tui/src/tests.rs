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

    use crate::app::{
        build_format_editor_rows, settings_field_kind, ActiveTransfer, AppState, FlatBook, Focus,
        Modal, SettingsDraft, SettingsEditor, SettingsFieldKind, StatusFilter,
        FORMAT_EDITOR_FORMATS, SETTINGS_FIELD_COUNT,
    };
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

    // Updated: Tab now cycles 3 panes: List → Activity → Header → List.
    #[test]
    fn tab_cycles_three_panes_forward() {
        let mut app = AppState::new();
        assert_eq!(app.focus, Focus::List);
        app.on_input(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Activity, "Tab: List → Activity");
        app.on_input(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Header, "Tab: Activity → Header");
        app.on_input(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::List, "Tab: Header → List (wrap)");
    }

    #[test]
    fn backtab_cycles_three_panes_reverse() {
        let mut app = AppState::new();
        assert_eq!(app.focus, Focus::List);
        app.on_input(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Header, "Shift-Tab: List → Header");
        app.on_input(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Activity, "Shift-Tab: Header → Activity");
        app.on_input(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::List, "Shift-Tab: Activity → List (wrap)");
    }

    // -----------------------------------------------------------------------
    // #44 / #65 / #66 — 3-pane focus model, global list cycle, arrow-cross
    // -----------------------------------------------------------------------

    /// `[` / `]` switch the active list from ANY pane without changing focus.
    #[test]
    fn bracket_list_cycle_from_list_focus() {
        let mut app = AppState::new();
        app.all_lists = vec![
            crate::app::ListSummary {
                id: "L1".into(),
                title: "List 1".into(),
                done: 0,
                total: 1,
            },
            crate::app::ListSummary {
                id: "L2".into(),
                title: "List 2".into(),
                done: 0,
                total: 1,
            },
        ];
        app.active_list_idx = 0;
        assert_eq!(app.focus, Focus::List);

        // `]` from List focus → next list, focus stays List.
        let intent = app.on_input(key(KeyCode::Char(']')));
        assert!(
            matches!(intent, Intent::SwitchList { ref id } if id == "L2"),
            "expected SwitchList L2, got {:?}",
            intent
        );
        assert_eq!(
            app.focus,
            Focus::List,
            "focus must not change on ] from List"
        );
        assert_eq!(app.active_list_idx, 1);

        // `[` from List focus → prev list, focus stays List.
        let intent2 = app.on_input(key(KeyCode::Char('[')));
        assert!(
            matches!(intent2, Intent::SwitchList { ref id } if id == "L1"),
            "expected SwitchList L1, got {:?}",
            intent2
        );
        assert_eq!(
            app.focus,
            Focus::List,
            "focus must not change on [ from List"
        );
        assert_eq!(app.active_list_idx, 0);
    }

    #[test]
    fn bracket_list_cycle_from_activity_focus() {
        let mut app = AppState::new();
        app.all_lists = vec![
            crate::app::ListSummary {
                id: "L1".into(),
                title: "List 1".into(),
                done: 0,
                total: 1,
            },
            crate::app::ListSummary {
                id: "L2".into(),
                title: "List 2".into(),
                done: 0,
                total: 1,
            },
        ];
        app.active_list_idx = 0;
        app.focus = Focus::Activity;

        // `]` from Activity focus → next list, focus stays Activity.
        let intent = app.on_input(key(KeyCode::Char(']')));
        assert!(matches!(intent, Intent::SwitchList { .. }));
        assert_eq!(
            app.focus,
            Focus::Activity,
            "focus must not change on ] from Activity"
        );
    }

    #[test]
    fn bracket_list_cycle_from_header_focus() {
        let mut app = AppState::new();
        app.all_lists = vec![
            crate::app::ListSummary {
                id: "L1".into(),
                title: "List 1".into(),
                done: 0,
                total: 1,
            },
            crate::app::ListSummary {
                id: "L2".into(),
                title: "List 2".into(),
                done: 0,
                total: 1,
            },
        ];
        app.active_list_idx = 0;
        app.focus = Focus::Header;

        let intent = app.on_input(key(KeyCode::Char(']')));
        assert!(matches!(intent, Intent::SwitchList { .. }));
        assert_eq!(
            app.focus,
            Focus::Header,
            "focus must not change on ] from Header"
        );
    }

    /// `←/→` navigate filter chips ONLY when Header is focused.
    #[test]
    fn left_right_filter_chips_header_pane_only() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        assert_eq!(app.filter, StatusFilter::All);

        // When List is focused, ←/→ must be no-ops.
        app.focus = Focus::List;
        app.on_input(key(KeyCode::Right));
        assert_eq!(
            app.filter,
            StatusFilter::All,
            "→ in List pane must not move filter"
        );
        app.on_input(key(KeyCode::Left));
        assert_eq!(
            app.filter,
            StatusFilter::All,
            "← in List pane must not move filter"
        );

        // When Activity is focused, also no-op.
        app.focus = Focus::Activity;
        app.on_input(key(KeyCode::Right));
        assert_eq!(
            app.filter,
            StatusFilter::All,
            "→ in Activity pane must not move filter"
        );

        // When Header is focused, → moves the chip.
        app.focus = Focus::Header;
        app.on_input(key(KeyCode::Right));
        assert_eq!(
            app.filter,
            StatusFilter::NeedsYou,
            "→ in Header pane must advance filter"
        );
        app.on_input(key(KeyCode::Left));
        assert_eq!(
            app.filter,
            StatusFilter::All,
            "← in Header pane must retreat filter"
        );
    }

    /// `↓` at the bottom of the book list crosses focus into Activity.
    #[test]
    fn down_at_list_bottom_crosses_to_activity() {
        let mut app = AppState::new();
        app.set_view(fixture_vm()); // 2 books
        assert_eq!(app.focus, Focus::List);
        // Move to the last book.
        app.selected = app.flat.len() - 1;
        // ↓ at the bottom should cross to Activity.
        app.on_input(key(KeyCode::Down));
        assert_eq!(
            app.focus,
            Focus::Activity,
            "↓ at list bottom must focus Activity"
        );
        assert_eq!(
            app.activity_selected, 0,
            "Activity selection resets to 0 on cross"
        );
    }

    #[test]
    fn j_at_list_bottom_crosses_to_activity() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.selected = app.flat.len() - 1;
        app.on_input(key(KeyCode::Char('j')));
        assert_eq!(
            app.focus,
            Focus::Activity,
            "'j' at list bottom must focus Activity"
        );
    }

    /// `↑` at the top of Activity crosses focus back into List.
    #[test]
    fn up_at_activity_top_crosses_to_list() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Activity;
        app.activity_selected = 0;
        app.on_input(key(KeyCode::Up));
        assert_eq!(app.focus, Focus::List, "↑ at activity top must focus List");
    }

    #[test]
    fn k_at_activity_top_crosses_to_list() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Activity;
        app.activity_selected = 0;
        app.on_input(key(KeyCode::Char('k')));
        assert_eq!(
            app.focus,
            Focus::List,
            "'k' at activity top must focus List"
        );
    }

    /// `↑` at a non-zero Activity position scrolls without crossing.
    #[test]
    fn up_in_activity_non_top_does_not_cross() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = (0..5)
            .map(|i| make_downloading_flat_book(&format!("Book {}", i), 0, i))
            .collect();
        app.focus = Focus::Activity;
        app.activity_selected = 2;
        app.on_input(key(KeyCode::Up));
        assert_eq!(
            app.focus,
            Focus::Activity,
            "↑ not at top must stay in Activity"
        );
        assert_eq!(app.activity_selected, 1, "↑ must retreat activity_selected");
    }

    /// Detail modal: ↓ at bottom of Variations crosses to History.
    #[test]
    fn detail_down_at_variations_bottom_crosses_to_history() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection()); // first book has 2 versions
                                                    // Put modal at the last variation (index 1 of 2).
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 1, // at bottom (max = 1 for 2 versions)
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        app.on_input(key(KeyCode::Down));
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 1,
                sub_focus: DetailSubFocus::History,
                history_selected: 0,
            }),
            "↓ at Variations bottom must cross to History"
        );
    }

    /// Detail modal: ↑ at top of History crosses back to Variations.
    #[test]
    fn detail_up_at_history_top_crosses_to_variations() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        // Modal in History sub-focus at history row 0.
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::History,
            history_selected: 0,
        });
        app.on_input(key(KeyCode::Up));
        // Should cross to Variations at its bottom.
        if let Some(Modal::Detail { sub_focus, .. }) = &app.modal {
            assert_eq!(
                *sub_focus,
                DetailSubFocus::Variations,
                "↑ at History top must cross to Variations"
            );
        } else {
            panic!("modal must still be Detail");
        }
    }

    /// Render: inactive book-list selection shows a dim ▌ when Activity/Header focused.
    #[test]
    fn render_inactive_list_selection_shows_dim_accent() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.selected = 0;
        // Focus Activity — list selection becomes inactive.
        app.focus = Focus::Activity;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        // The dim ▌ (U+258C) should appear (for the inactive selection).
        assert!(
            content.contains('\u{258c}'),
            "inactive list selection must show a dim ▌ accent"
        );
    }

    /// Render: Header-focused filter row shows the pane accent ▌.
    #[test]
    fn render_header_focus_shows_pane_accent() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Header;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        // The pane accent ▌ must appear in the filter row.
        assert!(
            content.contains('\u{258c}'),
            "Header-focused filter row must show ▌ pane accent"
        );
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
        // The list strip renders from all_lists; populate it (mirrors startup).
        app.all_lists.push(crate::app::ListSummary {
            id: "L1".into(),
            title: "Test List".into(),
            done: 1,
            total: 2,
        });

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
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
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

    /// Default draft used in settings tests (no view loaded — uses engine defaults).
    fn default_draft() -> SettingsDraft {
        SettingsDraft {
            format_pref: vec!["epub".into(), "pdf".into()],
            language: String::new(),
            auto_threshold: 0.85,
            near_threshold: 0.45,
            keep_top: 5,
            naming_template: "{seq:02} - {authors} - {title}.{ext}".into(),
            seq_per_group: true,
            out_dir: String::new(),
            max_concurrent: 5,
            max_attempts: 3,
            hedge_enabled: false,
            editor: SettingsEditor::Viewing,
        }
    }

    #[test]
    fn render_settings_modal_does_not_panic() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Settings);
        app.settings_draft = Some(default_draft());
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

    // -----------------------------------------------------------------------
    // FIX 1 — detail modal variation ↑/↓ navigation
    // -----------------------------------------------------------------------

    #[test]
    fn detail_modal_down_advances_selected() {
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        // Open detail on the first book (which has 2 versions).
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        let intent = app.on_input(key(KeyCode::Down));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 1,
                sub_focus: crate::app::DetailSubFocus::Variations,
                history_selected: 0,
            }),
            "Down inside detail modal should advance variation selected to 1"
        );
    }

    #[test]
    fn detail_modal_up_retreats_selected() {
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 1,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        let intent = app.on_input(key(KeyCode::Up));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 0,
                sub_focus: crate::app::DetailSubFocus::Variations,
                history_selected: 0,
            }),
            "Up inside detail modal should retreat variation selected to 0"
        );
    }

    // #65: ↓ at the bottom of Variations now crosses to History instead of clamping.
    #[test]
    fn detail_modal_down_at_last_version_crosses_to_history() {
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        // Already at the last variation (index 1 of 2).
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 1,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        app.on_input(key(KeyCode::Down));
        // Per #65: ↓ at the bottom of Variations crosses to History.
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 1,
                sub_focus: crate::app::DetailSubFocus::History,
                history_selected: 0,
            }),
            "↓ at last variation must cross to History (#65)"
        );
    }

    #[test]
    fn detail_modal_j_k_also_navigate() {
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        app.on_input(key(KeyCode::Char('j')));
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 1,
                sub_focus: crate::app::DetailSubFocus::Variations,
                history_selected: 0,
            }),
            "'j' must advance variation selected"
        );
        app.on_input(key(KeyCode::Char('k')));
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 0,
                sub_focus: crate::app::DetailSubFocus::Variations,
                history_selected: 0,
            }),
            "'k' must retreat variation selected"
        );
    }

    #[test]
    fn render_detail_modal_selected_variant_visible() {
        // Render detail modal with selected=1 — the buffer should show the ▶ marker.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 1,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        // ▶ (U+25B6) should appear in the buffer as the selection marker.
        assert!(
            content.contains('\u{25b6}'),
            "detail modal selected row must show ▶ marker"
        );
    }

    // -----------------------------------------------------------------------
    // FIX 3 — page up/down in the book list
    // -----------------------------------------------------------------------

    #[test]
    fn shift_j_moves_selection_page_down() {
        // Build a fixture with enough books to page through.
        use libgen_core::model::{BookInput, BookRequest, DownloadList, Group};
        use libgen_engine::viewmodel::build_with_id;
        let mut g = Group::new("G");
        for i in 0..20 {
            g.books.push(BookRequest::new(BookInput {
                title: format!("Book {}", i),
                authors: vec!["A".into()],
                ..Default::default()
            }));
        }
        let list = DownloadList {
            title: "Big".into(),
            settings: libgen_core::model::ListSettings::default(),
            groups: vec![g],
        };
        let vm = build_with_id("big".into(), &list);

        let mut app = AppState::new();
        app.set_view(vm);
        // Simulate a rendered viewport height of 10.
        app.last_rects.book_table.height = 10;
        assert_eq!(app.selected, 0);

        // Shift-J (uppercase J) = page down.
        let intent = app.on_input(key(KeyCode::Char('J')));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(
            app.selected, 10,
            "Shift-J should jump the selection down by one page (10 rows)"
        );
    }

    #[test]
    fn shift_k_moves_selection_page_up() {
        use libgen_core::model::{BookInput, BookRequest, DownloadList, Group};
        use libgen_engine::viewmodel::build_with_id;
        let mut g = Group::new("G");
        for i in 0..20 {
            g.books.push(BookRequest::new(BookInput {
                title: format!("Book {}", i),
                authors: vec!["A".into()],
                ..Default::default()
            }));
        }
        let list = DownloadList {
            title: "Big".into(),
            settings: libgen_core::model::ListSettings::default(),
            groups: vec![g],
        };
        let vm = build_with_id("big".into(), &list);

        let mut app = AppState::new();
        app.set_view(vm);
        app.last_rects.book_table.height = 10;
        app.selected = 15;

        let intent = app.on_input(key(KeyCode::Char('K')));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(
            app.selected, 5,
            "Shift-K should jump the selection up by one page (10 rows)"
        );
    }

    #[test]
    fn shift_down_key_moves_selection_page_down() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
        };
        use libgen_core::model::{BookInput, BookRequest, DownloadList, Group};
        use libgen_engine::viewmodel::build_with_id;
        let mut g = Group::new("G");
        for i in 0..20 {
            g.books.push(BookRequest::new(BookInput {
                title: format!("Book {}", i),
                authors: vec!["A".into()],
                ..Default::default()
            }));
        }
        let list = DownloadList {
            title: "Big".into(),
            settings: libgen_core::model::ListSettings::default(),
            groups: vec![g],
        };
        let vm = build_with_id("big".into(), &list);

        let mut app = AppState::new();
        app.set_view(vm);
        app.last_rects.book_table.height = 8;
        assert_eq!(app.selected, 0);

        let intent = app.on_input(Event::Key(KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(
            app.selected, 8,
            "Shift-Down should jump the selection down by one page"
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

        // The first book row is at approximately y=4 in our layout:
        // row 0=list strip, 1=filter, 2=rule, 3=first book-table row.
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

    // -----------------------------------------------------------------------
    // Settings modal — editor tests
    // -----------------------------------------------------------------------

    /// Open the settings modal with a default draft on `app`.
    fn open_settings_with_draft(app: &mut AppState) {
        app.modal = Some(Modal::Settings);
        app.settings_draft = Some(default_draft());
        app.settings_selected = 0;
    }

    // ── field-kind map ──────────────────────────────────────────────────────

    #[test]
    fn settings_field_kinds_match_spec() {
        assert_eq!(settings_field_kind(0), SettingsFieldKind::FormatPref);
        assert_eq!(settings_field_kind(1), SettingsFieldKind::Language);
        assert_eq!(settings_field_kind(2), SettingsFieldKind::F32);
        assert_eq!(settings_field_kind(3), SettingsFieldKind::F32);
        assert_eq!(settings_field_kind(4), SettingsFieldKind::Usize);
        assert_eq!(settings_field_kind(5), SettingsFieldKind::Text);
        assert_eq!(settings_field_kind(6), SettingsFieldKind::Text);
        assert_eq!(settings_field_kind(7), SettingsFieldKind::Bool);
        assert_eq!(settings_field_kind(8), SettingsFieldKind::Usize);
        assert_eq!(settings_field_kind(9), SettingsFieldKind::U32);
        assert_eq!(settings_field_kind(10), SettingsFieldKind::Bool);
    }

    #[test]
    fn settings_field_count_constant_is_11() {
        // All indices 0-10 are editable; 11+ are display-only.
        assert_eq!(SETTINGS_FIELD_COUNT, 11);
    }

    // ── TOGGLE field (space) ────────────────────────────────────────────────

    #[test]
    fn space_toggles_bool_field_sub_grouping() {
        // Field 7 = "Sub-grouping" (seq_per_group bool).
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 7;

        let before = app.settings_draft.as_ref().unwrap().seq_per_group;
        app.on_input(key(KeyCode::Char(' ')));
        let after = app.settings_draft.as_ref().unwrap().seq_per_group;

        assert_ne!(before, after, "space must toggle seq_per_group");
    }

    #[test]
    fn space_toggles_hedged_field() {
        // Field 10 = "Hedged" (hedge_enabled bool).
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 10;

        let before = app.settings_draft.as_ref().unwrap().hedge_enabled;
        app.on_input(key(KeyCode::Char(' ')));
        let after = app.settings_draft.as_ref().unwrap().hedge_enabled;

        assert_ne!(before, after, "space must toggle hedge_enabled");
    }

    #[test]
    fn double_space_toggle_restores_original_bool() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 7;
        let original = app.settings_draft.as_ref().unwrap().seq_per_group;
        app.on_input(key(KeyCode::Char(' ')));
        app.on_input(key(KeyCode::Char(' ')));
        assert_eq!(
            app.settings_draft.as_ref().unwrap().seq_per_group,
            original,
            "two toggles should restore original value"
        );
    }

    // ── NUMBER field — ←/→ nudge ────────────────────────────────────────────

    #[test]
    fn right_arrow_nudges_f32_field_up() {
        // Field 2 = auto_threshold (f32, step 0.05).
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 2;
        let before = app.settings_draft.as_ref().unwrap().auto_threshold;

        app.on_input(key(KeyCode::Right));
        let after = app.settings_draft.as_ref().unwrap().auto_threshold;

        assert!(
            (after - before - 0.05).abs() < 1e-4,
            "→ must nudge auto_threshold by +0.05, was {before}, now {after}"
        );
    }

    #[test]
    fn left_arrow_nudges_f32_field_down() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 2;
        let before = app.settings_draft.as_ref().unwrap().auto_threshold;

        app.on_input(key(KeyCode::Left));
        let after = app.settings_draft.as_ref().unwrap().auto_threshold;

        assert!(
            (before - after - 0.05).abs() < 1e-4,
            "← must nudge auto_threshold by -0.05"
        );
    }

    #[test]
    fn right_arrow_nudges_usize_field() {
        // Field 4 = keep_top (usize).
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 4;
        let before = app.settings_draft.as_ref().unwrap().keep_top;

        app.on_input(key(KeyCode::Right));
        assert_eq!(
            app.settings_draft.as_ref().unwrap().keep_top,
            before + 1,
            "→ must increment keep_top by 1"
        );
    }

    #[test]
    fn left_arrow_clamps_usize_field_at_1() {
        // keep_top starts at 5 in default draft; nudge it down to 1 and check it doesn't go below.
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 4;
        // Set to 1 directly.
        app.settings_draft.as_mut().unwrap().keep_top = 1;
        app.on_input(key(KeyCode::Left));
        assert_eq!(
            app.settings_draft.as_ref().unwrap().keep_top,
            1,
            "keep_top must not go below 1"
        );
    }

    // ── INLINE EDIT (Enter → type → Enter commit) ───────────────────────────

    #[test]
    fn enter_on_number_field_enters_edit_mode() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 4; // keep_top
        app.on_input(key(KeyCode::Enter));
        assert!(
            matches!(
                app.settings_draft.as_ref().unwrap().editor,
                SettingsEditor::Editing(_)
            ),
            "Enter must enter inline edit mode"
        );
    }

    #[test]
    fn inline_edit_then_commit_updates_field() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 4; // keep_top (usize)
        app.on_input(key(KeyCode::Enter)); // start edit
                                           // Clear the pre-filled buffer and type "7"
        for _ in 0..10 {
            // backspace enough times to clear any pre-filled value
            app.on_input(key(KeyCode::Backspace));
        }
        app.on_input(key(KeyCode::Char('7')));
        app.on_input(key(KeyCode::Enter)); // commit
        assert_eq!(
            app.settings_draft.as_ref().unwrap().editor,
            SettingsEditor::Viewing,
            "after commit editor should return to Viewing"
        );
        assert_eq!(
            app.settings_draft.as_ref().unwrap().keep_top,
            7,
            "keep_top should be updated to 7"
        );
    }

    #[test]
    fn inline_edit_esc_cancels_without_changing_value() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 4; // keep_top = 5
        app.on_input(key(KeyCode::Enter)); // start edit
        app.on_input(key(KeyCode::Backspace));
        app.on_input(key(KeyCode::Char('9'))); // would change to 9
        app.on_input(key(KeyCode::Esc)); // cancel
        assert_eq!(
            app.settings_draft.as_ref().unwrap().editor,
            SettingsEditor::Viewing,
            "Esc must exit edit mode"
        );
        assert_eq!(
            app.settings_draft.as_ref().unwrap().keep_top,
            5,
            "Esc must NOT change the value"
        );
    }

    #[test]
    fn text_field_enter_enters_edit_mode() {
        // Field 6 = "Naming template" (Text).
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 6;
        app.on_input(key(KeyCode::Enter));
        assert!(
            matches!(
                app.settings_draft.as_ref().unwrap().editor,
                SettingsEditor::Editing(_)
            ),
            "Enter on text field must enter Editing mode"
        );
    }

    // ── LANGUAGE field — "any" display and picker ───────────────────────────

    #[test]
    fn language_field_shows_match_title_language_when_empty() {
        // Field index 1; default_draft() has language = "".
        let draft = default_draft();
        assert_eq!(draft.language, "", "default language is empty string");
        assert_eq!(
            draft.field_value(1),
            "match title language",
            "empty language must display as 'match title language' (#58)"
        );
    }

    #[test]
    fn enter_on_language_field_opens_lang_picker() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 1; // Language
        app.on_input(key(KeyCode::Enter));
        assert!(
            matches!(
                app.settings_draft.as_ref().unwrap().editor,
                SettingsEditor::LangPicker { .. }
            ),
            "Enter on Language must open LangPicker"
        );
    }

    #[test]
    fn lang_picker_selects_language() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 1;
        app.on_input(key(KeyCode::Enter)); // open picker (selected = 0 = "match title language")
        app.on_input(key(KeyCode::Down)); // move to "English" (index 1)
        app.on_input(key(KeyCode::Enter)); // commit
        assert_eq!(
            app.settings_draft.as_ref().unwrap().language,
            "English",
            "selecting English from picker must update language"
        );
        assert_eq!(
            app.settings_draft.as_ref().unwrap().editor,
            SettingsEditor::Viewing
        );
    }

    #[test]
    fn lang_picker_match_title_language_stores_empty_string() {
        // Selecting "match title language" (the first option, index 0) must store "".
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_draft.as_mut().unwrap().language = "English".into();
        app.settings_selected = 1;
        app.on_input(key(KeyCode::Enter)); // open picker; starts at "English" (index 1)
        app.on_input(key(KeyCode::Up)); // move to "match title language" (index 0)
        app.on_input(key(KeyCode::Enter)); // commit "match title language"
        assert_eq!(
            app.settings_draft.as_ref().unwrap().language,
            "",
            "selecting 'match title language' must store empty string (#58)"
        );
    }

    #[test]
    fn lang_picker_esc_does_not_change_language() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_draft.as_mut().unwrap().language = "German".into();
        app.settings_selected = 1;
        app.on_input(key(KeyCode::Enter));
        app.on_input(key(KeyCode::Down)); // move selection
        app.on_input(key(KeyCode::Esc)); // cancel
        assert_eq!(
            app.settings_draft.as_ref().unwrap().language,
            "German",
            "Esc in lang picker must not change the language"
        );
    }

    // ── FORMAT EDITOR sub-modal ─────────────────────────────────────────────

    #[test]
    fn build_format_editor_rows_puts_included_first() {
        let pref = vec!["pdf".to_string(), "epub".to_string()];
        let rows = build_format_editor_rows(&pref);
        // First two should be included (pdf, epub in that order).
        assert_eq!(rows[0], (true, "pdf".to_string()));
        assert_eq!(rows[1], (true, "epub".to_string()));
        // Rest should be excluded.
        for (inc, _) in &rows[2..] {
            assert!(!inc, "rows after included block must not be included");
        }
        // Total row count = FORMAT_EDITOR_FORMATS length.
        assert_eq!(rows.len(), FORMAT_EDITOR_FORMATS.len());
    }

    #[test]
    fn build_format_editor_rows_all_excluded_when_empty_pref() {
        let rows = build_format_editor_rows(&[]);
        assert!(
            rows.iter().all(|(inc, _)| !inc),
            "all rows must be excluded"
        );
        assert_eq!(rows.len(), FORMAT_EDITOR_FORMATS.len());
    }

    #[test]
    fn format_editor_opens_on_enter() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 0; // "Preferred formats"
        app.on_input(key(KeyCode::Enter));
        assert!(
            matches!(
                app.settings_draft.as_ref().unwrap().editor,
                SettingsEditor::FormatEditor { .. }
            ),
            "Enter on format field must open FormatEditor"
        );
    }

    #[test]
    fn format_editor_space_toggles_inclusion() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        // draft starts with ["epub", "pdf"]; field 0 opens format editor.
        app.settings_selected = 0;
        app.on_input(key(KeyCode::Enter)); // open editor
                                           // Cursor is at 0 ("epub" — currently included).
                                           // Space should exclude it.
        app.on_input(key(KeyCode::Char(' ')));
        if let Some(SettingsDraft {
            editor:
                SettingsEditor::FormatEditor {
                    ref rows,
                    cursor: _,
                },
            ..
        }) = app.settings_draft
        {
            // epub was at index 0 and was included; after space it should be excluded.
            // It may have moved to the excluded block; find it.
            let epub_row = rows.iter().find(|(_, n)| n == "epub");
            assert!(
                epub_row.is_some(),
                "epub must still appear in the format editor"
            );
            assert!(
                !epub_row.unwrap().0,
                "epub must now be excluded after space"
            );
        } else {
            panic!("expected FormatEditor mode");
        }
    }

    #[test]
    fn format_editor_j_moves_down_in_priority() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        // draft: format_pref = ["epub", "pdf"]. Open editor, cursor at 0 = epub.
        app.settings_selected = 0;
        app.on_input(key(KeyCode::Enter));
        // Press J — epub should swap with pdf (both are included).
        app.on_input(key(KeyCode::Char('J')));
        if let Some(SettingsDraft {
            editor: SettingsEditor::FormatEditor { ref rows, cursor },
            ..
        }) = app.settings_draft
        {
            // After J, cursor moved to 1, and pdf is now at index 0.
            assert_eq!(cursor, 1, "cursor should be at index 1 after J");
            assert_eq!(rows[0].1, "pdf", "pdf must now be at priority 1");
            assert_eq!(rows[1].1, "epub", "epub must now be at priority 2");
        } else {
            panic!("expected FormatEditor mode");
        }
    }

    #[test]
    fn format_editor_k_moves_up_in_priority() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 0;
        app.on_input(key(KeyCode::Enter));
        // Move cursor to pdf (index 1) then press K.
        app.on_input(key(KeyCode::Down));
        app.on_input(key(KeyCode::Char('K')));
        if let Some(SettingsDraft {
            editor: SettingsEditor::FormatEditor { ref rows, cursor },
            ..
        }) = app.settings_draft
        {
            assert_eq!(cursor, 0, "cursor should be at index 0 after K");
            assert_eq!(rows[0].1, "pdf", "pdf must now be at priority 1");
        } else {
            panic!("expected FormatEditor mode");
        }
    }

    #[test]
    fn format_editor_enter_commits_and_updates_format_pref() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        // Start: epub, pdf included.
        app.settings_selected = 0;
        app.on_input(key(KeyCode::Enter)); // open
        app.on_input(key(KeyCode::Char('J'))); // swap epub → pdf first
        app.on_input(key(KeyCode::Enter)); // commit
        let pref = &app.settings_draft.as_ref().unwrap().format_pref;
        assert_eq!(pref[0], "pdf", "after J then commit, pdf must be first");
        assert_eq!(pref[1], "epub", "epub must be second");
        assert_eq!(
            app.settings_draft.as_ref().unwrap().editor,
            SettingsEditor::Viewing,
            "editor must return to Viewing after commit"
        );
    }

    // ── Save / Discard key mapping ───────────────────────────────────────────

    #[test]
    fn s_in_viewing_returns_save_settings_intent() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        let intent = app.on_input(key(KeyCode::Char('s')));
        // 's' → save & close (modal is closed, intent is SaveSettings).
        assert_eq!(intent, Intent::SaveSettings);
        assert!(app.modal.is_none(), "modal must be closed after s");
        // Draft is kept alive for the dispatcher.
        assert!(
            app.settings_draft.is_some(),
            "draft must remain until dispatcher processes SaveSettings"
        );
    }

    #[test]
    fn esc_in_viewing_returns_discard_settings_intent() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_draft.as_mut().unwrap().auto_threshold = 0.99;
        let intent = app.on_input(key(KeyCode::Esc));
        // Esc → discard & close (draft cleared, intent is DiscardSettings).
        assert_eq!(intent, Intent::DiscardSettings);
        assert!(app.modal.is_none(), "modal must be closed after Esc");
        assert!(
            app.settings_draft.is_none(),
            "draft must be cleared on Esc discard"
        );
    }

    #[test]
    fn q_in_viewing_returns_discard_intent_and_clears_draft() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        // Modify the draft so we can verify discard restores nothing (draft is simply cleared).
        app.settings_draft.as_mut().unwrap().auto_threshold = 0.99;
        let intent = app.on_input(key(KeyCode::Char('q')));
        assert_eq!(intent, Intent::DiscardSettings);
        assert!(app.modal.is_none(), "modal must be closed after q");
        assert!(
            app.settings_draft.is_none(),
            "draft must be cleared on discard"
        );
    }

    // ── Navigation clamps ───────────────────────────────────────────────────

    #[test]
    fn down_navigation_clamps_at_last_field() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        // Navigate down many times.
        for _ in 0..30 {
            app.on_input(key(KeyCode::Down));
        }
        assert_eq!(
            app.settings_selected,
            SETTINGS_FIELD_COUNT - 1,
            "settings_selected must clamp at SETTINGS_FIELD_COUNT - 1"
        );
    }

    #[test]
    fn up_navigation_clamps_at_zero() {
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 3;
        for _ in 0..10 {
            app.on_input(key(KeyCode::Up));
        }
        assert_eq!(
            app.settings_selected, 0,
            "settings_selected must clamp at 0"
        );
    }

    // ── Render with draft ───────────────────────────────────────────────────

    #[test]
    fn render_settings_modal_with_draft_does_not_panic() {
        let backend = TestBackend::new(132, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Settings);
        app.settings_draft = Some(default_draft());
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn render_settings_modal_format_editor_does_not_panic() {
        let backend = TestBackend::new(132, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        let mut draft = default_draft();
        draft.editor = SettingsEditor::FormatEditor {
            rows: build_format_editor_rows(&draft.format_pref),
            cursor: 0,
        };
        app.modal = Some(Modal::Settings);
        app.settings_draft = Some(draft);
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn render_settings_modal_lang_picker_does_not_panic() {
        let backend = TestBackend::new(132, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        let mut draft = default_draft();
        draft.editor = SettingsEditor::LangPicker {
            options: vec!["any".into(), "English".into(), "German".into()],
            selected: 1,
        };
        app.modal = Some(Modal::Settings);
        app.settings_draft = Some(draft);
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn render_settings_modal_inline_edit_shows_cursor() {
        let backend = TestBackend::new(132, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        let mut draft = default_draft();
        draft.editor = SettingsEditor::Editing("0.90".into());
        app.settings_selected = 2; // auto_threshold field
        app.modal = Some(Modal::Settings);
        app.settings_draft = Some(draft);
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        let buf: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        // The edit buffer should appear in the rendered output.
        assert!(
            buf.contains("0.90"),
            "rendered output must contain the edit buffer '0.90'"
        );
    }

    // -----------------------------------------------------------------------
    // #45 — :pause / :start / :start-all command parsing (pure reducer side)
    // -----------------------------------------------------------------------

    #[test]
    fn command_buf_pause_emits_command_intent() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        for c in "pause".chars() {
            app.on_input(key(KeyCode::Char(c)));
        }
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Command("pause".into()));
    }

    #[test]
    fn command_buf_start_emits_command_intent() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        for c in "start".chars() {
            app.on_input(key(KeyCode::Char(c)));
        }
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Command("start".into()));
    }

    #[test]
    fn command_buf_resume_emits_command_intent() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        for c in "resume".chars() {
            app.on_input(key(KeyCode::Char(c)));
        }
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Command("resume".into()));
    }

    #[test]
    fn command_buf_start_all_emits_command_intent() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        for c in "start-all".chars() {
            app.on_input(key(KeyCode::Char(c)));
        }
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Command("start-all".into()));
    }

    #[test]
    fn command_buf_resume_all_emits_command_intent() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        for c in "resume-all".chars() {
            app.on_input(key(KeyCode::Char(c)));
        }
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Command("resume-all".into()));
    }

    // -----------------------------------------------------------------------
    // #48 — Confirm modal (delete-list) state machine
    // -----------------------------------------------------------------------

    #[test]
    fn confirm_modal_y_emits_confirm_delete_intent() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Confirm {
            title: "Test List".into(),
            n_books: 2,
            target_id: "list42".into(),
        });
        let intent = app.on_input(key(KeyCode::Char('y')));
        assert_eq!(
            intent,
            Intent::ConfirmDelete {
                id: "list42".into()
            },
            "y in Confirm modal must emit ConfirmDelete"
        );
        assert!(app.modal.is_none(), "Confirm modal must close after y");
    }

    #[test]
    fn confirm_modal_n_closes_without_delete() {
        let mut app = AppState::new();
        app.modal = Some(Modal::Confirm {
            title: "My List".into(),
            n_books: 5,
            target_id: "list7".into(),
        });
        let intent = app.on_input(key(KeyCode::Char('n')));
        assert_eq!(intent, Intent::Redraw, "n must not emit delete");
        assert!(app.modal.is_none(), "modal must close after n");
    }

    #[test]
    fn confirm_modal_esc_closes_without_delete() {
        let mut app = AppState::new();
        app.modal = Some(Modal::Confirm {
            title: "My List".into(),
            n_books: 3,
            target_id: "list9".into(),
        });
        let intent = app.on_input(key(KeyCode::Esc));
        assert_eq!(intent, Intent::Redraw, "Esc must not emit delete");
        assert!(app.modal.is_none(), "modal must close after Esc");
    }

    #[test]
    fn render_confirm_modal_does_not_panic() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Confirm {
            title: "Test List".into(),
            n_books: 2,
            target_id: "list1".into(),
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(
            content.contains("Test List"),
            "confirm modal must show the list title"
        );
        assert!(
            content.contains("2 book"),
            "confirm modal must show the book count"
        );
    }

    // -----------------------------------------------------------------------
    // #53 — :add-md5 / :refresh-mirrors / :cleanup / :delete command parsing
    // -----------------------------------------------------------------------

    #[test]
    fn command_add_md5_emits_command_intent() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        let cmd = "add-md5 aabbccddeeff00112233445566778899";
        for c in cmd.chars() {
            app.on_input(key(KeyCode::Char(c)));
        }
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(
            intent,
            Intent::Command("add-md5 aabbccddeeff00112233445566778899".into())
        );
    }

    #[test]
    fn command_refresh_mirrors_emits_command_intent() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        for c in "refresh-mirrors".chars() {
            app.on_input(key(KeyCode::Char(c)));
        }
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Command("refresh-mirrors".into()));
    }

    #[test]
    fn command_cleanup_emits_command_intent() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        for c in "cleanup".chars() {
            app.on_input(key(KeyCode::Char(c)));
        }
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Command("cleanup".into()));
    }

    #[test]
    fn command_delete_emits_command_intent() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        for c in "delete".chars() {
            app.on_input(key(KeyCode::Char(c)));
        }
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Command("delete".into()));
    }

    #[test]
    fn new_commands_appear_in_tab_completion() {
        // All new commands must be discoverable via Tab-completion.
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':'))); // enter command mode, buf = ""
        app.on_input(key(KeyCode::Tab)); // open wildmenu with all commands

        let new_cmds = &[
            "pause",
            "start",
            "resume",
            "start-all",
            "resume-all",
            "delete",
            "add-md5",
            "refresh-mirrors",
            "cleanup",
        ];
        for &cmd in new_cmds {
            assert!(
                app.completion_candidates.iter().any(|c| c == cmd),
                "command '{cmd}' must appear in Tab-completion candidates"
            );
        }
    }

    // -----------------------------------------------------------------------
    // #57 — detail sub-focus (Tab + history nav)
    // -----------------------------------------------------------------------

    #[test]
    fn detail_modal_tab_toggles_sub_focus() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        // First Tab → History focused.
        app.on_input(key(KeyCode::Tab));
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 0,
                sub_focus: DetailSubFocus::History,
                history_selected: 0,
            }),
            "Tab should switch sub-focus to History"
        );
        // Second Tab → back to Variations.
        app.on_input(key(KeyCode::Tab));
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 0,
                sub_focus: DetailSubFocus::Variations,
                history_selected: 0,
            }),
            "Second Tab should toggle sub-focus back to Variations"
        );
    }

    #[test]
    fn detail_modal_history_nav_advances_history_selected() {
        use crate::app::DetailSubFocus;
        // Build a fixture with history events.
        use libgen_core::model::{BookInput, BookRequest, DownloadList, Group};
        use libgen_engine::viewmodel::build_with_id;
        let mut g = Group::new("G");
        let mut req = BookRequest::new(BookInput {
            title: "Book With History".into(),
            authors: vec!["Author".into()],
            ..Default::default()
        });
        // Inject some history events.
        req.log_event(None, None, "discovered", String::from("found candidates"));
        req.log_event(None, None, "matched", String::from("auto-matched"));
        req.log_event(None, None, "downloading", String::from("started"));
        g.books.push(req);
        let list = DownloadList {
            title: "T".into(),
            settings: libgen_core::model::ListSettings::default(),
            groups: vec![g],
        };
        let vm = build_with_id("test".into(), &list);

        let mut app = AppState::new();
        app.set_view(vm);
        // Focus History sub-pane.
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::History,
            history_selected: 0,
        });
        // ↓ should advance history_selected.
        app.on_input(key(KeyCode::Down));
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 0,
                sub_focus: DetailSubFocus::History,
                history_selected: 1,
            }),
            "Down with History focused should advance history_selected"
        );
    }

    #[test]
    fn detail_modal_history_scroll_clamps_at_last_event() {
        use crate::app::DetailSubFocus;
        use libgen_core::model::{BookInput, BookRequest, DownloadList, Group};
        use libgen_engine::viewmodel::build_with_id;
        let mut g = Group::new("G");
        let mut req = BookRequest::new(BookInput {
            title: "Book".into(),
            authors: vec![],
            ..Default::default()
        });
        req.log_event(None, None, "discovered", String::from("found"));
        req.log_event(None, None, "matched", String::from("ok"));
        g.books.push(req);
        let list = DownloadList {
            title: "T".into(),
            settings: libgen_core::model::ListSettings::default(),
            groups: vec![g],
        };
        let vm = build_with_id("test".into(), &list);
        let mut app = AppState::new();
        app.set_view(vm);

        // Start at last event (index 1 of 2).
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::History,
            history_selected: 1,
        });
        // ↓ must not go past the last event.
        app.on_input(key(KeyCode::Down));
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 0,
                sub_focus: DetailSubFocus::History,
                history_selected: 1,
            }),
            "Down must clamp at the last history event"
        );
    }

    // -----------------------------------------------------------------------
    // #49 — manual re-query
    // -----------------------------------------------------------------------

    #[test]
    fn detail_modal_s_opens_requery_modal() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        app.on_input(key(KeyCode::Char('s')));
        // Should now be in ReQuery modal with buf prefilled with the book's title.
        match &app.modal {
            Some(Modal::ReQuery {
                book_flat_index: 0,
                buf,
            }) => {
                assert!(
                    !buf.is_empty(),
                    "ReQuery buf should be pre-filled with book title"
                );
            }
            other => panic!("expected Modal::ReQuery, got {other:?}"),
        }
    }

    #[test]
    fn requery_modal_enter_emits_requery_book_intent() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::ReQuery {
            book_flat_index: 0,
            buf: "New Title".into(),
        });
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(
            intent,
            Intent::ReQueryBook {
                group_path: vec![0],
                book_index: 0,
                title: "New Title".into(),
            },
            "Enter in ReQuery modal should emit ReQueryBook"
        );
        assert!(app.modal.is_none(), "modal should be cleared after submit");
    }

    #[test]
    fn requery_modal_esc_returns_to_detail() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::ReQuery {
            book_flat_index: 0,
            buf: "whatever".into(),
        });
        app.on_input(key(KeyCode::Esc));
        assert!(
            matches!(app.modal, Some(Modal::Detail { .. })),
            "Esc in ReQuery should return to Detail modal"
        );
    }

    // -----------------------------------------------------------------------
    // #50 — book-level actions: edit / remove / mark-not-found
    // -----------------------------------------------------------------------

    #[test]
    fn detail_modal_e_opens_edit_book_modal() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        app.on_input(key(KeyCode::Char('e')));
        match &app.modal {
            Some(Modal::EditBook {
                book_flat_index: 0,
                title_buf,
                field: crate::app::EditBookField::Title,
                ..
            }) => {
                assert!(
                    !title_buf.is_empty(),
                    "EditBook title_buf should be pre-filled"
                );
            }
            other => panic!("expected Modal::EditBook, got {other:?}"),
        }
    }

    #[test]
    fn edit_book_modal_enter_emits_edit_book_intent() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::EditBook {
            book_flat_index: 0,
            title_buf: "New Title".into(),
            author_buf: "Author A, Author B".into(),
            field: crate::app::EditBookField::Title,
        });
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(
            intent,
            Intent::EditBook {
                group_path: vec![0],
                book_index: 0,
                title: "New Title".into(),
                authors: vec!["Author A".into(), "Author B".into()],
            },
            "Enter in EditBook modal should emit EditBook intent"
        );
        assert!(app.modal.is_none(), "modal should be cleared after submit");
    }

    #[test]
    fn detail_modal_x_opens_confirm_remove() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        app.on_input(key(KeyCode::Char('x')));
        assert_eq!(
            app.modal,
            Some(Modal::ConfirmBookRemove { book_flat_index: 0 }),
            "'x' should open ConfirmBookRemove"
        );
    }

    #[test]
    fn confirm_remove_y_emits_remove_book_intent() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::ConfirmBookRemove { book_flat_index: 0 });
        let intent = app.on_input(key(KeyCode::Char('y')));
        assert_eq!(
            intent,
            Intent::RemoveBook {
                group_path: vec![0],
                book_index: 0,
            },
            "y in ConfirmBookRemove should emit RemoveBook"
        );
        assert!(app.modal.is_none(), "modal should be cleared after confirm");
    }

    #[test]
    fn confirm_remove_n_returns_to_detail() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::ConfirmBookRemove { book_flat_index: 0 });
        app.on_input(key(KeyCode::Char('n')));
        assert!(
            matches!(app.modal, Some(Modal::Detail { .. })),
            "n in ConfirmBookRemove should return to Detail modal"
        );
    }

    #[test]
    fn detail_modal_m_emits_mark_not_found() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        let intent = app.on_input(key(KeyCode::Char('m')));
        assert_eq!(
            intent,
            Intent::MarkNotFound {
                group_path: vec![0],
                book_index: 0,
            },
            "'m' in detail modal should emit MarkNotFound"
        );
        assert!(app.modal.is_none(), "modal should be cleared after mark");
    }

    #[test]
    fn list_view_m_emits_mark_not_found() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        let intent = app.on_input(key(KeyCode::Char('m')));
        assert_eq!(
            intent,
            Intent::MarkNotFound {
                group_path: vec![0],
                book_index: 0,
            },
            "'m' in list view should emit MarkNotFound for the selected book"
        );
    }

    #[test]
    fn list_view_x_opens_confirm_remove() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.on_input(key(KeyCode::Char('x')));
        assert_eq!(
            app.modal,
            Some(Modal::ConfirmBookRemove { book_flat_index: 0 }),
            "'x' in list view should open ConfirmBookRemove"
        );
    }

    #[test]
    fn list_view_e_opens_edit_book_modal() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.on_input(key(KeyCode::Char('e')));
        match &app.modal {
            Some(Modal::EditBook {
                book_flat_index: 0,
                title_buf,
                field: crate::app::EditBookField::Title,
                ..
            }) => {
                assert!(
                    !title_buf.is_empty(),
                    "EditBook title_buf should be pre-filled"
                );
            }
            other => panic!("expected Modal::EditBook for list view 'e', got {other:?}"),
        }
    }

    #[test]
    fn render_requery_modal_does_not_panic() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::ReQuery {
            book_flat_index: 0,
            buf: "Treasure".into(),
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn render_edit_book_modal_does_not_panic() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::EditBook {
            book_flat_index: 0,
            title_buf: "Treasure Island".into(),
            author_buf: "Stevenson".into(),
            field: crate::app::EditBookField::Title,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    #[test]
    fn render_confirm_remove_modal_does_not_panic() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::ConfirmBookRemove { book_flat_index: 0 });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    // -----------------------------------------------------------------------
    // #46 — ASCII-art banner on the empty screen
    // -----------------------------------------------------------------------

    #[test]
    fn render_empty_screen_contains_ascii_banner() {
        // The empty/first-run screen must render the ASCII-art "KWIRE" banner (#46).
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        assert!(app.view.is_none());
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        // The wordmark is a multi-row figlet banner built from block glyphs.
        assert!(
            content.contains("██"),
            "empty screen should contain the ASCII-art block-letter banner"
        );
    }

    #[test]
    fn render_empty_screen_banner_height_fits() {
        // With content_h = 20 (6-row banner), a 24-row terminal still renders without panic.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    // -----------------------------------------------------------------------
    // #51 — per-transfer Activity pane controls (p / c / r)
    // -----------------------------------------------------------------------

    /// Insert a dummy transfer into app.transfers under a 32-char md5 key.
    fn insert_transfer(app: &mut AppState, md5: &str) {
        app.transfers.insert(
            md5.to_string(),
            ActiveTransfer {
                md5: md5.to_string(),
                host: "libgen.li".into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn activity_focus_p_emits_pause_transfer() {
        let mut app = AppState::new();
        let md5 = "a".repeat(32);
        insert_transfer(&mut app, &md5);
        app.focus = Focus::Activity;
        let intent = app.on_input(key(KeyCode::Char('p')));
        assert_eq!(
            intent,
            Intent::PauseTransfer { md5 },
            "p in Activity focus should emit PauseTransfer"
        );
    }

    #[test]
    fn activity_focus_c_emits_cancel_transfer() {
        let mut app = AppState::new();
        let md5 = "b".repeat(32);
        insert_transfer(&mut app, &md5);
        app.focus = Focus::Activity;
        let intent = app.on_input(key(KeyCode::Char('c')));
        assert_eq!(
            intent,
            Intent::CancelTransfer { md5 },
            "c in Activity focus should emit CancelTransfer"
        );
    }

    #[test]
    fn activity_focus_r_emits_resume_transfer() {
        let mut app = AppState::new();
        let md5 = "c".repeat(32);
        insert_transfer(&mut app, &md5);
        app.focus = Focus::Activity;
        let intent = app.on_input(key(KeyCode::Char('r')));
        assert_eq!(
            intent,
            Intent::ResumeTransfer { md5 },
            "r in Activity focus should emit ResumeTransfer"
        );
    }

    #[test]
    fn activity_focus_p_no_transfers_returns_redraw() {
        let mut app = AppState::new();
        // No transfers in the map.
        app.focus = Focus::Activity;
        let intent = app.on_input(key(KeyCode::Char('p')));
        assert_eq!(
            intent,
            Intent::Redraw,
            "p with no transfers should return Redraw"
        );
    }

    #[test]
    fn activity_focus_selection_moves_and_targets_correct_md5() {
        // With two transfers the sorted second md5 should be targeted after ↓.
        let mut app = AppState::new();
        let md5_a = "a".repeat(32); // sorts first
        let md5_b = "b".repeat(32); // sorts second
        insert_transfer(&mut app, &md5_a);
        insert_transfer(&mut app, &md5_b);
        app.focus = Focus::Activity;
        assert_eq!(app.activity_selected, 0);

        // Move down once → now at index 1 (md5_b, alphabetically second).
        app.on_input(key(KeyCode::Down));
        assert_eq!(app.activity_selected, 1);

        let intent = app.on_input(key(KeyCode::Char('p')));
        assert_eq!(
            intent,
            Intent::PauseTransfer { md5: md5_b },
            "after ↓ p should target the second alphabetical transfer"
        );
    }

    #[test]
    fn list_focus_p_still_emits_book_pause() {
        // When focus is List, p must still emit book-level Pause (not PauseTransfer).
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        insert_transfer(&mut app, &"a".repeat(32));
        assert_eq!(app.focus, Focus::List);
        let intent = app.on_input(key(KeyCode::Char('p')));
        assert!(
            matches!(intent, Intent::Pause { .. }),
            "List focus p should emit book-level Pause, got {intent:?}"
        );
    }

    #[test]
    fn list_focus_r_still_emits_book_retry() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        insert_transfer(&mut app, &"a".repeat(32));
        assert_eq!(app.focus, Focus::List);
        let intent = app.on_input(key(KeyCode::Char('r')));
        assert!(
            matches!(intent, Intent::Retry { .. }),
            "List focus r should emit book-level Retry, got {intent:?}"
        );
    }

    #[test]
    fn focused_transfer_md5_returns_sorted_key() {
        // focused_transfer_md5 must return keys in sorted order.
        let mut app = AppState::new();
        insert_transfer(&mut app, &"z".repeat(32));
        insert_transfer(&mut app, &"a".repeat(32));
        insert_transfer(&mut app, &"m".repeat(32));
        // activity_selected = 0 → "a..." (alphabetically first)
        assert_eq!(app.focused_transfer_md5(), Some("a".repeat(32)));
        app.activity_selected = 1;
        assert_eq!(app.focused_transfer_md5(), Some("m".repeat(32)));
        app.activity_selected = 2;
        assert_eq!(app.focused_transfer_md5(), Some("z".repeat(32)));
    }

    // -----------------------------------------------------------------------
    // #52 Reorganize modal — open/scroll/confirm/cancel state machine + render
    // -----------------------------------------------------------------------

    /// A few synthetic old→new path pairs for driving the Reorganize modal.
    fn sample_reorg_diff() -> Vec<(String, String)> {
        vec![
            (
                "/books/Old Title.epub".into(),
                "/books/List/01 - Author - Title.epub".into(),
            ),
            (
                "/books/Another.pdf".into(),
                "/books/List/02 - Author - Another.pdf".into(),
            ),
            (
                "/books/Third.mobi".into(),
                "/books/List/03 - Author - Third.mobi".into(),
            ),
        ]
    }

    #[test]
    fn reorganize_modal_scroll_clamps_within_bounds() {
        let mut app = AppState::new();
        app.modal = Some(Modal::Reorganize {
            diff: sample_reorg_diff(),
            selected: 0,
        });

        // Up at the top stays at 0.
        let intent = app.on_input(key(KeyCode::Up));
        assert_eq!(intent, Intent::Redraw);
        match &app.modal {
            Some(Modal::Reorganize { selected, .. }) => assert_eq!(*selected, 0),
            other => panic!("expected Reorganize modal, got {other:?}"),
        }

        // Two downs → index 2 (last of three).
        app.on_input(key(KeyCode::Down));
        app.on_input(key(KeyCode::Char('j')));
        match &app.modal {
            Some(Modal::Reorganize { selected, .. }) => assert_eq!(*selected, 2),
            other => panic!("expected Reorganize modal, got {other:?}"),
        }

        // Another down clamps at the last row (len - 1).
        app.on_input(key(KeyCode::Down));
        match &app.modal {
            Some(Modal::Reorganize { selected, .. }) => assert_eq!(*selected, 2),
            other => panic!("expected Reorganize modal, got {other:?}"),
        }
    }

    #[test]
    fn reorganize_modal_apply_dispatches_intent_and_closes() {
        let mut app = AppState::new();
        app.modal = Some(Modal::Reorganize {
            diff: sample_reorg_diff(),
            selected: 1,
        });
        let intent = app.on_input(key(KeyCode::Char('y')));
        assert_eq!(intent, Intent::ApplyReorganize);
        assert!(app.modal.is_none(), "apply should close the modal");
    }

    #[test]
    fn reorganize_modal_cancel_closes_without_apply() {
        for cancel in [KeyCode::Char('n'), KeyCode::Esc, KeyCode::Char('q')] {
            let mut app = AppState::new();
            app.modal = Some(Modal::Reorganize {
                diff: sample_reorg_diff(),
                selected: 0,
            });
            let intent = app.on_input(key(cancel));
            assert_eq!(intent, Intent::Redraw, "cancel must not apply");
            assert!(app.modal.is_none(), "cancel should close the modal");
        }
    }

    #[test]
    fn render_reorganize_modal_shows_paths_and_count() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Reorganize {
            diff: sample_reorg_diff(),
            selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(buf.contains("Reorganize"), "title should render");
        assert!(buf.contains("would move"), "count subheader should render");
        assert!(buf.contains("apply"), "apply/cancel hint should render");
    }

    // -----------------------------------------------------------------------
    // #53 — :download-series command
    // -----------------------------------------------------------------------

    /// Both the command and its alias must be Tab-completable (dispatch glue).
    #[test]
    fn download_series_in_tab_completion() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        app.on_input(key(KeyCode::Tab));
        for cmd in ["download-series", "series"] {
            assert!(
                app.completion_candidates.iter().any(|c| c == cmd),
                "command '{cmd}' must appear in Tab-completion candidates"
            );
        }
    }

    #[test]
    fn plan_series_add_no_title_errs() {
        let err = crate::plan_series_add("   ", None).unwrap_err();
        assert!(err.to_lowercase().contains("title"), "msg was: {err}");
    }

    #[test]
    fn plan_series_add_no_series_errs() {
        // Lookup returned None → not part of a known series.
        let err = crate::plan_series_add("The Wonderful Wizard of Oz", None).unwrap_err();
        assert!(err.to_lowercase().contains("series"), "msg was: {err}");
    }

    #[test]
    fn plan_series_add_empty_members_errs() {
        let series = libgen_core::series::Series {
            key: "OL123L".into(),
            name: "Oz".into(),
            members: vec![],
        };
        let err = crate::plan_series_add("The Wonderful Wizard of Oz", Some(&series)).unwrap_err();
        assert!(err.to_lowercase().contains("series"), "msg was: {err}");
    }

    #[test]
    fn plan_series_add_returns_siblings_in_order() {
        use libgen_core::series::{Series, SeriesMember};
        let series = Series {
            key: "OL123L".into(),
            name: "Oz".into(),
            members: vec![
                SeriesMember {
                    title: "The Wonderful Wizard of Oz".into(),
                    position: Some(1),
                    ..Default::default()
                },
                SeriesMember {
                    title: "The Marvelous Land of Oz".into(),
                    position: Some(2),
                    ..Default::default()
                },
                // A blank member title is dropped (defensive).
                SeriesMember {
                    title: "   ".into(),
                    position: Some(3),
                    ..Default::default()
                },
            ],
        };
        let (titles, name) =
            crate::plan_series_add("The Wonderful Wizard of Oz", Some(&series)).unwrap();
        assert_eq!(name, "Oz");
        assert_eq!(
            titles,
            vec![
                "The Wonderful Wizard of Oz".to_string(),
                "The Marvelous Land of Oz".to_string(),
            ]
        );
    }
}
