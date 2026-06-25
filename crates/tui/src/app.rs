//! [`AppState`] — the pure, side-effect-free app state and its reducers.
//!
//! `on_input` NEVER does I/O; it returns an [`Intent`] that the event loop
//! dispatches. `apply` folds engine events into the state.  Both are trivially
//! unit-testable because they take and return plain data.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use libgen_engine::{ViewBook, ViewGroup, ViewModel};
use ratatui::layout::Rect;

use crate::intent::Intent;

/// Commands supported in `:` command-line mode (used for Tab-completion).
const COMMANDS: &[&str] = &[
    "import",
    "add",
    "open",
    "requery",
    "settings",
    "pause-all",
    "quit",
    "help",
];

// ---------------------------------------------------------------------------
// Focus
// ---------------------------------------------------------------------------

/// Which panel currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    List,
    Activity,
}

// ---------------------------------------------------------------------------
// Status filter (§ wireframe filter row)
// ---------------------------------------------------------------------------

/// The six coarse status-filter chips (same vocabulary as the ViewModel's
/// `discovery` + `acquisition` roll-up).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusFilter {
    #[default]
    All,
    NeedsYou,   // discovery == needs_selection
    Check,      // review flag set
    Cannot,     // discovery == not_found  OR  state == failed/cancelled
    InProgress, // any variation state == downloading
    Done,       // acquisition.done >= 1 && active == 0
}

impl StatusFilter {
    pub fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "All",
            StatusFilter::NeedsYou => "Needs you",
            StatusFilter::Check => "Check",
            StatusFilter::Cannot => "Cannot",
            StatusFilter::InProgress => "In progress",
            StatusFilter::Done => "Done",
        }
    }

    /// Cycle through filters in order.
    pub fn next(self) -> StatusFilter {
        match self {
            StatusFilter::All => StatusFilter::NeedsYou,
            StatusFilter::NeedsYou => StatusFilter::Check,
            StatusFilter::Check => StatusFilter::Cannot,
            StatusFilter::Cannot => StatusFilter::InProgress,
            StatusFilter::InProgress => StatusFilter::Done,
            StatusFilter::Done => StatusFilter::All,
        }
    }
}

// ---------------------------------------------------------------------------
// Modal
// ---------------------------------------------------------------------------

/// Overlay modals that take over keyboard input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// "Choose a copy" variation picker.
    Picker {
        book_flat_index: usize,
        /// The picker's own selection index (row within the variation list).
        selected: usize,
    },
    /// Book detail + history.
    Detail { book_flat_index: usize },
    /// Settings key-value editor.
    Settings,
    /// Full help screen.
    Help,
}

// ---------------------------------------------------------------------------
// LastRects — stores panel Rects from the last render for mouse hit-testing
// ---------------------------------------------------------------------------

/// Rects from the most-recent render pass; used for mouse hit-testing.
/// All fields default to `Rect::default()` (zero-sized, at origin).
#[derive(Debug, Clone, Default)]
pub struct LastRects {
    pub list_strip: Rect,
    pub filter_row: Rect,
    pub book_table: Rect,
    pub activity: Rect,
    pub hint_bar: Rect,
    /// `(row_rect, flat_index)` for each rendered book row.
    pub book_rows: Vec<(Rect, usize)>,
    /// `(chip_rect, StatusFilter)` for each rendered filter chip.
    pub filter_chips: Vec<(Rect, StatusFilter)>,
}

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// Live in-flight transfer telemetry, updated from Progress events.
#[derive(Debug, Clone, Default)]
pub struct ActiveTransfer {
    pub md5: String,
    pub host: String,
    pub bytes_done: u64,
    pub total_bytes: Option<u64>,
    pub speed_bps: Option<u64>,
    pub eta_secs: Option<u64>,
    /// Derived from the most-recent ViewBook that has this md5.
    pub title: String,
}

/// Full TUI application state.  Only plain data — no I/O handles.
pub struct AppState {
    /// The projected library snapshot the UI renders from.
    pub view: Option<ViewModel>,

