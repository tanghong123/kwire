//! [`AppState`] — the pure, side-effect-free app state and its reducers.
//!
//! `on_input` NEVER does I/O; it returns an [`Intent`] that the event loop
//! dispatches. `apply` folds engine events into the state.  Both are trivially
//! unit-testable because they take and return plain data.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use libgen_engine::{AppSettings, ViewBook, ViewGroup, ViewModel};
use ratatui::layout::Rect;

use crate::intent::Intent;

// ---------------------------------------------------------------------------
// Settings modal constants + types
// ---------------------------------------------------------------------------

/// Number of navigable (user-editable) fields in the Settings modal.
pub const SETTINGS_FIELD_COUNT: usize = 11;

/// Language options offered in the Language picker; first entry is "match title language"
/// (the default — means "no preference", stored as empty string in the engine).
pub const SETTINGS_LANGUAGES: &[&str] = &[
    "match title language",
    "English",
    "German",
    "French",
    "Spanish",
    "Chinese",
    "Russian",
    "Japanese",
    "Italian",
    "Portuguese",
];

/// All format names shown in the Format Editor, in display order.
pub const FORMAT_EDITOR_FORMATS: &[&str] =
    &["epub", "pdf", "mobi", "azw3", "djvu", "cbz", "fb2", "txt"];

/// Which kind of editor a Settings field uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsFieldKind {
    /// Opens the Format Editor sub-modal.
    FormatPref,
    /// Opens the Language picker popup.
    Language,
    /// Inline edit: typed decimal; ←/→ nudge by 0.05.
    F32,
    /// Inline edit: typed integer; ←/→ nudge by 1.
    Usize,
    /// Inline edit: typed integer; ←/→ nudge by 1.
    U32,
    /// Inline text edit.
    Text,
    /// Space (or ⏎) toggles between on/off.
    Bool,
    /// Shown but not editable.
    ReadOnly,
}

/// Return the editor kind for field index `idx`.
pub fn settings_field_kind(idx: usize) -> SettingsFieldKind {
    match idx {
        0 => SettingsFieldKind::FormatPref,
        1 => SettingsFieldKind::Language,
        2 | 3 => SettingsFieldKind::F32,
        4 | 8 => SettingsFieldKind::Usize,
        5 | 6 => SettingsFieldKind::Text,
        7 | 10 => SettingsFieldKind::Bool,
        9 => SettingsFieldKind::U32,
        _ => SettingsFieldKind::ReadOnly,
    }
}

/// Build the full row list for the Format Editor from `format_pref` (ordered
/// included formats).  Included formats come first (in priority order),
/// excluded formats follow in the canonical display order.
pub fn build_format_editor_rows(format_pref: &[String]) -> Vec<(bool, String)> {
    let mut rows: Vec<(bool, String)> = format_pref
        .iter()
        .filter(|f| FORMAT_EDITOR_FORMATS.contains(&f.as_str()))
        .map(|f| (true, f.clone()))
        .collect();
    for &f in FORMAT_EDITOR_FORMATS {
        if !format_pref.iter().any(|p| p == f) {
            rows.push((false, f.to_string()));
        }
    }
    rows
}

/// Inline editor mode within the Settings modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsEditor {
    /// Normal navigation — no field actively being edited.
    Viewing,
    /// Inline text/number edit; the `String` is the current buffer.
    Editing(String),
    /// Format editor sub-modal.
    FormatEditor {
        /// `(included, ext_name)` rows — included rows first, in priority order.
        rows: Vec<(bool, String)>,
        /// Cursor position within `rows`.
        cursor: usize,
    },
    /// Language picker popup.
    LangPicker {
        options: Vec<String>,
        /// Currently highlighted option index.
        selected: usize,
    },
}

impl Default for SettingsEditor {
    fn default() -> Self {
        SettingsEditor::Viewing
    }
}

/// Staged settings edits while the Settings modal is open.
///
/// Initialised from the current `ViewModel` + `AppSettings` when the modal
/// opens.  On save (`Esc`) emitted as `Intent::SaveSettings`; on discard
/// (`q` / `Ctrl-G`) the modal closes without touching the engine.
#[derive(Debug, Clone)]
pub struct SettingsDraft {
    // ── Per-list settings ────────────────────────────────────────────────────
    /// Ordered preferred format names, e.g. `["epub", "pdf"]`.
    pub format_pref: Vec<String>,
    /// Display language.  `""` means "no preference" / "match title language" (engine → `None`).
    pub language: String,
    pub auto_threshold: f32,
    pub near_threshold: f32,
    pub keep_top: usize,
    pub naming_template: String,
    /// `true` = sequence numbers are scoped per-group; `false` = per-list.
    pub seq_per_group: bool,
    // ── App-wide settings ────────────────────────────────────────────────────
    /// Override download directory; `""` = engine default.
    pub out_dir: String,
    /// Global concurrent-download cap (`G`).
    pub max_concurrent: usize,
    /// Max retry attempts per download mirror.
    pub max_attempts: u32,
    /// Whether speculative (hedged) downloading is enabled.
    pub hedge_enabled: bool,
    // ── Editor state ─────────────────────────────────────────────────────────
    pub editor: SettingsEditor,
}

