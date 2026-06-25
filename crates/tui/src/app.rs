//! [`AppState`] — the pure, side-effect-free app state and its reducers.
//!
//! `on_input` NEVER does I/O; it returns an [`Intent`] that the event loop
//! dispatches. `apply` folds engine events into the state.  Both are trivially
//! unit-testable because they take and return plain data.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use libgen_engine::{ViewBook, ViewGroup, ViewModel};

use crate::intent::Intent;

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
// AppState
// ---------------------------------------------------------------------------

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
                        self.move_selection(1);
                        Intent::Redraw
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.move_selection_up();
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
                    _ => Intent::Redraw,
                }
            }

            // ---------------------------------------------------------------
            // Mouse: clicks map to the same intents as keys
            // ---------------------------------------------------------------
            Event::Mouse(me) => match me.kind {
                MouseEventKind::ScrollDown => {
                    self.move_selection(1);
                    Intent::Redraw
                }
                MouseEventKind::ScrollUp => {
                    self.move_selection_up();
                    Intent::Redraw
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    // Hit-testing is Stage 3 (needs the last-frame Rects).
                    Intent::Redraw
                }
                _ => Intent::Redraw,
            },

            Event::Resize(_, _) => Intent::Redraw,
            _ => Intent::Redraw,
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

    /// Handle key events while in command-line mode (`:`).
    fn handle_command_input(&mut self, ev: Event) -> Intent {
        match ev {
            Event::Key(KeyEvent { code, .. }) => match code {
                KeyCode::Enter => {
                    let line = self.command_buf.take().unwrap_or_default();
                    Intent::Command(line)
                }
                KeyCode::Esc => {
                    self.command_buf = None;
                    Intent::Redraw
                }
                KeyCode::Char(c) => {
                    if let Some(ref mut b) = self.command_buf {
                        b.push(c);
                    }
                    Intent::Redraw
                }
                KeyCode::Backspace => {
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