    /// Flat ordered list of all visible books (group + book pairs) after
    /// filtering.  Rebuilt whenever `view` or `filter` changes.
    pub flat: Vec<FlatBook>,

    /// Index into `flat` for the currently selected row.
    pub selected: usize,

    /// Active status filter.
    pub filter: StatusFilter,

    /// Which panel holds focus.
    pub focus: Focus,

    /// Whether the Activity pane is expanded.
    pub activity_expanded: bool,

    /// Redraw-tick counter (incremented once per 120 ms tick; drives spinner).
    pub tick: u64,

    /// Command-line mode: if `Some(_)` the hint bar is replaced by an edit field.
    pub command_buf: Option<String>,

    /// Active overlay modal, if any.
    pub modal: Option<Modal>,

    /// Activity pane scroll offset (when focus == Activity).
    pub activity_selected: usize,

    /// Rects from the most-recent render pass (for mouse hit-testing).
    pub last_rects: LastRects,

    /// Which settings row is selected in the Settings modal.
    pub settings_selected: usize,

    /// Inline edit buffer for the Settings modal.
    pub settings_edit: Option<String>,

    /// Live scheduler telemetry keyed by md5. Updated by Progress events from the engine.
    pub transfers: std::collections::HashMap<String, ActiveTransfer>,

    /// Tab-completion candidates for the `:` command line.
    /// Non-empty while the wildmenu is visible.
    pub completion_candidates: Vec<String>,

    /// Index of the highlighted candidate within `completion_candidates`.
    pub completion_index: usize,
}

/// A single visible book row, carrying enough context to dispatch engine calls.
#[derive(Debug, Clone)]
pub struct FlatBook {
    pub group_name: String,
    /// Index into `ViewModel::groups` that owns this book.
    pub group_index: usize,
    /// Index within that group's `books` slice.
    pub book_index_in_group: usize,
    pub book: ViewBook,
}

impl AppState {
    /// Construct an empty state (no list loaded yet).
    pub fn new() -> Self {
        AppState {
            view: None,
            flat: Vec::new(),
            selected: 0,
            filter: StatusFilter::All,
            focus: Focus::List,
            activity_expanded: true,
            tick: 0,
            command_buf: None,
            modal: None,
            activity_selected: 0,
            last_rects: LastRects::default(),
            settings_selected: 0,
            settings_edit: None,
            transfers: std::collections::HashMap::new(),
            completion_candidates: Vec::new(),
            completion_index: 0,
        }
    }

