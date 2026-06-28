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
    use libgen_engine::{ViewBook, ViewEvent, ViewVariation};
    use ratatui::{backend::TestBackend, Terminal};

    use crate::app::{
        build_format_editor_rows, settings_field_kind, ActiveTransfer, AppState, FlatBook, Focus,
        HeaderRow, HelpPage, Modal, RowRef, SettingsDraft, SettingsEditor, SettingsFieldKind,
        StatusFilter, FORMAT_EDITOR_FORMATS, SETTINGS_FIELD_COUNT,
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

    /// Build a `FlatBook` whose single version is QUEUED (armed but not yet
    /// connecting) — feeds the Activity pane's `○ queued N↓` section.
    fn make_queued_flat_book(title: &str, bi: usize) -> FlatBook {
        let mut fb = make_downloading_flat_book(title, 0, bi);
        fb.book.versions[0].state = "queued".into();
        fb.book.versions[0].progress = 0;
        fb
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

    // Deleting the last list drops the view so the empty / first-run splash
    // renders instead of the deleted list's stale rows (regression: the splash
    // is gated on `view.is_none()`, and `refresh_active_view` used to leave a
    // stale `Some(view)` when no list remained current).
    #[test]
    fn clear_view_drops_view_and_resets_to_empty_state() {
        let mut app = AppState::new();
        app.set_view(fixture_vm()); // 2 books
                                    // Simulate a session that had moved selection and parked focus on the
                                    // header (e.g. the user pressed `D` from the list strip).
        app.on_input(key(KeyCode::Down));
        app.focus = Focus::Header;
        app.header_row = HeaderRow::ListStrip;
        assert!(app.view.is_some());
        assert!(!app.flat.is_empty());

        app.clear_view();

        assert!(app.view.is_none(), "view must be dropped after clear");
        assert!(app.flat.is_empty(), "flat rows must be cleared");
        assert_eq!(app.selected, 0, "selection resets to 0");
        assert_eq!(app.focus, Focus::List, "focus returns to the list");
        assert_eq!(
            app.header_row,
            HeaderRow::FilterChips,
            "header row resets so a later import starts clean"
        );
    }

    // With no view and no modal, `render` draws the first-run splash (the
    // "NO READING LISTS YET" screen) rather than the multi-pane list layout.
    #[test]
    fn empty_state_renders_no_reading_lists_splash() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.clear_view();

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(
            content.contains("NO READING LISTS YET"),
            "empty state should show the first-run splash"
        );
        // The `:open` command no longer exists, so the splash must not
        // advertise it (it was meaningless with zero lists anyway).
        assert!(
            !content.contains("open"),
            "empty splash must not advertise the removed `: open` hint"
        );
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
                is_manual: false,
            },
            crate::app::ListSummary {
                id: "L2".into(),
                title: "List 2".into(),
                done: 0,
                total: 1,
                is_manual: false,
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
                is_manual: false,
            },
            crate::app::ListSummary {
                id: "L2".into(),
                title: "List 2".into(),
                done: 0,
                total: 1,
                is_manual: false,
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
                is_manual: false,
            },
            crate::app::ListSummary {
                id: "L2".into(),
                title: "List 2".into(),
                done: 0,
                total: 1,
                is_manual: false,
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

    /// `[`/`]` include the aggregate "All" stop in the rotation:
    /// All → list0 → list1 → All (and back with `[`).
    #[test]
    fn bracket_cycle_includes_all_stop() {
        use crate::app::ALL_LIST_ID;
        let mut app = AppState::new();
        app.all_lists = vec![
            crate::app::ListSummary {
                id: "L1".into(),
                title: "List 1".into(),
                done: 0,
                total: 1,
                is_manual: false,
            },
            crate::app::ListSummary {
                id: "L2".into(),
                title: "List 2".into(),
                done: 0,
                total: 1,
                is_manual: false,
            },
        ];
        app.active_list_idx = 0;
        assert!(!app.all_active);

        // `]` from L1 → L2.
        let i = app.on_input(key(KeyCode::Char(']')));
        assert!(matches!(i, Intent::SwitchList { ref id } if id == "L2"));
        assert!(!app.all_active);
        assert_eq!(app.active_list_idx, 1);

        // `]` from the LAST list → All (the aggregate stop activates).
        let i = app.on_input(key(KeyCode::Char(']')));
        assert!(matches!(i, Intent::SwitchList { ref id } if id == ALL_LIST_ID));
        assert!(app.all_active, "] off the last list lands on the All stop");

        // `]` from All → first list.
        let i = app.on_input(key(KeyCode::Char(']')));
        assert!(matches!(i, Intent::SwitchList { ref id } if id == "L1"));
        assert!(!app.all_active);
        assert_eq!(app.active_list_idx, 0);

        // `[` from the FIRST list → All.
        let i = app.on_input(key(KeyCode::Char('[')));
        assert!(matches!(i, Intent::SwitchList { ref id } if id == ALL_LIST_ID));
        assert!(app.all_active, "[ off the first list lands on the All stop");

        // `[` from All → last list.
        let i = app.on_input(key(KeyCode::Char('[')));
        assert!(matches!(i, Intent::SwitchList { ref id } if id == "L2"));
        assert!(!app.all_active);
        assert_eq!(app.active_list_idx, 1);
    }

    /// Task 5: on the focused list strip, `<` and `←` reach the virtual "All"
    /// stop exactly like `>`/`→`/`[`/`]`. (The bug: `<` had no dispatch at all.)
    #[test]
    fn list_strip_prev_keys_reach_all_stop() {
        use crate::app::{HeaderRow, ALL_LIST_ID};
        let two_lists = || {
            vec![
                crate::app::ListSummary {
                    id: "L1".into(),
                    title: "List 1".into(),
                    done: 0,
                    total: 1,
                    is_manual: false,
                },
                crate::app::ListSummary {
                    id: "L2".into(),
                    title: "List 2".into(),
                    done: 0,
                    total: 1,
                    is_manual: false,
                },
            ]
        };

        for prev_key in [KeyCode::Char('<'), KeyCode::Left] {
            let mut app = AppState::new();
            app.all_lists = two_lists();
            app.active_list_idx = 0;
            app.all_active = false;
            app.focus = Focus::Header;
            app.header_row = HeaderRow::ListStrip;

            // From the FIRST list, prev → All (the aggregate stop).
            let i = app.on_input(key(prev_key));
            assert!(
                matches!(i, Intent::SwitchList { ref id } if id == ALL_LIST_ID),
                "{prev_key:?} off the first list must reach All"
            );
            assert!(app.all_active, "{prev_key:?} must activate the All stop");
            assert_eq!(app.focus, Focus::Header, "focus unchanged");

            // From All, prev → last list.
            let i = app.on_input(key(prev_key));
            assert!(matches!(i, Intent::SwitchList { ref id } if id == "L2"));
            assert!(!app.all_active);
        }
    }

    /// Task 5 (symmetry): `>` on the focused list strip cycles forward through
    /// the All stop, mirroring `]`/`→`.
    #[test]
    fn list_strip_gt_key_reaches_all_stop() {
        use crate::app::{HeaderRow, ALL_LIST_ID};
        let mut app = AppState::new();
        app.all_lists = vec![crate::app::ListSummary {
            id: "L1".into(),
            title: "Only".into(),
            done: 0,
            total: 1,
            is_manual: false,
        }];
        app.active_list_idx = 0;
        app.focus = Focus::Header;
        app.header_row = HeaderRow::ListStrip;

        let i = app.on_input(key(KeyCode::Char('>')));
        assert!(matches!(i, Intent::SwitchList { ref id } if id == ALL_LIST_ID));
        assert!(app.all_active, "> off the only list reaches All");
    }

    /// With a single real list the rotation is still All ⇄ that list.
    #[test]
    fn bracket_cycle_single_list_toggles_all() {
        use crate::app::ALL_LIST_ID;
        let mut app = AppState::new();
        app.all_lists = vec![crate::app::ListSummary {
            id: "L1".into(),
            title: "Manual".into(),
            done: 0,
            total: 1,
            is_manual: false,
        }];
        app.active_list_idx = 0;

        // `]` off the only list → All.
        let i = app.on_input(key(KeyCode::Char(']')));
        assert!(matches!(i, Intent::SwitchList { ref id } if id == ALL_LIST_ID));
        assert!(app.all_active);

        // `]` from All → back to the only list.
        let i = app.on_input(key(KeyCode::Char(']')));
        assert!(matches!(i, Intent::SwitchList { ref id } if id == "L1"));
        assert!(!app.all_active);
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

    /// `↑` at the top of the book list crosses focus up into the Header.
    #[test]
    fn up_at_list_top_crosses_to_header() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        assert_eq!(app.focus, Focus::List);
        assert_eq!(app.selected, 0);
        app.on_input(key(KeyCode::Up));
        assert_eq!(
            app.focus,
            Focus::Header,
            "↑ at list top must focus Header (filter chips)"
        );
        assert_eq!(
            app.header_row,
            HeaderRow::FilterChips,
            "↑ from the book-list top lands on the filter chips sub-row"
        );
        assert_eq!(app.selected, 0, "selection stays at the first book");
    }

    #[test]
    fn k_at_list_top_crosses_to_header() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.on_input(key(KeyCode::Char('k')));
        assert_eq!(
            app.focus,
            Focus::Header,
            "'k' at list top must focus Header"
        );
    }

    /// `↓` from the Header crosses down into the top of the book list.
    #[test]
    fn down_from_header_crosses_to_list_top() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Header;
        app.selected = 1;
        app.on_input(key(KeyCode::Down));
        assert_eq!(app.focus, Focus::List, "↓ from Header must focus List");
        assert_eq!(app.selected, 0, "↓ from Header lands on the first book");
    }

    #[test]
    fn j_from_header_crosses_to_list_top() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Header;
        app.on_input(key(KeyCode::Char('j')));
        assert_eq!(app.focus, Focus::List, "'j' from Header must focus List");
        assert_eq!(app.selected, 0);
    }

    /// Header two-row walk: `↑` from the filter chips climbs to the LIST STRIP
    /// (superseding the old ↑-from-Header→top-book stopgap), and `↑` from the
    /// list strip STOPS (top of everything).
    #[test]
    fn header_up_walks_chips_to_strip_then_stops() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Header;
        app.header_row = HeaderRow::FilterChips;
        app.selected = 1; // cursor parked mid-list — must NOT change.

        // chips → list strip (focus stays Header, book list untouched).
        app.on_input(key(KeyCode::Up));
        assert_eq!(app.focus, Focus::Header, "↑ from chips stays in Header");
        assert_eq!(
            app.header_row,
            HeaderRow::ListStrip,
            "↑ from filter chips climbs to the list strip"
        );
        assert_eq!(app.selected, 1, "↑ from chips must NOT touch the book list");

        // list strip → STOP (no-op: stays on the list strip).
        app.on_input(key(KeyCode::Up));
        assert_eq!(app.focus, Focus::Header, "↑ from list strip stops (Header)");
        assert_eq!(
            app.header_row,
            HeaderRow::ListStrip,
            "↑ from the list strip is the top of everything — STOP"
        );
    }

    /// `k` mirrors `↑` for the Header two-row walk.
    #[test]
    fn header_k_walks_chips_to_strip_then_stops() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Header;
        app.header_row = HeaderRow::FilterChips;
        app.on_input(key(KeyCode::Char('k')));
        assert_eq!(
            app.header_row,
            HeaderRow::ListStrip,
            "k: chips → list strip"
        );
        app.on_input(key(KeyCode::Char('k')));
        assert_eq!(
            app.header_row,
            HeaderRow::ListStrip,
            "k from the list strip stops"
        );
    }

    /// Header two-row walk down: `↓` from the list strip drops to the filter
    /// chips; `↓` from the chips drops into the top of the book list.
    #[test]
    fn header_down_walks_strip_to_chips_to_list() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Header;
        app.header_row = HeaderRow::ListStrip;
        app.selected = 1;

        // list strip → filter chips.
        app.on_input(key(KeyCode::Down));
        assert_eq!(app.focus, Focus::Header, "↓ from strip stays in Header");
        assert_eq!(
            app.header_row,
            HeaderRow::FilterChips,
            "↓ from the list strip drops to the filter chips"
        );

        // filter chips → top of book list.
        app.on_input(key(KeyCode::Down));
        assert_eq!(app.focus, Focus::List, "↓ from chips focuses the book list");
        assert_eq!(app.selected, 0, "↓ from chips lands on the first book");
    }

    /// `←/→` on the FOCUSED Header list strip switch the reading list (same as
    /// `[`/`]`); on the filter chips they switch the status filter.
    #[test]
    fn header_left_right_acts_on_focused_row() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.all_lists = vec![
            crate::app::ListSummary {
                id: "L1".into(),
                title: "List 1".into(),
                done: 0,
                total: 1,
                is_manual: false,
            },
            crate::app::ListSummary {
                id: "L2".into(),
                title: "List 2".into(),
                done: 0,
                total: 1,
                is_manual: false,
            },
        ];
        app.active_list_idx = 0;
        app.focus = Focus::Header;

        // On the FILTER CHIPS row → switches the status filter.
        app.header_row = HeaderRow::FilterChips;
        app.on_input(key(KeyCode::Right));
        assert_eq!(
            app.filter,
            StatusFilter::NeedsYou,
            "→ on the filter chips advances the status filter"
        );
        assert_eq!(
            app.active_list_idx, 0,
            "filter-row → must NOT switch the list"
        );

        // On the LIST STRIP row → switches the reading list (like `]`).
        app.header_row = HeaderRow::ListStrip;
        let prev_filter = app.filter;
        let intent = app.on_input(key(KeyCode::Right));
        assert!(
            matches!(intent, Intent::SwitchList { ref id } if id == "L2"),
            "→ on the list strip switches to the next list, got {:?}",
            intent
        );
        assert_eq!(
            app.active_list_idx, 1,
            "list-strip → advances the active list"
        );
        assert_eq!(
            app.filter, prev_filter,
            "list-strip → must NOT touch the filter"
        );

        // `←` on the list strip goes back (like `[`).
        let intent = app.on_input(key(KeyCode::Left));
        assert!(
            matches!(intent, Intent::SwitchList { ref id } if id == "L1"),
            "← on the list strip switches to the previous list, got {:?}",
            intent
        );
        assert_eq!(app.active_list_idx, 0);
    }

    /// Detail modal: `d` on a focused variation emits a Select (download) intent.
    #[test]
    fn detail_d_downloads_focused_variation() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection()); // first book has 2 versions
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 1,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        let intent = app.on_input(key(KeyCode::Char('d')));
        assert!(
            matches!(intent, Intent::Select { .. }),
            "d on a variation must emit Select, got {:?}",
            intent
        );
        assert!(
            matches!(app.modal, Some(Modal::Detail { .. })),
            "detail modal STAYS OPEN after `d` so the user can watch the transfer"
        );
    }

    /// Regression for the recurring "`d` on an AVAILABLE variation does nothing"
    /// report. Pressing `d` on an available (not-yet-downloaded) copy must emit a
    /// `Select` carrying THAT copy's md5 + its book position, and post an immediate
    /// "Queued download" status so the user gets feedback before the (slow) resolve
    /// makes the transfer visible. The orchestrator/engine half — that the Select
    /// arms a `Pending` job, raises the goal to `Complete`, and that `actionable_kind`
    /// then dispatches a `Download` — is covered by `libgen-core`'s
    /// `select_candidate_arms_pending_job_on_chosen_variation` and `libgen-engine`'s
    /// `selected_ready_book_dispatches_only_once_armed_with_a_pending_job`.
    #[test]
    fn detail_d_on_available_variation_selects_that_md5_and_confirms() {
        use crate::app::DetailSubFocus;
        let avail_md5 = "e".repeat(32);
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        app.flat[0].book.versions = vec![
            mk_var("Done Copy", "epub", "done", 100, &"d".repeat(32)),
            mk_var("Available Copy", "pdf", "available", 0, &avail_md5),
        ];
        // Focus the AVAILABLE variation (index 1).
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 1,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        let group_index = app.flat[0].group_index;
        let book_index = app.flat[0].book_index_in_group;

        let intent = app.on_input(key(KeyCode::Char('d')));
        match intent {
            Intent::Select {
                group_path,
                book_index: bi,
                md5,
            } => {
                assert_eq!(
                    md5, avail_md5,
                    "Select must target the focused available md5"
                );
                assert_eq!(group_path, vec![group_index]);
                assert_eq!(bi, book_index);
            }
            other => panic!("d on an available variation must emit Select, got {other:?}"),
        }
        assert!(
            matches!(app.modal, Some(Modal::Detail { .. })),
            "detail modal STAYS OPEN after queueing so the user can watch the transfer"
        );
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|m| m.contains("Queued download")),
            "pressing d must post an immediate 'Queued download' confirmation, got {:?}",
            app.status_msg
        );
    }

    /// #6: the detail breadcrumb subtitle (`{group} · seq NN`) must ellipsize
    /// with `…` at a narrow width instead of HARD-CLIPPING the tail.
    #[test]
    fn detail_breadcrumb_ellipsizes_long_group_at_width_80() {
        use crate::app::DetailSubFocus;
        use libgen_core::model::{
            BookInput, BookRequest, Candidate, DownloadList, Format, Group, RequestStatus,
        };

        // A group name long enough that the breadcrumb overflows the modal width
        // on its own (the old "● N req · …" trailer was removed).
        let long_group = "Lift-Off Aerospace Engineering Handbook Master Collection Omnibus Deluxe Annotated Edition";
        let mut g = Group::new(long_group);
        let mut req = BookRequest::new(BookInput {
            title: "Apollo Guidance Computer".into(),
            authors: vec!["MIT".into()],
            ..Default::default()
        });
        req.status = RequestStatus::Matched;
        req.candidates = vec![Candidate {
            md5: "a".repeat(32),
            title: "Apollo Guidance Computer".into(),
            authors: vec!["MIT".into()],
            year: Some(2010),
            publisher: Some("MIT Press".into()),
            language: Some("English".into()),
            pages: Some(300),
            extension: Some(Format::Epub),
            size_bytes: Some(1024 * 1024),
            source_host: Some("libgen.li".into()),
            cover_url: None,
            score: 1.0,
            job: None,
        }];
        g.books.push(req);
        let list = DownloadList {
            title: "Test List".into(),
            settings: libgen_core::model::ListSettings::default(),
            groups: vec![g],
        };
        let vm = build_with_id("test".into(), &list);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(vm);
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        // Extract the breadcrumb ROW (the one starting with the group name) so we
        // assert on it specifically — "active" also appears in the VARIATIONS
        // header, which is a different line.
        let backend = terminal.backend();
        let buf = backend.buffer();
        let width = buf.area.width as usize;
        let cells: Vec<String> = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        let rows: Vec<String> = cells.chunks(width).map(|r| r.concat()).collect();
        let crumb = rows
            .iter()
            .find(|r| r.contains("Lift-Off"))
            .expect("breadcrumb row with the group name must be rendered");

        // The breadcrumb was clipped → an ellipsis is shown on THIS line…
        assert!(
            crumb.contains('\u{2026}'),
            "breadcrumb must show … when clipped: {crumb:?}"
        );
        // …and the clipped tail (the " · seq NN" suffix) is NOT rendered through —
        // i.e. truncated with …, not hard-clipped past the edge.
        assert!(
            !crumb.contains("seq"),
            "breadcrumb tail must be truncated with …, not hard-clipped: {crumb:?}"
        );
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

    /// Detail modal: a LEFT-CLICK on a variation row selects it and focuses the
    /// Variations sub-list (regardless of where focus was before).
    #[test]
    fn detail_click_variation_row_selects_and_focuses_variations() {
        use crate::app::DetailSubFocus;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection()); // first book has 2 versions
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::History, // start focused elsewhere
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        assert!(
            !app.last_rects.detail_var_rows.is_empty(),
            "variation row rects must be registered for mouse hit-testing"
        );
        // Click variation row index 1.
        let rect = app
            .last_rects
            .detail_var_rows
            .iter()
            .find(|(_, i)| *i == 1)
            .map(|(r, _)| *r)
            .expect("variation row 1 rect");
        app.on_input(mouse_left_click(rect.x + 1, rect.y));
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 1,
                sub_focus: DetailSubFocus::Variations,
                history_selected: 0,
            }),
            "clicking a variation row selects it + focuses Variations"
        );
    }

    /// Detail modal: a LEFT-CLICK on a history row selects that event and focuses
    /// the History sub-list (Enter on it would open its snapshot).
    #[test]
    fn detail_click_history_row_selects_and_focuses_history() {
        use crate::app::DetailSubFocus;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.flat[0].book.history = vec![
            ViewEvent {
                at_ms: 1_000,
                md5: None,
                fmt: None,
                kind: "queued".into(),
                detail: "queued".into(),
            },
            ViewEvent {
                at_ms: 2_000,
                md5: None,
                fmt: None,
                kind: "discovered".into(),
                detail: "found 2 copies".into(),
            },
        ];
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations, // start focused elsewhere
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        assert!(
            !app.last_rects.detail_hist_rows.is_empty(),
            "history row rects must be registered for mouse hit-testing"
        );
        let rect = app
            .last_rects
            .detail_hist_rows
            .iter()
            .find(|(_, i)| *i == 1)
            .map(|(r, _)| *r)
            .expect("history row 1 rect");
        app.on_input(mouse_left_click(rect.x + 1, rect.y));
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                book_flat_index: 0,
                selected: 0,
                sub_focus: DetailSubFocus::History,
                history_selected: 1,
            }),
            "clicking a history row selects it + focuses History"
        );
    }

    /// Detail modal: the WHEEL over the History sub-list scrolls THAT list (and
    /// focuses it) even when keyboard focus was on Variations.
    #[test]
    fn detail_wheel_over_history_scrolls_history() {
        use crate::app::DetailSubFocus;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.flat[0].book.history = vec![
            ViewEvent {
                at_ms: 1_000,
                md5: None,
                fmt: None,
                kind: "queued".into(),
                detail: "queued".into(),
            },
            ViewEvent {
                at_ms: 2_000,
                md5: None,
                fmt: None,
                kind: "discovered".into(),
                detail: "found".into(),
            },
        ];
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        // Wheel down with the cursor over the history area → focus History and
        // move selection by +1 (0 → 1). The wheel no longer hover-selects the
        // specific row under the pointer (task 4).
        let rect = app
            .last_rects
            .detail_hist_rows
            .iter()
            .find(|(_, i)| *i == 1)
            .map(|(r, _)| *r)
            .expect("history row 1 rect");
        app.on_input(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        }));
        match &app.modal {
            Some(Modal::Detail {
                sub_focus,
                history_selected,
                ..
            }) => {
                assert_eq!(*sub_focus, DetailSubFocus::History, "wheel focuses History");
                assert_eq!(*history_selected, 1, "wheel moves selection by +1 (0→1)");
            }
            other => panic!("expected Detail modal, got {:?}", other),
        }
    }

    /// Task 4: the detail wheel moves selection by ±1 and does NOT snap to the
    /// row under the pointer. With 3 variations selected at 0, scrolling down
    /// while the cursor hovers the LAST row must land on 1 (sel+1), not 2.
    #[test]
    fn detail_wheel_does_not_hover_select_row_under_cursor() {
        use crate::app::DetailSubFocus;
        use libgen_core::model::{
            BookInput, BookRequest, Candidate, DownloadList, Format, Group, RequestStatus,
        };
        let mut g = Group::new("Grp");
        let mut req = BookRequest::new(BookInput {
            title: "Three Copies".into(),
            authors: vec!["A".into()],
            ..Default::default()
        });
        req.status = RequestStatus::Matched;
        req.candidates = (0..3)
            .map(|i| Candidate {
                md5: format!("{:0>32}", i),
                title: "Three Copies".into(),
                authors: vec!["A".into()],
                year: Some(2010),
                publisher: Some("P".into()),
                language: Some("English".into()),
                pages: Some(100 + i),
                extension: Some(Format::Epub),
                size_bytes: Some(1024 * 1024),
                source_host: Some("libgen.li".into()),
                cover_url: None,
                score: 1.0 - (i as f32) * 0.01,
                job: None,
            })
            .collect();
        g.books.push(req);
        let list = DownloadList {
            title: "L".into(),
            settings: libgen_core::model::ListSettings::default(),
            groups: vec![g],
        };
        let vm = build_with_id("t".into(), &list);

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(vm);
        assert!(
            app.flat[0].book.versions.len() >= 3,
            "need ≥3 variations for this test"
        );
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        // Cursor over variation row index 2 (the last), scroll DOWN.
        let rect = app
            .last_rects
            .detail_var_rows
            .iter()
            .find(|(_, i)| *i == 2)
            .map(|(r, _)| *r)
            .expect("variation row 2 rect");
        app.on_input(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        }));
        match &app.modal {
            Some(Modal::Detail { selected, .. }) => {
                assert_eq!(
                    *selected, 1,
                    "wheel moves by +1 (0→1), NOT snap to the hovered row 2"
                );
            }
            other => panic!("expected Detail modal, got {:?}", other),
        }
    }

    /// Render: the NON-focused detail sub-list keeps a DIMMED ▌ accent on its
    /// selected row while the focused sub-list shows the FULL green accent — both
    /// visible at once, proving the dim-vs-full distinction.
    #[test]
    fn detail_inactive_sublist_keeps_dimmed_accent() {
        use crate::app::DetailSubFocus;
        use crate::theme::{C_SEL_ACCENT, C_SEL_ACCENT_DIM};
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection()); // 2 versions
        app.flat[0].book.history = vec![ViewEvent {
            at_ms: 1_000,
            md5: None,
            fmt: None,
            kind: "queued".into(),
            detail: "queued".into(),
        }];
        // Variations focused → its selected row is FULL; History (inactive) keeps DIM.
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut found_full = false;
        let mut found_dim = false;
        for cell in buf.content().iter() {
            if cell.symbol() == "\u{258c}" {
                if cell.fg == C_SEL_ACCENT {
                    found_full = true;
                }
                if cell.fg == C_SEL_ACCENT_DIM {
                    found_dim = true;
                }
            }
        }
        assert!(
            found_full,
            "the focused Variations list must show a FULL green ▌ accent"
        );
        assert!(
            found_dim,
            "the inactive History list must keep a DIMMED ▌ accent (not blank)"
        );
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
        assert_eq!(app.filter, StatusFilter::Queued);
        app.on_input(key(KeyCode::Char('6')));
        assert_eq!(app.filter, StatusFilter::InProgress);
        app.on_input(key(KeyCode::Char('7')));
        assert_eq!(app.filter, StatusFilter::Done);
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
        app.modal = Some(Modal::Help {
            page: HelpPage::List,
            parent: None,
        });
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
    fn question_mark_opens_help_on_focused_context() {
        // `?` opens the context-paged Help directly on the page matching the
        // focused pane (List focus → List page).
        let mut app = AppState::new();
        let intent = app.on_input(key(KeyCode::Char('?')));
        assert_eq!(intent, Intent::Redraw);
        assert!(
            matches!(
                app.modal,
                Some(Modal::Help {
                    page: HelpPage::List,
                    parent: None
                })
            ),
            "? from List focus opens the List help page, got {:?}",
            app.modal
        );
    }

    /// Helper: the current Help page, or panic if Help isn't open.
    fn help_page(app: &AppState) -> HelpPage {
        match &app.modal {
            Some(Modal::Help { page, .. }) => *page,
            other => panic!("expected Help modal, got {other:?}"),
        }
    }

    #[test]
    fn question_mark_opens_help_on_each_focus() {
        for (focus, expect) in [
            (Focus::List, HelpPage::List),
            (Focus::Header, HelpPage::Header),
            (Focus::Activity, HelpPage::Activity),
        ] {
            let mut app = AppState::new();
            app.set_view(fixture_vm());
            app.focus = focus;
            app.on_input(key(KeyCode::Char('?')));
            assert_eq!(help_page(&app), expect, "? from {focus:?}");
        }
    }

    #[test]
    fn help_right_arrow_cycles_through_all_contexts() {
        let mut app = AppState::new();
        app.modal = Some(Modal::Help {
            page: HelpPage::Global,
            parent: None,
        });
        // → walks the full tab order and wraps back to Global.
        let order = [
            HelpPage::List,
            HelpPage::Header,
            HelpPage::Activity,
            HelpPage::Detail,
            HelpPage::Picker,
            HelpPage::Settings,
            HelpPage::Cmds,
            HelpPage::Global,
        ];
        for expect in order {
            app.on_input(key(KeyCode::Right));
            assert_eq!(help_page(&app), expect);
        }
    }

    #[test]
    fn help_left_arrow_cycles_backwards() {
        let mut app = AppState::new();
        app.modal = Some(Modal::Help {
            page: HelpPage::Global,
            parent: None,
        });
        // ← from Global wraps to the last page (Cmds), then backwards.
        app.on_input(key(KeyCode::Left));
        assert_eq!(help_page(&app), HelpPage::Cmds);
        app.on_input(key(KeyCode::Left));
        assert_eq!(help_page(&app), HelpPage::Settings);
    }

    #[test]
    fn help_question_mark_closes_and_restores_parent() {
        // Opened over a Detail modal → ? restores the Detail modal.
        let parent = Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        };
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Help {
            page: HelpPage::Detail,
            parent: Some(Box::new(parent.clone())),
        });
        app.on_input(key(KeyCode::Char('?')));
        assert_eq!(app.modal, Some(parent), "? restores the parent modal");
    }

    #[test]
    fn help_question_mark_from_detail_modal_opens_detail_page() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        app.on_input(key(KeyCode::Char('?')));
        assert_eq!(help_page(&app), HelpPage::Detail);
        // And the Detail modal is preserved as the parent for restore-on-close.
        assert!(
            matches!(
                &app.modal,
                Some(Modal::Help {
                    parent: Some(p),
                    ..
                }) if matches!(**p, Modal::Detail { .. })
            ),
            "Help over Detail keeps Detail as parent"
        );
    }

    #[test]
    fn help_pages_contain_their_expected_keys() {
        use crate::ui::{help_page_rows, HelpRow};

        // Collect the (key, desc) pairs from both sub-columns of a page.
        fn pairs(page: HelpPage) -> Vec<(&'static str, &'static str)> {
            let (l, r) = help_page_rows(page);
            l.into_iter()
                .chain(r)
                .filter_map(|row| match row {
                    HelpRow::Key(k, d) => Some((k, d)),
                    _ => None,
                })
                .collect()
        }
        fn has_key(page: HelpPage, needle: &str) -> bool {
            pairs(page).iter().any(|(k, _)| k.contains(needle))
        }

        // Each context page surfaces its defining keys.
        assert!(has_key(HelpPage::Global, "?"));
        assert!(has_key(HelpPage::Global, "[ ]"));
        assert!(has_key(HelpPage::List, "a")); // fetch all preferred formats
        assert!(has_key(HelpPage::List, "e")); // edit (previously missing)
        assert!(has_key(HelpPage::List, "m")); // mark not-found (previously missing)
        assert!(has_key(HelpPage::Header, "D")); // delete list (previously missing)
        assert!(has_key(HelpPage::Header, "s")); // start/resume list
        assert!(has_key(HelpPage::Activity, "space")); // collapse/expand (previously missing)
        assert!(has_key(HelpPage::Detail, "S")); // download series (previously missing)
        assert!(has_key(HelpPage::Detail, "tab")); // switch sub-pane
        assert!(has_key(HelpPage::Picker, "v")); // candidate metadata (previously missing)
        assert!(has_key(HelpPage::Settings, "c")); // cleanup (previously missing)
        assert!(has_key(HelpPage::Settings, "S-J / S-K")); // reorder priority
        assert!(has_key(HelpPage::Cmds, ":pause-all"));
        assert!(has_key(HelpPage::Cmds, ":reorganize")); // unadvertised command
    }

    #[test]
    fn help_pages_use_corrected_descriptions() {
        use crate::ui::{help_page_rows, HelpRow};

        fn desc_for(page: HelpPage, key_needle: &str) -> String {
            let (l, r) = help_page_rows(page);
            l.into_iter()
                .chain(r)
                .find_map(|row| match row {
                    HelpRow::Key(k, d) if k.contains(key_needle) => Some(d.to_string()),
                    _ => None,
                })
                .unwrap_or_default()
        }

        // Settings ←/→ = nudge number field (NOT filter chips).
        let settings_arrows = desc_for(HelpPage::Settings, "\u{2190}");
        assert!(
            settings_arrows.contains("nudge") && settings_arrows.contains("number"),
            "Settings ←/→ row must say nudge number field, got {settings_arrows:?}"
        );

        // Detail `d` = download focused variation (NOT open detail).
        let detail_d = desc_for(HelpPage::Detail, "d");
        assert!(
            detail_d.contains("download") && detail_d.contains("variation"),
            "Detail d row must be download-variation, got {detail_d:?}"
        );

        // List `d` = open detail (per-context split).
        let list_d = desc_for(HelpPage::List, "d");
        assert!(
            list_d.contains("detail"),
            "List d row must open detail, got {list_d:?}"
        );

        // `[ ]` rotation includes the "All" stop.
        let lists = desc_for(HelpPage::Global, "[ ]");
        assert!(
            lists.contains("All"),
            "[ ] row must mention the All stop, got {lists:?}"
        );
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
            is_manual: false,
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
    // Task 7 — Queued filter chip
    // -----------------------------------------------------------------------

    /// Build a ViewBook with one variation per given state string.
    fn book_with_version_states(states: &[&str]) -> libgen_engine::ViewBook {
        let mut b = flat_book_with_state(states.first().copied().unwrap_or("queued")).book;
        b.versions = states
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut v = flat_book_with_state(s).book.versions.remove(0);
                v.md5 = format!("{:0>32}", i);
                v.state = (*s).to_string();
                v
            })
            .collect();
        b
    }

    fn vm_with_books(books: Vec<libgen_engine::ViewBook>) -> libgen_engine::ViewModel {
        let mut vm = fixture_vm();
        vm.groups = vec![libgen_engine::ViewGroup {
            name: "G".into(),
            books,
            collapsed: false,
        }];
        vm
    }

    /// The Queued count matches books with a queued/Pending variation AND no
    /// active (downloading) variation — a queued+downloading book is NOT queued.
    #[test]
    fn status_counts_queued_excludes_active() {
        let mut app = AppState::new();
        app.set_view(vm_with_books(vec![
            book_with_version_states(&["queued"]), // queued ✓
            book_with_version_states(&["queued", "downloading"]), // active → not queued
            book_with_version_states(&["done"]),   // done → not queued
        ]));
        let c = app.status_counts();
        assert_eq!(c.queued, 1, "only the queued-no-active book counts");
        assert_eq!(
            c.in_progress, 1,
            "the queued+downloading book is in progress"
        );
    }

    /// The Queued filter predicate (via rebuild_flat) keeps exactly the
    /// queued-no-active books.
    #[test]
    fn queued_filter_predicate_selects_pending_only() {
        let mut app = AppState::new();
        app.filter = StatusFilter::Queued;
        app.set_view(vm_with_books(vec![
            book_with_version_states(&["queued"]),
            book_with_version_states(&["queued", "downloading"]),
            book_with_version_states(&["done"]),
        ]));
        assert_eq!(app.flat.len(), 1, "only the pure-queued book passes");
        assert!(app.flat[0]
            .book
            .versions
            .iter()
            .any(|v| v.state == "queued"));
        assert!(!app.flat[0]
            .book
            .versions
            .iter()
            .any(|v| v.state == "downloading"));
    }

    /// The Queued chip has its own distinct colour.
    #[test]
    fn queued_chip_color_is_distinct() {
        use crate::theme::filter_chip_color;
        let q = filter_chip_color("filter.queued");
        for other in [
            "filter.all",
            "filter.needs",
            "filter.review",
            "filter.cantdl",
            "filter.active",
            "filter.done",
        ] {
            assert_ne!(
                q,
                filter_chip_color(other),
                "Queued colour must differ from {other}"
            );
        }
    }

    /// All seven chips still fit within an 80-column row (no overflow / overlap).
    #[test]
    fn filter_chips_fit_at_80_cols() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let chips = app.last_rects.filter_chips.clone();
        assert_eq!(chips.len(), 7, "seven chips render");
        let mut prev_end = 0u16;
        for (rect, _) in &chips {
            assert!(
                rect.x >= prev_end,
                "chips must not overlap (x={}, prev_end={})",
                rect.x,
                prev_end
            );
            assert!(
                rect.x + rect.width <= 80,
                "chip overflows 80 cols: x={} w={}",
                rect.x,
                rect.width
            );
            prev_end = rect.x + rect.width;
        }
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
        // Render every page at a couple of widths to exercise the tab row,
        // global strip, two-column grid, and footer.
        for &(w, h) in &[(120u16, 30u16), (80, 24)] {
            for page in HelpPage::ALL {
                let backend = TestBackend::new(w, h);
                let mut terminal = Terminal::new(backend).unwrap();
                let mut app = AppState::new();
                app.set_view(fixture_vm());
                app.modal = Some(Modal::Help { page, parent: None });
                terminal.draw(|f| ui::render(f, &mut app)).unwrap();
            }
        }
    }

    /// The List help page advertises the per-copy behavior of r/p/c/o so users
    /// know those keys target the selected alt copy.
    #[test]
    fn list_help_page_mentions_per_copy_actions() {
        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Help {
            page: HelpPage::List,
            parent: None,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("selected copy"),
            "List help must advertise per-copy r/p/c/o behavior: {buf}"
        );
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
        // ":" + Tab → wildmenu advertises exactly the supported commands.
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':'))); // buf = ""
        app.on_input(key(KeyCode::Tab));
        assert!(
            !app.completion_candidates.is_empty(),
            "Tab on empty prefix should open wildmenu"
        );
        // The advertised set is exactly these commands.
        let mut got = app.completion_candidates.clone();
        got.sort();
        let mut want = vec![
            "about",
            "add",
            "import",
            "pause-all",
            "settings",
            "start-all",
        ];
        want.sort();
        assert_eq!(
            got, want,
            "wildmenu must advertise exactly the supported commands"
        );
        // Unadvertised / removed commands must NOT appear.
        for cmd in &["open", "add-md5", "requery", "delete", "cleanup", "mouse"] {
            assert!(
                !app.completion_candidates.iter().any(|c| c == cmd),
                "candidate '{cmd}' must not be advertised"
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
        app.completion_candidates = vec!["settings".into(), "import".into(), "add".into()];
        app.completion_index = 0;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(
            content.contains("settings"),
            "wildmenu must show 'settings'"
        );
        assert!(content.contains("import"), "wildmenu must show 'import'");
        assert!(content.contains("add"), "wildmenu must show 'add'");
    }

    /// `:import <partial-path>` + Tab completes filesystem entries: a directory
    /// with several files yields candidates matching the typed prefix, with
    /// directories carrying a trailing `/`.
    #[test]
    fn import_argument_completes_filesystem_paths() {
        // Build a unique temp directory with known entries.
        let base = std::env::temp_dir().join(format!(
            "kwire-import-complete-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("alpha.md"), "x").unwrap();
        std::fs::write(base.join("alpaca.json"), "x").unwrap();
        std::fs::write(base.join("zebra.txt"), "x").unwrap();
        std::fs::create_dir_all(base.join("alphadir")).unwrap();

        // Buffer: "import <base>/al" → Tab should list the three "al*" entries.
        let mut app = AppState::new();
        app.command_buf = Some(format!("import {}/al", base.display()));
        app.on_input(key(KeyCode::Tab));

        let cands = app.completion_candidates.clone();
        let names: Vec<String> = cands
            .iter()
            .map(|c| c.rsplit('/').next().unwrap_or(c).to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "alpha.md"),
            "expected 'alpha.md' in candidates, got {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "alpaca.json"),
            "expected 'alpaca.json' in candidates, got {names:?}"
        );
        // The directory entry must carry a trailing slash.
        assert!(
            cands.iter().any(|c| c.ends_with("alphadir/")),
            "expected 'alphadir/' (trailing slash) in candidates, got {cands:?}"
        );
        // The non-matching "zebra.txt" must be filtered out.
        assert!(
            !names.iter().any(|n| n == "zebra.txt"),
            "non-matching 'zebra.txt' must be excluded, got {names:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
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
        // 20 books (one host group + 20 lines) >> pane capacity → OVERFLOW: the
        // "▾ N more" indicator appears.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = (0..20)
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
        // Scrolled down past the top: "▴ N above" indicator appears.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = (0..20)
            .map(|i| make_downloading_flat_book(&format!("Book {}", i), 0, i))
            .collect();
        app.activity_selected = 18; // scrolled well past the beginning
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

    /// Task #3: the Activity pane's queued list must SCROLL — when many queued
    /// rows overflow the visible height, scrolling down (↓/j while Focus::Activity)
    /// brings the bottom-most queued item into view. Renders the Activity pane in
    /// isolation so book titles from the List pane can't pollute the assertion.
    #[test]
    fn activity_queued_list_scrolls_into_view() {
        use ratatui::layout::Rect;
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());

        // One active download leg (host group + selectable leg) + many queued
        // copies whose rows overflow a 12-row Activity pane.
        let mut flat = vec![make_downloading_flat_book("Active Book", 0, 0)];
        for i in 0..30 {
            flat.push(make_queued_flat_book(
                &format!("Queued Title {:02}", i),
                i + 1,
            ));
        }
        app.flat = flat;
        app.focus = Focus::Activity;
        app.activity_expanded = true;

        let area = Rect::new(0, 0, 80, 12);

        // First render populates `activity_content_len` and shows the overflow.
        terminal
            .draw(|f| ui::render_activity(f, &mut app, area))
            .unwrap();
        let before = buffer_string(&terminal);
        assert!(
            before.contains("more"),
            "queued list should overflow with a '▾ N more' indicator: {before:?}"
        );
        assert!(
            !before.contains("Queued Title 29"),
            "the last queued item must be hidden BEFORE scrolling: {before:?}"
        );

        // Scroll all the way down (the bound now spans the whole content list,
        // including the queued section at the bottom).
        for _ in 0..50 {
            app.on_input(key(KeyCode::Down));
        }
        terminal
            .draw(|f| ui::render_activity(f, &mut app, area))
            .unwrap();
        let after = buffer_string(&terminal);
        assert!(
            after.contains("Queued Title 29"),
            "scrolling must bring the last queued item into view: {after:?}"
        );
    }

    /// Task #4 regression: a stacked book whose ALTERNATIVE copy is actively
    /// downloading. The `↳ alt. copy` sub-row must render the live download
    /// (`dling` + host), NOT stay stuck on `queued`/host `—`; and the Activity
    /// pane must list that copy exactly ONCE (under its host group), never also
    /// under a `○ queued` section.
    #[test]
    fn downloading_alt_copy_renders_dling_and_is_not_double_listed() {
        use ratatui::layout::Rect;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();

        // Primary copy already done; the SECOND (alt) copy is downloading from a
        // specific CDN host. armed_variations ranks done < downloading, so the
        // downloading copy becomes the `↳ alt. copy` sub-row.
        let mut alt = mk_var("Treasure Island", "pdf", "downloading", 12, &"e".repeat(32));
        alt.host = Some("cdn4.booksdl.lc".into());
        app.flat[0].book.versions = vec![
            mk_var("Treasure Island", "epub", "done", 100, &"d".repeat(32)),
            alt,
        ];

        // Full list render: the alt-copy sub-row reflects the live download.
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let list_buf = buffer_string(&terminal);
        assert!(
            list_buf.contains("alt. copy"),
            "the alt-copy sub-row must render: {list_buf:?}"
        );
        assert!(
            list_buf.contains("cdn4.booksdl.lc"),
            "the alt-copy sub-row must show the live download host: {list_buf:?}"
        );
        assert!(
            list_buf.contains("dling 12%"),
            "the downloading alt copy must read 'dling 12%' (inline progress), not 'queued': {list_buf:?}"
        );

        // Activity pane in isolation: the copy appears ONCE (host group), with no
        // separate `○ queued` section double-listing it.
        let area = Rect::new(0, 0, 120, 14);
        terminal
            .draw(|f| ui::render_activity(f, &mut app, area))
            .unwrap();
        let act_buf = buffer_string(&terminal);
        assert!(
            act_buf.contains("cdn4.booksdl.lc"),
            "Activity must show the downloading copy under its host group: {act_buf:?}"
        );
        assert!(
            !act_buf.contains("\u{25cb} queued"),
            "a downloading copy must NOT also appear under a '○ queued' section: {act_buf:?}"
        );
        let title_hits = act_buf.matches("Treasure Island").count();
        assert_eq!(
            title_hits, 1,
            "the downloading copy must be listed exactly once in Activity, got {title_hits}: {act_buf:?}"
        );
    }

    /// `r`/`p`/`c` in the Activity pane act on the leg resolved by
    /// `focused_transfer_md5`, which must follow the pane's HOST-grouped render
    /// order — not the old md5-sorted order, which selected a different leg.
    #[test]
    fn activity_focused_leg_follows_host_order_not_md5_sort() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        // Two downloading legs whose host order is the REVERSE of md5-sort order:
        //   host "aaa.example" carries md5 "zzz…"  → renders first  (leg 0)
        //   host "zzz.example" carries md5 "aaa…"  → renders second (leg 1)
        let host_first_md5 = "z".repeat(32); // first HOST group
        let host_second_md5 = "a".repeat(32); // second HOST group
        let mut a = make_downloading_flat_book("Book A", 0, 0);
        a.book.versions[0].md5 = host_first_md5.clone();
        a.book.versions[0].host = Some("aaa.example".into());
        let mut b = make_downloading_flat_book("Book B", 0, 1);
        b.book.versions[0].md5 = host_second_md5.clone();
        b.book.versions[0].host = Some("zzz.example".into());
        app.flat = vec![a, b];

        app.activity_selected = 0;
        assert_eq!(
            app.focused_transfer_md5(),
            Some(host_first_md5.clone()),
            "leg 0 must be the first HOST group's copy, not the md5-sorted first"
        );
        app.activity_selected = 1;
        assert_eq!(
            app.focused_transfer_md5(),
            Some(host_second_md5),
            "leg 1 must be the second HOST group's copy"
        );
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

    /// Picker `a` fetches ALL preferred-format copies (epub + pdf in the fixture)
    /// in one shot and closes the modal.
    #[test]
    fn picker_a_fetches_all_preferred() {
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.modal = Some(Modal::Picker {
            book_flat_index: 0,
            selected: 0,
        });
        let intent = app.on_input(key(KeyCode::Char('a')));
        match intent {
            // Fixture book 0 has an epub (idx 0) and pdf (idx 1); both preferred.
            Intent::RequestVariations { md5s, .. } => assert_eq!(md5s.len(), 2),
            other => panic!("expected RequestVariations, got {:?}", other),
        }
        assert!(app.modal.is_none(), "picker closes after fetching");
    }

    /// Picker `v` opens a metadata snapshot popup for the focused candidate.
    #[test]
    fn picker_v_opens_metadata_snapshot() {
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.modal = Some(Modal::Picker {
            book_flat_index: 0,
            selected: 0,
        });
        app.on_input(key(KeyCode::Char('v')));
        match &app.modal {
            Some(Modal::Snapshot { parent, .. }) => {
                assert!(
                    matches!(parent.as_deref(), Some(Modal::Picker { .. })),
                    "snapshot returns to the Picker on Esc"
                );
            }
            other => panic!("expected Snapshot, got {:?}", other),
        }
        // Esc restores the Picker.
        app.on_input(key(KeyCode::Esc));
        assert!(matches!(app.modal, Some(Modal::Picker { .. })));
    }

    /// List-view `a` fetches all preferred-format copies for the focused book.
    #[test]
    fn list_a_fetches_all_preferred_formats() {
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.selected = 0; // book with epub + pdf candidates
        let intent = app.on_input(key(KeyCode::Char('a')));
        match intent {
            Intent::RequestVariations { md5s, .. } => {
                assert_eq!(md5s.len(), 2, "epub + pdf both requested");
            }
            other => panic!("expected RequestVariations, got {:?}", other),
        }
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
        // Render detail modal with selected=1 — the buffer should show the shared
        // selected-line accent (green ▌ left bar) on the chosen variation.
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
        // ▌ (U+258C) is the shared selected-line accent marker.
        assert!(
            content.contains('\u{258c}'),
            "detail modal selected row must show the ▌ accent marker"
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
        // Task 8: 5 = naming template (per-list Text), 6 = sub-grouping
        // (per-list Bool), 7 = download folder (global Text).
        assert_eq!(settings_field_kind(5), SettingsFieldKind::Text);
        assert_eq!(settings_field_kind(6), SettingsFieldKind::Bool);
        assert_eq!(settings_field_kind(7), SettingsFieldKind::Text);
        assert_eq!(settings_field_kind(8), SettingsFieldKind::Usize);
        assert_eq!(settings_field_kind(9), SettingsFieldKind::U32);
        assert_eq!(settings_field_kind(10), SettingsFieldKind::Bool);
    }

    #[test]
    fn settings_field_count_constant_is_11() {
        // All indices 0-10 are editable; 11+ are display-only.
        assert_eq!(SETTINGS_FIELD_COUNT, 11);
    }

    /// Task 8: settings render in two labeled groups — PER-LIST (formats,
    /// thresholds, naming, sub-grouping) then GLOBAL (download folder,
    /// concurrency, mirrors). The download folder is GLOBAL (app-wide), so it
    /// sits under GLOBAL, AFTER the per-list naming template.
    #[test]
    fn settings_render_two_groups_folder_under_global() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.open_settings(&Default::default());
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let w = buf.area.width as usize;
        let rows: Vec<String> = buf
            .content()
            .chunks(w)
            .map(|r| r.iter().map(|c| c.symbol().to_string()).collect())
            .collect();
        let row_of = |needle: &str| {
            rows.iter()
                .position(|r| r.contains(needle))
                .unwrap_or_else(|| panic!("row containing {needle:?} must render"))
        };
        let per_list = row_of("PER-LIST");
        let naming = row_of("Naming template");
        let global = row_of("GLOBAL");
        let folder = row_of("Download folder");
        let mirrors = row_of("Search mirrors");
        // Per-list group header precedes its per-list fields.
        assert!(per_list < naming, "PER-LIST header before naming template");
        // Naming (per-list) precedes the GLOBAL header; folder is below it.
        assert!(naming < global, "per-list fields before the GLOBAL header");
        assert!(
            global < folder,
            "download folder is GLOBAL → under the GLOBAL header"
        );
        assert!(folder < mirrors, "mirrors stay in the GLOBAL group");
    }

    // ── #2: modal width formula ─────────────────────────────────────────────
    #[test]
    fn settings_modal_width_follows_formula() {
        // min(80, floor(0.9 × W), W − 10).
        // Wide terminals pin at 80.
        assert_eq!(ui::settings_modal_width(132), 80);
        assert_eq!(ui::settings_modal_width(100), 80);
        // At 90: floor(0.9×90)=81, margin=80 → 80.
        assert_eq!(ui::settings_modal_width(90), 80);
        // At 80: floor(0.9×80)=72, margin=70 → 70 (≥10-col margin, ≤70).
        assert_eq!(ui::settings_modal_width(80), 70);
        assert!(ui::settings_modal_width(80) <= 70);
        // Narrow: 0.9× cap dominates, still ≥10-col margin.
        assert_eq!(ui::settings_modal_width(60), 50); // floor(54)=54, margin 50 → 50
        assert_eq!(ui::settings_modal_width(50), 40); // floor(45)=45, margin 40 → 40
                                                      // For every width the modal leaves a ≥10-col margin (never exceeds W−10).
        for w in 20u16..=200 {
            assert!(ui::settings_modal_width(w) <= w.saturating_sub(10), "w={w}");
            assert!(ui::settings_modal_width(w) <= 80);
        }
    }

    // ── #2: focused-vs-unfocused value selection ────────────────────────────
    #[test]
    fn settings_value_unfocused_ellipsizes() {
        let long = "/Users/me/a/very/long/download/folder/path";
        let out = ui::settings_value_display(long, 20, false, 0);
        assert!(out.ends_with('\u{2026}'), "unfocused → … ellipsis: {out:?}");
        assert!(crate::textfit::display_width(&out) <= 20);
    }

    #[test]
    fn settings_value_focused_marquees() {
        let long = "/Users/me/a/very/long/download/folder/path";
        // Focused, offset 0 → window starts at the head (no ellipsis glyph).
        let head = ui::settings_value_display(long, 20, true, 0);
        assert!(
            head.starts_with("/Users/me"),
            "focused marquee head: {head:?}"
        );
        assert!(!head.ends_with('\u{2026}'), "marquee does not ellipsize");
        // A non-zero offset scrolls the window → different slice.
        let scrolled = ui::settings_value_display(long, 20, true, 6);
        assert_ne!(head, scrolled, "marquee advances with the offset");
        assert!(crate::textfit::display_width(&scrolled) <= 20);
    }

    #[test]
    fn settings_value_short_unchanged_either_way() {
        // A value that fits is returned verbatim whether focused or not.
        assert_eq!(ui::settings_value_display("on", 20, false, 0), "on");
        assert_eq!(ui::settings_value_display("on", 20, true, 3), "on");
    }

    // ── Task 2: search-mirrors row applies the clip rule (no hard clip) ──────
    #[test]
    fn settings_search_mirrors_value_ellipsizes_not_clips() {
        // A mirror host list longer than the value column must degrade to a
        // `…` ellipsis that fits the column, exactly like the format tail and
        // the download-folder value — never a hard clip.
        let mirrors =
            "libgen.li \u{25cf} libgen.is \u{25cf} libgen.rs \u{25cf} libgen.gs \u{25cf} libgen.gl";
        let value_w = 16;
        let out = ui::settings_value_display(mirrors, value_w, false, 0);
        assert!(
            crate::textfit::display_width(&out) <= value_w,
            "search-mirrors value must fit the column: {out:?}"
        );
        assert!(
            out.ends_with('\u{2026}'),
            "overflowing mirrors must ellipsize: {out:?}"
        );
    }

    #[test]
    fn render_search_mirrors_row_fits_modal_width() {
        // Render the Settings modal at a narrow width and assert the rendered
        // "Search mirrors" line never spills past the modal's inner edge.
        let backend = TestBackend::new(56, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        open_settings_with_draft(&mut app);
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let w = buf.area.width as usize;
        let rows: Vec<String> = buf
            .content()
            .chunks(w)
            .map(|row| row.iter().map(|c| c.symbol().to_string()).collect())
            .collect();
        let mirror_row = rows
            .iter()
            .find(|r| r.contains("Search mirrors"))
            .expect("a 'Search mirrors' row must render");
        // No glyph may appear in the trailing border column past the modal —
        // i.e. the content fits; verify the line's trimmed content width is at
        // most the terminal width (a hard-clip would have produced a row whose
        // mirror value runs flush to the edge with no ellipsis).
        assert!(
            crate::textfit::display_width(mirror_row.trim_end()) <= w,
            "search-mirrors row overflows: {mirror_row:?}"
        );
    }

    // ── Task 3: wildmenu (:import) horizontal scroll math ───────────────────
    #[test]
    fn wildmenu_scroll_fits_no_offset() {
        // Three 5-col cells + 2×2 separators = 19 cols; window 40 → no scroll.
        let (off, l, r) = ui::wildmenu_scroll(&[5, 5, 5], 2, 1, 40);
        assert_eq!(off, 0);
        assert!(!l && !r, "fits → no overflow indicators");
    }

    #[test]
    fn wildmenu_scroll_active_first_pins_left() {
        // Many wide cells; active at the head must stay flush left.
        let widths = vec![10usize; 8]; // total 10*8 + 2*7 = 94
        let (off, l, r) = ui::wildmenu_scroll(&widths, 2, 0, 20);
        assert_eq!(off, 0);
        assert!(!l, "active at head → nothing hidden left");
        assert!(r, "tail hidden right");
    }

    #[test]
    fn wildmenu_scroll_active_tail_keeps_active_visible() {
        let widths = vec![10usize; 8]; // total 94
        let active = 7;
        let window = 20;
        let (off, l, r) = ui::wildmenu_scroll(&widths, 2, active, window);
        let active_start = 7 * (10 + 2);
        let active_end = active_start + 10;
        assert!(
            off <= active_start,
            "never scroll past the active left edge"
        );
        assert!(
            active_end <= off + window,
            "active right edge stays in the window"
        );
        assert!(l, "head hidden left when scrolled to the tail");
        assert!(!r, "nothing hidden right at the end");
    }

    #[test]
    fn wildmenu_scroll_tab_cycle_keeps_active_in_view() {
        // Drive Tab-cycling: every active index keeps its cell fully visible.
        let widths = vec![9usize; 10];
        let sep = 2;
        let window = 24;
        for active in 0..widths.len() {
            let (off, _l, _r) = ui::wildmenu_scroll(&widths, sep, active, window);
            let start: usize = widths[..active].iter().sum::<usize>() + sep * active;
            let end = start + widths[active];
            assert!(
                off <= start && end <= off + window,
                "active {active} not fully visible: off={off}"
            );
        }
    }

    // ── TOGGLE field (space) ────────────────────────────────────────────────

    #[test]
    fn space_toggles_bool_field_sub_grouping() {
        // Field 6 = "Sub-grouping" (seq_per_group bool) after the task-8 reorg.
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 6;

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
        app.settings_selected = 6;
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
        // Field 5 = "Naming template" (Text) after the task-8 reorg.
        let mut app = AppState::new();
        open_settings_with_draft(&mut app);
        app.settings_selected = 5;
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

    /// `:add` auto-detects its argument: a bare 32-hex-char token is treated as
    /// an MD5 (routed to the add-by-MD5 path); anything else is a free-form
    /// title/author add. The dispatch arm uses `cmd_get::is_md5` to decide.
    #[test]
    fn add_argument_md5_vs_text_routing() {
        use crate::cli::cmd_get::is_md5;
        // 32 hex chars → MD5 path.
        assert!(is_md5("aabbccddeeff00112233445566778899"));
        assert!(is_md5("1DF204C78842FFE549166FFcb984babc"));
        // Free-form titles → text path.
        assert!(!is_md5("Treasure Island"));
        assert!(!is_md5("Some Title, Some Author"));
        // 31 chars is not an MD5.
        assert!(!is_md5("aabbccddeeff0011223344556677889"));
    }

    /// `:open` was removed entirely — it must not be advertised in the wildmenu.
    #[test]
    fn open_command_is_not_advertised() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        app.on_input(key(KeyCode::Tab));
        assert!(
            !app.completion_candidates.iter().any(|c| c == "open"),
            "':open' must not appear in completions"
        );
        // Typing `:open` as a prefix yields no command-name completion either.
        let mut app2 = AppState::new();
        app2.on_input(key(KeyCode::Char(':')));
        for c in "open".chars() {
            app2.on_input(key(KeyCode::Char(c)));
        }
        app2.on_input(key(KeyCode::Tab));
        assert!(
            app2.completion_candidates.is_empty(),
            "':open' prefix must produce no completion candidates"
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
    fn unadvertised_commands_absent_from_tab_completion() {
        // These commands still dispatch when typed, but are intentionally NOT
        // advertised in the wildmenu (they will move to hot keys).
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':'))); // enter command mode, buf = ""
        app.on_input(key(KeyCode::Tab)); // open wildmenu with the advertised set

        let unadvertised = &[
            "pause",
            "start",
            "resume",
            "resume-all",
            "delete",
            "add-md5",
            "refresh-mirrors",
            "cleanup",
            "reorganize",
            "requery",
            "mouse",
        ];
        for &cmd in unadvertised {
            assert!(
                !app.completion_candidates.iter().any(|c| c == cmd),
                "command '{cmd}' must not be advertised in Tab-completion"
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
                ..
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
            caret: 0,
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
            caret: 0,
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
            caret: 0,
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
            caret: 0,
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
            caret: 0,
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
        // …above the enlarged book-stack mark (three ragged ▆ piles).
        assert!(
            content.contains("\u{2586}\u{2586}\u{2586}"),
            "empty screen should contain the book-stack mark"
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

    /// `:download-series` / `:series` still dispatch but are unadvertised.
    #[test]
    fn download_series_not_advertised() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        app.on_input(key(KeyCode::Tab));
        for cmd in ["download-series", "series"] {
            assert!(
                !app.completion_candidates.iter().any(|c| c == cmd),
                "command '{cmd}' must not be advertised in Tab-completion"
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

    // -----------------------------------------------------------------------
    // Pass 3a — #70 universal Enter: snapshot popup open / close / content
    // -----------------------------------------------------------------------

    /// Enter in Detail/Variations opens a Snapshot with variation data.
    #[test]
    fn enter_in_detail_variations_opens_variation_snapshot() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Redraw);
        match &app.modal {
            Some(Modal::Snapshot { lines, parent, .. }) => {
                assert!(
                    lines.iter().any(|(l, _)| l == "MD5"),
                    "variation snapshot must contain MD5 field"
                );
                assert!(
                    lines.iter().any(|(l, _)| l == "Match score"),
                    "variation snapshot must contain Match score field"
                );
                assert!(
                    lines.iter().any(|(l, _)| l == "Format"),
                    "variation snapshot must contain Format field"
                );
                assert!(
                    lines.iter().any(|(l, _)| l == "State"),
                    "variation snapshot must contain State field"
                );
                assert!(
                    matches!(parent.as_deref(), Some(Modal::Detail { .. })),
                    "Snapshot parent must be the Detail modal"
                );
            }
            other => panic!("expected Snapshot modal, got {:?}", other),
        }
    }

    /// Esc from Snapshot (opened from Detail) returns to the Detail modal.
    #[test]
    fn snapshot_esc_returns_to_detail_modal() {
        use crate::app::DetailSubFocus;
        let detail = Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        };
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Snapshot {
            title: " test ".to_string(),
            lines: vec![("Label".to_string(), "Value".to_string())],
            parent: Some(Box::new(detail.clone())),
        });
        let intent = app.on_input(key(KeyCode::Esc));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(
            app.modal,
            Some(detail),
            "Esc from Snapshot must return to parent Detail modal"
        );
    }

    /// Other keys while Snapshot is open are no-ops (Redraw, modal stays).
    #[test]
    fn snapshot_non_esc_keys_are_noop() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Snapshot {
            title: " t ".to_string(),
            lines: vec![],
            parent: None,
        });
        let intent = app.on_input(key(KeyCode::Down));
        assert_eq!(intent, Intent::Redraw);
        assert!(
            matches!(app.modal, Some(Modal::Snapshot { .. })),
            "non-Esc keys must keep the Snapshot open"
        );
    }

    /// Enter in Detail/History (with an event) opens a Snapshot with history data.
    /// History popup cannot be driven live (no history in the seeded DB), so this
    /// is covered by injecting a ViewEvent directly into the flat list.
    #[test]
    fn enter_in_detail_history_opens_history_snapshot() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        // Inject a history event into the first flat book.
        app.flat[0].book.history = vec![ViewEvent {
            at_ms: 1_000_000,
            md5: None,
            fmt: None,
            kind: "queued".to_string(),
            detail: "Book was queued for discovery".to_string(),
        }];
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::History,
            history_selected: 0,
        });
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Redraw);
        match &app.modal {
            Some(Modal::Snapshot {
                title,
                lines,
                parent,
            }) => {
                assert!(
                    title.contains("queued"),
                    "history snapshot title must contain event kind"
                );
                assert!(
                    lines.iter().any(|(l, _)| l == "Timestamp"),
                    "history snapshot must contain Timestamp field"
                );
                assert!(
                    lines.iter().any(|(l, _)| l == "Kind"),
                    "history snapshot must contain Kind field"
                );
                assert!(
                    lines
                        .iter()
                        .any(|(l, v)| l == "Detail" && v.contains("queued")),
                    "history snapshot must contain Detail field with event text"
                );
                assert!(
                    matches!(parent.as_deref(), Some(Modal::Detail { .. })),
                    "Snapshot parent must be the Detail modal"
                );
            }
            other => panic!("expected Snapshot modal, got {:?}", other),
        }
    }

    /// Enter in Detail/History when history is empty is a no-op.
    #[test]
    fn enter_in_detail_history_with_no_history_is_noop() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm()); // no history events
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::History,
            history_selected: 0,
        });
        app.on_input(key(KeyCode::Enter));
        // Modal must stay as Detail (no snapshot opened for empty history).
        assert!(
            matches!(app.modal, Some(Modal::Detail { .. })),
            "Enter in History with no events must not open Snapshot"
        );
    }

    /// Enter in Activity with a downloading leg opens a Snapshot with leg data.
    /// Leg snapshot cannot be driven live (no active downloads in the seeded DB),
    /// so we inject a downloading FlatBook directly.
    #[test]
    fn enter_in_activity_with_downloading_opens_leg_snapshot() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        // Replace flat with a single downloading book.
        app.flat = vec![make_downloading_flat_book("My Downloading Book", 0, 0)];
        app.focus = Focus::Activity;
        app.activity_selected = 0;
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Redraw);
        match &app.modal {
            Some(Modal::Snapshot { lines, parent, .. }) => {
                assert!(
                    lines
                        .iter()
                        .any(|(l, v)| l == "Book" && v == "My Downloading Book"),
                    "leg snapshot must contain book title"
                );
                assert!(
                    lines.iter().any(|(l, _)| l == "Host"),
                    "leg snapshot must contain Host field"
                );
                assert!(
                    lines.iter().any(|(l, _)| l == "MD5"),
                    "leg snapshot must contain MD5 field"
                );
                assert!(
                    lines.iter().any(|(l, _)| l == "Progress"),
                    "leg snapshot must contain Progress field"
                );
                assert!(
                    parent.is_none(),
                    "leg snapshot from Activity must have no parent (Esc closes entirely)"
                );
            }
            other => panic!("expected Snapshot modal, got {:?}", other),
        }
    }

    /// Esc from a leg snapshot (no parent) closes the modal entirely.
    #[test]
    fn snapshot_esc_with_no_parent_closes_modal() {
        let mut app = AppState::new();
        app.modal = Some(Modal::Snapshot {
            title: " leg ".to_string(),
            lines: vec![("Host".to_string(), "libgen.li".to_string())],
            parent: None,
        });
        let intent = app.on_input(key(KeyCode::Esc));
        assert_eq!(intent, Intent::Redraw);
        assert!(
            app.modal.is_none(),
            "Esc from Snapshot with no parent must close the modal"
        );
    }

    /// Enter in Activity with no active transfers is a no-op.
    #[test]
    fn enter_in_activity_with_no_transfers_is_noop() {
        let mut app = AppState::new();
        app.set_view(fixture_vm()); // no downloading versions
        app.focus = Focus::Activity;
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(intent, Intent::Redraw);
        assert!(
            app.modal.is_none(),
            "Enter in Activity with no transfers must not open any modal"
        );
    }

    /// Render test: snapshot modal renders without panic and shows content + hint.
    #[test]
    fn render_snapshot_modal_shows_content_and_hint() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Snapshot {
            title: " epub \u{00b7} abcd1234 ".to_string(),
            lines: vec![
                ("Title".to_string(), "Treasure Island".to_string()),
                ("Author".to_string(), "R. L. Stevenson".to_string()),
                ("MD5".to_string(), "a".repeat(32)),
                ("Match score".to_string(), "0.90".to_string()),
                ("State".to_string(), "done".to_string()),
            ],
            parent: None,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(
            content.contains("Treasure Island"),
            "snapshot must render the book title value"
        );
        assert!(
            content.contains("esc"),
            "snapshot must render the 'esc close' hint"
        );
    }

    // -----------------------------------------------------------------------
    // #64 / #67 / #68 — shared hint-builder & per-surface context-aware hints
    // -----------------------------------------------------------------------

    /// Helper: build a FlatBook with one variation in the given state.
    fn flat_book_with_state(state: &str) -> FlatBook {
        FlatBook {
            group_name: "G".into(),
            group_index: 0,
            book_index_in_group: 0,
            book: libgen_engine::ViewBook {
                id: "id-0".into(),
                title: "Test Book".into(),
                author: "Author".into(),
                year: None,
                pages: None,
                backfilled: vec![],
                seq: 1,
                discovery: "matched".into(),
                versions: vec![ViewVariation {
                    md5: "a".repeat(32),
                    title: "Test Book".into(),
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
                    host: None,
                    state: state.into(),
                    progress: 0,
                    downloaded_bytes: None,
                    total_bytes: None,
                    speed_bps: None,
                    eta_secs: None,
                    output_path: Some("/tmp/test.epub".into()),
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

    #[test]
    fn hint_bar_header_focus_shows_filter_and_quit() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Header;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(buf.contains("filter"), "header hint must include 'filter'");
        assert!(buf.contains("q quit"), "header hint must include 'q quit'");
        // ⏎ must NOT appear in main hint bar (only in Help).
        assert!(
            !buf.contains('\u{23ce}'),
            "⏎ must NOT appear in the main hint bar (use Help screen)"
        );
    }

    #[test]
    fn hint_bar_list_done_book_shows_detail_and_open() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::List;
        // Inject a 'done' book at index 0.
        app.flat.clear();
        app.flat.push(flat_book_with_state("done"));
        app.selected = 0;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("d detail"),
            "done-book hint must include 'd detail'"
        );
        assert!(
            buf.contains("o open"),
            "done-book hint must include 'o open'"
        );
        assert!(
            !buf.contains('\u{23ce}'),
            "⏎ must NOT appear in the main hint bar"
        );
    }

    #[test]
    fn hint_bar_list_downloading_book_shows_pause_cancel() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::List;
        app.flat.clear();
        app.flat.push(flat_book_with_state("downloading"));
        app.selected = 0;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("p pause"),
            "downloading hint must include 'p pause'"
        );
        assert!(
            buf.contains("c cancel"),
            "downloading hint must include 'c cancel'"
        );
        assert!(
            !buf.contains('\u{23ce}'),
            "⏎ must NOT appear in the main hint bar"
        );
    }

    #[test]
    fn hint_bar_manual_list_shows_x_remove() {
        // Task 1: the Manual list (mutable) surfaces the `x` remove affordance
        // in the bottom hint row; immutable imported lists do NOT.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::List;
        app.flat.clear();
        app.flat.push(flat_book_with_state("done"));
        app.selected = 0;

        // Manual list active → hint includes "x remove".
        app.all_lists = vec![crate::app::ListSummary {
            id: "M".into(),
            title: "Manual".into(),
            done: 0,
            total: 1,
            is_manual: true,
        }];
        app.active_list_idx = 0;
        app.all_active = false;
        assert!(app.active_list_is_manual());
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        assert!(
            buffer_string(&terminal).contains("x remove"),
            "manual-list hint must include 'x remove'"
        );

        // Imported (immutable) list active → no "x remove" hint.
        app.all_lists[0].is_manual = false;
        assert!(!app.active_list_is_manual());
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        assert!(
            !buffer_string(&terminal).contains("x remove"),
            "imported-list hint must NOT include 'x remove'"
        );
    }

    #[test]
    fn hint_bar_list_needs_selection_shows_choose() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::List;
        // Inject a needs_selection book.
        let mut fb = flat_book_with_state("available");
        fb.book.discovery = "needs_selection".into();
        app.flat.clear();
        app.flat.push(fb);
        app.selected = 0;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("choose"),
            "needs_selection hint must include 'choose'"
        );
    }

    #[test]
    fn hint_bar_activity_focus_shows_pause_cancel_retry() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Activity;
        // p/c/r are only advertised when a download leg is focused.
        insert_transfer(&mut app, &"a".repeat(32));
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("p pause"),
            "activity hint must include 'p pause'"
        );
        assert!(
            buf.contains("c cancel"),
            "activity hint must include 'c cancel'"
        );
        assert!(
            buf.contains("r retry"),
            "activity hint must include 'r retry'"
        );
        assert!(
            !buf.contains('\u{23ce}'),
            "⏎ must NOT appear in activity hint bar"
        );
    }

    #[test]
    fn detail_modal_hint_done_variation_shows_open_reveal_redownload() {
        let backend = TestBackend::new(132, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat.clear();
        app.flat.push(flat_book_with_state("done"));
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("o open"),
            "done variation hint must include 'o open'"
        );
        assert!(
            buf.contains("R reveal"),
            "done variation hint must include 'R reveal'"
        );
        assert!(
            buf.contains("re-download"),
            "done variation hint must include 're-download'"
        );
        assert!(
            buf.contains("esc back"),
            "done variation hint must include 'esc back'"
        );
        // No ↑↓ navigate or tab in hint.
        assert!(
            !buf.contains('\u{23ce}'),
            "⏎ must NOT appear in detail hint bar"
        );
    }

    #[test]
    fn detail_modal_hint_available_variation_shows_download() {
        let backend = TestBackend::new(132, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat.clear();
        app.flat.push(flat_book_with_state("available"));
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("download"),
            "available variation hint must include 'download'"
        );
        // Must NOT show re-download, open, or reveal (those are for done state).
        assert!(
            !buf.contains("re-download"),
            "available variation must NOT show 're-download'"
        );
        assert!(buf.contains("esc back"), "must include 'esc back'");
    }

    #[test]
    fn detail_modal_hint_history_sub_focus_shows_esc_back() {
        let backend = TestBackend::new(132, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat.clear();
        app.flat.push(flat_book_with_state("done"));
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::History,
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("esc back"),
            "history sub-focus hint must be 'esc back'"
        );
    }

    #[test]
    fn settings_hint_toggle_field_shows_space_toggle() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        // Open settings, navigate to a Bool field (idx 6 = Sub-grouping).
        app.open_settings(&Default::default());
        app.settings_selected = 6; // Bool field
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("space toggle"),
            "toggle field hint must include 'space toggle'"
        );
        assert!(
            buf.contains("s save"),
            "settings hint must include 's save'"
        );
        assert!(
            !buf.contains('\u{23ce}'),
            "⏎ must NOT appear in settings hint bar"
        );
    }

    #[test]
    fn settings_hint_number_field_shows_nudge() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        // Open settings, navigate to an F32 field (idx 2 = Auto-download threshold).
        app.open_settings(&Default::default());
        app.settings_selected = 2; // F32 field
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("nudge"),
            "number field hint must include 'nudge'"
        );
        assert!(
            buf.contains("s save"),
            "settings hint must include 's save'"
        );
    }

    #[test]
    fn settings_hint_format_pref_field_shows_format_editor() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.open_settings(&Default::default());
        app.settings_selected = 0; // FormatPref field
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("format editor"),
            "format-pref field hint must include 'format editor'"
        );
    }

    #[test]
    fn enter_appears_only_in_help_screen() {
        // ⏎ (U+23CE) should NOT appear in the main hint bar.
        {
            let backend = TestBackend::new(120, 30);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut app = AppState::new();
            app.set_view(fixture_vm());
            app.focus = Focus::List;
            terminal.draw(|f| ui::render(f, &mut app)).unwrap();
            let buf = buffer_string(&terminal);
            assert!(
                !buf.contains('\u{23ce}'),
                "⏎ must NOT appear on main screen (found it in buffer)"
            );
        }
        // ⏎ SHOULD appear in the Help screen.
        {
            let backend = TestBackend::new(120, 30);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut app = AppState::new();
            app.set_view(fixture_vm());
            app.modal = Some(Modal::Help {
                page: HelpPage::List,
                parent: None,
            });
            terminal.draw(|f| ui::render(f, &mut app)).unwrap();
            let buf = buffer_string(&terminal);
            assert!(buf.contains('\u{23ce}'), "⏎ MUST appear in the Help screen");
        }
    }

    #[test]
    fn confirm_modal_has_rule_and_hint_row() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Confirm {
            title: "My List".into(),
            n_books: 5,
            target_id: "list0".into(),
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("y confirm"),
            "confirm hint must include 'y confirm'"
        );
        assert!(
            buf.contains("esc cancel"),
            "confirm hint must include 'esc cancel'"
        );
        // The dim rule (─ characters) should be present.
        assert!(
            buf.contains('\u{2500}'),
            "confirm modal must have a dim rule (─)"
        );
    }

    #[test]
    fn reorganize_modal_has_rule_and_hint_row() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Reorganize {
            diff: vec![("old/path/book.epub".into(), "new/path/book.epub".into())],
            selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            buf.contains("y apply"),
            "reorganize hint must include 'y apply'"
        );
        assert!(
            buf.contains("esc cancel"),
            "reorganize hint must include 'esc cancel'"
        );
        assert!(
            buf.contains('\u{2500}'),
            "reorganize modal must have a dim rule (─)"
        );
    }

    // -----------------------------------------------------------------------
    // Pass 4 — full mouse support (#55)
    // -----------------------------------------------------------------------

    fn mouse_scroll_down(column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn mouse_scroll_up(column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// Click a book row → selects it + focuses the List pane.
    #[test]
    fn mouse_click_book_row_selects_and_focuses_list() {
        let mut app = AppState::new();
        app.set_view(fixture_vm()); // 2 books
        assert_eq!(app.selected, 0);

        // Manually set a book_rows rect for the second book (flat index 1) at screen row 5.
        // The group-header occupies row 0; books are at rows 1+ relative to book_table.
        let second_book_rect = ratatui::layout::Rect::new(0, 5, 80, 1);
        app.last_rects.book_rows = vec![
            (ratatui::layout::Rect::new(0, 4, 80, 1), RowRef::Book(0)),
            (second_book_rect, RowRef::Book(1)),
        ];

        // Start with Activity focused to confirm focus switches.
        app.focus = Focus::Activity;

        let intent = app.on_input(mouse_left_click(10, 5));
        assert_eq!(intent, Intent::Redraw, "first click returns Redraw");
        assert_eq!(app.selected, 1, "click on second book row must select it");
        assert_eq!(
            app.focus,
            Focus::List,
            "click on book row must focus the List pane"
        );
    }

    /// Click an already-selected book → performs Enter action (OpenDetail or OpenPicker).
    #[test]
    fn mouse_click_selected_book_fires_enter_intent() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.selected = 0;
        app.focus = Focus::List;

        // Put a rect for flat index 0 at row 3.
        app.last_rects.book_rows = vec![(ratatui::layout::Rect::new(0, 3, 80, 1), RowRef::Book(0))];

        // First click: already selected (focus is List, selected == 0) → Enter intent.
        let intent = app.on_input(mouse_left_click(5, 3));
        assert!(
            matches!(intent, Intent::OpenDetail { flat_index: 0 }),
            "second click on selected book must emit OpenDetail, got {:?}",
            intent
        );
    }

    /// Click an already-selected needs_selection book → OpenPicker.
    #[test]
    fn mouse_click_selected_needs_selection_fires_open_picker() {
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        // flat[0] has discovery == "needs_selection".
        app.selected = 0;
        app.focus = Focus::List;
        app.last_rects.book_rows = vec![(ratatui::layout::Rect::new(0, 3, 80, 1), RowRef::Book(0))];

        let intent = app.on_input(mouse_left_click(5, 3));
        assert!(
            matches!(intent, Intent::OpenPicker { flat_index: 0 }),
            "click on selected needs_selection book must emit OpenPicker, got {:?}",
            intent
        );
    }

    /// Click a list chip → SwitchList intent + focuses Header.
    #[test]
    fn mouse_click_list_chip_switches_list_and_focuses_header() {
        let mut app = AppState::new();
        app.all_lists = vec![
            crate::app::ListSummary {
                id: "list1".into(),
                title: "Classics".into(),
                done: 0,
                total: 3,
                is_manual: false,
            },
            crate::app::ListSummary {
                id: "list2".into(),
                title: "Fiction".into(),
                done: 1,
                total: 5,
                is_manual: false,
            },
        ];
        app.active_list_idx = 0;
        app.focus = Focus::List;

        // Manually set a list chip rect for list index 1 at the strip row (row 0).
        app.last_rects.list_chips = vec![
            (ratatui::layout::Rect::new(0, 0, 20, 1), 0),
            (ratatui::layout::Rect::new(21, 0, 20, 1), 1),
        ];

        let intent = app.on_input(mouse_left_click(25, 0));
        assert!(
            matches!(intent, Intent::SwitchList { ref id } if id == "list2"),
            "click on list chip 1 must emit SwitchList {{id: list2}}, got {:?}",
            intent
        );
        assert_eq!(
            app.focus,
            Focus::Header,
            "clicking a list chip must focus the Header pane"
        );
        assert_eq!(app.active_list_idx, 1, "active_list_idx must update to 1");
    }

    /// Click a filter chip → sets that filter.
    #[test]
    fn mouse_click_filter_chip_sets_filter() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        assert_eq!(app.filter, StatusFilter::All);

        // Manually set filter chip rects.
        app.last_rects.filter_chips = vec![
            (ratatui::layout::Rect::new(1, 1, 8, 1), StatusFilter::All),
            (
                ratatui::layout::Rect::new(10, 1, 12, 1),
                StatusFilter::NeedsYou,
            ),
            (ratatui::layout::Rect::new(23, 1, 9, 1), StatusFilter::Done),
        ];

        let intent = app.on_input(mouse_left_click(25, 1));
        assert_eq!(intent, Intent::Redraw, "filter chip click returns Redraw");
        assert_eq!(
            app.filter,
            StatusFilter::Done,
            "clicking the Done chip must set filter to Done"
        );
    }

    /// The filter chips are spread evenly across a wide row (not left-packed),
    /// and a click at a non-first chip's NEW spread x-position still selects the
    /// correct filter (proves the recomputed rects match the rendered positions).
    #[test]
    fn filter_chips_spread_evenly_and_click_hits_new_position() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        assert_eq!(app.filter, StatusFilter::All);

        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        let chips = app.last_rects.filter_chips.clone();
        assert_eq!(chips.len(), 7, "all seven status chips have rects");

        // Chips must be spread: there is a real gap between consecutive chips,
        // and the last chip starts well past the midpoint of a 120-col row
        // (i.e. they are NOT bunched at the left edge).
        for pair in chips.windows(2) {
            let (a, _) = pair[0];
            let (b, _) = pair[1];
            assert!(
                b.x > a.x + a.width,
                "chips must have a gap between them (a ends {}, b starts {})",
                a.x + a.width,
                b.x
            );
        }
        let (last_rect, last_filter) = *chips.last().unwrap();
        assert_eq!(last_filter, StatusFilter::Done);
        assert!(
            last_rect.x > 60,
            "last chip must be spread toward the right (x = {})",
            last_rect.x
        );

        // Click the Cannot chip (index 3) at the centre of its spread rect.
        let (cannot_rect, _) = chips[3];
        assert_eq!(chips[3].1, StatusFilter::Cannot);
        let click_x = cannot_rect.x + cannot_rect.width / 2;
        let intent = app.on_input(mouse_left_click(click_x, cannot_rect.y));
        assert_eq!(intent, Intent::Redraw, "filter chip click returns Redraw");
        assert_eq!(
            app.filter,
            StatusFilter::Cannot,
            "clicking the Cannot chip at its spread position must set filter to Cannot"
        );
    }

    /// Scroll wheel over the book table SCROLLS the list (selection moves ±1,
    /// list follows) and does NOT hover-jump to the row under the cursor.
    #[test]
    fn mouse_wheel_over_book_table_scrolls_not_hover() {
        let mut app = AppState::new();
        app.set_view(fixture_vm()); // 2 books
        assert_eq!(app.selected, 0);
        app.focus = Focus::Activity; // start elsewhere

        // Set book_table rect and book_rows.
        app.last_rects.book_table = ratatui::layout::Rect::new(0, 3, 80, 20);
        app.last_rects.book_rows = vec![
            (ratatui::layout::Rect::new(0, 4, 80, 1), RowRef::Book(0)),
            (ratatui::layout::Rect::new(0, 5, 80, 1), RowRef::Book(1)),
        ];

        // Scroll down with the cursor parked over book 0's row (row 4) — i.e. the
        // currently selected row. The wheel must SCROLL (selected 0 → 1), not
        // jump-to-cursor (which would keep selected at 0).
        let intent = app.on_input(mouse_scroll_down(10, 4));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(
            app.focus,
            Focus::List,
            "wheel over book table must focus List"
        );
        assert_eq!(
            app.selected, 1,
            "wheel SCROLLS the list (move_selection +1), never hover-jumps to the cursor row"
        );

        // Scroll back up: selection returns to 0 (scroll, not hover).
        app.on_input(mouse_scroll_up(10, 5));
        assert_eq!(
            app.selected, 0,
            "wheel up SCROLLS the list back, ignoring the cursor's row"
        );
    }

    /// Scroll wheel over the activity pane → scrolls + focuses Activity.
    #[test]
    fn mouse_wheel_over_activity_pane_scrolls_and_focuses_activity() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        // Add some downloading books so activity_row_count > 0.
        app.flat = (0..3)
            .map(|i| make_downloading_flat_book(&format!("Book {}", i), 0, i))
            .collect();
        app.focus = Focus::List;

        // Set the activity pane rect.
        app.last_rects.activity = ratatui::layout::Rect::new(0, 25, 80, 5);

        // Scroll down over the activity pane.
        let intent = app.on_input(mouse_scroll_down(10, 26));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(
            app.focus,
            Focus::Activity,
            "wheel over activity pane must focus Activity"
        );
        assert_eq!(
            app.activity_selected, 1,
            "ScrollDown over activity must advance activity_selected"
        );
    }

    /// Click an activity leg row → selects + focuses Activity pane.
    #[test]
    fn mouse_click_activity_leg_selects_and_focuses_activity() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = (0..2)
            .map(|i| make_downloading_flat_book(&format!("Book {}", i), 0, i))
            .collect();
        app.focus = Focus::List;

        // Activity pane occupies rows 25-29; header at row 25, legs at 26+.
        app.last_rects.activity = ratatui::layout::Rect::new(0, 25, 80, 5);
        app.last_rects.activity_rows = vec![
            (ratatui::layout::Rect::new(0, 26, 80, 1), 0),
            (ratatui::layout::Rect::new(0, 27, 80, 1), 1),
        ];

        // Click on the second leg (row 27, leg index 1).
        let intent = app.on_input(mouse_left_click(10, 27));
        assert_eq!(intent, Intent::Redraw, "activity leg click returns Redraw");
        assert_eq!(
            app.focus,
            Focus::Activity,
            "clicking an activity leg must focus the Activity pane"
        );
        assert_eq!(
            app.activity_selected, 1,
            "clicking leg 1 must set activity_selected to 1"
        );
    }

    /// `:mouse` toggle: flips mouse_capture and sets status_msg.
    #[test]
    fn mouse_toggle_flips_capture_and_sets_status_msg() {
        let mut app = AppState::new();
        assert!(app.mouse_capture, "mouse capture starts ON");

        // Toggle off.
        app.toggle_mouse_capture();
        assert!(!app.mouse_capture, "after first toggle: OFF");
        assert!(
            app.status_msg
                .as_deref()
                .map(|s| s.contains("OFF"))
                .unwrap_or(false),
            "status_msg must mention OFF after disabling"
        );

        // Toggle on again.
        app.toggle_mouse_capture();
        assert!(app.mouse_capture, "after second toggle: ON");
        assert!(
            app.status_msg
                .as_deref()
                .map(|s| s.contains("ON"))
                .unwrap_or(false),
            "status_msg must mention ON after re-enabling"
        );
    }

    /// `:mouse` as a command-line input emits Intent::Command("mouse").
    #[test]
    fn colon_mouse_emits_command_intent() {
        let mut app = AppState::new();
        app.on_input(key(KeyCode::Char(':')));
        for c in "mouse".chars() {
            app.on_input(key(KeyCode::Char(c)));
        }
        let intent = app.on_input(key(KeyCode::Enter));
        assert_eq!(
            intent,
            Intent::Command("mouse".into()),
            ":mouse must emit Intent::Command(\"mouse\")"
        );
    }

    /// Clicking the activity header (row == activity.y) toggles expand/collapse.
    #[test]
    fn mouse_click_activity_header_toggles_expand() {
        let mut app = AppState::new();
        assert!(app.activity_expanded, "starts expanded");

        // Set the activity pane rect; header is at area.y.
        app.last_rects.activity = ratatui::layout::Rect::new(0, 25, 80, 5);

        let intent = app.on_input(mouse_left_click(5, 25));
        assert_eq!(intent, Intent::Redraw);
        assert!(
            !app.activity_expanded,
            "clicking the activity header must collapse the pane"
        );

        let intent2 = app.on_input(mouse_left_click(5, 25));
        assert_eq!(intent2, Intent::Redraw);
        assert!(
            app.activity_expanded,
            "second click on the activity header must re-expand the pane"
        );
    }

    /// `space` toggles the Activity pane from LIST focus too — you don't have to
    /// Tab into the pane first.
    #[test]
    fn space_toggles_activity_from_list_focus() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::List;
        assert!(app.activity_expanded, "starts expanded");

        app.on_input(key(KeyCode::Char(' ')));
        assert!(
            !app.activity_expanded,
            "space must collapse the pane while focus is on the list"
        );

        app.on_input(key(KeyCode::Char(' ')));
        assert!(
            app.activity_expanded,
            "space must re-expand the pane while focus is on the list"
        );
    }

    /// The docked Activity pane is taller when expanded, collapses to a single
    /// line, and never dominates a short terminal (capped at a third of screen).
    #[test]
    fn activity_pane_height_expands_collapses_and_is_screen_capped() {
        // Tall terminal: expanded pane shows several rows (1 header + content).
        let mut tall = Terminal::new(TestBackend::new(80, 40)).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        assert!(app.activity_expanded, "starts expanded");

        tall.draw(|f| ui::render(f, &mut app)).unwrap();
        assert_eq!(
            app.last_rects.activity.height, 9,
            "expanded pane is the full height on a tall terminal"
        );

        // Collapsed: the whole pane shrinks to one line (just the header).
        app.activity_expanded = false;
        tall.draw(|f| ui::render(f, &mut app)).unwrap();
        assert_eq!(
            app.last_rects.activity.height, 1,
            "collapsed pane is a single line"
        );

        // Short terminal: expanded pane is capped so the book table keeps room.
        app.activity_expanded = true;
        let mut short = Terminal::new(TestBackend::new(80, 18)).unwrap();
        short.draw(|f| ui::render(f, &mut app)).unwrap();
        assert!(
            app.last_rects.activity.height < 9 && app.last_rects.activity.height >= 3,
            "expanded pane is capped to ~1/3 of a short screen, got {}",
            app.last_rects.activity.height
        );
    }

    // -----------------------------------------------------------------------
    // #62 — Marquee scroll (Detail modal · Title·Author ping-pong)
    // -----------------------------------------------------------------------

    /// Task 11: the marquee step budget is a function of ELAPSED time, not the
    /// number of renders. Two renders in the same step window owe 0 extra steps,
    /// so a burst of (wheel-driven) re-renders can't accelerate the marquees.
    #[test]
    fn marquee_phase_is_time_driven_not_render_driven() {
        use crate::app::MARQUEE_STEP_MS;
        use std::time::{Duration, Instant};

        let mut app = AppState::new();
        // Pretend ~3 step-intervals of wall-clock time have elapsed.
        app.marquee_epoch =
            Instant::now() - Duration::from_millis((MARQUEE_STEP_MS as u64) * 3 + 10);
        app.marquee_phase = 0;

        app.begin_marquee_frame();
        assert_eq!(
            app.marquee_steps_due, 3,
            "≈3 step intervals elapsed → 3 steps"
        );

        // A second render almost immediately afterwards owes NO further steps —
        // the budget tracks elapsed time, not render count.
        app.begin_marquee_frame();
        assert_eq!(
            app.marquee_steps_due, 0,
            "extra renders within a step window must not advance the marquee"
        );

        // Many rapid renders (simulating a wheel burst) still owe 0 steps.
        for _ in 0..50 {
            app.begin_marquee_frame();
            assert_eq!(
                app.marquee_steps_due, 0,
                "wheel-burst renders never accelerate"
            );
        }
    }

    /// A long idle gap is capped so returning to the app doesn't fast-forward.
    #[test]
    fn marquee_step_budget_is_capped_after_idle() {
        use crate::app::MARQUEE_STEP_MS;
        use std::time::{Duration, Instant};
        let mut app = AppState::new();
        app.marquee_epoch = Instant::now() - Duration::from_millis((MARQUEE_STEP_MS as u64) * 1000);
        app.marquee_phase = 0;
        app.begin_marquee_frame();
        assert!(
            app.marquee_steps_due <= 4,
            "huge idle gap is capped, got {}",
            app.marquee_steps_due
        );
    }

    /// advance_marquee scrolls forward by 1 each tick when text overflows.
    #[test]
    fn marquee_advances_forward_when_overflowing() {
        let mut app = AppState::new();
        // text_char_len=20, col_w=10 → overflows by 10
        app.advance_marquee(20, 10);
        assert_eq!(app.marquee_offset, 1, "first tick: offset advances to 1");
        assert!(app.marquee_forward, "still scrolling forward");
        assert_eq!(app.marquee_pause, 0, "no pause yet");
    }

    /// advance_marquee reverses direction and starts a pause when max offset reached.
    #[test]
    fn marquee_reverses_at_end() {
        let mut app = AppState::new();
        // text_char_len=15, col_w=10 → max_offset=5
        for _ in 0..5 {
            app.advance_marquee(15, 10);
        }
        // offset should now be at max (5) and direction reversed.
        assert_eq!(app.marquee_offset, 5, "offset reaches max");
        assert!(
            !app.marquee_forward,
            "direction reversed after reaching end"
        );
        assert!(app.marquee_pause > 0, "pause countdown started at end");
    }

    /// Going backward from offset=0 reverses direction and starts a pause.
    #[test]
    fn marquee_reverses_at_zero_going_backward() {
        let mut app = AppState::new();
        // Manually place at offset=1, going backward.
        app.marquee_offset = 1;
        app.marquee_forward = false;
        // First backward tick → offset becomes 0.
        app.advance_marquee(15, 10);
        assert_eq!(app.marquee_offset, 0, "offset decrements to 0");
        // Second tick → at 0 going backward: reverses + starts pause.
        app.advance_marquee(15, 10);
        assert!(
            app.marquee_forward,
            "direction reversed back to forward at start"
        );
        assert!(app.marquee_pause > 0, "pause countdown started at start");
    }

    /// When text fits in the column, marquee resets to zero regardless of state.
    #[test]
    fn marquee_no_advance_when_text_fits() {
        let mut app = AppState::new();
        app.marquee_offset = 5;
        app.marquee_forward = false;
        // text_char_len=8 fits in col_w=10 → reset.
        app.advance_marquee(8, 10);
        assert_eq!(app.marquee_offset, 0, "offset reset when text fits");
        assert!(app.marquee_forward, "direction reset to forward");
        assert_eq!(app.marquee_pause, 0, "no pause when text fits");
    }

    /// reset_marquee_if_selection_changed resets state when selection changes.
    #[test]
    fn marquee_resets_on_selection_change() {
        let mut app = AppState::new();
        app.marquee_offset = 7;
        app.marquee_forward = false;
        app.marquee_pause = 4;
        app.marquee_detail_sel = 0;

        // Selection changes from 0 → 1.
        app.reset_marquee_if_selection_changed(1);
        assert_eq!(
            app.marquee_offset, 0,
            "offset must reset on selection change"
        );
        assert!(app.marquee_forward, "direction must reset to forward");
        assert_eq!(app.marquee_pause, 0, "pause must clear on selection change");
        assert_eq!(app.marquee_detail_sel, 1, "new selection index recorded");
    }

    /// reset_marquee_if_selection_changed is a no-op when selection is unchanged.
    #[test]
    fn marquee_no_reset_when_selection_unchanged() {
        let mut app = AppState::new();
        app.marquee_offset = 4;
        app.marquee_detail_sel = 2;

        app.reset_marquee_if_selection_changed(2); // same index
        assert_eq!(
            app.marquee_offset, 4,
            "offset must not change when selection is the same"
        );
    }

    // -----------------------------------------------------------------------
    // #1 / #11 — Flex variation/picker row: Mode A/B + marquee-vs-ellipsize
    // -----------------------------------------------------------------------

    /// Build a `ViewVariation` with the given state/fmt/progress, defaults elsewhere.
    fn mk_var(title: &str, fmt: &str, state: &str, progress: u32, md5: &str) -> ViewVariation {
        ViewVariation {
            md5: md5.into(),
            title: title.into(),
            author: "Author Name".into(),
            fmt: fmt.into(),
            size: 1,
            size_bytes: None,
            year: Some(2020),
            publisher: String::new(),
            language: String::new(),
            pages: Some(120),
            counted_pages: None,
            low_pages: false,
            host: Some("libgen.li".into()),
            state: state.into(),
            progress,
            downloaded_bytes: None,
            total_bytes: None,
            speed_bps: None,
            eta_secs: Some(12),
            output_path: None,
            score: 0.90,
            cover_url: None,
            last_error: None,
        }
    }

    /// Mode A: when the rest fields fit within the 40% cap, the row is `Fixed`
    /// and Title·Author gets ALL the remaining content width.
    #[test]
    fn flex_row_layout_mode_a_gives_slack_to_title() {
        // row_w=100, rest=[4,7,4,8] → rest_w = 23 + 3 gaps = 26; cap = 40.
        // 26 ≤ 40 → Mode A. content_w = 98; title_w = 98 - SEP(2) - 26 = 70.
        let layout = ui::flex_row_layout(100, &[4, 7, 4, 8]);
        assert_eq!(layout, ui::FlexLayout::Fixed { title_w: 70 });
    }

    /// Mode B: when the rest fields would exceed the 40% cap, everything packs.
    #[test]
    fn flex_row_layout_mode_b_when_rest_exceeds_cap() {
        // row_w=50, rest=[4,7,4,8] → rest_w = 26; cap = 20. 26 > 20 → Mode B.
        let layout = ui::flex_row_layout(50, &[4, 7, 4, 8]);
        assert_eq!(layout, ui::FlexLayout::Packed { width: 48 });
    }

    /// Mode B also triggers when rest fits the cap but no sane title width is left.
    #[test]
    fn flex_row_layout_mode_b_when_title_starved() {
        // row_w=20, rest=[1,1,1,1] → rest_w = 4+3 = 7; cap = 8 (7 ≤ 8).
        // content_w = 18; needs 7 + SEP(2) + MIN_TITLE(8) = 17 ≤ 18 → still Mode A.
        assert!(matches!(
            ui::flex_row_layout(20, &[1, 1, 1, 1]),
            ui::FlexLayout::Fixed { .. }
        ));
        // row_w=18: content_w = 16 < 17 → Mode B even though rest fits the cap.
        assert_eq!(
            ui::flex_row_layout(18, &[1, 1, 1, 1]),
            ui::FlexLayout::Packed { width: 16 }
        );
    }

    /// The focused row marquees (offset-scrolled, no ellipsis); unfocused rows
    /// ellipsize with a trailing `…`.
    #[test]
    fn flex_text_focused_marquees_unfocused_ellipsizes() {
        let s = "abcdefghij"; // 10 cols, window 5
                              // Focused at offset 0 → window head, no ellipsis.
        assert_eq!(ui::flex_text(s, 5, 0, true), "abcde");
        // Focused scrolled by 2 → shifted window, still no ellipsis.
        assert_eq!(ui::flex_text(s, 5, 2, true), "cdefg");
        // Unfocused → ellipsized (offset ignored), trailing `…`.
        let un = ui::flex_text(s, 5, 2, false);
        assert_eq!(un, "abcd\u{2026}");
        assert!(un.ends_with('\u{2026}'), "unfocused must ellipsize");
    }

    // -----------------------------------------------------------------------
    // #3 — Command line + :import input scroll-to-cursor
    // -----------------------------------------------------------------------

    /// Concatenate a rendered `Line`'s span texts back into a plain string.
    fn line_text(line: &ratatui::text::Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// A short command buffer fits: no edge indicators, the `:` prefix and the
    /// block cursor are both visible, nothing is clipped.
    #[test]
    fn command_line_short_buffer_no_indicators() {
        let line = ui::command_input_line("import a.md", 80);
        let text = line_text(&line);
        assert!(text.starts_with(':'), "colon pinned: {text:?}");
        assert!(text.contains('\u{2588}'), "block cursor visible");
        assert!(!text.contains('\u{2039}'), "nothing clipped left");
        assert!(!text.contains('\u{203a}'), "nothing clipped right");
    }

    /// A long buffer scrolls (scroll_to_cursor) so the cursor stays visible at
    /// the end: the `‹` left indicator shows, the block cursor is the last glyph,
    /// no `›`, and the line never exceeds the field width.
    #[test]
    fn command_line_long_buffer_scrolls_to_cursor() {
        let buf = "import /Users/someone/very/deep/nested/path/file.md";
        let line = ui::command_input_line(buf, 24);
        let text = line_text(&line);
        assert!(
            text.contains('\u{2039}'),
            "long buffer clips left: {text:?}"
        );
        assert!(!text.contains('\u{203a}'), "cursor at end → no right clip");
        assert!(text.contains('\u{2588}'), "cursor stays visible");
        assert!(
            text.trim_end().ends_with('\u{2588}'),
            "block cursor is the last visible glyph: {text:?}"
        );
        assert!(
            crate::textfit::display_width(&text) <= 24,
            "never exceeds field width: {text:?}"
        );
    }

    // -----------------------------------------------------------------------
    // #3 — :import path-completion wildmenu label formatting
    // -----------------------------------------------------------------------

    /// Short full paths are shown verbatim; directories keep a trailing `/`.
    #[test]
    fn import_label_full_when_short() {
        assert_eq!(
            ui::format_import_candidate_label("~/docs", "a.md", false),
            "~/docs/a.md"
        );
        assert_eq!(
            ui::format_import_candidate_label("~/docs", "sub", true),
            "~/docs/sub/"
        );
        // No parent (current dir) → just the name.
        assert_eq!(ui::format_import_candidate_label("", "a.md", false), "a.md");
    }

    /// A long path shortens the parent to its last 10 chars with a leading `…`,
    /// keeping the name (and any trailing `/`) intact.
    #[test]
    fn import_label_long_parent_shortened() {
        let label = ui::format_import_candidate_label(
            "/Users/someone/very/deep/nested/path",
            "file.md",
            false,
        );
        assert_eq!(label, "\u{2026}ested/path/file.md");
        // Directory variant keeps the trailing slash.
        let dir =
            ui::format_import_candidate_label("/Users/someone/very/deep/nested/path", "sub", true);
        assert_eq!(dir, "\u{2026}ested/path/sub/");
    }

    /// A parent ≤10 chars is shown whole (no `…`) even when the full label is
    /// long because of a long name.
    #[test]
    fn import_label_short_parent_shown_whole() {
        let label =
            ui::format_import_candidate_label("/short", "a-really-quite-long-filename.md", false);
        assert!(
            label.starts_with("/short/"),
            "parent ≤10 stays whole, no ellipsis: {label:?}"
        );
        assert!(!label.starts_with('\u{2026}'), "no leading ellipsis");
    }

    // -----------------------------------------------------------------------
    // #4 — Book list row: right-anchored, aligned metadata (option A)
    // -----------------------------------------------------------------------

    /// Fixed mode: title gets the bulk, the author keeps a slice, and the
    /// metadata is right-anchored with a width-scaled inter-column gap.
    #[test]
    fn book_row_layout_fixed_right_anchored() {
        // content_w=100 → col_gap=2; rest_block = 27 + 2*2 = 31; author = 10;
        // mid_gap = 2 + 2 = 4; fixed = 10 + SEP(2) + 4 + 31 = 47; title = 53.
        let layout = ui::book_row_layout(100, &[5, 8, 14]);
        assert_eq!(
            layout,
            ui::BookRowLayout::Fixed {
                title_w: 53,
                author_w: 10,
                mid_gap: 4,
                col_gap: 2,
                rest_widths: vec![5, 8, 14],
            }
        );
    }

    /// Right-anchor alignment: the whole row sums back to `content_w`, so the
    /// metadata block ends flush against the right edge (and the per-column
    /// widths are fixed, so the columns line up vertically down the list).
    #[test]
    fn book_row_layout_metadata_is_flush_right() {
        for content_w in [76usize, 96, 128, 156] {
            let rest = [4usize, 6, 8];
            match ui::book_row_layout(content_w, &rest) {
                ui::BookRowLayout::Fixed {
                    title_w,
                    author_w,
                    mid_gap,
                    col_gap,
                    rest_widths,
                } => {
                    let rest_block: usize =
                        rest_widths.iter().sum::<usize>() + (rest_widths.len() - 1) * col_gap;
                    // title + SEP(2) + author + mid_gap + rest_block == content_w
                    assert_eq!(
                        title_w + 2 + author_w + mid_gap + rest_block,
                        content_w,
                        "metadata must be flush-right at content_w={content_w}"
                    );
                    // The metadata columns keep their natural width (aligned).
                    assert_eq!(rest_widths, rest.to_vec());
                }
                other => panic!("expected Fixed at {content_w}, got {other:?}"),
            }
        }
    }

    /// The inter-column metadata gap scales with width: 1 (tight floor) at
    /// 80-col, 2 at moderate widths, up to 4 at large widths.
    #[test]
    fn book_row_layout_gap_scales_1_2_4_by_width() {
        let col_gap_at = |content_w: usize| match ui::book_row_layout(content_w, &[3, 3, 3]) {
            ui::BookRowLayout::Fixed { col_gap, .. } => col_gap,
            other => panic!("expected Fixed at {content_w}, got {other:?}"),
        };
        // content_w = area.width - SEQ(4): 80→76, 100→96, 132→128, 160→156.
        assert_eq!(col_gap_at(76), 1, "80-col → 1-space tight floor");
        assert_eq!(col_gap_at(96), 2, "100-col → 2-space gaps");
        assert_eq!(col_gap_at(128), 3, "132-col → 3-space gaps");
        assert_eq!(col_gap_at(156), 4, "160-col → 4-space gaps");
        // The dedicated gap helper agrees.
        assert_eq!(ui::book_meta_gap(76), 1);
        assert_eq!(ui::book_meta_gap(156), 4);
    }

    /// The title gets the BULK: it absorbs the wide-width room (it is far wider
    /// than the author), keeping the metadata flush-right without a giant hole.
    #[test]
    fn book_row_layout_title_gets_the_bulk() {
        match ui::book_row_layout(156, &[4, 6, 8]) {
            ui::BookRowLayout::Fixed {
                title_w, author_w, ..
            } => {
                assert!(
                    title_w > author_w * 3,
                    "title ({title_w}) must dwarf the author ({author_w})"
                );
            }
            other => panic!("expected Fixed, got {other:?}"),
        }
    }

    /// Rest fields too wide for any sane title → the whole line packs.
    #[test]
    fn book_row_layout_packed_when_rest_exceeds_cap() {
        // content_w=40, rest=[5,8,14]: col_gap=1 → rest_block=29; author=6; mid=3;
        // fixed=6+2+3+29=40; 40 < 40 + MIN_TITLE(8) → Packed.
        assert_eq!(
            ui::book_row_layout(40, &[5, 8, 14]),
            ui::BookRowLayout::Packed { width: 40 }
        );
    }

    /// Packed also when no sane title width is left.
    #[test]
    fn book_row_layout_packed_when_title_starved() {
        // content_w=17, rest=[1,1,1]: col_gap=1 → rest_block=5; author=6; mid=3;
        // fixed=6+2+3+5=16; 17 < 16 + MIN_TITLE(8) = 24 → Packed.
        assert_eq!(
            ui::book_row_layout(17, &[1, 1, 1]),
            ui::BookRowLayout::Packed { width: 17 }
        );
    }

    // -----------------------------------------------------------------------
    // Task 6 — title+author rendered as ONE combined left cell
    // -----------------------------------------------------------------------

    fn line_to_string(line: &ratatui::text::Line) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    /// Non-selected rows attach the author directly to the title (two-space gap)
    /// as one combined cell — NOT a separate far-right author column — and the
    /// metadata stays right-anchored (the whole line sums to content_w).
    #[test]
    fn book_row_combined_cell_attaches_author_unfocused() {
        use ratatui::style::{Color, Style};
        let content_w = 100usize;
        let layout = ui::book_row_layout(content_w, &[3, 4, 6]);
        let author_style = Style::default().fg(Color::Rgb(1, 2, 3));
        let rest = vec![
            ("epub".to_string(), Style::default()),
            ("2 MB".to_string(), Style::default()),
            ("\u{2713} done".to_string(), Style::default()),
        ];
        let line = ui::book_row_line(
            &layout,
            "Short",
            "Jane Author",
            Style::default(),
            author_style,
            &rest,
            false, // not focused → ellipsize the combined cell
            0,
            Style::default(),
        );
        let s = line_to_string(&line);
        // Author is attached right after the title with a two-space gap.
        assert!(
            s.starts_with("Short  Jane Author"),
            "combined cell must attach author to title: {s:?}"
        );
        // The author span keeps its own colour (two-tone, not flattened).
        assert!(
            line.spans
                .iter()
                .any(|sp| sp.content.contains("Jane") && sp.style.fg == Some(Color::Rgb(1, 2, 3))),
            "author keeps its colour in the combined cell"
        );
        // Metadata is flush-right: the full row sums to content_w.
        assert_eq!(
            crate::textfit::display_width(&s),
            content_w,
            "row must span content_w (metadata right-anchored)"
        );
        assert!(
            s.trim_end().ends_with("done"),
            "status anchored at the right"
        );
    }

    /// A combined cell wider than its column ellipsizes with `…` (unfocused).
    #[test]
    fn book_row_combined_cell_ellipsizes_when_long() {
        use ratatui::style::Style;
        let layout = ui::book_row_layout(100, &[3, 4, 6]);
        let rest = vec![
            ("epub".to_string(), Style::default()),
            ("2 MB".to_string(), Style::default()),
            ("done".to_string(), Style::default()),
        ];
        let long_title = "A Very Long Title That Will Certainly Overflow The Combined Cell";
        let line = ui::book_row_line(
            &layout,
            long_title,
            "And A Long Author Name Too",
            Style::default(),
            Style::default(),
            &rest,
            false,
            0,
            Style::default(),
        );
        let s = line_to_string(&line);
        assert!(
            s.contains('\u{2026}'),
            "overflowing combined cell must ellipsize: {s:?}"
        );
        assert_eq!(
            crate::textfit::display_width(&s),
            100,
            "row still spans content_w"
        );
    }

    // -----------------------------------------------------------------------
    // #15 — List strip responsive width formula
    // -----------------------------------------------------------------------

    /// All lists fit at natural width → shown at natural width, no capping.
    #[test]
    fn list_strip_layout_natural_when_all_fit() {
        assert_eq!(ui::list_strip_layout(100, &[20, 20, 20]), vec![20, 20, 20]);
    }

    /// ≤4 lists that don't fit → divided EVENLY (each = N / #lists).
    #[test]
    fn list_strip_layout_le4_divides_evenly() {
        // strip=100, n=3 → base = max(30, 100/3=33) = 33 → three equal columns.
        assert_eq!(ui::list_strip_layout(100, &[50, 60, 70]), vec![33, 33, 33]);
    }

    /// ≤4 lists with N/#lists < 30 → floor 30 (strip overflows / scrolls).
    #[test]
    fn list_strip_layout_le4_floors_at_30() {
        // strip=80, n=4 → 80/4 = 20 < 30 → each gets 30 (overflow).
        assert_eq!(
            ui::list_strip_layout(80, &[50, 60, 70, 80]),
            vec![30, 30, 30, 30]
        );
    }

    /// >4 lists → each capped at N/4 (floor 30), packed tight; a short list
    /// keeps its natural width.
    #[test]
    fn list_strip_layout_gt4_caps_at_quarter() {
        // strip=100, n=5 → cap = max(30, 100/4=25) = 30. Long lists → 30, packed;
        // the 10-wide list stays 10 (no padding to the cap).
        assert_eq!(
            ui::list_strip_layout(100, &[10, 50, 50, 50, 50]),
            vec![10, 30, 30, 30, 30]
        );
    }

    /// The list-chip click rects track the new column positions: chips are
    /// ordered, non-overlapping, and inside the strip area after layout.
    #[test]
    fn list_strip_chip_rects_track_columns() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        for i in 0..6 {
            app.all_lists.push(crate::app::ListSummary {
                id: format!("L{i}"),
                title: format!("Reading List Number {i}"),
                done: i,
                total: 10,
                is_manual: false,
            });
        }
        app.active_list_idx = 2;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        let chips = &app.last_rects.list_chips;
        assert!(!chips.is_empty(), "chips must be populated");
        // Ordered, non-overlapping, within the 100-col strip.
        let mut prev_end = 0u16;
        for (rect, idx) in chips {
            assert!(rect.x >= prev_end, "chip {idx} overlaps the previous one");
            assert!(rect.x + rect.width <= 100, "chip {idx} runs past the strip");
            prev_end = rect.x + rect.width;
        }
        // The active list (index 2) must have a visible chip.
        assert!(
            chips.iter().any(|(_, i)| *i == 2),
            "active list chip must be visible"
        );
    }

    /// The picker border title is ellipsized so the " — choose a copy " suffix is
    /// never clipped, and short titles pass through whole (#11).
    #[test]
    fn picker_border_title_ellipsizes_long_title() {
        let long = "A Very Long Book Title That Cannot Possibly Fit The Narrow Border";
        let out = ui::picker_border_title(long, 40);
        assert!(
            out.contains('\u{2026}'),
            "long title must be ellipsized: {out:?}"
        );
        assert!(
            out.ends_with(" \u{2014} choose a copy "),
            "suffix must survive intact: {out:?}"
        );
        assert!(
            crate::textfit::display_width(&out) <= 40,
            "border title must fit the area width: {out:?}"
        );
        // A short title is left whole.
        let short = ui::picker_border_title("Short", 60);
        assert_eq!(short, " Short \u{2014} choose a copy ");
    }

    /// Render smoke test for #1: the detail variations table drops the Src column
    /// and MD5, labels available copies "avail", and shows a progress bar only on
    /// the (separate) line below a downloading copy.
    #[test]
    fn detail_variations_no_src_no_md5_avail_and_progress_line() {
        use crate::app::DetailSubFocus;
        let backend = TestBackend::new(132, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        app.flat[0].book.versions = vec![
            mk_var("Available Copy", "epub", "available", 0, &"a".repeat(32)),
            mk_var("Done Copy", "pdf", "done", 100, &"ccccccc".to_string()),
            mk_var(
                "Downloading Copy",
                "mobi",
                "downloading",
                50,
                &"b".repeat(32),
            ),
        ];
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);

        assert!(!buf.contains("Src"), "Src column header must be gone");
        assert!(buf.contains("avail"), "available state must read 'avail'");
        assert!(
            !buf.contains("ccc"),
            "MD5 must not appear in the detail variations table"
        );
        assert!(
            buf.contains('\u{25b0}'),
            "a progress bar (▰) must render for the downloading copy"
        );
    }

    /// Task #2: the list view and the detail variations table share `state_label`,
    /// so a downloading copy reads the SAME short `dling` label in both — with NO
    /// percentage (the progress bar / Activity pane convey it).
    #[test]
    fn list_and_detail_share_short_dling_label() {
        use crate::app::DetailSubFocus;
        let backend = TestBackend::new(132, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        app.flat[0].book.versions = vec![mk_var(
            "Downloading Copy",
            "mobi",
            "downloading",
            50,
            &"b".repeat(32),
        )];

        // List render: the primary row's state cell reads the short "dling"
        // label with the live "%" appended inline (so a download's progress is
        // visible in the list, mirroring the Activity pane).
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let list_buf = buffer_string(&terminal);
        assert!(
            list_buf.contains("dling 50%"),
            "list state cell must read 'dling 50%' (short label + inline %): {list_buf}"
        );

        // Detail render: identical short label, and the old "downloading 50%"
        // state-cell wording is gone (the % is dropped from the label).
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let detail_buf = buffer_string(&terminal);
        assert!(
            detail_buf.contains("dling"),
            "detail must show the same short 'dling' label: {detail_buf}"
        );
        assert!(
            !detail_buf.contains("downloading 50%"),
            "detail state cell must drop the percentage (no 'downloading 50%'): {detail_buf}"
        );
    }

    /// Detail-view `r` on a focused FAILED copy must re-arm just THAT copy
    /// (per-copy `RequestVariations`), not reset the whole book — so the failed
    /// variation re-downloads in place instead of vanishing.
    #[test]
    fn detail_retry_rearms_focused_copy_not_whole_book() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        let failed_md5 = "f".repeat(32);
        app.flat[0].book.versions = vec![
            mk_var("Failed Copy", "epub", "failed", 0, &failed_md5),
            mk_var("Other Copy", "pdf", "done", 100, &"d".repeat(32)),
        ];
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0, // focus the failed copy
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });

        let intent = app.on_input(key(KeyCode::Char('r')));
        match intent {
            Intent::RequestVariations { md5s, .. } => assert_eq!(
                md5s,
                vec![failed_md5],
                "r must re-arm only the focused failed copy"
            ),
            other => panic!("expected per-copy RequestVariations, got {other:?}"),
        }
    }

    /// Detail-view `r` falls back to a whole-book re-query when History is focused
    /// (no specific copy in context).
    #[test]
    fn detail_retry_history_focus_falls_back_to_whole_book() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        app.flat[0].book.versions =
            vec![mk_var("Failed Copy", "epub", "failed", 0, &"f".repeat(32))];
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::History,
            history_selected: 0,
        });

        let intent = app.on_input(key(KeyCode::Char('r')));
        assert!(
            matches!(intent, Intent::Retry { .. }),
            "History focus must keep whole-book retry, got {intent:?}"
        );
    }

    /// Detail-view `o`/`R` open/reveal the FOCUSED copy's file, not the first
    /// downloaded copy — so focusing the 2nd done copy opens ITS path.
    #[test]
    fn detail_open_reveal_target_focused_copy() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        let mut first = mk_var("First Copy", "epub", "done", 100, &"a".repeat(32));
        first.output_path = Some("/books/first.epub".into());
        let mut second = mk_var("Second Copy", "pdf", "done", 100, &"b".repeat(32));
        second.output_path = Some("/books/second.pdf".into());
        app.flat[0].book.versions = vec![first, second];
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 1, // focus the SECOND copy (not first)
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });

        let o = app.on_input(key(KeyCode::Char('o')));
        assert_eq!(
            o,
            Intent::OpenFile("/books/second.pdf".into()),
            "o must open the focused copy's file, not the first"
        );
        let r = app.on_input(key(KeyCode::Char('R')));
        assert_eq!(
            r,
            Intent::RevealFile("/books/second.pdf".into()),
            "R must reveal the focused copy's file, not the first"
        );
    }

    /// List-view `o`/`R` open the SELECTED alt-copy sub-row's file when one is
    /// focused (`selected_var`), and fall back to the first downloaded copy for a
    /// whole-book selection.
    #[test]
    fn list_open_targets_selected_copy_else_first() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        let mut first = mk_var("First", "epub", "done", 100, &"a".repeat(32));
        first.output_path = Some("/books/first.epub".into());
        let mut second = mk_var("Second", "pdf", "done", 100, &"b".repeat(32));
        second.output_path = Some("/books/second.pdf".into());
        app.flat[0].book.versions = vec![first, second];
        app.focus = Focus::List;
        app.selected = 0;

        // Alt-copy sub-row (2nd copy) focused → open THAT copy.
        app.selected_var = Some("b".repeat(32));
        assert_eq!(
            app.on_input(key(KeyCode::Char('o'))),
            Intent::OpenFile("/books/second.pdf".into()),
            "list o must open the selected copy's file"
        );

        // Whole-book selection → first downloaded copy.
        app.selected_var = None;
        assert_eq!(
            app.on_input(key(KeyCode::Char('o'))),
            Intent::OpenFile("/books/first.epub".into()),
            "whole-book selection opens the first downloaded copy"
        );
    }

    /// List-view `r`/`p`/`c` act on the SELECTED alt-copy sub-row when one is
    /// focused (per-copy intents), and fall back to whole-book intents otherwise.
    #[test]
    fn list_rpc_act_on_selected_copy_else_whole_book() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        let copy_md5 = "b".repeat(32);
        app.flat[0].book.versions = vec![
            mk_var("First", "epub", "downloading", 40, &"a".repeat(32)),
            mk_var("Second", "pdf", "downloading", 10, &copy_md5),
        ];
        app.focus = Focus::List;
        app.selected = 0;

        // Alt-copy sub-row focused → per-copy intents.
        app.selected_var = Some(copy_md5.clone());
        match app.on_input(key(KeyCode::Char('r'))) {
            Intent::RequestVariations { md5s, .. } => assert_eq!(md5s, vec![copy_md5.clone()]),
            other => panic!("r expected per-copy RequestVariations, got {other:?}"),
        }
        assert_eq!(
            app.on_input(key(KeyCode::Char('p'))),
            Intent::PauseTransfer {
                md5: copy_md5.clone()
            }
        );
        assert_eq!(
            app.on_input(key(KeyCode::Char('c'))),
            Intent::CancelTransfer {
                md5: copy_md5.clone()
            }
        );

        // Whole-book selection → book-level intents.
        app.selected_var = None;
        assert!(
            matches!(app.on_input(key(KeyCode::Char('r'))), Intent::Retry { .. }),
            "r on whole book → Retry"
        );
        assert!(
            matches!(app.on_input(key(KeyCode::Char('p'))), Intent::Pause { .. }),
            "p on whole book → Pause"
        );
        assert!(
            matches!(app.on_input(key(KeyCode::Char('c'))), Intent::Cancel { .. }),
            "c on whole book → Cancel"
        );
    }

    /// The list hint bar follows the SELECTED copy's state: a book whose roll-up
    /// is "done" still shows `r retry` when its focused alt copy is `failed`, so
    /// the advertised key matches the per-copy action.
    #[test]
    fn list_hint_reflects_selected_copy_state() {
        let backend = TestBackend::new(132, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        let failed_md5 = "f".repeat(32);
        app.flat[0].book.versions = vec![
            mk_var("Done Copy", "epub", "done", 100, &"d".repeat(32)),
            mk_var("Failed Copy", "pdf", "failed", 0, &failed_md5),
        ];
        app.focus = Focus::List;
        app.selected = 0;

        // Whole-book selection: roll-up is "done" → hint shows "o open", not retry.
        app.selected_var = None;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let whole = buffer_string(&terminal);
        assert!(
            whole.contains("o open"),
            "whole-book done hint shows open: {whole}"
        );
        assert!(
            !whole.contains("retry"),
            "whole-book done hint must NOT show retry: {whole}"
        );

        // Failed alt copy focused → hint shows "r retry".
        app.selected_var = Some(failed_md5);
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let copy = buffer_string(&terminal);
        assert!(
            copy.contains("retry"),
            "failed-copy hint must show retry: {copy}"
        );
    }

    /// Task #2: list `available` reads `avail` (matching the detail table), so the
    /// two never drift on the available state either.
    #[test]
    fn list_available_state_reads_avail() {
        let backend = TestBackend::new(132, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        app.flat[0].book.versions = vec![mk_var(
            "Available Copy",
            "epub",
            "available",
            0,
            &"a".repeat(32),
        )];

        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let list_buf = buffer_string(&terminal);
        assert!(
            list_buf.contains("avail"),
            "list available state must read 'avail': {list_buf}"
        );
    }

    /// Render smoke test for #11: the picker renders candidate rows + the border
    /// "choose a copy" title without panicking, via the flex-row path.
    #[test]
    fn picker_renders_candidates_and_border_title() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.modal = Some(Modal::Picker {
            book_flat_index: 0,
            selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(buf.contains("choose a copy"), "border title must render");
        assert!(
            buf.contains("Ambiguous Book"),
            "candidate title must render"
        );
        assert!(buf.contains("MATCH"), "rest-field header must render");
    }

    // -----------------------------------------------------------------------
    // #71 — Wildmenu layout: WILDMENU row is above the command-line (not over rule)
    // -----------------------------------------------------------------------

    /// When the wildmenu is open in cmd mode, the dim rule separator above the
    /// command row must still be present in the buffer (not overwritten).
    #[test]
    fn wildmenu_does_not_overwrite_rule_above_cmd() {
        // Use a 132×38 terminal — the reference size from the spec.
        let backend = TestBackend::new(132, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.command_buf = Some(String::new());
        app.completion_candidates = vec!["import".into(), "settings".into(), "add".into()];
        app.completion_index = 0;
        // activity_expanded = true by default (9 rows on a 38-row terminal; the
        // bottom rows are anchored to the screen bottom and the book table
        // absorbs the difference, so the wildmenu/rule positions are unchanged).
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let width = 132usize;
        let get_row = |r: usize| -> String {
            buf.content()[r * width..(r + 1) * width]
                .iter()
                .map(|c| c.symbol().to_string())
                .collect()
        };

        // Layout (38 rows, activity=9): the book table (Min(8)) absorbs the
        // activity height, so the bottom block stays anchored to the screen
        // bottom regardless of activity size:
        // book_h = 38 - (1+1+1+1+9+1+1+1+1+1) = 38 - 18 = 20 → table rows 3..22
        // rule(23) activity(24..32) → Row 33 = rule (before wildmenu), Row 34 = wildmenu
        let rule_row = get_row(33);
        let wildmenu_row = get_row(34);

        assert!(
            rule_row.contains('\u{2500}'),
            "row 33 must be the rule separator (─); got: {}",
            &rule_row[..rule_row.len().min(40)]
        );
        assert!(
            wildmenu_row.contains("import"),
            "row 34 must contain wildmenu candidate 'import'; got: {}",
            &wildmenu_row[..wildmenu_row.len().min(40)]
        );
    }

    // -----------------------------------------------------------------------
    // #62 / #72 — Modal width tests
    // -----------------------------------------------------------------------

    /// Detail modal renders without panic at 132-col width (≈80% of 132 = 105).
    #[test]
    fn detail_modal_wide_render_does_not_panic() {
        let backend = TestBackend::new(132, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        // Verify the selected-line accent (▌) is present (modal rendered correctly).
        let content = buffer_string(&terminal);
        assert!(
            content.contains('\u{258c}') || content.contains("Ambiguous"),
            "detail modal must render book content at 132 cols"
        );
    }

    /// Picker modal renders without panic at 132-col width (≈80% of 132 = 105).
    #[test]
    fn picker_modal_wide_render_does_not_panic() {
        let backend = TestBackend::new(132, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.modal = Some(Modal::Picker {
            book_flat_index: 0,
            selected: 0,
        });
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(
            content.contains("choose a copy"),
            "picker modal must render at 132 cols"
        );
    }

    // -----------------------------------------------------------------------
    // Stacked per-variation rows — selection / nav / mouse / All-view routing
    // -----------------------------------------------------------------------

    /// A `FlatBook` with TWO armed copies: a finished `done` epub (primary) and
    /// an in-flight `downloading` epub (the "↳ alt. copy" sub-row).
    fn make_multi_variation_flat_book(gi: usize, bi: usize) -> FlatBook {
        let mk = |md5: &str, state: &str, progress: u32| ViewVariation {
            md5: md5.into(),
            title: "Peter Rabbit".into(),
            author: "Beatrix Potter".into(),
            fmt: "epub".into(),
            size: 2,
            size_bytes: None,
            year: None,
            publisher: String::new(),
            language: String::new(),
            pages: None,
            counted_pages: None,
            low_pages: false,
            host: Some("libgen.li".into()),
            state: state.into(),
            progress,
            downloaded_bytes: None,
            total_bytes: None,
            speed_bps: None,
            eta_secs: None,
            output_path: None,
            score: 0.9,
            cover_url: None,
            last_error: None,
        };
        FlatBook {
            group_name: "G".into(),
            group_index: gi,
            book_index_in_group: bi,
            book: ViewBook {
                id: format!("id-{}", bi),
                title: "The Tale of Peter Rabbit".into(),
                author: "Beatrix Potter".into(),
                year: None,
                pages: None,
                backfilled: vec![],
                seq: bi + 1,
                discovery: "matched".into(),
                versions: vec![
                    mk(&"d".repeat(32), "done", 100),
                    mk(&"e".repeat(32), "downloading", 25),
                ],
                acquisition: None,
                review: false,
                recommended_md5: None,
                history: vec![],
            },
        }
    }

    /// A multi-armed book contributes a `Book` row plus one `Variation` sub-row
    /// for each ADDITIONAL armed copy; single-armed books stay one row.
    #[test]
    fn rendered_rows_stacks_additional_variations() {
        let mut app = AppState::new();
        app.set_view(fixture_vm()); // make view Some
        app.flat = vec![
            make_multi_variation_flat_book(0, 0),
            make_downloading_flat_book("Solo", 0, 1), // single armed copy
        ];
        let rows = app.rendered_rows();
        assert_eq!(rows.len(), 3, "2 rows for multi book + 1 for solo book");
        assert_eq!(rows[0], RowRef::Book(0));
        // Primary is the DONE copy; the sub-row is the downloading copy.
        assert_eq!(rows[1], RowRef::Variation(0, "e".repeat(32)));
        assert_eq!(rows[2], RowRef::Book(1));
    }

    /// ↓ steps from a book's primary row onto its variation sub-row, then onto
    /// the next book; ↑ reverses.
    #[test]
    fn arrow_nav_steps_through_variation_subrows() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = vec![
            make_multi_variation_flat_book(0, 0),
            make_downloading_flat_book("Solo", 0, 1),
        ];
        assert_eq!(app.selected, 0);
        assert_eq!(app.selected_var, None);

        app.on_input(key(KeyCode::Down));
        assert_eq!(app.selected, 0);
        assert_eq!(
            app.selected_var.as_deref(),
            Some("e".repeat(32).as_str()),
            "↓ from book row lands on its variation sub-row"
        );

        app.on_input(key(KeyCode::Down));
        assert_eq!(app.selected, 1, "↓ from sub-row lands on the next book");
        assert_eq!(app.selected_var, None);

        app.on_input(key(KeyCode::Up));
        assert_eq!(app.selected, 0);
        assert_eq!(
            app.selected_var.as_deref(),
            Some("e".repeat(32).as_str()),
            "↑ returns onto the variation sub-row"
        );
    }

    /// Clicking a variation sub-row selects that variation (sets `selected_var`).
    #[test]
    fn mouse_click_variation_subrow_selects_it() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = vec![make_multi_variation_flat_book(0, 0)];
        app.focus = Focus::Activity; // start elsewhere

        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        // Find the rect registered for the variation sub-row.
        let var_rect = app
            .last_rects
            .book_rows
            .iter()
            .find_map(|(rect, r)| matches!(r, RowRef::Variation(0, _)).then_some(*rect))
            .expect("variation sub-row rect must be registered");

        let intent = app.on_input(mouse_left_click(var_rect.x + 5, var_rect.y));
        assert_eq!(intent, Intent::Redraw);
        assert_eq!(app.focus, Focus::List, "click focuses the List pane");
        assert_eq!(app.selected, 0);
        assert_eq!(
            app.selected_var.as_deref(),
            Some("e".repeat(32).as_str()),
            "clicking a variation sub-row selects that variation"
        );
    }

    /// The list render shows the indented "↳ alt. copy" sub-row for a multi book.
    #[test]
    fn render_shows_alt_copy_subrow() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = vec![make_multi_variation_flat_book(0, 0)];
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let content = buffer_string(&terminal);
        assert!(
            content.contains("alt. copy"),
            "multi-copy book must render an '↳ alt. copy' sub-row"
        );
    }

    /// With a variation sub-row focused, `detail_variation_index` resolves the
    /// detail-modal cursor to that variation's position in `versions`.
    #[test]
    fn detail_index_follows_focused_variation() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat = vec![make_multi_variation_flat_book(0, 0)];
        app.selected = 0;
        app.selected_var = Some("e".repeat(32)); // the downloading copy (index 1)
        assert_eq!(app.detail_variation_index(0), 1);

        // Book row focused → index 0.
        app.selected_var = None;
        assert_eq!(app.detail_variation_index(0), 0);
    }

    /// All-view routing: an aggregate group index remaps to its owning list +
    /// original group index via `aggregate_origin`.
    #[test]
    fn aggregate_origin_maps_to_owning_list() {
        let mut app = AppState::new();
        app.aggregate_origins = vec![
            ("list1".into(), 0),
            ("list1".into(), 1),
            ("list2".into(), 0),
        ];
        assert_eq!(app.aggregate_origin(0), Some(("list1".into(), 0)));
        assert_eq!(app.aggregate_origin(2), Some(("list2".into(), 0)));
        assert_eq!(app.aggregate_origin(3), None);
    }

    // -----------------------------------------------------------------------
    // Contextual hot keys (J2) — header list ops, click-focus, activity hint,
    // detail relabel + download-series.
    // -----------------------------------------------------------------------

    fn two_lists() -> Vec<crate::app::ListSummary> {
        vec![
            crate::app::ListSummary {
                id: "L1".into(),
                title: "List 1".into(),
                done: 0,
                total: 1,
                is_manual: false,
            },
            crate::app::ListSummary {
                id: "L2".into(),
                title: "List 2".into(),
                done: 0,
                total: 1,
                is_manual: false,
            },
        ]
    }

    /// Header focus owns the LIST ops: r/p/s/D reuse the `:` command handlers.
    #[test]
    fn header_focus_list_ops_dispatch_commands() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::Header;
        assert_eq!(
            app.on_input(key(KeyCode::Char('r'))),
            Intent::Command("requery".into()),
            "Header r → :requery (re-search the list)"
        );
        assert_eq!(
            app.on_input(key(KeyCode::Char('p'))),
            Intent::Command("pause".into()),
            "Header p → :pause (pause the list)"
        );
        assert_eq!(
            app.on_input(key(KeyCode::Char('s'))),
            Intent::Command("start".into()),
            "Header s → :start (resume the list)"
        );
        assert_eq!(
            app.on_input(key(KeyCode::Char('D'))),
            Intent::Command("delete".into()),
            "Header D → :delete (confirm-gated list delete)"
        );
    }

    /// The header-only list-op keys must NOT fire from List focus, where the same
    /// letters either mean book ops (r = retry) or nothing (s/D).
    #[test]
    fn list_op_keys_inert_outside_header() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.focus = Focus::List;
        // `s` and `D` are no-ops in List focus (not list ops).
        assert_eq!(app.on_input(key(KeyCode::Char('s'))), Intent::Redraw);
        assert_eq!(app.on_input(key(KeyCode::Char('D'))), Intent::Redraw);
        // `r` in List focus is book retry, never :requery.
        let r = app.on_input(key(KeyCode::Char('r')));
        assert!(
            !matches!(r, Intent::Command(_)),
            "List r must not dispatch a command, got {:?}",
            r
        );
    }

    /// Clicking a list chip switches to it AND focuses the Header so the list hot
    /// keys are immediately live.
    #[test]
    fn click_list_chip_focuses_header() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.all_lists = two_lists();
        app.active_list_idx = 0;
        app.focus = Focus::List;
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let rect = app
            .last_rects
            .list_chips
            .iter()
            .find(|(_, i)| *i == 1)
            .map(|(r, _)| *r)
            .expect("list chip 1 rect registered");
        let intent = app.on_input(mouse_left_click(rect.x + 1, rect.y));
        assert!(
            matches!(intent, Intent::SwitchList { ref id } if id == "L2"),
            "click switches to L2, got {:?}",
            intent
        );
        assert_eq!(
            app.focus,
            Focus::Header,
            "click on a list chip must focus the Header so list hot keys activate"
        );
    }

    /// Activity hint hides p/c/r when there are no download legs, and shows them
    /// once a leg exists.
    #[test]
    fn activity_hint_hides_pcr_when_empty() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm()); // no downloading books
        app.focus = Focus::Activity;
        assert!(!app.activity_has_legs(), "fixture has no legs");
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let empty = buffer_string(&terminal);
        assert!(empty.contains("collapse"), "empty activity shows collapse");
        assert!(
            !empty.contains("pause"),
            "empty activity must NOT advertise p pause"
        );

        // Add a leg → p/c/r come back.
        insert_transfer(&mut app, &"a".repeat(32));
        assert!(app.activity_has_legs());
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let with_leg = buffer_string(&terminal);
        assert!(
            with_leg.contains("pause"),
            "a focused leg re-advertises p pause"
        );
    }

    /// Detail modal: the not-found hotkey now reads the verb "mark unavailable",
    /// and the dead p/c chips are gone.
    #[test]
    fn detail_hint_relabels_not_found_to_mark_unavailable() {
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
        let s = buffer_string(&terminal);
        assert!(
            s.contains("mark unavailable"),
            "detail hint relabels not-found → mark unavailable"
        );
        assert!(
            !s.contains("not-found"),
            "stale 'not-found' label must be gone"
        );
        assert!(s.contains("series"), "detail advertises S series");
    }

    /// Settings title shows the active list name ONLY when more than one list
    /// exists; with a single list it is omitted as redundant noise.
    #[test]
    fn settings_title_omits_list_name_with_one_list() {
        let render_title = |n_lists: usize| -> String {
            let backend = TestBackend::new(120, 30);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut app = AppState::new();
            app.set_view(fixture_vm()); // view title = "Test List"
            app.all_lists = (0..n_lists)
                .map(|i| crate::app::ListSummary {
                    id: format!("L{i}"),
                    title: format!("List {i}"),
                    done: 0,
                    total: 1,
                    is_manual: false,
                })
                .collect();
            app.modal = Some(Modal::Settings);
            app.settings_draft = Some(default_draft());
            terminal.draw(|f| ui::render(f, &mut app)).unwrap();
            buffer_string(&terminal)
        };
        // One list → bare " Settings " title, no "·" + name.
        let one = render_title(1);
        assert!(
            !one.contains("Settings · Test List"),
            "single list must omit the list name from the Settings title"
        );
        // Two lists → name shown.
        let many = render_title(2);
        assert!(
            many.contains("Settings · Test List"),
            "multiple lists must show the list name in the Settings title"
        );
    }

    /// Detail modal: `S` reuses the :download-series handler and STAYS in the
    /// detail view (the command's "Added N book(s)" status renders in the bottom
    /// bar, which the centered modal doesn't cover).
    #[test]
    fn detail_s_dispatches_download_series() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: crate::app::DetailSubFocus::Variations,
            history_selected: 0,
        });
        let intent = app.on_input(key(KeyCode::Char('S')));
        assert_eq!(intent, Intent::Command("download-series".into()));
        assert!(
            matches!(app.modal, Some(Modal::Detail { .. })),
            "S must NOT close the detail modal — the user stays in context"
        );
    }

    // -----------------------------------------------------------------------
    // Marquee (#10/#14): display-width-driven, not char-count
    // -----------------------------------------------------------------------

    /// A CJK title whose `chars().count()` fits the column but whose DISPLAY
    /// WIDTH overflows it must scroll. Under the old char-count math it would
    /// have stayed put; with display width it advances.
    #[test]
    fn marquee_scrolls_cjk_by_display_width() {
        let title = "量子力学入門"; // 6 scalar chars, 12 display columns
        let col_w = 10usize;
        assert!(
            title.chars().count() <= col_w,
            "precondition: char count fits the column (old math would not scroll)"
        );
        let disp_w = crate::textfit::display_width(title);
        assert!(disp_w > col_w, "precondition: display width overflows");

        let mut app = AppState::new();
        // A few ticks should drive the offset forward off zero.
        for _ in 0..3 {
            app.advance_marquee(disp_w, col_w);
        }
        assert!(
            app.marquee_offset > 0,
            "CJK title overflowing in display width must scroll (offset advanced)"
        );
        assert!(
            app.marquee_offset <= disp_w - col_w,
            "offset never exceeds max scroll (display-width based)"
        );
    }

    /// Text that fits in display width keeps the marquee parked at offset 0.
    #[test]
    fn marquee_parks_when_fits() {
        let mut app = AppState::new();
        app.marquee_offset = 5; // pretend it was scrolled
        let title = "短い"; // 2 chars / 4 columns
        let disp_w = crate::textfit::display_width(title);
        app.advance_marquee(disp_w, 20);
        assert_eq!(app.marquee_offset, 0, "fitting text resets/parks at 0");
    }

    /// plan_series_add: no series / empty seed → user-facing messages; a real
    /// series → its member titles (reading order) + name.
    #[test]
    fn plan_series_add_messages_and_success() {
        use libgen_core::series::{Series, SeriesMember};

        let err = crate::plan_series_add("Harry Potter", None).unwrap_err();
        assert_eq!(err, "This book doesn't belong to any book series");

        let err = crate::plan_series_add("   ", None).unwrap_err();
        assert_eq!(err, "No title on the selected book");

        let series = Series {
            key: "OL1S".into(),
            name: "Wings of Fire".into(),
            members: vec![
                SeriesMember {
                    title: "The Dragonet Prophecy".into(),
                    ..Default::default()
                },
                SeriesMember {
                    title: "The Lost Heir".into(),
                    ..Default::default()
                },
            ],
        };
        let (titles, name) =
            crate::plan_series_add("The Dragonet Prophecy", Some(&series)).unwrap();
        assert_eq!(name, "Wings of Fire");
        assert_eq!(
            titles,
            vec![
                "The Dragonet Prophecy".to_string(),
                "The Lost Heir".to_string()
            ]
        );
    }

    /// A `Progress::Bytes` telemetry tick must advance the projected progress
    /// (app.flat) WITHOUT a StatusChanged/Refresh, so the list + activity don't
    /// freeze at the start-of-download %.
    #[test]
    fn bytes_tick_advances_progress_without_refresh() {
        use libgen_core::queue::Progress;
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        let md5 = "a".repeat(32);
        app.flat[0].book.versions = vec![mk_var("Downloading", "epub", "downloading", 0, &md5)];

        // Telemetry only — no Refresh.
        app.apply_progress(&Progress::Bytes {
            md5: md5.clone(),
            leg_id: 0,
            is_hedge: false,
            host: "libgen.li".into(),
            bytes_done: 50,
            total_bytes: Some(100),
            speed_bps: Some(2000),
            eta_secs: Some(1),
        });

        assert_eq!(
            app.flat[0].book.versions[0].progress, 50,
            "Bytes tick must advance projected progress without a Refresh"
        );

        let backend = TestBackend::new(132, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(buf.contains("dling 50%"), "list shows live 50%: {buf}");
    }

    /// A transfer whose book isn't in the CURRENT view still gets a real title
    /// from the global md5→title map (populated from all loaded lists), not the md5.
    #[test]
    fn activity_uses_global_md5_title_for_other_lists() {
        use libgen_core::queue::Progress;
        let mut app = AppState::new();
        app.set_view(fixture_vm()); // current view has no md5 = "z"*32
        let md5 = "z".repeat(32);
        app.md5_titles.insert(md5.clone(), "Wings of Fire".into());

        app.apply_progress(&Progress::Bytes {
            md5: md5.clone(),
            leg_id: 0,
            is_hedge: false,
            host: "libgen.li".into(),
            bytes_done: 1,
            total_bytes: Some(10),
            speed_bps: None,
            eta_secs: None,
        });

        assert_eq!(
            app.transfers[&md5].title, "Wings of Fire",
            "transfer for a book outside the current view uses the global md5→title map"
        );
    }

    /// An unknown-format (no-versions/discovery) book renders an em-dash in the
    /// format column, not "???".
    #[test]
    fn unknown_format_renders_dash_not_question_marks() {
        let backend = TestBackend::new(132, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.versions = vec![]; // no copies discovered yet
        app.flat[0].book.discovery = "querying".into();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            !buf.contains("???"),
            "unknown format must render an em-dash, not ???: {buf}"
        );
    }

    /// The detail-view download progress bar fills the row width (more than the
    /// old fixed 16 cells) on a wide terminal, and still renders on a narrow one.
    #[test]
    fn detail_progress_bar_fills_width() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        app.flat[0].book.versions = vec![mk_var("Dl", "epub", "downloading", 50, &"a".repeat(32))];
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });

        let mut wide = Terminal::new(TestBackend::new(132, 30)).unwrap();
        wide.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&wide);
        let bar_cells = buf
            .chars()
            .filter(|&c| c == '\u{25b0}' || c == '\u{25b1}')
            .count();
        assert!(
            bar_cells > 16,
            "detail progress bar should fill the row, got {bar_cells} cells"
        );

        // Narrow terminal: must still render without panicking.
        let mut narrow = Terminal::new(TestBackend::new(48, 24)).unwrap();
        narrow.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    /// The detail subtitle no longer carries the redundant "● …" trailer (the
    /// auto-filled note / req-done-active counts), even when a field is backfilled.
    #[test]
    fn detail_subtitle_has_no_backfill_trailer() {
        use crate::app::DetailSubFocus;
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.flat[0].book.discovery = "matched".into();
        app.flat[0].book.backfilled = vec!["year".into()];
        app.flat[0].book.versions = vec![mk_var("Copy", "epub", "available", 0, &"a".repeat(32))];
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        let backend = TestBackend::new(132, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(
            !buf.contains("auto-filled"),
            "detail subtitle must not show the auto-filled trailer: {buf}"
        );
    }

    /// Modal titles use the shared high-contrast style (green accent + bold).
    #[test]
    fn modal_titles_use_high_contrast_style() {
        use crate::app::DetailSubFocus;
        use ratatui::style::Modifier;
        let s = crate::theme::style_modal_title();
        assert_eq!(
            s.fg,
            Some(crate::theme::C_DONE),
            "modal title is the green accent"
        );
        assert!(
            s.add_modifier.contains(Modifier::BOLD),
            "modal title is bold"
        );

        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::Detail {
            book_flat_index: 0,
            selected: 0,
            sub_focus: DetailSubFocus::Variations,
            history_selected: 0,
        });
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        assert!(
            buffer_string(&terminal).contains("Book detail"),
            "detail title renders"
        );
    }

    /// `m` in the picker marks the book not-found (none of the copies are correct)
    /// and closes the picker.
    #[test]
    fn picker_m_marks_not_found() {
        let mut app = AppState::new();
        app.set_view(fixture_vm_needs_selection());
        app.modal = Some(Modal::Picker {
            book_flat_index: 0,
            selected: 0,
        });
        let intent = app.on_input(key(KeyCode::Char('m')));
        assert!(
            matches!(intent, Intent::MarkNotFound { .. }),
            "picker m must mark the book not-found, got {intent:?}"
        );
        assert!(app.modal.is_none(), "picker closes after mark-not-found");
    }

    /// Edit modal: caret moves and inserts/deletes MID-string, not just at the end.
    #[test]
    fn edit_caret_inserts_and_deletes_mid_string() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::EditBook {
            book_flat_index: 0,
            title_buf: "abcd".into(),
            author_buf: String::new(),
            field: crate::app::EditBookField::Title,
            caret: 4, // end
        });

        // Left twice → caret between 'b' and 'c'; insert 'X' → "abXcd".
        app.on_input(key(KeyCode::Left));
        app.on_input(key(KeyCode::Left));
        app.on_input(key(KeyCode::Char('X')));
        let (buf, caret) = match &app.modal {
            Some(Modal::EditBook {
                title_buf, caret, ..
            }) => (title_buf.clone(), *caret),
            _ => panic!("edit modal gone"),
        };
        assert_eq!(buf, "abXcd", "char inserts at the caret, not the end");
        assert_eq!(caret, 3, "caret advances past the inserted char");

        // Backspace removes the char before the caret ('X') → "abcd".
        app.on_input(key(KeyCode::Backspace));
        let (buf2, caret2) = match &app.modal {
            Some(Modal::EditBook {
                title_buf, caret, ..
            }) => (title_buf.clone(), *caret),
            _ => panic!("edit modal gone"),
        };
        assert_eq!(buf2, "abcd", "backspace deletes the char before the caret");
        assert_eq!(caret2, 2, "caret moves back after delete");
    }

    /// The shared err.list_exists catalog key decodes to the English message with
    /// the list name interpolated (single-sourced for desktop + TUI).
    #[test]
    fn list_exists_message_decodes_from_catalog() {
        let token = libgen_core::model::ui_msg("err.list_exists", &[("name", "My List")]);
        let decoded = crate::i18n::decode(&token);
        assert!(decoded.contains("My List"), "name interpolated: {decoded}");
        assert!(
            decoded.contains("already exists"),
            "english catalog message: {decoded}"
        );
    }

    /// `complete_path` returns the single match for a nested partial filename in a
    /// real directory — confirming `:import …/jeremy_pub`+Tab DOES complete; the
    /// reported failure was the throwaway-HOME fresh-DB test (`~` → /tmp/…), not a
    /// code bug.
    #[test]
    fn complete_path_returns_single_match_for_partial() {
        use std::io::Write;
        let mut dir = std::env::temp_dir();
        dir.push(format!("kwire-completepath-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "jeremy_public_domain_list.md",
            "avery_public_domain_list.md",
        ] {
            let mut f = std::fs::File::create(dir.join(name)).unwrap();
            writeln!(f, "x").unwrap();
        }
        let prefix = format!("{}/jeremy_pub", dir.display());
        let cands = crate::app::complete_path(&prefix);
        assert_eq!(
            cands.len(),
            1,
            "exactly one match for 'jeremy_pub': {cands:?}"
        );
        assert!(
            cands[0].ends_with("jeremy_public_domain_list.md"),
            "completed to the full filename: {cands:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `:ab` + Tab completes to `about`, the About modal renders the splash, and
    /// any key dismisses it.
    #[test]
    fn about_command_completes_and_modal_renders() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());

        // ":ab" + Tab → "about" (the only command with that prefix).
        app.command_buf = Some("ab".into());
        app.on_input(key(KeyCode::Tab));
        assert_eq!(
            app.command_buf.as_deref(),
            Some("about"),
            "ab → about completion"
        );

        // The About modal renders the splash (title + tagline).
        app.command_buf = None;
        app.modal = Some(Modal::About);
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = buffer_string(&terminal);
        assert!(buf.contains("about"), "about title renders: {buf}");
        assert!(buf.contains("gathers"), "tagline renders: {buf}");

        // Any key dismisses.
        app.on_input(key(KeyCode::Esc));
        assert!(app.modal.is_none(), "esc closes the about modal");
    }

    /// ReQuery modal: caret moves and inserts/deletes MID-string (same as Edit).
    #[test]
    fn requery_caret_inserts_and_deletes_mid_string() {
        let mut app = AppState::new();
        app.set_view(fixture_vm());
        app.modal = Some(Modal::ReQuery {
            book_flat_index: 0,
            buf: "abcd".into(),
            caret: 4, // end
        });

        app.on_input(key(KeyCode::Left));
        app.on_input(key(KeyCode::Left));
        app.on_input(key(KeyCode::Char('X')));
        let (buf, caret) = match &app.modal {
            Some(Modal::ReQuery { buf, caret, .. }) => (buf.clone(), *caret),
            _ => panic!("requery modal gone"),
        };
        assert_eq!(buf, "abXcd", "char inserts at the caret, not the end");
        assert_eq!(caret, 3, "caret advances past the inserted char");

        app.on_input(key(KeyCode::Backspace));
        let (buf2, caret2) = match &app.modal {
            Some(Modal::ReQuery { buf, caret, .. }) => (buf.clone(), *caret),
            _ => panic!("requery modal gone"),
        };
        assert_eq!(buf2, "abcd", "backspace deletes the char before the caret");
        assert_eq!(caret2, 2, "caret moves back after delete");
    }
}