impl SettingsDraft {
    /// The display value for field `idx` used by the render pass.
    pub fn field_value(&self, idx: usize) -> String {
        match idx {
            0 => {
                if self.format_pref.is_empty() {
                    "—".into()
                } else {
                    self.format_pref.join(", ")
                }
            }
            1 => {
                if self.language.is_empty() {
                    "match title language".into()
                } else {
                    self.language.clone()
                }
            }
            2 => format!("{:.2}", self.auto_threshold),
            3 => format!("{:.2}", self.near_threshold),
            4 => self.keep_top.to_string(),
            5 => {
                if self.out_dir.is_empty() {
                    "~/Books/Kwire (default)".into()
                } else {
                    self.out_dir.clone()
                }
            }
            6 => self.naming_template.clone(),
            7 => {
                if self.seq_per_group {
                    "on".into()
                } else {
                    "off".into()
                }
            }
            8 => self.max_concurrent.to_string(),
            9 => self.max_attempts.to_string(),
            10 => {
                if self.hedge_enabled {
                    "on".into()
                } else {
                    "off".into()
                }
            }
            11 => "libgen.li  libgen.is  libgen.rs".into(),
            12 => "libgen.li  libgen.pw  ipfs".into(),
            _ => String::new(),
        }
    }
}

/// Commands supported in `:` command-line mode (used for Tab-completion).
const COMMANDS: &[&str] = &[
    "import",
    "add",
    "add-md5",
    "open",
    "requery",
    "settings",
    "pause-all",
    "pause",
    "start-all",
    "resume-all",
    "start",
    "resume",
    "delete",
    "refresh-mirrors",
    "cleanup",
    "reorganize",
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

/// Which sub-pane is focused inside `Modal::Detail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailSubFocus {
    Variations,
    History,
}

/// Which field is being edited in `Modal::EditBook`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditBookField {
    Title,
    Author,
}

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
    Detail {
        book_flat_index: usize,
        /// Which variation row is currently highlighted (↑/↓ when Variations focused).
        selected: usize,
        /// Which sub-pane is focused (Tab toggles).
        sub_focus: DetailSubFocus,
        /// Which history row is highlighted (↑/↓ when History focused).
        history_selected: usize,
    },
    /// Settings key-value editor.
    Settings,
    /// Full help screen.
    Help,
    /// Delete-list confirmation: "Delete '<title>' and its N books? [y/n]"
    Confirm {
        /// Human-readable list title.
        title: String,
        /// Number of books in the list.
        n_books: usize,
        /// The engine id (`"listN"`) of the list to delete.
        target_id: String,
    },
    /// Inline re-query: user types a corrected title for a single book.
    ReQuery { book_flat_index: usize, buf: String },
    /// Inline edit: user edits title and/or author for a single book.
    EditBook {
        book_flat_index: usize,
        title_buf: String,
        author_buf: String,
        field: EditBookField,
    },
    /// Confirm removing a book from the list: [y/n].
    ConfirmBookRemove { book_flat_index: usize },
    /// Reorganize-files preview: the (current path → correct path) pairs that
    /// would move under the saved naming-template / download-folder / sub-grouping
    /// scheme. Scrollable; `[y] apply  [n / esc] cancel`.
    Reorganize {
        /// (old path → new path) pairs across all loaded lists.
        diff: Vec<(String, String)>,
        /// Highlighted/scroll row within the diff list.
        selected: usize,
    },
}

// ---------------------------------------------------------------------------
// ListSummary — title + counts for one loaded reading list
// ---------------------------------------------------------------------------

/// Summary of a loaded reading list shown in the list strip.
#[derive(Debug, Clone, Default)]
pub struct ListSummary {
    /// Engine ID (used for switching the active list).
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Number of books that are fully done.
    pub done: usize,
    /// Total number of books in this list.
    pub total: usize,
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

    /// Which settings field is focused in the Settings modal (0-based field index).
    pub settings_selected: usize,

    /// Staged edits for the Settings modal; `Some` while the modal is open.
    pub settings_draft: Option<SettingsDraft>,

    /// Live scheduler telemetry keyed by md5. Updated by Progress events from the engine.
    pub transfers: std::collections::HashMap<String, ActiveTransfer>,

    /// Tab-completion candidates for the `:` command line.
    /// Non-empty while the wildmenu is visible.
    pub completion_candidates: Vec<String>,

    /// Index of the highlighted candidate within `completion_candidates`.
    pub completion_index: usize,

    /// Transient one-shot status message (shown in the hint bar until the
    /// next keypress, then cleared automatically in `on_input`).
    pub status_msg: Option<String>,

    /// Submitted `:` commands, oldest first (for ↑/↓ history recall).
    pub cmd_history: Vec<String>,

    /// History-browsing cursor: `None` = live buffer, `Some(0)` = most recent
    /// entry, `Some(n-1)` = oldest.  Reset on submit / Esc / any typing.
    cmd_history_cursor: Option<usize>,

    /// Draft buffer saved when the user first presses ↑ (restored on ↓ past newest).
    cmd_history_draft: String,

    /// Summaries for ALL loaded reading lists (id, title, done, total).
    /// Shown in the list strip.  Updated at startup and after each import/switch.
    pub all_lists: Vec<ListSummary>,