    /// Apply a raw engine Progress event into the live transfer map.
    pub fn apply_progress(&mut self, p: &libgen_core::queue::Progress) {
        use libgen_core::queue::Progress::*;
        match p {
            Resolved {
                md5,
                host,
                total_bytes,
                ..
            } => {
                let t = self.transfers.entry(md5.clone()).or_default();
                t.md5 = md5.clone();
                t.host = host.clone();
                t.total_bytes = *total_bytes;
            }
            Bytes {
                md5,
                host,
                bytes_done,
                total_bytes,
                speed_bps,
                eta_secs,
                ..
            } => {
                let t = self.transfers.entry(md5.clone()).or_default();
                t.md5 = md5.clone();
                t.host = host.clone();
                t.bytes_done = *bytes_done;
                t.total_bytes = *total_bytes;
                t.speed_bps = *speed_bps;
                t.eta_secs = *eta_secs;
                // Populate title from current ViewModel if we can find it.
                if t.title.is_empty() {
                    if let Some(vm) = &self.view {
                        'outer: for g in &vm.groups {
                            for b in &g.books {
                                if b.versions.iter().any(|v| v.md5 == *md5) {
                                    t.title = b.title.clone();
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
            }
            Done { md5, .. } => {
                self.transfers.remove(md5);
            }
            Cancelled { md5, .. } | Failed { md5, .. } => {
                self.transfers.remove(md5);
            }
            _ => {}
        }
    }

    /// Replace the projected view and rebuild the flat list.
    pub fn set_view(&mut self, vm: ViewModel) {
        self.view = Some(vm);
        self.rebuild_flat();
        // Clamp selection in case the new list is shorter.
        if !self.flat.is_empty() && self.selected >= self.flat.len() {
            self.selected = self.flat.len() - 1;
        }
    }

    // -----------------------------------------------------------------------
    // Pure reducer — NO side effects
    // -----------------------------------------------------------------------

    /// Process one terminal [`Event`]; return the [`Intent`] the event loop
    /// should act on.  This method MUST be side-effect-free (no I/O, no
    /// network, no locks).
    pub fn on_input(&mut self, ev: Event) -> Intent {
        // If command mode is active, route there first.
        if self.command_buf.is_some() {
            return self.handle_command_input(ev);
        }

        // If a modal is open, route input there.
        if self.modal.is_some() {
            return self.handle_modal_input(ev);
        }

        match ev {
            // ---------------------------------------------------------------
            // Keyboard
            // ---------------------------------------------------------------
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => {
                // Ctrl-C always quits.
                if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
                    return Intent::Quit;
                }
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => Intent::Quit,
                    KeyCode::Down | KeyCode::Char('j') => {
                        match self.focus {
                            Focus::List => self.move_selection(1),
                            Focus::Activity => self.scroll_activity(1),
                        }
                        Intent::Redraw
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        match self.focus {
                            Focus::List => self.move_selection_up(),
                            Focus::Activity => self.scroll_activity_up(),
                        }
                        Intent::Redraw
                    }
                    KeyCode::Tab => {
                        self.focus = match self.focus {
                            Focus::List => Focus::Activity,
                            Focus::Activity => Focus::List,
                        };
                        Intent::Redraw
                    }
                    KeyCode::Char('/') => {
                        self.filter = self.filter.next();
                        self.rebuild_flat();
                        Intent::Redraw
                    }
                    KeyCode::Char('1') => {
                        self.filter = StatusFilter::All;
                        self.rebuild_flat();
                        Intent::Redraw
                    }
                    KeyCode::Char('2') => {
                        self.filter = StatusFilter::NeedsYou;
                        self.rebuild_flat();
                        Intent::Redraw
                    }
                    KeyCode::Char('3') => {
                        self.filter = StatusFilter::Check;
                        self.rebuild_flat();
                        Intent::Redraw
                    }
                    KeyCode::Char('4') => {
                        self.filter = StatusFilter::Cannot;
                        self.rebuild_flat();
                        Intent::Redraw
                    }
                    KeyCode::Char('5') => {
                        self.filter = StatusFilter::InProgress;
                        self.rebuild_flat();
                        Intent::Redraw
                    }
                    KeyCode::Char('6') => {
                        self.filter = StatusFilter::Done;
                        self.rebuild_flat();
                        Intent::Redraw
                    }
                    KeyCode::Char(':') => {
                        self.command_buf = Some(String::new());
                        Intent::Redraw
                    }
                    KeyCode::Enter => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            if fb.book.discovery == "needs_selection" {
                                Intent::OpenPicker {
                                    flat_index: self.selected,
                                }
                            } else {
                                Intent::OpenDetail {
                                    flat_index: self.selected,
                                }
                            }
                        } else {
                            Intent::Redraw
                        }
                    }
                    KeyCode::Char('d') => Intent::OpenDetail {
                        flat_index: self.selected,
                    },
                    KeyCode::Char('r') => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            Intent::Retry {
                                group_path: vec![fb.group_index],
                                book_index: fb.book_index_in_group,
                            }
                        } else {
                            Intent::Redraw
                        }
                    }
                    KeyCode::Char('p') => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            Intent::Pause {
                                group_path: vec![fb.group_index],
                                book_index: fb.book_index_in_group,
                            }
                        } else {
                            Intent::Redraw
                        }
                    }
                    KeyCode::Char('c') => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            Intent::Cancel {
                                group_path: vec![fb.group_index],
                                book_index: fb.book_index_in_group,
                            }
                        } else {
                            Intent::Redraw
                        }
                    }
                    KeyCode::Char('o') => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            if let Some(path) =
                                fb.book.versions.iter().find_map(|v| v.output_path.clone())
                            {
                                return Intent::OpenFile(path);
                            }
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('R') => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            if let Some(path) =
                                fb.book.versions.iter().find_map(|v| v.output_path.clone())
                            {
                                return Intent::RevealFile(path);
                            }
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('?') => Intent::OpenHelp,
                    KeyCode::Left | KeyCode::Right => Intent::Redraw,
                    KeyCode::Char('a') => {
                        // Request all preferred format variations — UI-only stub for Stage 3.
                        Intent::Redraw
                    }
                    _ => Intent::Redraw,
                }
            }

            // ---------------------------------------------------------------
            // Mouse: clicks map to the same intents as keys
            // ---------------------------------------------------------------
            Event::Mouse(me) => match me.kind {
                MouseEventKind::ScrollDown => {
                    // Wheel scrolls whichever pane holds focus (§6).
                    match self.focus {
                        Focus::List => self.move_selection(1),
                        Focus::Activity => self.scroll_activity(1),
                    }
                    Intent::Redraw
                }
                MouseEventKind::ScrollUp => {
                    match self.focus {
                        Focus::List => self.move_selection_up(),
                        Focus::Activity => self.scroll_activity_up(),
                    }
                    Intent::Redraw
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    let col = me.column;
                    let row = me.row;

                    // Hit-test book rows.
                    let book_rows = self.last_rects.book_rows.clone();
                    for (rect, flat_index) in &book_rows {
                        if col >= rect.x
                            && col < rect.x + rect.width
                            && row >= rect.y
                            && row < rect.y + rect.height
                        {
                            self.selected = *flat_index;
                            return Intent::Redraw;
                        }
                    }

                    // Hit-test filter chips.
                    let filter_chips = self.last_rects.filter_chips.clone();
                    for (rect, filter) in &filter_chips {
                        if col >= rect.x
                            && col < rect.x + rect.width
                            && row >= rect.y
                            && row < rect.y + rect.height
                        {
                            self.filter = *filter;
                            self.rebuild_flat();
                            return Intent::Redraw;
                        }
                    }

                    // Hit-test activity header to toggle expand.
                    let act = self.last_rects.activity;
                    if col >= act.x && col < act.x + act.width && row == act.y {
                        self.activity_expanded = !self.activity_expanded;
                        return Intent::Redraw;
                    }

                    Intent::Redraw
                }
                _ => Intent::Redraw,
            },

            Event::Resize(_, _) => Intent::Redraw,
            _ => Intent::Redraw,
        }
    }

    // -----------------------------------------------------------------------
    // Modal input routing
    // -----------------------------------------------------------------------

    fn handle_modal_input(&mut self, ev: Event) -> Intent {
        let modal = match &self.modal {
            Some(m) => m.clone(),
            None => return Intent::Redraw,
        };

        match &modal {
            Modal::Picker {
                book_flat_index,
                selected,
            } => {
                let flat_index = *book_flat_index;
                let sel = *selected;
                match ev {
                    Event::Key(KeyEvent { code, .. }) => match code {
                        KeyCode::Esc => {
                            self.modal = None;
                            Intent::Redraw
                        }
                        KeyCode::Enter => {
                            // Select the chosen variation.
                            if let Some(fb) = self.flat.get(flat_index) {
                                if let Some(v) = fb.book.versions.get(sel) {
                                    let md5 = v.md5.clone();
                                    self.modal = None;
                                    return Intent::Select {
                                        group_path: vec![fb.group_index],
                                        book_index: fb.book_index_in_group,
                                        md5,
                                    };
                                }
                            }
                            self.modal = None;
                            Intent::Redraw
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let max = self
                                .flat
                                .get(flat_index)
                                .map(|fb| fb.book.versions.len().saturating_sub(1))
                                .unwrap_or(0);
                            let new_sel = (sel + 1).min(max);
                            self.modal = Some(Modal::Picker {
                                book_flat_index: flat_index,
                                selected: new_sel,
                            });
                            Intent::Redraw
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            let new_sel = sel.saturating_sub(1);
                            self.modal = Some(Modal::Picker {
                                book_flat_index: flat_index,
                                selected: new_sel,
                            });
                            Intent::Redraw
                        }
                        KeyCode::Char('a') => {
                            // Request all preferred format variations — stub.
                            Intent::Redraw
                        }
                        _ => Intent::Redraw,
                    },
                    _ => Intent::Redraw,
                }
            }

            Modal::Detail { book_flat_index } => {
                let flat_index = *book_flat_index;
                match ev {
                    Event::Key(KeyEvent { code, .. }) => match code {
                        KeyCode::Esc => {
                            self.modal = None;
                            Intent::Redraw
                        }
                        KeyCode::Char('o') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                if let Some(path) =
                                    fb.book.versions.iter().find_map(|v| v.output_path.clone())
                                {
                                    return Intent::OpenFile(path);
                                }
                            }
                            Intent::Redraw
                        }
                        KeyCode::Char('R') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                if let Some(path) =
                                    fb.book.versions.iter().find_map(|v| v.output_path.clone())
                                {
                                    return Intent::RevealFile(path);
                                }
                            }
                            Intent::Redraw
                        }
                        KeyCode::Char('r') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                return Intent::Retry {
                                    group_path: vec![fb.group_index],
                                    book_index: fb.book_index_in_group,
                                };
                            }
                            Intent::Redraw
                        }
                        _ => Intent::Redraw,
                    },
                    _ => Intent::Redraw,
                }
            }

            Modal::Settings => match ev {
                Event::Key(KeyEvent { code, .. }) => match code {
                    KeyCode::Esc => {
                        self.modal = None;
                        self.settings_edit = None;
                        Intent::Redraw
                    }
                    KeyCode::Enter => {
                        // Commit inline edit — stub for Stage 3.
                        self.settings_edit = None;
                        Intent::Redraw
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.settings_selected = self.settings_selected.saturating_add(1);
                        Intent::Redraw
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.settings_selected = self.settings_selected.saturating_sub(1);
                        Intent::Redraw
                    }
                    _ => Intent::Redraw,
                },
                _ => Intent::Redraw,
            },

            Modal::Help => match ev {
                Event::Key(KeyEvent { code, .. }) => match code {
                    KeyCode::Esc | KeyCode::Char('?') => {
                        self.modal = None;
                        Intent::Redraw
                    }
                    _ => Intent::Redraw,
                },
                _ => Intent::Redraw,
            },
        }
    }

    // -----------------------------------------------------------------------
    // Tick (redraw timer — advance spinner)
    // -----------------------------------------------------------------------

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn move_selection(&mut self, delta: usize) {
        if self.flat.is_empty() {
            return;
        }
        self.selected = (self.selected + delta).min(self.flat.len() - 1);
    }

    fn move_selection_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Number of in-flight transfer rows the Activity pane can scroll through.
    /// Counts BOOKS that have at least one downloading version (matching the
    /// one-row-per-book display in `render_activity`), with a fallback to the
    /// live-telemetry transfer count.
    fn activity_row_count(&self) -> usize {
        let flat_count = self
            .flat
            .iter()
            .filter(|fb| fb.book.versions.iter().any(|v| v.state == "downloading"))
            .count();
        if flat_count > 0 {
            flat_count
        } else {
            self.transfers.len()
        }
    }

    fn scroll_activity(&mut self, delta: usize) {
        let n = self.activity_row_count();
        if n == 0 {
            self.activity_selected = 0;
            return;
        }
        self.activity_selected = (self.activity_selected + delta).min(n - 1);
    }

    fn scroll_activity_up(&mut self) {
        if self.activity_selected > 0 {
            self.activity_selected -= 1;
        }
    }

    /// Handle key events while in command-line mode (`:`).
    fn handle_command_input(&mut self, ev: Event) -> Intent {
        match ev {
            Event::Key(KeyEvent { code, .. }) => match code {
                // ── Completion navigation ──────────────────────────────────
                KeyCode::Tab => {
                    self.handle_completion_tab(false);
                    Intent::Redraw
                }
                KeyCode::BackTab => {
                    self.handle_completion_tab(true);
                    Intent::Redraw
                }

                // ── Submit / accept ────────────────────────────────────────
                KeyCode::Enter => {
                    if !self.completion_candidates.is_empty() {
                        // Wildmenu open: accept selection, don't submit yet.
                        let candidate = self.completion_candidates[self.completion_index].clone();
                        self.accept_completion(&candidate);
                        Intent::Redraw
                    } else {
                        let line = self.command_buf.take().unwrap_or_default();
                        Intent::Command(line)
                    }
                }

                // ── Cancel / close ─────────────────────────────────────────
                KeyCode::Esc => {
                    if !self.completion_candidates.is_empty() {
                        // Close the wildmenu but keep the buffer (vim behaviour).
                        self.completion_candidates.clear();
                        self.completion_index = 0;
                    } else {
                        self.command_buf = None;
                    }
                    Intent::Redraw
                }

                // ── Character input ────────────────────────────────────────
                KeyCode::Char(c) => {
                    if c == ' ' && !self.completion_candidates.is_empty() {
                        // Space while wildmenu open: accept current candidate.
                        let candidate = self.completion_candidates[self.completion_index].clone();
                        self.accept_completion(&candidate);
                    } else {
                        // Any other char: clear completions and append.
                        self.completion_candidates.clear();
                        self.completion_index = 0;
                        if let Some(ref mut b) = self.command_buf {
                            b.push(c);
                        }
                    }
                    Intent::Redraw
                }
                KeyCode::Backspace => {
                    self.completion_candidates.clear();
                    self.completion_index = 0;
                    if let Some(ref mut b) = self.command_buf {
                        b.pop();
                    }
                    Intent::Redraw
                }
                _ => Intent::Redraw,
            },
            _ => Intent::Redraw,
        }
    }

    /// Compute or cycle Tab-completions.
    /// `reverse = true` → Shift-Tab (cycle backward).
    fn handle_completion_tab(&mut self, reverse: bool) {
        if !self.completion_candidates.is_empty() {
            // Cycle within the existing wildmenu.
            let n = self.completion_candidates.len();
            self.completion_index = if reverse {
                self.completion_index.checked_sub(1).unwrap_or(n - 1)
            } else {
                (self.completion_index + 1) % n
            };
        } else {
            // Compute fresh candidates from the current buffer.
            let buf = self.command_buf.clone().unwrap_or_default();
            let candidates = self.compute_completions(&buf);
            match candidates.len() {
                0 => {} // No matches — do nothing.
                1 => {
                    // Exactly one match: fill directly, no wildmenu.
                    let filled = Self::completed_buf(&buf, &candidates[0]);
                    if let Some(ref mut b) = self.command_buf {
                        *b = filled;
                    }
                }
                _ => {
                    // Multiple matches: open the wildmenu at index 0.
                    self.completion_candidates = candidates;
                    self.completion_index = 0;
                }
            }
        }
    }

    /// Accept `candidate` into the command buffer and close the wildmenu.
    fn accept_completion(&mut self, candidate: &str) {
        let buf = self.command_buf.clone().unwrap_or_default();
        let filled = Self::completed_buf(&buf, candidate);
        if let Some(ref mut b) = self.command_buf {
            *b = filled;
        }
        self.completion_candidates.clear();
        self.completion_index = 0;
    }

    /// Build the completed buffer string from the current raw buffer and a
    /// candidate: replaces only the token being completed (command name or
    /// the last argument after the first space).
    fn completed_buf(buf: &str, candidate: &str) -> String {
        if let Some(space_pos) = buf.find(' ') {
            // Completing an argument — keep the command, replace the argument.
            format!("{} {}", &buf[..space_pos], candidate)
        } else {
            // Completing the command name itself.
            candidate.to_string()
        }
    }

    /// Return Tab-completion candidates for the given raw buffer text.
    fn compute_completions(&self, buf: &str) -> Vec<String> {
        let trimmed = buf.trim_start();
        if let Some(space_pos) = trimmed.find(' ') {
            // Buffer already has a command word; complete the argument.
            let cmd = &trimmed[..space_pos];
            let arg = trimmed[space_pos + 1..].trim_start();
            if cmd == "open" {
                if let Some(vm) = &self.view {
                    let lp = arg.to_lowercase();
                    return std::iter::once(vm.title.clone())
                        .filter(|name| name.to_lowercase().starts_with(&lp))
                        .collect();
                }
            }
            vec![]
        } else {
            // Completing the command name by prefix.
            COMMANDS
                .iter()
                .filter(|cmd| cmd.starts_with(trimmed))
                .map(|s| s.to_string())
                .collect()
        }
    }

    /// Rebuild the flat ordered book list from the current view + filter.
    pub fn rebuild_flat(&mut self) {
        self.flat.clear();
        let vm = match &self.view {
            Some(v) => v.clone(),
            None => return,
        };
        for (gi, group) in vm.groups.iter().enumerate() {
            for (bi, book) in group.books.iter().enumerate() {
                if self.passes_filter(book) {
                    self.flat.push(FlatBook {
                        group_name: group.name.clone(),
                        group_index: gi,
                        book_index_in_group: bi,
                        book: book.clone(),
                    });
                }
            }
        }
        // Clamp selection.
        if !self.flat.is_empty() && self.selected >= self.flat.len() {
            self.selected = self.flat.len() - 1;
        }
    }

    fn passes_filter(&self, book: &ViewBook) -> bool {
        match self.filter {
            StatusFilter::All => true,
            StatusFilter::NeedsYou => book.discovery == "needs_selection",
            StatusFilter::Check => book.review,
            StatusFilter::Cannot => {
                book.discovery == "not_found"
                    || book
                        .versions
                        .iter()
                        .any(|v| v.state == "failed" || v.state == "cancelled")
            }
            StatusFilter::InProgress => book.versions.iter().any(|v| v.state == "downloading"),
            StatusFilter::Done => book
                .acquisition
                .as_ref()
                .map(|a| a.done >= 1 && a.active == 0)
                .unwrap_or(false),
        }
    }

    // -----------------------------------------------------------------------
    // Status counts for the filter row
    // -----------------------------------------------------------------------

    pub fn status_counts(&self) -> StatusCounts {
        let mut c = StatusCounts::default();
        let groups: &[ViewGroup] = match &self.view {
            Some(v) => &v.groups,
            None => return c,
        };
        for g in groups {
            for book in &g.books {
                c.total += 1;
                if book.discovery == "needs_selection" {
                    c.needs_you += 1;
                }
                if book.review {
                    c.check += 1;
                }
                let has_cannot = book.discovery == "not_found"
                    || book.versions.iter().any(|v| v.state == "failed");
                if has_cannot {
                    c.cannot += 1;
                }
                let in_progress = book.versions.iter().any(|v| v.state == "downloading");
                if in_progress {
                    c.in_progress += 1;
                }
                let done = book
                    .acquisition
                    .as_ref()
                    .map(|a| a.done >= 1 && a.active == 0)
                    .unwrap_or(false);
                if done {
                    c.done += 1;
                }
            }
        }
        c
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-filter counts for the status-filter row.
#[derive(Debug, Default, Clone)]
pub struct StatusCounts {
    pub total: usize,
    pub needs_you: usize,
    pub check: usize,
    pub cannot: usize,
    pub in_progress: usize,
    pub done: usize,
}