    /// Index into `all_lists` for the currently displayed list.
    pub active_list_idx: usize,
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
            settings_draft: None,
            transfers: std::collections::HashMap::new(),
            completion_candidates: Vec::new(),
            completion_index: 0,
            status_msg: None,
            cmd_history: Vec::new(),
            cmd_history_cursor: None,
            cmd_history_draft: String::new(),
            all_lists: Vec::new(),
            active_list_idx: 0,
        }
    }

    /// Open the Settings modal and initialise a draft from the current view and
    /// global app settings.  Call this instead of setting `modal` directly so
    /// the draft is always in sync.
    pub fn open_settings(&mut self, app_settings: &AppSettings) {
        let draft = if let Some(v) = &self.view {
            let s = &v.settings;
            SettingsDraft {
                format_pref: s.format_pref.clone(),
                language: if s.language.is_empty() {
                    String::new()
                } else {
                    s.language.clone()
                },
                auto_threshold: s.auto_threshold,
                near_threshold: s.near_threshold,
                keep_top: s.keep_top,
                naming_template: s.naming_template.clone(),
                seq_per_group: s.seq_per_group,
                out_dir: app_settings.out_dir.clone(),
                max_concurrent: app_settings.max_concurrent_downloads,
                max_attempts: app_settings.max_attempts,
                hedge_enabled: app_settings.hedge_enabled,
                editor: SettingsEditor::Viewing,
            }
        } else {
            SettingsDraft {
                format_pref: vec!["epub".into(), "pdf".into()],
                language: String::new(),
                auto_threshold: 0.85,
                near_threshold: 0.45,
                keep_top: 5,
                naming_template: "{seq:02} - {authors} - {title}.{ext}".into(),
                seq_per_group: true,
                out_dir: app_settings.out_dir.clone(),
                max_concurrent: app_settings.max_concurrent_downloads,
                max_attempts: app_settings.max_attempts,
                hedge_enabled: app_settings.hedge_enabled,
                editor: SettingsEditor::Viewing,
            }
        };
        self.settings_draft = Some(draft);
        self.settings_selected = 0;
        self.modal = Some(Modal::Settings);
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
        // Clear any transient status message on every input event.
        self.status_msg = None;

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
                    KeyCode::Down => {
                        if modifiers.contains(KeyModifiers::SHIFT) {
                            if self.focus == Focus::List {
                                self.move_selection_page_down();
                            }
                        } else {
                            match self.focus {
                                Focus::List => self.move_selection(1),
                                Focus::Activity => self.scroll_activity(1),
                            }
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('j') => {
                        match self.focus {
                            Focus::List => self.move_selection(1),
                            Focus::Activity => self.scroll_activity(1),
                        }
                        Intent::Redraw
                    }
                    // Shift-J = page down in the book list.
                    KeyCode::Char('J') => {
                        if self.focus == Focus::List {
                            self.move_selection_page_down();
                        }
                        Intent::Redraw
                    }
                    KeyCode::Up => {
                        if modifiers.contains(KeyModifiers::SHIFT) {
                            if self.focus == Focus::List {
                                self.move_selection_page_up();
                            }
                        } else {
                            match self.focus {
                                Focus::List => self.move_selection_up(),
                                Focus::Activity => self.scroll_activity_up(),
                            }
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('k') => {
                        match self.focus {
                            Focus::List => self.move_selection_up(),
                            Focus::Activity => self.scroll_activity_up(),
                        }
                        Intent::Redraw
                    }
                    // Shift-K = page up in the book list.
                    KeyCode::Char('K') => {
                        if self.focus == Focus::List {
                            self.move_selection_page_up();
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
                        self.cmd_history_cursor = None;
                        self.cmd_history_draft = String::new();
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
                    KeyCode::Char('r') => match self.focus {
                        Focus::Activity => {
                            if let Some(md5) = self.focused_transfer_md5() {
                                Intent::ResumeTransfer { md5 }
                            } else {
                                Intent::Redraw
                            }
                        }
                        Focus::List => {
                            if let Some(fb) = self.flat.get(self.selected) {
                                Intent::Retry {
                                    group_path: vec![fb.group_index],
                                    book_index: fb.book_index_in_group,
                                }
                            } else {
                                Intent::Redraw
                            }
                        }
                    },
                    KeyCode::Char('p') => match self.focus {
                        Focus::Activity => {
                            if let Some(md5) = self.focused_transfer_md5() {
                                Intent::PauseTransfer { md5 }
                            } else {
                                Intent::Redraw
                            }
                        }
                        Focus::List => {
                            if let Some(fb) = self.flat.get(self.selected) {
                                Intent::Pause {
                                    group_path: vec![fb.group_index],
                                    book_index: fb.book_index_in_group,
                                }
                            } else {
                                Intent::Redraw
                            }
                        }
                    },
                    KeyCode::Char('c') => match self.focus {
                        Focus::Activity => {
                            if let Some(md5) = self.focused_transfer_md5() {
                                Intent::CancelTransfer { md5 }
                            } else {
                                Intent::Redraw
                            }
                        }
                        Focus::List => {
                            if let Some(fb) = self.flat.get(self.selected) {
                                Intent::Cancel {
                                    group_path: vec![fb.group_index],
                                    book_index: fb.book_index_in_group,
                                }
                            } else {
                                Intent::Redraw
                            }
                        }
                    },
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
                    KeyCode::Left => {
                        if self.all_lists.len() > 1 {
                            let new_idx = self
                                .active_list_idx
                                .checked_sub(1)
                                .unwrap_or(self.all_lists.len() - 1);
                            self.active_list_idx = new_idx;
                            let id = self.all_lists[new_idx].id.clone();
                            return Intent::SwitchList { id };
                        }
                        Intent::Redraw
                    }
                    KeyCode::Right => {
                        if self.all_lists.len() > 1 {
                            let new_idx = (self.active_list_idx + 1) % self.all_lists.len();
                            self.active_list_idx = new_idx;
                            let id = self.all_lists[new_idx].id.clone();
                            return Intent::SwitchList { id };
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('a') => {
                        // Request all preferred format variations — UI-only stub for Stage 3.
                        Intent::Redraw
                    }
                    // #50 book-level actions from list view.
                    KeyCode::Char('e') => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            let flat_idx = self.selected;
                            let title_buf = fb.book.title.clone();
                            let author_buf = fb.book.author.clone();
                            self.modal = Some(Modal::EditBook {
                                book_flat_index: flat_idx,
                                title_buf,
                                author_buf,
                                field: EditBookField::Title,
                            });
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('x') | KeyCode::Delete => {
                        let flat_idx = self.selected;
                        if self.flat.get(flat_idx).is_some() {
                            self.modal = Some(Modal::ConfirmBookRemove {
                                book_flat_index: flat_idx,
                            });
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('m') => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            return Intent::MarkNotFound {
                                group_path: vec![fb.group_index],
                                book_index: fb.book_index_in_group,
                            };
                        }
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

            Modal::Detail {
                book_flat_index,
                selected,
                sub_focus,
                history_selected,
            } => {
                let flat_index = *book_flat_index;
                let sel = *selected;
                let sf = sub_focus.clone();
                let hist_sel = *history_selected;
                match ev {
                    Event::Key(KeyEvent { code, .. }) => match code {
                        KeyCode::Esc => {
                            self.modal = None;
                            Intent::Redraw
                        }
                        KeyCode::Tab => {
                            let new_sf = match sf {
                                DetailSubFocus::Variations => DetailSubFocus::History,
                                DetailSubFocus::History => DetailSubFocus::Variations,
                            };
                            self.modal = Some(Modal::Detail {
                                book_flat_index: flat_index,
                                selected: sel,
                                sub_focus: new_sf,
                                history_selected: hist_sel,
                            });
                            Intent::Redraw
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            match sf {
                                DetailSubFocus::Variations => {
                                    let max = self
                                        .flat
                                        .get(flat_index)
                                        .map(|fb| fb.book.versions.len().saturating_sub(1))
                                        .unwrap_or(0);
                                    let new_sel = (sel + 1).min(max);
                                    self.modal = Some(Modal::Detail {
                                        book_flat_index: flat_index,
                                        selected: new_sel,
                                        sub_focus: sf,
                                        history_selected: hist_sel,
                                    });
                                }
                                DetailSubFocus::History => {
                                    let max = self
                                        .flat
                                        .get(flat_index)
                                        .map(|fb| fb.book.history.len().saturating_sub(1))
                                        .unwrap_or(0);
                                    let new_hist = (hist_sel + 1).min(max);
                                    self.modal = Some(Modal::Detail {
                                        book_flat_index: flat_index,
                                        selected: sel,
                                        sub_focus: sf,
                                        history_selected: new_hist,
                                    });
                                }
                            }
                            Intent::Redraw
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            match sf {
                                DetailSubFocus::Variations => {
                                    let new_sel = sel.saturating_sub(1);
                                    self.modal = Some(Modal::Detail {
                                        book_flat_index: flat_index,
                                        selected: new_sel,
                                        sub_focus: sf,
                                        history_selected: hist_sel,
                                    });
                                }
                                DetailSubFocus::History => {
                                    let new_hist = hist_sel.saturating_sub(1);
                                    self.modal = Some(Modal::Detail {
                                        book_flat_index: flat_index,
                                        selected: sel,
                                        sub_focus: sf,
                                        history_selected: new_hist,
                                    });
                                }
                            }
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
                        // #49 manual re-query: open inline search input.
                        KeyCode::Char('s') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                let title = fb.book.title.clone();
                                self.modal = Some(Modal::ReQuery {
                                    book_flat_index: flat_index,
                                    buf: title,
                                });
                            }
                            Intent::Redraw
                        }
                        // #50 edit title/author.
                        KeyCode::Char('e') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                let title_buf = fb.book.title.clone();
                                let author_buf = fb.book.author.clone();
                                self.modal = Some(Modal::EditBook {
                                    book_flat_index: flat_index,
                                    title_buf,
                                    author_buf,
                                    field: EditBookField::Title,
                                });
                            }
                            Intent::Redraw
                        }
                        // #50 remove book with confirmation.
                        KeyCode::Char('x') | KeyCode::Delete => {
                            self.modal = Some(Modal::ConfirmBookRemove {
                                book_flat_index: flat_index,
                            });
                            Intent::Redraw
                        }
                        // #50 mark not-found.
                        KeyCode::Char('m') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                let intent = Intent::MarkNotFound {
                                    group_path: vec![fb.group_index],
                                    book_index: fb.book_index_in_group,
                                };
                                self.modal = None;
                                return intent;
                            }
                            Intent::Redraw
                        }
                        _ => Intent::Redraw,
                    },
                    _ => Intent::Redraw,
                }
            }

            Modal::ReQuery {
                book_flat_index,
                buf,
            } => {
                let flat_index = *book_flat_index;
                let buf = buf.clone();
                match ev {
                    Event::Key(KeyEvent { code, .. }) => match code {
                        KeyCode::Esc => {
                            // Return to the detail modal.
                            self.modal = Some(Modal::Detail {
                                book_flat_index: flat_index,
                                selected: 0,
                                sub_focus: DetailSubFocus::Variations,
                                history_selected: 0,
                            });
                            Intent::Redraw
                        }
                        KeyCode::Enter => {
                            let trimmed = buf.trim().to_string();
                            if !trimmed.is_empty() {
                                if let Some(fb) = self.flat.get(flat_index) {
                                    let intent = Intent::ReQueryBook {
                                        group_path: vec![fb.group_index],
                                        book_index: fb.book_index_in_group,
                                        title: trimmed,
                                    };
                                    self.modal = None;
                                    return intent;
                                }
                            }
                            self.modal = None;
                            Intent::Redraw
                        }
                        KeyCode::Backspace => {
                            let mut new_buf = buf;
                            new_buf.pop();
                            self.modal = Some(Modal::ReQuery {
                                book_flat_index: flat_index,
                                buf: new_buf,
                            });
                            Intent::Redraw
                        }
                        KeyCode::Char(c) => {
                            let mut new_buf = buf;
                            new_buf.push(c);
                            self.modal = Some(Modal::ReQuery {
                                book_flat_index: flat_index,
                                buf: new_buf,
                            });
                            Intent::Redraw
                        }
                        _ => Intent::Redraw,
                    },
                    _ => Intent::Redraw,
                }
            }

            Modal::EditBook {
                book_flat_index,
                title_buf,
                author_buf,
                field,
            } => {
                let flat_index = *book_flat_index;
                let mut tbuf = title_buf.clone();
                let mut abuf = author_buf.clone();
                let fld = field.clone();
                match ev {
                    Event::Key(KeyEvent { code, .. }) => match code {
                        KeyCode::Esc => {
                            self.modal = Some(Modal::Detail {
                                book_flat_index: flat_index,
                                selected: 0,
                                sub_focus: DetailSubFocus::Variations,
                                history_selected: 0,
                            });
                            Intent::Redraw
                        }
                        KeyCode::Tab => {
                            let new_field = match fld {
                                EditBookField::Title => EditBookField::Author,
                                EditBookField::Author => EditBookField::Title,
                            };
                            self.modal = Some(Modal::EditBook {
                                book_flat_index: flat_index,
                                title_buf: tbuf,
                                author_buf: abuf,
                                field: new_field,
                            });
                            Intent::Redraw
                        }
                        KeyCode::Enter => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                let authors: Vec<String> = abuf
                                    .split(',')
                                    .map(|s| s.trim().to_string())
                                    .filter(|s| !s.is_empty())
                                    .collect();
                                let intent = Intent::EditBook {
                                    group_path: vec![fb.group_index],
                                    book_index: fb.book_index_in_group,
                                    title: tbuf,
                                    authors,
                                };
                                self.modal = None;
                                return intent;
                            }
                            self.modal = None;
                            Intent::Redraw
                        }
                        KeyCode::Backspace => {
                            match fld {
                                EditBookField::Title => {
                                    tbuf.pop();
                                }
                                EditBookField::Author => {
                                    abuf.pop();
                                }
                            }
                            self.modal = Some(Modal::EditBook {
                                book_flat_index: flat_index,
                                title_buf: tbuf,
                                author_buf: abuf,
                                field: fld,
                            });
                            Intent::Redraw
                        }
                        KeyCode::Char(c) => {
                            match fld {
                                EditBookField::Title => tbuf.push(c),
                                EditBookField::Author => abuf.push(c),
                            }
                            self.modal = Some(Modal::EditBook {
                                book_flat_index: flat_index,
                                title_buf: tbuf,
                                author_buf: abuf,
                                field: fld,
                            });
                            Intent::Redraw
                        }
                        _ => Intent::Redraw,
                    },
                    _ => Intent::Redraw,
                }
            }

            Modal::ConfirmBookRemove { book_flat_index } => {
                let flat_index = *book_flat_index;
                match ev {
                    Event::Key(KeyEvent { code, .. }) => match code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                let intent = Intent::RemoveBook {
                                    group_path: vec![fb.group_index],
                                    book_index: fb.book_index_in_group,
                                };
                                self.modal = None;
                                return intent;
                            }
                            self.modal = None;
                            Intent::Redraw
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            // Go back to detail.
                            self.modal = Some(Modal::Detail {
                                book_flat_index: flat_index,
                                selected: 0,
                                sub_focus: DetailSubFocus::Variations,
                                history_selected: 0,
                            });
                            Intent::Redraw
                        }
                        _ => Intent::Redraw,
                    },
                    _ => Intent::Redraw,
                }
            }

            Modal::Settings => {
                // Determine the current sub-mode without holding a borrow on `self`.
                let sub_mode: u8 = match &self.settings_draft {
                    Some(d) => match &d.editor {
                        SettingsEditor::Viewing => 0,
                        SettingsEditor::Editing(_) => 1,
                        SettingsEditor::FormatEditor { .. } => 2,
                        SettingsEditor::LangPicker { .. } => 3,
                    },
                    None => {
                        // Shouldn't happen — guard against it.
                        self.modal = None;
                        return Intent::Redraw;
                    }
                };
                match sub_mode {
                    2 => self.handle_format_editor_input(ev),
                    3 => self.handle_lang_picker_input(ev),
                    1 => self.handle_inline_edit_input(ev),
                    _ => self.handle_settings_viewing_input(ev),
                }
            }

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

            Modal::Confirm { target_id, .. } => {
                let id = target_id.clone();
                match ev {
                    Event::Key(KeyEvent { code, .. }) => match code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            self.modal = None;
                            Intent::ConfirmDelete { id }
                        }
                        KeyCode::Char('n')
                        | KeyCode::Char('N')
                        | KeyCode::Esc
                        | KeyCode::Char('q') => {
                            self.modal = None;
                            Intent::Redraw
                        }
                        _ => Intent::Redraw,
                    },
                    _ => Intent::Redraw,
                }
            }

            Modal::Reorganize { diff, selected } => {
                let len = diff.len();
                let cur = *selected;
                match ev {
                    Event::Key(KeyEvent { code, .. }) => match code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            self.modal = None;
                            Intent::ApplyReorganize
                        }
                        KeyCode::Char('n')
                        | KeyCode::Char('N')
                        | KeyCode::Esc
                        | KeyCode::Char('q') => {
                            self.modal = None;
                            Intent::Redraw
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let next = (cur + 1).min(len.saturating_sub(1));
                            self.modal = Some(Modal::Reorganize {
                                diff: diff.clone(),
                                selected: next,
                            });
                            Intent::Redraw
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.modal = Some(Modal::Reorganize {
                                diff: diff.clone(),
                                selected: cur.saturating_sub(1),
                            });
                            Intent::Redraw
                        }
                        _ => Intent::Redraw,
                    },
                    _ => Intent::Redraw,
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Settings modal — sub-mode handlers
    // -----------------------------------------------------------------------

    /// Handle input while the Settings modal is in Viewing mode.
    fn handle_settings_viewing_input(&mut self, ev: Event) -> Intent {
        match ev {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match code {
                // Esc or 's' → save & close (modal closed here; draft consumed by dispatcher)
                KeyCode::Esc | KeyCode::Char('s') => {
                    self.modal = None;
                    // Keep settings_draft alive for the dispatcher to read.
                    Intent::SaveSettings
                }
                // 'q' or Ctrl-G → discard & close
                KeyCode::Char('q') => {
                    self.settings_draft = None;
                    self.modal = None;
                    Intent::DiscardSettings
                }
                KeyCode::Char('g') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.settings_draft = None;
                    self.modal = None;
                    Intent::DiscardSettings
                }
                // Navigation
                KeyCode::Down | KeyCode::Char('j') => {
                    self.settings_selected =
                        (self.settings_selected + 1).min(SETTINGS_FIELD_COUNT - 1);
                    Intent::Redraw
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.settings_selected = self.settings_selected.saturating_sub(1);
                    Intent::Redraw
                }
                // ←/→ nudge number fields
                KeyCode::Left => {
                    self.nudge_settings_field(-1);
                    Intent::Redraw
                }
                KeyCode::Right => {
                    self.nudge_settings_field(1);
                    Intent::Redraw
                }
                // Space: toggle Bool fields (also works on non-Bool — no-op)
                KeyCode::Char(' ') => {
                    self.toggle_settings_field();
                    Intent::Redraw
                }
                // Enter: open sub-modal / start inline edit / toggle Bool
                KeyCode::Enter => {
                    self.enter_settings_field();
                    Intent::Redraw
                }
                _ => Intent::Redraw,
            },
            _ => Intent::Redraw,
        }
    }

    /// Handle input while the Format Editor sub-modal is open.
    fn handle_format_editor_input(&mut self, ev: Event) -> Intent {
        let Event::Key(KeyEvent { code, .. }) = ev else {
            return Intent::Redraw;
        };
        match code {
            KeyCode::Esc | KeyCode::Enter => {
                // Commit: extract included formats in priority order.
                let new_pref: Vec<String> = if let Some(SettingsDraft {
                    editor: SettingsEditor::FormatEditor { ref rows, .. },
                    ..
                }) = self.settings_draft
                {
                    rows.iter()
                        .filter(|(inc, _)| *inc)
                        .map(|(_, n)| n.clone())
                        .collect()
                } else {
                    vec![]
                };
                if let Some(d) = &mut self.settings_draft {
                    d.format_pref = new_pref;
                    d.editor = SettingsEditor::Viewing;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(SettingsDraft {
                    editor:
                        SettingsEditor::FormatEditor {
                            ref rows,
                            ref mut cursor,
                        },
                    ..
                }) = self.settings_draft
                {
                    let len = rows.len();
                    if len > 0 {
                        *cursor = (*cursor + 1).min(len - 1);
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(SettingsDraft {
                    editor: SettingsEditor::FormatEditor { ref mut cursor, .. },
                    ..
                }) = self.settings_draft
                {
                    *cursor = cursor.saturating_sub(1);
                }
            }
            KeyCode::Char(' ') => {
                // Toggle inclusion for the cursor row.
                if let Some(SettingsDraft {
                    editor:
                        SettingsEditor::FormatEditor {
                            ref mut rows,
                            ref mut cursor,
                        },
                    ..
                }) = self.settings_draft
                {
                    let cur = *cursor;
                    if cur < rows.len() {
                        let (was_included, name) = rows.remove(cur);
                        let now_included = !was_included;
                        if now_included {
                            // Insert at the end of the already-included block.
                            let ins = rows.iter().position(|(inc, _)| !inc).unwrap_or(rows.len());
                            rows.insert(ins, (true, name));
                            *cursor = ins;
                        } else {
                            // Insert at the start of the excluded block.
                            let ins = rows.iter().position(|(inc, _)| !inc).unwrap_or(rows.len());
                            rows.insert(ins, (false, name));
                            *cursor = ins;
                        }
                    }
                }
            }
            // J: move focused included format down in priority.
            KeyCode::Char('J') => {
                if let Some(SettingsDraft {
                    editor:
                        SettingsEditor::FormatEditor {
                            ref mut rows,
                            ref mut cursor,
                        },
                    ..
                }) = self.settings_draft
                {
                    let cur = *cursor;
                    if cur + 1 < rows.len() && rows[cur].0 && rows[cur + 1].0 {
                        rows.swap(cur, cur + 1);
                        *cursor = cur + 1;
                    }
                }
            }
            // K: move focused included format up in priority.
            KeyCode::Char('K') => {
                if let Some(SettingsDraft {
                    editor:
                        SettingsEditor::FormatEditor {
                            ref mut rows,
                            ref mut cursor,
                        },
                    ..
                }) = self.settings_draft
                {
                    let cur = *cursor;
                    if cur > 0 && rows[cur].0 && rows[cur - 1].0 {
                        rows.swap(cur, cur - 1);
                        *cursor = cur - 1;
                    }
                }
            }
            _ => {}
        }
        Intent::Redraw
    }

    /// Handle input while the Language picker popup is open.
    fn handle_lang_picker_input(&mut self, ev: Event) -> Intent {
        let Event::Key(KeyEvent { code, .. }) = ev else {
            return Intent::Redraw;
        };
        match code {
            KeyCode::Esc => {
                // Cancel — keep existing language.
                if let Some(d) = &mut self.settings_draft {
                    d.editor = SettingsEditor::Viewing;
                }
            }
            KeyCode::Enter => {
                // Commit selected language.
                let lang = if let Some(SettingsDraft {
                    editor:
                        SettingsEditor::LangPicker {
                            ref options,
                            selected,
                        },
                    ..
                }) = self.settings_draft
                {
                    options[selected].clone()
                } else {
                    String::new()
                };
                if let Some(d) = &mut self.settings_draft {
                    d.language = if lang == "match title language" {
                        String::new()
                    } else {
                        lang
                    };
                    d.editor = SettingsEditor::Viewing;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(SettingsDraft {
                    editor:
                        SettingsEditor::LangPicker {
                            ref options,
                            ref mut selected,
                        },
                    ..
                }) = self.settings_draft
                {
                    let len = options.len();
                    if len > 0 {
                        *selected = (*selected + 1).min(len - 1);
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(SettingsDraft {
                    editor:
                        SettingsEditor::LangPicker {
                            ref mut selected, ..
                        },
                    ..
                }) = self.settings_draft
                {
                    *selected = selected.saturating_sub(1);
                }
            }
            _ => {}
        }
        Intent::Redraw
    }

    /// Handle input while an inline text/number edit buffer is active.
    fn handle_inline_edit_input(&mut self, ev: Event) -> Intent {
        let idx = self.settings_selected;
        let Event::Key(KeyEvent { code, .. }) = ev else {
            return Intent::Redraw;
        };
        match code {
            KeyCode::Esc => {
                if let Some(d) = &mut self.settings_draft {
                    d.editor = SettingsEditor::Viewing;
                }
            }
            KeyCode::Enter => {
                // Snapshot the buffer, then switch to Viewing, then commit.
                let committed: String = if let Some(SettingsDraft {
                    editor: SettingsEditor::Editing(ref buf),
                    ..
                }) = self.settings_draft
                {
                    buf.clone()
                } else {
                    String::new()
                };
                if let Some(d) = &mut self.settings_draft {
                    d.editor = SettingsEditor::Viewing;
                }
                self.commit_inline_edit(idx, &committed);
            }
            KeyCode::Backspace => {
                if let Some(SettingsDraft {
                    editor: SettingsEditor::Editing(ref mut buf),
                    ..
                }) = self.settings_draft
                {
                    buf.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(SettingsDraft {
                    editor: SettingsEditor::Editing(ref mut buf),
                    ..
                }) = self.settings_draft
                {
                    // Accept digits, '.', and printable path/template characters.
                    if c.is_ascii_graphic() || c == ' ' {
                        buf.push(c);
                    }
                }
            }
            _ => {}
        }
        Intent::Redraw
    }

    // -----------------------------------------------------------------------
    // Settings modal — field-mutation helpers
    // -----------------------------------------------------------------------

    /// Nudge a number field left (`dir = -1`) or right (`dir = +1`).
    fn nudge_settings_field(&mut self, dir: i32) {
        let idx = self.settings_selected;
        let Some(d) = &mut self.settings_draft else {
            return;
        };
        match (settings_field_kind(idx), idx) {
            (SettingsFieldKind::F32, 2) => {
                d.auto_threshold = (d.auto_threshold + 0.05 * dir as f32).clamp(0.0, 1.0);
                // Round to 2 dp to avoid float drift.
                d.auto_threshold = (d.auto_threshold * 100.0).round() / 100.0;
            }
            (SettingsFieldKind::F32, 3) => {
                d.near_threshold = (d.near_threshold + 0.05 * dir as f32).clamp(0.0, 1.0);
                d.near_threshold = (d.near_threshold * 100.0).round() / 100.0;
            }
            (SettingsFieldKind::Usize, 4) => {
                if dir > 0 {
                    d.keep_top = d.keep_top.saturating_add(1);
                } else {
                    d.keep_top = d.keep_top.saturating_sub(1).max(1);
                }
            }
            (SettingsFieldKind::Usize, 8) => {
                if dir > 0 {
                    d.max_concurrent = d.max_concurrent.saturating_add(1);
                } else {
                    d.max_concurrent = d.max_concurrent.saturating_sub(1).max(1);
                }
            }
            (SettingsFieldKind::U32, 9) => {
                if dir > 0 {
                    d.max_attempts = d.max_attempts.saturating_add(1);
                } else {
                    d.max_attempts = d.max_attempts.saturating_sub(1).max(1);
                }
            }
            _ => {}
        }
    }

    /// Toggle a Bool field.
    fn toggle_settings_field(&mut self) {
        let idx = self.settings_selected;
        let Some(d) = &mut self.settings_draft else {
            return;
        };
        match idx {
            7 => d.seq_per_group = !d.seq_per_group,
            10 => d.hedge_enabled = !d.hedge_enabled,
            _ => {}
        }
    }

    /// On Enter: open the sub-modal for FormatPref / Language; start inline
    /// edit for number/text fields; toggle Bool fields.
    fn enter_settings_field(&mut self) {
        let idx = self.settings_selected;
        let Some(d) = &mut self.settings_draft else {
            return;
        };
        match settings_field_kind(idx) {
            SettingsFieldKind::FormatPref => {
                let rows = build_format_editor_rows(&d.format_pref);
                d.editor = SettingsEditor::FormatEditor { rows, cursor: 0 };
            }
            SettingsFieldKind::Language => {
                let options: Vec<String> =
                    SETTINGS_LANGUAGES.iter().map(|s| s.to_string()).collect();
                let lang_now = if d.language.is_empty() {
                    "match title language"
                } else {
                    d.language.as_str()
                };
                let selected = options.iter().position(|o| o == lang_now).unwrap_or(0);
                d.editor = SettingsEditor::LangPicker { options, selected };
            }
            SettingsFieldKind::F32
            | SettingsFieldKind::Usize
            | SettingsFieldKind::U32
            | SettingsFieldKind::Text => {
                let buf = d.field_value(idx);
                d.editor = SettingsEditor::Editing(buf);
            }
            SettingsFieldKind::Bool => match idx {
                7 => d.seq_per_group = !d.seq_per_group,
                10 => d.hedge_enabled = !d.hedge_enabled,
                _ => {}
            },
            SettingsFieldKind::ReadOnly => {}
        }
    }

    /// Parse and apply the committed inline-edit buffer to the draft.
    fn commit_inline_edit(&mut self, idx: usize, value: &str) {
        let Some(d) = &mut self.settings_draft else {
            return;
        };
        let v = value.trim();
        match idx {
            2 => {
                if let Ok(f) = v.parse::<f32>() {
                    d.auto_threshold = f.clamp(0.0, 1.0);
                }
            }
            3 => {
                if let Ok(f) = v.parse::<f32>() {
                    d.near_threshold = f.clamp(0.0, 1.0);
                }
            }
            4 => {
                if let Ok(n) = v.parse::<usize>() {
                    d.keep_top = n.max(1);
                }
            }
            5 => d.out_dir = v.to_string(),
            6 => {
                if !v.is_empty() {
                    d.naming_template = v.to_string();
                }
            }
            8 => {
                if let Ok(n) = v.parse::<usize>() {
                    d.max_concurrent = n.max(1);
                }
            }
            9 => {
                if let Ok(n) = v.parse::<u32>() {
                    d.max_attempts = n.max(1);
                }
            }
            _ => {}
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

    /// Move down by one viewport height (Shift-Down / Shift-J).
    fn move_selection_page_down(&mut self) {
        if self.flat.is_empty() {
            return;
        }
        // Use the book-table height from the last render as the page size.
        let page = self.last_rects.book_table.height.max(1) as usize;
        self.selected = (self.selected + page).min(self.flat.len() - 1);
    }

    /// Move up by one viewport height (Shift-Up / Shift-K).
    fn move_selection_page_up(&mut self) {
        let page = self.last_rects.book_table.height.max(1) as usize;
        self.selected = self.selected.saturating_sub(page);
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

    /// Return the md5 of the transfer row currently focused in the Activity pane.
    ///
    /// Transfers are sorted by md5 for a stable deterministic ordering so that
    /// `activity_selected` always maps to the same transfer regardless of HashMap
    /// iteration order.
    pub fn focused_transfer_md5(&self) -> Option<String> {
        let mut keys: Vec<&String> = self.transfers.keys().collect();
        keys.sort();
        keys.get(self.activity_selected).map(|k| (*k).clone())
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

                // ── History navigation (only when wildmenu is closed) ──────
                KeyCode::Up => {
                    if self.completion_candidates.is_empty() {
                        let n = self.cmd_history.len();
                        if n > 0 {
                            match self.cmd_history_cursor {
                                None => {
                                    // Save in-progress draft and jump to most recent.
                                    self.cmd_history_draft =
                                        self.command_buf.clone().unwrap_or_default();
                                    self.cmd_history_cursor = Some(0);
                                    if let Some(ref mut b) = self.command_buf {
                                        *b = self.cmd_history[n - 1].clone();
                                    }
                                }
                                Some(i) if i + 1 < n => {
                                    // Go one step older.
                                    let next = i + 1;
                                    self.cmd_history_cursor = Some(next);
                                    if let Some(ref mut b) = self.command_buf {
                                        *b = self.cmd_history[n - 1 - next].clone();
                                    }
                                }
                                _ => {} // Already at oldest entry — do nothing.
                            }
                        }
                    }
                    Intent::Redraw
                }

                KeyCode::Down => {
                    if self.completion_candidates.is_empty() {
                        match self.cmd_history_cursor {
                            None => {} // Already at the live buffer — nothing to do.
                            Some(0) => {
                                // Return to the saved draft.
                                self.cmd_history_cursor = None;
                                let draft = std::mem::take(&mut self.cmd_history_draft);
                                if let Some(ref mut b) = self.command_buf {
                                    *b = draft;
                                }
                            }
                            Some(i) => {
                                // Go one step newer.
                                let prev = i - 1;
                                self.cmd_history_cursor = Some(prev);
                                let n = self.cmd_history.len();
                                if let Some(ref mut b) = self.command_buf {
                                    *b = self.cmd_history[n - 1 - prev].clone();
                                }
                            }
                        }
                    }
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
                        // Push non-empty commands to history.
                        if !line.is_empty() {
                            self.cmd_history.push(line.clone());
                        }
                        self.cmd_history_cursor = None;
                        self.cmd_history_draft = String::new();
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
                        self.cmd_history_cursor = None;
                        self.cmd_history_draft = String::new();
                    }
                    Intent::Redraw
                }

                // ── Character input ────────────────────────────────────────
                KeyCode::Char(c) => {
                    // Any typing resets history browsing (keeps the current buffer).
                    self.cmd_history_cursor = None;
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
                    // Backspace also resets history browsing.
                    self.cmd_history_cursor = None;
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
