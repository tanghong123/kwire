//! [`AppState`] — the pure, side-effect-free app state and its reducers.
//!
//! `on_input` NEVER does I/O; it returns an [`Intent`] that the event loop
//! dispatches. `apply` folds engine events into the state.  Both are trivially
//! unit-testable because they take and return plain data.

use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use libgen_engine::{AppSettings, ViewBook, ViewEvent, ViewGroup, ViewModel, ViewVariation};
use ratatui::layout::Rect;

use crate::intent::Intent;

/// Sentinel list id for the aggregate "All" stop in the list strip — the view
/// that merges every loaded reading list. Mirrors the engine's `current`
/// sentinel (`crates/engine/src/state.rs`).
pub const ALL_LIST_ID: &str = "__all__";

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
        // 5 = naming template (per-list), 7 = download folder (global) — both Text.
        5 | 7 => SettingsFieldKind::Text,
        // 6 = sub-grouping (per-list), 10 = hedged (global) — both Bool.
        6 | 10 => SettingsFieldKind::Bool,
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
/// opens.  On save (`s`) emitted as `Intent::SaveSettings`; on discard
/// (`Esc` / `q` / `Ctrl-G`) the modal closes without touching the engine.
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
            // 5/6 are per-list (naming, sub-grouping); 7 is the GLOBAL download
            // folder — grouped with the other app-wide settings (task 8).
            5 => self.naming_template.clone(),
            6 => {
                if self.seq_per_group {
                    "on".into()
                } else {
                    "off".into()
                }
            }
            7 => {
                if self.out_dir.is_empty() {
                    "~/Books/Kwire (default)".into()
                } else {
                    self.out_dir.clone()
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
// Advertised command-line commands (wildmenu + Help). Other commands
// (`:requery`, `:delete`, `:reorganize`, `:cleanup`, `:pause`, `:start`,
// `:download-series`, `:refresh-mirrors`, `:mouse`, …) still dispatch when
// typed but are intentionally not advertised — they will move to hot keys.
const COMMANDS: &[&str] = &[
    "settings",
    "import",
    "add",
    "about",
    "start-all",
    "pause-all",
];

/// Filesystem-path completion for `:import <partial-path>`.
///
/// Splits the partial into a directory portion and a file-name prefix, expands
/// a leading `~`, lists the directory, and returns matching entries with the
/// user's typed directory prefix preserved (so the buffer stays in `~/…` form).
/// Directories get a trailing `/` so Tab can descend into them. Bad paths
/// simply yield no candidates — never panics.
pub(crate) fn complete_path(arg: &str) -> Vec<String> {
    // Directory portion (incl. trailing `/`) vs. the file-name prefix.
    let (dir_typed, prefix) = match arg.rfind('/') {
        Some(i) => (&arg[..=i], &arg[i + 1..]),
        None => ("", arg),
    };
    let read_dir = if dir_typed.is_empty() {
        ".".to_string()
    } else {
        crate::expand_tilde(dir_typed)
    };
    let mut out: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&read_dir) {
        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if !name.starts_with(prefix) {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let suffix = if is_dir { "/" } else { "" };
            out.push(format!("{dir_typed}{name}{suffix}"));
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Focus
// ---------------------------------------------------------------------------

/// Which panel currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// The Header pane: list strip + status-filter row.
    /// `←/→` navigate filter chips (and apply them); Tab cycles to List.
    Header,
    /// The book-list pane (the hero pane).  Default on launch.
    #[default]
    List,
    /// The docked downloads pane.
    Activity,
}

/// Which of the Header pane's two rows currently has the keyboard sub-focus.
/// Only meaningful while [`Focus::Header`] is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeaderRow {
    /// The reading-list strip (top row): `←/→` switch the active reading list.
    ListStrip,
    /// The status-filter chips (lower row): `←/→` switch the status filter.
    /// Default sub-row — `↑` from the book-list top lands here.
    #[default]
    FilterChips,
}

// ---------------------------------------------------------------------------
// Help pages (context-paged Help redesign)
// ---------------------------------------------------------------------------

/// One page of the context-paged Help overlay. `?` opens the page matching the
/// live context (focused pane / open modal); `←`/`→` cycle through them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpPage {
    Global,
    List,
    Header,
    Activity,
    Detail,
    Picker,
    Settings,
    Cmds,
}

impl HelpPage {
    /// All pages, in tab-row order.
    pub const ALL: [HelpPage; 8] = [
        HelpPage::Global,
        HelpPage::List,
        HelpPage::Header,
        HelpPage::Activity,
        HelpPage::Detail,
        HelpPage::Picker,
        HelpPage::Settings,
        HelpPage::Cmds,
    ];

    /// Index of this page within [`HelpPage::ALL`].
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|&p| p == self).unwrap_or(0)
    }

    /// Next page (wraps): `→`.
    pub fn next(self) -> HelpPage {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    /// Previous page (wraps): `←`.
    pub fn prev(self) -> HelpPage {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// The page that matches the currently focused pane (no modal open).
    pub fn from_focus(focus: Focus) -> HelpPage {
        match focus {
            Focus::Header => HelpPage::Header,
            Focus::List => HelpPage::List,
            Focus::Activity => HelpPage::Activity,
        }
    }

    /// Short tab-row label.
    pub fn tab_label(self) -> &'static str {
        match self {
            HelpPage::Global => "Global",
            HelpPage::List => "List",
            HelpPage::Header => "Header",
            HelpPage::Activity => "Activity",
            HelpPage::Detail => "Detail",
            HelpPage::Picker => "Picker",
            HelpPage::Settings => "Settings",
            HelpPage::Cmds => "Cmds",
        }
    }
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
    Queued,     // a queued/Pending variation AND no downloading (active) one
    InProgress, // any variation state == downloading
    Done,       // acquisition.done >= 1 && active == 0
}

impl StatusFilter {
    /// Catalog key for this filter's chrome label (shared with the desktop).
    pub fn label_key(self) -> &'static str {
        match self {
            StatusFilter::All => "filter.all",
            StatusFilter::NeedsYou => "filter.needs",
            StatusFilter::Check => "filter.review",
            StatusFilter::Cannot => "filter.cantdl",
            StatusFilter::Queued => "filter.queued",
            StatusFilter::InProgress => "filter.active",
            StatusFilter::Done => "filter.done",
        }
    }

    /// The filter's localized chrome label, resolved from the shared catalog so
    /// it matches the desktop's English (e.g. "Cannot download", "Check
    /// download").
    pub fn label(self) -> String {
        crate::i18n::tr(self.label_key())
    }

    /// TUI-only space-saving abbreviation of [`label`], used as a fallback when
    /// the full localized chip labels overflow a narrow row. These are *not*
    /// part of the shared i18n catalog — they exist purely so the filter row
    /// stays clip-free at ~80 columns.
    pub fn label_short(self) -> &'static str {
        match self {
            StatusFilter::All => "All",
            StatusFilter::NeedsYou => "Needs U",
            StatusFilter::Check => "Check DL",
            StatusFilter::Cannot => "Can't DL",
            StatusFilter::Queued => "Queued",
            StatusFilter::InProgress => "In prog",
            StatusFilter::Done => "Done",
        }
    }

    /// Cycle through filters in order (right arrow).
    pub fn next(self) -> StatusFilter {
        match self {
            StatusFilter::All => StatusFilter::NeedsYou,
            StatusFilter::NeedsYou => StatusFilter::Check,
            StatusFilter::Check => StatusFilter::Cannot,
            StatusFilter::Cannot => StatusFilter::Queued,
            StatusFilter::Queued => StatusFilter::InProgress,
            StatusFilter::InProgress => StatusFilter::Done,
            StatusFilter::Done => StatusFilter::All,
        }
    }

    /// Cycle through filters in reverse order (left arrow).
    pub fn prev(self) -> StatusFilter {
        match self {
            StatusFilter::All => StatusFilter::Done,
            StatusFilter::NeedsYou => StatusFilter::All,
            StatusFilter::Check => StatusFilter::NeedsYou,
            StatusFilter::Cannot => StatusFilter::Check,
            StatusFilter::Queued => StatusFilter::Cannot,
            StatusFilter::InProgress => StatusFilter::Queued,
            StatusFilter::Done => StatusFilter::InProgress,
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
    /// Context-paged help overlay. Opens on `page` (matching the live context);
    /// `←`/`→` page through contexts. `parent` is the modal to restore on close
    /// when Help was opened from on top of another modal (Detail/Picker/Settings).
    Help {
        page: HelpPage,
        parent: Option<Box<Modal>>,
    },
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
    ReQuery {
        book_flat_index: usize,
        buf: String,
        /// Caret position (char index) within `buf`.
        caret: usize,
    },
    /// Inline edit: user edits title and/or author for a single book.
    EditBook {
        book_flat_index: usize,
        title_buf: String,
        author_buf: String,
        field: EditBookField,
        /// Caret position (char index) within the ACTIVE field's buffer.
        caret: usize,
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
    /// Generic snapshot popup: a centered sub-modal with a title and a list of
    /// `(label, value)` rows.  Esc returns to `parent` (or closes if `None`).
    Snapshot {
        title: String,
        lines: Vec<(String, String)>,
        /// Modal to return to on Esc; `None` means close entirely.
        parent: Option<Box<Modal>>,
    },
    /// The `:about` splash — the empty-screen wordmark + tagline in a modal.
    /// Closes on Esc. No state.
    About,
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
    /// Count of variations currently downloading (any > 0 ⇒ the list is "running").
    pub downloading: usize,
    /// Count of variations paused mid-transfer (any > 0, with none downloading,
    /// ⇒ the list is "paused"). Mirrors the desktop sidebar's per-list status dot.
    pub paused: usize,
    /// True for the singleton mutable **Manual** list (mirrors the engine's
    /// `ListSettings::is_manual`). The list view shows per-book add/remove
    /// affordances (and the `x` remove hint) only for this list.
    pub is_manual: bool,
}

// ---------------------------------------------------------------------------
// RowRef — identity of a single rendered row in the main book list
// ---------------------------------------------------------------------------

/// Identity of one rendered row in the main book table. A book with two or more
/// ARMED (requested) variations renders its primary copy on a `Book` row and one
/// `Variation` sub-row per additional armed copy. Used for selection state,
/// arrow-nav, and mouse hit-testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowRef {
    /// The book's primary row (flat-book index).
    Book(usize),
    /// An indented "↳ alt. copy" sub-row: (flat-book index, variation md5).
    Variation(usize, String),
}

/// Ordering rank for an armed variation's display: lower sorts earlier (primary).
/// done < downloading < queued/paused < failed/cancelled < anything else.
fn variation_display_rank(state: &str) -> u8 {
    match state {
        "done" => 0,
        "downloading" => 1,
        "queued" => 2,
        "paused" => 3,
        "failed" | "cancelled" => 4,
        _ => 5,
    }
}

/// The ARMED (requested — `state != "available"`) variations of a book, in
/// display order (primary first). Empty when the book has no armed copies (pure
/// discovery / pick state). Both the renderer and the selection/nav model derive
/// the stacked-row layout from this single function so they never diverge.
pub fn armed_variations(book: &ViewBook) -> Vec<&ViewVariation> {
    let mut v: Vec<&ViewVariation> = book
        .versions
        .iter()
        .filter(|v| v.state != "available")
        .collect();
    // Stable sort keeps mirror/insertion order within the same state class.
    v.sort_by_key(|x| variation_display_rank(&x.state));
    v
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
    /// `(row_rect, RowRef)` for each rendered row (book row or variation
    /// sub-row), in render order.
    pub book_rows: Vec<(Rect, RowRef)>,
    /// `(chip_rect, StatusFilter)` for each rendered filter chip.
    pub filter_chips: Vec<(Rect, StatusFilter)>,
    /// `(chip_rect, list_index)` for each list chip in the strip.
    pub list_chips: Vec<(Rect, usize)>,
    /// `(row_rect, leg_index)` for each rendered activity leg row.
    pub activity_rows: Vec<(Rect, usize)>,
    /// Detail modal — the Variations sub-list area (for wheel hit-testing).
    pub detail_var_area: Rect,
    /// Detail modal — the History sub-list area (for wheel hit-testing).
    pub detail_hist_area: Rect,
    /// Detail modal — `(row_rect, variation_index)` per rendered variation row.
    pub detail_var_rows: Vec<(Rect, usize)>,
    /// Detail modal — `(row_rect, history_index)` per rendered (visible) history row.
    pub detail_hist_rows: Vec<(Rect, usize)>,
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

/// Byte offset of char index `ci` within `s`, clamped to `s.len()` — for
/// UTF-8-safe insert/remove at a caret position.
fn byte_at_char(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len())
}

/// Return `true` when the terminal cell `(col, row)` falls inside `rect`.
fn point_in_rect(col: u16, row: u16, rect: Rect) -> bool {
    rect.width > 0
        && rect.height > 0
        && col >= rect.x
        && col < rect.x + rect.width
        && row >= rect.y
        && row < rect.y + rect.height
}

/// Full TUI application state.  Only plain data — no I/O handles.
pub struct AppState {
    /// The projected library snapshot the UI renders from.
    pub view: Option<ViewModel>,

    /// Flat ordered list of all visible books (group + book pairs) after
    /// filtering.  Rebuilt whenever `view` or `filter` changes.
    pub flat: Vec<FlatBook>,

    /// Index into `flat` for the currently selected book (the OWNING book of the
    /// focused row — a book row OR one of its variation sub-rows).
    pub selected: usize,

    /// When a variation SUB-ROW is focused, the md5 of that armed variation
    /// (within the book at `selected`). `None` = the book's primary row is
    /// focused. Validated/cleared by `rebuild_flat` when the variation vanishes.
    pub selected_var: Option<String>,

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

    /// Total number of windowable content lines in the (expanded) Activity pane
    /// — host headers + transfer legs + the queued header + queued rows. Set by
    /// `render_activity` each frame and read by `scroll_activity` so the scroll
    /// bound covers the whole list (incl. the queued section at the bottom), not
    /// just the selectable download legs. 0 while collapsed.
    pub activity_content_len: usize,

    /// Rects from the most-recent render pass (for mouse hit-testing).
    pub last_rects: LastRects,

    /// Which settings field is focused in the Settings modal (0-based field index).
    pub settings_selected: usize,

    /// Staged edits for the Settings modal; `Some` while the modal is open.
    pub settings_draft: Option<SettingsDraft>,

    /// Live scheduler telemetry keyed by md5. Updated by Progress events from the engine.
    pub transfers: std::collections::HashMap<String, ActiveTransfer>,

    /// Global md5 → book-title map across ALL loaded lists (rebuilt each refresh
    /// from every list's snapshot). Lets the Activity pane label a transfer whose
    /// book isn't in the CURRENT view (e.g. a download from a list you're not
    /// viewing) with its real title instead of the bare md5.
    pub md5_titles: std::collections::HashMap<String, String>,

    /// Tab-completion candidates for the `:` command line.
    /// Non-empty while the wildmenu is visible.
    pub completion_candidates: Vec<String>,

    /// Index of the highlighted candidate within `completion_candidates`.
    pub completion_index: usize,

    /// Transient one-shot status message (shown in the hint bar until the
    /// next keypress, then cleared automatically in `on_input`).
    pub status_msg: Option<String>,

    /// Whether mouse capture is currently enabled.
    /// Toggled by `:mouse`; the event loop acts on the change.
    pub mouse_capture: bool,

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

    /// `true` when the aggregate "All" stop in the list strip is the active
    /// selection (the `__all__` sentinel view), rather than a single real list.
    /// The `[`/`]` cycle steps onto this as one extra stop before the first list.
    pub all_active: bool,

    /// Which Header sub-row (list strip / filter chips) is focused. Only used
    /// while `focus == Focus::Header`; `↑/↓` walk between the two rows and the
    /// book list, `←/→` act on whichever row is focused.
    pub header_row: HeaderRow,

    /// Origin map for the aggregate "All" view: index = aggregate group index,
    /// value = (owning list id, original group index within that list). Lets
    /// actions taken in All view route to the OWNING list's orchestrator. Empty
    /// when not viewing the aggregate.
    pub aggregate_origins: Vec<(String, usize)>,

    // ── Marquee scroll state (Detail modal · Title·Author column) ─────────────
    /// Character offset into the selected variation's "Title · Author" string.
    /// The rendered text starts at this offset and is clipped to the column width.
    pub marquee_offset: usize,
    /// `true` = scrolling forward (revealing the tail); `false` = scrolling back.
    pub marquee_forward: bool,
    /// Countdown (in ticks) for the pause held at each end of the ping-pong.
    pub marquee_pause: u8,
    /// The variation-selection index that was active when the marquee was last
    /// reset.  A change here resets offset + direction.
    pub marquee_detail_sel: usize,

    // ── Marquee scroll state (focused VARIATION / picker ROW · #1/#11) ─────────
    /// Column offset for the FOCUSED variation/picker row's flexing title·author
    /// (Mode A) or whole packed line (Mode B). Independent of the book-header
    /// marquee above so the two animate without fighting. Only ONE row (the
    /// focused one) ever animates; unfocused rows ellipsize.
    pub var_marquee_offset: usize,
    pub var_marquee_forward: bool,
    pub var_marquee_pause: u8,
    /// Row index active when the row marquee was last reset (selection change
    /// resets offset + direction). Shared by the Detail variations table and the
    /// Picker candidate list — only one of those modals is open at a time.
    pub var_marquee_sel: usize,

    // ── Marquee scroll state (list strip · ACTIVE list column · #15) ───────────
    /// Column offset for the active list's title when it overflows its strip
    /// column. Independent of the row/header marquees so the always-rendered
    /// strip never fights the Detail/list-row animations.
    pub list_marquee_offset: usize,
    pub list_marquee_forward: bool,
    pub list_marquee_pause: u8,
    /// Active-list identity when the strip marquee was last reset (a change —
    /// switching the active list — resets offset + direction).
    pub list_marquee_sel: usize,

    // ── Marquee scroll state (Settings modal · FOCUSED row value · #2) ─────────
    /// Column offset for the focused settings row's long value (Download folder,
    /// Naming template, …) when it overflows its value column and the row is not
    /// being edited. Independent of the other marquees so the modal animation
    /// never fights the list/strip/row marquees underneath.
    pub settings_marquee_offset: usize,
    pub settings_marquee_forward: bool,
    pub settings_marquee_pause: u8,
    /// Settings field index active when the marquee was last reset (moving rows
    /// resets offset + direction).
    pub settings_marquee_sel: usize,

    // ── Marquee scroll state (Activity pane · FOCUSED transfer-leg title · #9) ──
    /// Column offset for the focused download leg's title when it overflows the
    /// flex region left of the pinned status (fmt · % · bar · eta). Independent
    /// of the other marquees so the activity animation never fights them.
    pub activity_marquee_offset: usize,
    pub activity_marquee_forward: bool,
    pub activity_marquee_pause: u8,
    /// Leg index active when the activity marquee was last reset (moving the
    /// selection resets offset + direction).
    pub activity_marquee_sel: usize,

    // ── Time-driven marquee stepping (task 11) ────────────────────────────────
    /// Wall-clock epoch all marquee phases are measured from. Marquees advance
    /// one ping-pong step per [`MARQUEE_STEP_MS`] of ELAPSED time rather than
    /// once per render, so a burst of redraws (e.g. mouse-wheel scrolling the
    /// book list) no longer speeds the marquees up.
    pub marquee_epoch: Instant,
    /// The elapsed-time phase (`elapsed / MARQUEE_STEP_MS`) last applied to the
    /// marquees — `begin_marquee_frame` diffs against it to find how many steps
    /// are owed this frame.
    pub marquee_phase: u64,
    /// Steps owed to every marquee this frame (`begin_marquee_frame` recomputes
    /// it from elapsed time; the `advance_*_marquee` helpers consume it). Starts
    /// at 1 so direct (non-render) callers — unit tests — still step once.
    pub marquee_steps_due: u32,
}

/// How long (wall-clock) one marquee ping-pong step takes. Pairs with the 120 ms
/// redraw tick and keeps the end-of-travel pause at ~960 ms (8 steps). Marquee
/// speed is now a function of ELAPSED time, independent of render frequency.
pub const MARQUEE_STEP_MS: u128 = 120;

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

/// Shared ping-pong stepping for a marquee's `(offset, forward, pause)` triple.
///
/// Used by both the book-header marquee (`advance_marquee`) and the focused
/// variation/picker row marquee (`advance_var_marquee`). Offset is in display
/// columns; pair with `textfit::marquee_window` / `marquee_char_range`.
fn step_marquee(
    offset: &mut usize,
    forward: &mut bool,
    pause: &mut u8,
    text_disp_w: usize,
    col_w: usize,
    steps: u32,
) {
    if text_disp_w <= col_w {
        // Text fits: park at zero (regardless of `steps`).
        *offset = 0;
        *forward = true;
        *pause = 0;
        return;
    }
    let max_offset = text_disp_w.saturating_sub(col_w);
    // Apply `steps` ping-pong increments — `steps` is how many MARQUEE_STEP_MS
    // intervals elapsed since the last frame (usually 0 or 1), so the scroll
    // speed tracks wall-clock time rather than redraw frequency (task 11).
    for _ in 0..steps {
        if *pause > 0 {
            *pause -= 1;
            continue;
        }
        if *forward {
            *offset = (*offset + 1).min(max_offset);
            if *offset >= max_offset {
                *forward = false;
                *pause = 8; // ~960 ms pause at the end
            }
        } else if *offset == 0 {
            *forward = true;
            *pause = 8; // ~960 ms pause at the start
        } else {
            *offset -= 1;
        }
    }
}

impl AppState {
    /// Construct an empty state (no list loaded yet).
    pub fn new() -> Self {
        AppState {
            view: None,
            flat: Vec::new(),
            selected: 0,
            selected_var: None,
            filter: StatusFilter::All,
            focus: Focus::List,
            activity_expanded: true,
            tick: 0,
            command_buf: None,
            modal: None,
            activity_selected: 0,
            activity_content_len: 0,
            last_rects: LastRects::default(),
            settings_selected: 0,
            settings_draft: None,
            transfers: std::collections::HashMap::new(),
            md5_titles: std::collections::HashMap::new(),
            completion_candidates: Vec::new(),
            completion_index: 0,
            status_msg: None,
            mouse_capture: true,
            cmd_history: Vec::new(),
            cmd_history_cursor: None,
            cmd_history_draft: String::new(),
            all_lists: Vec::new(),
            active_list_idx: 0,
            all_active: false,
            header_row: HeaderRow::FilterChips,
            aggregate_origins: Vec::new(),
            marquee_offset: 0,
            marquee_forward: true,
            marquee_pause: 0,
            marquee_detail_sel: 0,
            var_marquee_offset: 0,
            var_marquee_forward: true,
            var_marquee_pause: 0,
            var_marquee_sel: 0,
            list_marquee_offset: 0,
            list_marquee_forward: true,
            list_marquee_pause: 0,
            list_marquee_sel: 0,
            settings_marquee_offset: 0,
            settings_marquee_forward: true,
            settings_marquee_pause: 0,
            settings_marquee_sel: 0,
            activity_marquee_offset: 0,
            activity_marquee_forward: true,
            activity_marquee_pause: 0,
            activity_marquee_sel: 0,
            marquee_epoch: Instant::now(),
            marquee_phase: 0,
            // Default to one step so direct unit-test callers of the
            // `advance_*_marquee` helpers still advance by one; the render loop
            // overrides this each frame via `begin_marquee_frame`.
            marquee_steps_due: 1,
        }
    }

    /// Recompute how many marquee steps are owed this frame from ELAPSED time
    /// (task 11). Call once at the start of each render: marquees then advance at
    /// a constant wall-clock rate ([`MARQUEE_STEP_MS`] per step) regardless of how
    /// often the screen is redrawn, so wheel-triggered re-renders don't speed
    /// them up. The delta is capped so returning from a long idle/pause doesn't
    /// fast-forward the scroll.
    pub fn begin_marquee_frame(&mut self) {
        let phase = (self.marquee_epoch.elapsed().as_millis() / MARQUEE_STEP_MS) as u64;
        let due = phase.saturating_sub(self.marquee_phase);
        self.marquee_steps_due = due.min(4) as u32;
        self.marquee_phase = phase;
    }

    /// Return the same `Intent` that `Enter` would produce for the currently
    /// selected book.  Used by the mouse handler to implement "click selected → Enter".
    fn enter_action_for_selected(&self) -> Intent {
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

    /// Toggle mouse capture on/off and set a status message.
    ///
    /// The actual `EnableMouseCapture` / `DisableMouseCapture` crossterm calls
    /// are performed by the event loop in `main.rs` after it reads
    /// `app.mouse_capture`.
    pub fn toggle_mouse_capture(&mut self) {
        self.mouse_capture = !self.mouse_capture;
        if self.mouse_capture {
            self.status_msg = Some("Mouse capture: ON  (Shift/Option-drag for text select)".into());
        } else {
            self.status_msg = Some("Mouse capture: OFF  (re-enable with :mouse)".into());
        }
    }

    // -----------------------------------------------------------------------
    // Marquee helpers (Detail modal · Title·Author ping-pong scroll)
    // -----------------------------------------------------------------------

    /// Advance the marquee by one tick for the Detail modal's Title·Author field.
    ///
    /// `text_disp_w` — terminal **display width** (columns) of the full
    ///                 "Title · Author" string, via `textfit::display_width`.
    ///                 Must be display width, NOT `chars().count()`, so CJK /
    ///                 emoji titles trigger and scroll correctly (#10/#14).
    /// `col_w`       — terminal-cell width available for the column.
    ///
    /// `marquee_offset` is therefore measured in display columns; pair it with
    /// `textfit::marquee_window` / `marquee_char_range` when slicing.
    /// Must be called once per render tick while the Detail modal is open.
    pub fn advance_marquee(&mut self, text_disp_w: usize, col_w: usize) {
        let steps = self.marquee_steps_due;
        step_marquee(
            &mut self.marquee_offset,
            &mut self.marquee_forward,
            &mut self.marquee_pause,
            text_disp_w,
            col_w,
            steps,
        );
    }

    /// Advance the FOCUSED variation/picker row's marquee by one tick (#1/#11).
    ///
    /// Same ping-pong mechanics as [`advance_marquee`] but over the dedicated
    /// `var_marquee_*` state. Call once per render for the focused row only —
    /// `text_disp_w` is the display width of that row's flexing text (Mode A
    /// title·author, or Mode B whole packed line) and `col_w` its window width.
    pub fn advance_var_marquee(&mut self, text_disp_w: usize, col_w: usize) {
        let steps = self.marquee_steps_due;
        step_marquee(
            &mut self.var_marquee_offset,
            &mut self.var_marquee_forward,
            &mut self.var_marquee_pause,
            text_disp_w,
            col_w,
            steps,
        );
    }

    /// Reset the focused-row marquee when the picker/variation selection changes.
    pub fn reset_var_marquee_if_changed(&mut self, new_sel: usize) {
        if new_sel != self.var_marquee_sel {
            self.var_marquee_offset = 0;
            self.var_marquee_forward = true;
            self.var_marquee_pause = 0;
            self.var_marquee_sel = new_sel;
        }
    }

    /// Advance the active list's strip marquee by one tick (#15).
    ///
    /// Same ping-pong mechanics as the others over the dedicated `list_marquee_*`
    /// state. `text_disp_w` is the active list label's display width and `col_w`
    /// its strip-column width.
    pub fn advance_list_marquee(&mut self, text_disp_w: usize, col_w: usize) {
        let steps = self.marquee_steps_due;
        step_marquee(
            &mut self.list_marquee_offset,
            &mut self.list_marquee_forward,
            &mut self.list_marquee_pause,
            text_disp_w,
            col_w,
            steps,
        );
    }

    /// Reset the strip marquee when the active list changes (#15).
    pub fn reset_list_marquee_if_changed(&mut self, new_sel: usize) {
        if new_sel != self.list_marquee_sel {
            self.list_marquee_offset = 0;
            self.list_marquee_forward = true;
            self.list_marquee_pause = 0;
            self.list_marquee_sel = new_sel;
        }
    }

    /// Advance the focused Settings row's value marquee by one tick (#2).
    ///
    /// Same ping-pong mechanics as the others over the dedicated
    /// `settings_marquee_*` state. `text_disp_w` is the focused value's display
    /// width and `col_w` its value-column width; call once per render while the
    /// Settings modal is open (park it — `(0, 1)` — while a field is editing).
    pub fn advance_settings_marquee(&mut self, text_disp_w: usize, col_w: usize) {
        let steps = self.marquee_steps_due;
        step_marquee(
            &mut self.settings_marquee_offset,
            &mut self.settings_marquee_forward,
            &mut self.settings_marquee_pause,
            text_disp_w,
            col_w,
            steps,
        );
    }

    /// Reset the Settings value marquee when the selected field changes (#2).
    pub fn reset_settings_marquee_if_changed(&mut self, new_sel: usize) {
        if new_sel != self.settings_marquee_sel {
            self.settings_marquee_offset = 0;
            self.settings_marquee_forward = true;
            self.settings_marquee_pause = 0;
            self.settings_marquee_sel = new_sel;
        }
    }

    /// Advance the focused download leg's title marquee by one tick (#9).
    ///
    /// Same ping-pong mechanics as the others over the dedicated
    /// `activity_marquee_*` state. `text_disp_w` is the leg title's display width
    /// and `col_w` the flex region left of the pinned status; call once per
    /// render for the focused leg only.
    pub fn advance_activity_marquee(&mut self, text_disp_w: usize, col_w: usize) {
        let steps = self.marquee_steps_due;
        step_marquee(
            &mut self.activity_marquee_offset,
            &mut self.activity_marquee_forward,
            &mut self.activity_marquee_pause,
            text_disp_w,
            col_w,
            steps,
        );
    }

    /// Reset the activity-leg marquee when the focused leg changes (#9).
    pub fn reset_activity_marquee_if_changed(&mut self, new_sel: usize) {
        if new_sel != self.activity_marquee_sel {
            self.activity_marquee_offset = 0;
            self.activity_marquee_forward = true;
            self.activity_marquee_pause = 0;
            self.activity_marquee_sel = new_sel;
        }
    }

    /// Reset the marquee when the variation selection changes.
    ///
    /// Call at the start of each Detail-modal render with the current
    /// `detail_selected` index.  A change resets offset, direction, and pause.
    pub fn reset_marquee_if_selection_changed(&mut self, new_sel: usize) {
        if new_sel != self.marquee_detail_sel {
            self.marquee_offset = 0;
            self.marquee_forward = true;
            self.marquee_pause = 0;
            self.marquee_detail_sel = new_sel;
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
                // Populate title from the current ViewModel, falling back to the
                // GLOBAL md5→title map (covers a transfer whose book isn't in the
                // current view — e.g. a download from a list you're not viewing),
                // so the Activity pane shows a real title instead of the md5.
                if t.title.is_empty() {
                    let from_view = self.view.as_ref().and_then(|vm| {
                        vm.groups
                            .iter()
                            .flat_map(|g| &g.books)
                            .find(|b| b.versions.iter().any(|v| v.md5 == *md5))
                            .map(|b| b.title.clone())
                    });
                    if let Some(title) = from_view.or_else(|| self.md5_titles.get(md5).cloned()) {
                        t.title = title;
                    }
                }
                // Live-update the PROJECTED view (app.flat) in place. The list rows
                // and the Activity ViewModel path read app.flat, which is otherwise
                // only rebuilt on `Refresh` (StatusChanged/Done) — so without this
                // the displayed % freezes at its start-of-download value (~0%) until
                // Done. The periodic redraw tick then paints the advancing %.
                self.update_flat_progress(
                    md5,
                    *bytes_done,
                    *total_bytes,
                    *speed_bps,
                    *eta_secs,
                    host,
                );
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

    /// Live-patch the matching downloading copy's progress in the projected
    /// `app.flat` (by md5) from a `Progress::Bytes` tick, so the list rows and the
    /// Activity ViewModel path advance between `Refresh` events instead of freezing
    /// at the start-of-download %. The `%` is recomputed only when the total size
    /// is known; bytes/host/speed/eta are always updated.
    fn update_flat_progress(
        &mut self,
        md5: &str,
        bytes_done: u64,
        total_bytes: Option<u64>,
        speed_bps: Option<u64>,
        eta_secs: Option<u64>,
        host: &str,
    ) {
        let pct = match total_bytes {
            Some(total) if total > 0 => {
                Some((((bytes_done as f64 / total as f64) * 100.0).round() as u32).min(100))
            }
            _ => None,
        };
        for fb in &mut self.flat {
            for v in &mut fb.book.versions {
                if v.md5 == md5 {
                    if let Some(p) = pct {
                        v.progress = p;
                    }
                    v.total_bytes = total_bytes;
                    v.downloaded_bytes = Some(bytes_done);
                    v.speed_bps = speed_bps;
                    v.eta_secs = eta_secs;
                    if !host.is_empty() {
                        v.host = Some(host.to_string());
                    }
                }
            }
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

    /// Drop the active view entirely — used when the last list is deleted and no
    /// list remains current. Resets the flat rows, selection, and focus so the
    /// empty / first-run splash renders (gated on `view.is_none()`) instead of
    /// the deleted list's stale, orphaned rows, and so a later import starts
    /// from a clean cursor.
    pub fn clear_view(&mut self) {
        self.view = None;
        self.selected = 0;
        self.focus = Focus::List;
        self.header_row = HeaderRow::FilterChips;
        self.rebuild_flat(); // `view` is None → clears `flat`
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
                                Focus::List => {
                                    // #65 arrow-cross: ↓ at bottom → focus Activity
                                    if !self.flat.is_empty() && self.at_last_row() {
                                        self.focus = Focus::Activity;
                                        self.activity_selected = 0;
                                    } else {
                                        self.move_selection(1);
                                    }
                                }
                                Focus::Activity => self.scroll_activity(1),
                                // Header two-row walk: list strip → filter chips
                                // → top of book list.
                                Focus::Header => self.header_down(),
                            }
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('j') => {
                        match self.focus {
                            Focus::List => {
                                // #65 arrow-cross: j at bottom → focus Activity
                                if !self.flat.is_empty() && self.at_last_row() {
                                    self.focus = Focus::Activity;
                                    self.activity_selected = 0;
                                } else {
                                    self.move_selection(1);
                                }
                            }
                            Focus::Activity => self.scroll_activity(1),
                            // Header two-row walk (vim): strip → chips → list.
                            Focus::Header => self.header_down(),
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
                                Focus::List => {
                                    // arrow-cross: ↑ at top of List → Header
                                    // (lands on the filter chips sub-row).
                                    if self.at_first_row() {
                                        self.focus = Focus::Header;
                                        self.header_row = HeaderRow::FilterChips;
                                    } else {
                                        self.move_selection_up();
                                    }
                                }
                                Focus::Activity => {
                                    // #65 arrow-cross: ↑ at top → focus List
                                    if self.activity_selected == 0 {
                                        self.focus = Focus::List;
                                    } else {
                                        self.scroll_activity_up();
                                    }
                                }
                                // Header two-row walk: filter chips → list strip
                                // → STOP (top of everything).
                                Focus::Header => self.header_up(),
                            }
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('k') => {
                        match self.focus {
                            Focus::List => {
                                // arrow-cross: k at top of List → Header
                                // (lands on the filter chips sub-row).
                                if self.at_first_row() {
                                    self.focus = Focus::Header;
                                    self.header_row = HeaderRow::FilterChips;
                                } else {
                                    self.move_selection_up();
                                }
                            }
                            Focus::Activity => {
                                // #65 arrow-cross: k at top → focus List
                                if self.activity_selected == 0 {
                                    self.focus = Focus::List;
                                } else {
                                    self.scroll_activity_up();
                                }
                            }
                            // Header two-row walk (vim): chips → strip → STOP.
                            Focus::Header => self.header_up(),
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
                    // Tab: Header → List → Activity → (wrap back to Header).
                    KeyCode::Tab => {
                        self.focus = match self.focus {
                            Focus::Header => Focus::List,
                            Focus::List => Focus::Activity,
                            Focus::Activity => Focus::Header,
                        };
                        Intent::Redraw
                    }
                    // Shift-Tab: reverse cycle  Activity → List → Header → (wrap back to Activity).
                    KeyCode::BackTab => {
                        self.focus = match self.focus {
                            Focus::Header => Focus::Activity,
                            Focus::List => Focus::Header,
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
                        self.filter = StatusFilter::Queued;
                        self.rebuild_flat();
                        Intent::Redraw
                    }
                    KeyCode::Char('6') => {
                        self.filter = StatusFilter::InProgress;
                        self.rebuild_flat();
                        Intent::Redraw
                    }
                    KeyCode::Char('7') => {
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
                    // #70 universal Enter: dispatch by current focus.
                    KeyCode::Enter => match self.focus {
                        // Activity pane: open a leg snapshot for the focused transfer.
                        Focus::Activity => {
                            if let Some(m) = self.build_leg_snapshot_modal() {
                                self.modal = Some(m);
                            }
                            Intent::Redraw
                        }
                        // List / Header: existing open-detail / open-picker behaviour.
                        _ => {
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
                    },
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
                                // Alt-copy sub-row focused → re-arm just THAT copy
                                // (mirrors the detail-view `r` fix); whole-book row
                                // → re-query the whole book.
                                if let Some(md5) = self.selected_var.clone() {
                                    Intent::RequestVariations {
                                        group_path: vec![fb.group_index],
                                        book_index: fb.book_index_in_group,
                                        md5s: vec![md5],
                                    }
                                } else {
                                    Intent::Retry {
                                        group_path: vec![fb.group_index],
                                        book_index: fb.book_index_in_group,
                                    }
                                }
                            } else {
                                Intent::Redraw
                            }
                        }
                        // Header focus → re-search (re-query) the whole active list.
                        Focus::Header => Intent::Command("requery".into()),
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
                                // Alt-copy sub-row focused → pause just THAT copy's
                                // transfer; whole-book row → pause the whole book.
                                if let Some(md5) = self.selected_var.clone() {
                                    Intent::PauseTransfer { md5 }
                                } else {
                                    Intent::Pause {
                                        group_path: vec![fb.group_index],
                                        book_index: fb.book_index_in_group,
                                    }
                                }
                            } else {
                                Intent::Redraw
                            }
                        }
                        // Header focus → pause the whole active list.
                        Focus::Header => Intent::Command("pause".into()),
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
                                // Alt-copy sub-row focused → cancel just THAT copy's
                                // transfer; whole-book row → cancel the whole book.
                                if let Some(md5) = self.selected_var.clone() {
                                    Intent::CancelTransfer { md5 }
                                } else {
                                    Intent::Cancel {
                                        group_path: vec![fb.group_index],
                                        book_index: fb.book_index_in_group,
                                    }
                                }
                            } else {
                                Intent::Redraw
                            }
                        }
                        Focus::Header => Intent::Redraw,
                    },
                    KeyCode::Char(' ') => {
                        // Collapse / expand the Activity pane from ANYWHERE in the
                        // main view (list, header, or the pane itself) — no need to
                        // focus the pane first.
                        self.activity_expanded = !self.activity_expanded;
                        if !self.activity_expanded {
                            self.activity_selected = 0;
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('o') => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            if let Some(path) = list_target_path(fb, self.selected_var.as_deref()) {
                                return Intent::OpenFile(path);
                            }
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('R') => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            if let Some(path) = list_target_path(fb, self.selected_var.as_deref()) {
                                return Intent::RevealFile(path);
                            }
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('?') => {
                        // Open Help on the page matching the focused pane.
                        self.modal = Some(Modal::Help {
                            page: HelpPage::from_focus(self.focus),
                            parent: None,
                        });
                        Intent::Redraw
                    }
                    // `[` / `]` — GLOBAL list cycle: works from any pane, never
                    // changes focus. The rotation includes the aggregate "All"
                    // stop as one extra position before the first real list:
                    //   All → list0 → list1 → … → All  (and back with `[`).
                    KeyCode::Char('[') => self.cycle_list_prev(),
                    KeyCode::Char(']') => self.cycle_list_next(),
                    // `←/→` — act on the FOCUSED Header sub-row: on the filter
                    // chips → prev/next status filter; on the list strip →
                    // prev/next reading list (same switch as `[`/`]`). No-op when
                    // List or Activity is focused.
                    KeyCode::Left => {
                        if self.focus == Focus::Header {
                            match self.header_row {
                                HeaderRow::FilterChips => {
                                    self.filter = self.filter.prev();
                                    self.rebuild_flat();
                                }
                                HeaderRow::ListStrip => return self.cycle_list_prev(),
                            }
                        }
                        Intent::Redraw
                    }
                    KeyCode::Right => {
                        if self.focus == Focus::Header {
                            match self.header_row {
                                HeaderRow::FilterChips => {
                                    self.filter = self.filter.next();
                                    self.rebuild_flat();
                                }
                                HeaderRow::ListStrip => return self.cycle_list_next(),
                            }
                        }
                        Intent::Redraw
                    }
                    // `<` / `>` — char aliases for `←` / `→` (some users press the
                    // angle brackets to page the list strip). They had NO dispatch,
                    // so `<` never reached the aggregate "All" stop while `←` did.
                    KeyCode::Char('<') => {
                        if self.focus == Focus::Header {
                            match self.header_row {
                                HeaderRow::FilterChips => {
                                    self.filter = self.filter.prev();
                                    self.rebuild_flat();
                                }
                                HeaderRow::ListStrip => return self.cycle_list_prev(),
                            }
                        }
                        Intent::Redraw
                    }
                    KeyCode::Char('>') => {
                        if self.focus == Focus::Header {
                            match self.header_row {
                                HeaderRow::FilterChips => {
                                    self.filter = self.filter.next();
                                    self.rebuild_flat();
                                }
                                HeaderRow::ListStrip => return self.cycle_list_next(),
                            }
                        }
                        Intent::Redraw
                    }
                    // `a` — fetch ALL preferred-format copies for the focused book:
                    // one top-ranked candidate per format in the list's format
                    // preference, each armed as its own download.
                    KeyCode::Char('a') => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            let md5s = self.preferred_format_md5s(&fb.book);
                            if !md5s.is_empty() {
                                return Intent::RequestVariations {
                                    group_path: vec![fb.group_index],
                                    book_index: fb.book_index_in_group,
                                    md5s,
                                };
                            }
                        }
                        Intent::Redraw
                    }
                    // #50 book-level actions from list view.
                    KeyCode::Char('e') => {
                        if let Some(fb) = self.flat.get(self.selected) {
                            let flat_idx = self.selected;
                            let title_buf = fb.book.title.clone();
                            let author_buf = fb.book.author.clone();
                            let caret = title_buf.chars().count();
                            self.modal = Some(Modal::EditBook {
                                book_flat_index: flat_idx,
                                title_buf,
                                author_buf,
                                field: EditBookField::Title,
                                caret,
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
                    // ── Header-pane LIST ops (active ONLY when the Header is focused).
                    // These reuse the `:` command handlers so behaviour is identical.
                    // `s` start/resume the active list; `D` delete it (keeps y/n confirm).
                    KeyCode::Char('s') if self.focus == Focus::Header => {
                        Intent::Command("start".into())
                    }
                    KeyCode::Char('D') if self.focus == Focus::Header => {
                        Intent::Command("delete".into())
                    }
                    _ => Intent::Redraw,
                }
            }

            // ---------------------------------------------------------------
            // Mouse: clicks and wheel map to the same intents as keys.
            // Wheel follows the CURSOR position (not keyboard focus).
            // Single-click selects + focuses the pane; clicking the already-
            // selected item performs its Enter action.
            // ---------------------------------------------------------------
            Event::Mouse(me) => {
                let col = me.column;
                let row = me.row;
                match me.kind {
                    MouseEventKind::ScrollDown => {
                        // Follow cursor position, not keyboard focus.
                        if point_in_rect(col, row, self.last_rects.activity) {
                            // Wheel SCROLLS the pane (selection moves ±1, list
                            // follows); it does NOT hover-jump to the cursor row.
                            self.focus = Focus::Activity;
                            self.scroll_activity(1);
                        } else if point_in_rect(col, row, self.last_rects.book_table) {
                            self.focus = Focus::List;
                            self.move_selection(1);
                        } else {
                            // Fallback to focus-based (header / unknown area).
                            match self.focus {
                                Focus::List => self.move_selection(1),
                                Focus::Activity => self.scroll_activity(1),
                                Focus::Header => {}
                            }
                        }
                        Intent::Redraw
                    }
                    MouseEventKind::ScrollUp => {
                        if point_in_rect(col, row, self.last_rects.activity) {
                            // Wheel SCROLLS the pane; no hover-jump to the cursor.
                            self.focus = Focus::Activity;
                            self.scroll_activity_up();
                        } else if point_in_rect(col, row, self.last_rects.book_table) {
                            self.focus = Focus::List;
                            self.move_selection_up();
                        } else {
                            match self.focus {
                                Focus::List => self.move_selection_up(),
                                Focus::Activity => self.scroll_activity_up(),
                                Focus::Header => {}
                            }
                        }
                        Intent::Redraw
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        // 1. Book/variation rows: select + focus List; second click → Enter.
                        let book_rows = self.last_rects.book_rows.clone();
                        for (rect, rref) in &book_rows {
                            if point_in_rect(col, row, *rect) {
                                let already =
                                    self.row_is_focused(rref) && self.focus == Focus::List;
                                self.focus = Focus::List;
                                self.set_focus_row(rref);
                                if already {
                                    return self.enter_action_for_selected();
                                }
                                return Intent::Redraw;
                            }
                        }

                        // 2. List strip chips: switch list + focus Header (first click fires).
                        let list_chips = self.last_rects.list_chips.clone();
                        for (rect, list_idx) in &list_chips {
                            if point_in_rect(col, row, *rect) && *list_idx < self.all_lists.len() {
                                let id = self.all_lists[*list_idx].id.clone();
                                self.active_list_idx = *list_idx;
                                self.all_active = false;
                                self.focus = Focus::Header;
                                self.header_row = HeaderRow::ListStrip;
                                return Intent::SwitchList { id };
                            }
                        }

                        // 3. Filter chips: set filter (first click fires).
                        let filter_chips = self.last_rects.filter_chips.clone();
                        for (rect, filter) in &filter_chips {
                            if point_in_rect(col, row, *rect) {
                                self.filter = *filter;
                                self.focus = Focus::Header;
                                self.header_row = HeaderRow::FilterChips;
                                self.rebuild_flat();
                                return Intent::Redraw;
                            }
                        }

                        // 4. Activity leg rows: select + focus Activity.
                        let activity_rows = self.last_rects.activity_rows.clone();
                        for (rect, leg_idx) in &activity_rows {
                            if point_in_rect(col, row, *rect) {
                                self.focus = Focus::Activity;
                                self.activity_selected = *leg_idx;
                                return Intent::Redraw;
                            }
                        }

                        // 5. Activity header: toggle expand/collapse (first click fires).
                        let act = self.last_rects.activity;
                        if act.width > 0 && col >= act.x && col < act.x + act.width && row == act.y
                        {
                            self.activity_expanded = !self.activity_expanded;
                            return Intent::Redraw;
                        }

                        Intent::Redraw
                    }
                    _ => Intent::Redraw,
                }
            }

            Event::Resize(_, _) => Intent::Redraw,
            _ => Intent::Redraw,
        }
    }

    // -----------------------------------------------------------------------
    // Modal input routing
    // -----------------------------------------------------------------------

    fn handle_modal_input(&mut self, ev: Event) -> Intent {
        // Snapshot popup: Esc returns to the parent modal; all other keys are no-ops.
        // Handled here (before the clone below) so we can take ownership of `parent`.
        if matches!(&self.modal, Some(Modal::Snapshot { .. })) {
            if let Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            }) = ev
            {
                let parent_modal = if let Some(Modal::Snapshot { parent, .. }) = self.modal.take() {
                    parent.map(|b| *b)
                } else {
                    None
                };
                self.modal = parent_modal;
            }
            return Intent::Redraw;
        }

        // Context-paged Help: Esc/`?` close (restoring any parent modal); `←`/`→`
        // page through contexts. Handled before the clone so we can move `parent`
        // out on close.
        if matches!(&self.modal, Some(Modal::Help { .. })) {
            if let Event::Key(KeyEvent { code, .. }) = ev {
                match code {
                    KeyCode::Esc | KeyCode::Char('?') => {
                        let parent = if let Some(Modal::Help { parent, .. }) = self.modal.take() {
                            parent.map(|b| *b)
                        } else {
                            None
                        };
                        self.modal = parent;
                    }
                    KeyCode::Left => {
                        if let Some(Modal::Help { page, .. }) = &mut self.modal {
                            *page = page.prev();
                        }
                    }
                    KeyCode::Right => {
                        if let Some(Modal::Help { page, .. }) = &mut self.modal {
                            *page = page.next();
                        }
                    }
                    _ => {}
                }
            }
            return Intent::Redraw;
        }

        // `:about` splash — any key dismisses it.
        if matches!(&self.modal, Some(Modal::About)) {
            if let Event::Key(_) = ev {
                self.modal = None;
            }
            return Intent::Redraw;
        }

        let modal = match &self.modal {
            Some(m) => m.clone(),
            None => return Intent::Redraw,
        };

        match &modal {
            // Handled above (any key dismisses); arm kept for exhaustiveness.
            Modal::About => Intent::Redraw,
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
                        // Enter / d — pick THIS copy (the canonical single choose).
                        KeyCode::Enter | KeyCode::Char('d') => {
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
                        // `a` — fetch ALL preferred-format copies (one per format),
                        // each armed as its own download.
                        KeyCode::Char('a') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                let md5s = self.preferred_format_md5s(&fb.book);
                                if !md5s.is_empty() {
                                    self.modal = None;
                                    return Intent::RequestVariations {
                                        group_path: vec![fb.group_index],
                                        book_index: fb.book_index_in_group,
                                        md5s,
                                    };
                                }
                            }
                            Intent::Redraw
                        }
                        // `v` — show the focused candidate's metadata snapshot.
                        KeyCode::Char('v') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                if let Some(v) = fb.book.versions.get(sel) {
                                    let lines = build_variation_snapshot_lines(v);
                                    let parent = Modal::Picker {
                                        book_flat_index: flat_index,
                                        selected: sel,
                                    };
                                    self.modal = Some(Modal::Snapshot {
                                        title: format!(
                                            " {} \u{00b7} {} ",
                                            v.fmt,
                                            &v.md5[..8.min(v.md5.len())]
                                        ),
                                        lines,
                                        parent: Some(Box::new(parent)),
                                    });
                                }
                            }
                            Intent::Redraw
                        }
                        // `m` — none of these copies are correct → mark the book
                        // not-found and close the picker.
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
                        // `?` — open Help on the Picker page, restoring the picker on close.
                        KeyCode::Char('?') => {
                            self.modal = Some(Modal::Help {
                                page: HelpPage::Picker,
                                parent: Some(Box::new(modal.clone())),
                            });
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
                                    if sel >= max {
                                        // #65 arrow-cross: ↓ at bottom of Variations → History
                                        self.modal = Some(Modal::Detail {
                                            book_flat_index: flat_index,
                                            selected: sel,
                                            sub_focus: DetailSubFocus::History,
                                            history_selected: 0,
                                        });
                                    } else {
                                        self.modal = Some(Modal::Detail {
                                            book_flat_index: flat_index,
                                            selected: sel + 1,
                                            sub_focus: sf,
                                            history_selected: hist_sel,
                                        });
                                    }
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
                                    if hist_sel == 0 {
                                        // #65 arrow-cross: ↑ at top of History → Variations (bottom)
                                        let var_max = self
                                            .flat
                                            .get(flat_index)
                                            .map(|fb| fb.book.versions.len().saturating_sub(1))
                                            .unwrap_or(0);
                                        self.modal = Some(Modal::Detail {
                                            book_flat_index: flat_index,
                                            selected: var_max,
                                            sub_focus: DetailSubFocus::Variations,
                                            history_selected: 0,
                                        });
                                    } else {
                                        self.modal = Some(Modal::Detail {
                                            book_flat_index: flat_index,
                                            selected: sel,
                                            sub_focus: sf,
                                            history_selected: hist_sel - 1,
                                        });
                                    }
                                }
                            }
                            Intent::Redraw
                        }
                        KeyCode::Char('o') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                if let Some(path) = detail_target_path(fb, &sf, sel) {
                                    return Intent::OpenFile(path);
                                }
                            }
                            Intent::Redraw
                        }
                        KeyCode::Char('R') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                if let Some(path) = detail_target_path(fb, &sf, sel) {
                                    return Intent::RevealFile(path);
                                }
                            }
                            Intent::Redraw
                        }
                        KeyCode::Char('r') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                // Per-copy retry: when a specific variation is
                                // focused, re-arm just THAT copy (request_variation
                                // resets a Failed/Cancelled/Done job to Pending
                                // without clearing the other candidates), so a
                                // failed copy re-downloads in place instead of
                                // wiping every variation and re-querying the whole
                                // book. Whole-book re-query is reserved for History
                                // focus or a book with no copies discovered yet.
                                if sf == DetailSubFocus::Variations {
                                    if let Some(v) = fb.book.versions.get(sel) {
                                        let md5 = v.md5.clone();
                                        self.status_msg = Some(format!(
                                            "Retrying: {} ({})",
                                            fb.book.title, v.fmt
                                        ));
                                        return Intent::RequestVariations {
                                            group_path: vec![fb.group_index],
                                            book_index: fb.book_index_in_group,
                                            md5s: vec![md5],
                                        };
                                    }
                                }
                                return Intent::Retry {
                                    group_path: vec![fb.group_index],
                                    book_index: fb.book_index_in_group,
                                };
                            }
                            Intent::Redraw
                        }
                        // Download the currently-focused variation (its copy).
                        // Only meaningful while the Variations list is focused.
                        KeyCode::Char('d') => {
                            if sf == DetailSubFocus::Variations {
                                if let Some(fb) = self.flat.get(flat_index) {
                                    if let Some(v) = fb.book.versions.get(sel) {
                                        let md5 = v.md5.clone();
                                        let intent = Intent::Select {
                                            group_path: vec![fb.group_index],
                                            book_index: fb.book_index_in_group,
                                            md5,
                                        };
                                        // Immediate confirmation: the download takes
                                        // a few seconds to resolve before it visibly
                                        // connects, so without this the user sees
                                        // "nothing happen" and assumes `d` is broken.
                                        self.status_msg = Some(format!(
                                            "Queued download: {} ({})",
                                            fb.book.title, v.fmt
                                        ));
                                        // Stay in the detail view so the user can
                                        // watch this variation go queued→downloading
                                        // →done in place; the status line renders in
                                        // the bottom bar, which the modal doesn't cover.
                                        return intent;
                                    }
                                }
                            }
                            Intent::Redraw
                        }
                        // #49 manual re-query: open inline search input.
                        KeyCode::Char('s') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                let title = fb.book.title.clone();
                                let caret = title.chars().count();
                                self.modal = Some(Modal::ReQuery {
                                    book_flat_index: flat_index,
                                    buf: title,
                                    caret,
                                });
                            }
                            Intent::Redraw
                        }
                        // #50 edit title/author.
                        KeyCode::Char('e') => {
                            if let Some(fb) = self.flat.get(flat_index) {
                                let title_buf = fb.book.title.clone();
                                let author_buf = fb.book.author.clone();
                                let caret = title_buf.chars().count();
                                self.modal = Some(Modal::EditBook {
                                    book_flat_index: flat_index,
                                    title_buf,
                                    author_buf,
                                    field: EditBookField::Title,
                                    caret,
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
                        // #53 download-series — ONLY available in the detail (book)
                        // context. Reuses the `:download-series` handler, which acts
                        // on the list-selected book. Keep the detail modal open so the
                        // user stays in context; the command's "Added N book(s)" status
                        // renders in the bottom bar, which the centered modal doesn't cover.
                        KeyCode::Char('S') => Intent::Command("download-series".into()),
                        // #70 universal Enter: open a snapshot popup for the focused row.
                        KeyCode::Enter => {
                            let parent_detail = Modal::Detail {
                                book_flat_index: flat_index,
                                selected: sel,
                                sub_focus: sf.clone(),
                                history_selected: hist_sel,
                            };
                            if sf == DetailSubFocus::Variations {
                                if let Some(fb) = self.flat.get(flat_index) {
                                    if let Some(v) = fb.book.versions.get(sel) {
                                        let lines = build_variation_snapshot_lines(v);
                                        self.modal = Some(Modal::Snapshot {
                                            title: format!(
                                                " {} \u{00b7} {} ",
                                                v.fmt,
                                                &v.md5[..8.min(v.md5.len())]
                                            ),
                                            lines,
                                            parent: Some(Box::new(parent_detail)),
                                        });
                                    }
                                }
                            } else {
                                // DetailSubFocus::History
                                if let Some(fb) = self.flat.get(flat_index) {
                                    let n = fb.book.history.len();
                                    if n > 0 {
                                        // history is displayed chronologically (oldest-first),
                                        // so the selected row maps directly to the real index.
                                        let real_idx = hist_sel.min(n.saturating_sub(1));
                                        if let Some(ev) = fb.book.history.get(real_idx) {
                                            let lines = build_history_snapshot_lines(ev);
                                            self.modal = Some(Modal::Snapshot {
                                                title: format!(" {} ", ev.kind),
                                                lines,
                                                parent: Some(Box::new(parent_detail)),
                                            });
                                        }
                                    }
                                }
                            }
                            Intent::Redraw
                        }
                        // `?` — open Help on the Detail page, restoring detail on close.
                        KeyCode::Char('?') => {
                            self.modal = Some(Modal::Help {
                                page: HelpPage::Detail,
                                parent: Some(Box::new(modal.clone())),
                            });
                            Intent::Redraw
                        }
                        _ => Intent::Redraw,
                    },
                    // Mouse in the detail modal: click a row to select + focus its
                    // sub-list; wheel scrolls the sub-list UNDER THE CURSOR and
                    // hover-selects the row beneath it (mirrors the main-list model).
                    Event::Mouse(me) => {
                        let col = me.column;
                        let row = me.row;
                        let var_max = self
                            .flat
                            .get(flat_index)
                            .map(|fb| fb.book.versions.len().saturating_sub(1))
                            .unwrap_or(0);
                        let hist_max = self
                            .flat
                            .get(flat_index)
                            .map(|fb| fb.book.history.len().saturating_sub(1))
                            .unwrap_or(0);
                        let var_rows = self.last_rects.detail_var_rows.clone();
                        let hist_rows = self.last_rects.detail_hist_rows.clone();
                        let var_area = self.last_rects.detail_var_area;
                        let hist_area = self.last_rects.detail_hist_area;
                        match me.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                for (rect, idx) in &var_rows {
                                    if point_in_rect(col, row, *rect) {
                                        self.modal = Some(Modal::Detail {
                                            book_flat_index: flat_index,
                                            selected: (*idx).min(var_max),
                                            sub_focus: DetailSubFocus::Variations,
                                            history_selected: hist_sel,
                                        });
                                        return Intent::Redraw;
                                    }
                                }
                                for (rect, idx) in &hist_rows {
                                    if point_in_rect(col, row, *rect) {
                                        self.modal = Some(Modal::Detail {
                                            book_flat_index: flat_index,
                                            selected: sel,
                                            sub_focus: DetailSubFocus::History,
                                            history_selected: (*idx).min(hist_max),
                                        });
                                        return Intent::Redraw;
                                    }
                                }
                                Intent::Redraw
                            }
                            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                                // The wheel moves selection by ±1 within whichever
                                // sub-list the cursor is over. It does NOT hover-
                                // select the row beneath the pointer — that snap was
                                // removed for the main book list, and the detail
                                // modal matches it (task 4).
                                let down = matches!(me.kind, MouseEventKind::ScrollDown);
                                if point_in_rect(col, row, var_area) {
                                    let new_sel = if down {
                                        (sel + 1).min(var_max)
                                    } else {
                                        sel.saturating_sub(1)
                                    };
                                    self.modal = Some(Modal::Detail {
                                        book_flat_index: flat_index,
                                        selected: new_sel,
                                        sub_focus: DetailSubFocus::Variations,
                                        history_selected: hist_sel,
                                    });
                                } else if point_in_rect(col, row, hist_area) {
                                    let new_hist = if down {
                                        (hist_sel + 1).min(hist_max)
                                    } else {
                                        hist_sel.saturating_sub(1)
                                    };
                                    self.modal = Some(Modal::Detail {
                                        book_flat_index: flat_index,
                                        selected: sel,
                                        sub_focus: DetailSubFocus::History,
                                        history_selected: new_hist,
                                    });
                                }
                                Intent::Redraw
                            }
                            _ => Intent::Redraw,
                        }
                    }
                    _ => Intent::Redraw,
                }
            }

            Modal::ReQuery {
                book_flat_index,
                buf,
                caret,
            } => {
                let flat_index = *book_flat_index;
                let mut buf = buf.clone();
                let mut caret = *caret;
                let mut reopen = true;
                let ret = match ev {
                    Event::Key(KeyEvent { code, .. }) => match code {
                        KeyCode::Esc => {
                            reopen = false;
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
                            reopen = false;
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
                        KeyCode::Left => {
                            caret = caret.saturating_sub(1);
                            Intent::Redraw
                        }
                        KeyCode::Right => {
                            caret = (caret + 1).min(buf.chars().count());
                            Intent::Redraw
                        }
                        KeyCode::Home => {
                            caret = 0;
                            Intent::Redraw
                        }
                        KeyCode::End => {
                            caret = buf.chars().count();
                            Intent::Redraw
                        }
                        KeyCode::Backspace => {
                            if caret > 0 {
                                let byte = byte_at_char(&buf, caret - 1);
                                buf.remove(byte);
                                caret -= 1;
                            }
                            Intent::Redraw
                        }
                        KeyCode::Char(c) => {
                            let n = buf.chars().count();
                            let byte = byte_at_char(&buf, caret.min(n));
                            buf.insert(byte, c);
                            caret = caret.min(n) + 1;
                            Intent::Redraw
                        }
                        _ => Intent::Redraw,
                    },
                    _ => Intent::Redraw,
                };
                if reopen {
                    self.modal = Some(Modal::ReQuery {
                        book_flat_index: flat_index,
                        buf,
                        caret,
                    });
                }
                ret
            }

            Modal::EditBook {
                book_flat_index,
                title_buf,
                author_buf,
                field,
                caret,
            } => {
                let flat_index = *book_flat_index;
                let mut tbuf = title_buf.clone();
                let mut abuf = author_buf.clone();
                let mut fld = field.clone();
                let mut caret = *caret;
                // Length (char count) of whichever field is active.
                let active_len = |t: &str, a: &str, f: &EditBookField| match f {
                    EditBookField::Title => t.chars().count(),
                    EditBookField::Author => a.chars().count(),
                };
                let mut reopen = true;
                let ret = match ev {
                    Event::Key(KeyEvent { code, .. }) => match code {
                        KeyCode::Esc => {
                            reopen = false;
                            self.modal = Some(Modal::Detail {
                                book_flat_index: flat_index,
                                selected: 0,
                                sub_focus: DetailSubFocus::Variations,
                                history_selected: 0,
                            });
                            Intent::Redraw
                        }
                        KeyCode::Tab => {
                            fld = match fld {
                                EditBookField::Title => EditBookField::Author,
                                EditBookField::Author => EditBookField::Title,
                            };
                            caret = active_len(&tbuf, &abuf, &fld); // caret to end of new field
                            Intent::Redraw
                        }
                        KeyCode::Enter => {
                            reopen = false;
                            if let Some(fb) = self.flat.get(flat_index) {
                                let authors: Vec<String> = abuf
                                    .split(',')
                                    .map(|s| s.trim().to_string())
                                    .filter(|s| !s.is_empty())
                                    .collect();
                                let intent = Intent::EditBook {
                                    group_path: vec![fb.group_index],
                                    book_index: fb.book_index_in_group,
                                    title: tbuf.clone(),
                                    authors,
                                };
                                self.modal = None;
                                return intent;
                            }
                            self.modal = None;
                            Intent::Redraw
                        }
                        KeyCode::Left => {
                            caret = caret.saturating_sub(1);
                            Intent::Redraw
                        }
                        KeyCode::Right => {
                            caret = (caret + 1).min(active_len(&tbuf, &abuf, &fld));
                            Intent::Redraw
                        }
                        KeyCode::Home => {
                            caret = 0;
                            Intent::Redraw
                        }
                        KeyCode::End => {
                            caret = active_len(&tbuf, &abuf, &fld);
                            Intent::Redraw
                        }
                        KeyCode::Backspace => {
                            if caret > 0 {
                                let buf = match fld {
                                    EditBookField::Title => &mut tbuf,
                                    EditBookField::Author => &mut abuf,
                                };
                                let byte = byte_at_char(buf, caret - 1);
                                buf.remove(byte);
                                caret -= 1;
                            }
                            Intent::Redraw
                        }
                        KeyCode::Char(c) => {
                            let buf = match fld {
                                EditBookField::Title => &mut tbuf,
                                EditBookField::Author => &mut abuf,
                            };
                            let n = buf.chars().count();
                            let byte = byte_at_char(buf, caret.min(n));
                            buf.insert(byte, c);
                            caret = caret.min(n) + 1;
                            Intent::Redraw
                        }
                        _ => Intent::Redraw,
                    },
                    _ => Intent::Redraw,
                };
                if reopen {
                    self.modal = Some(Modal::EditBook {
                        book_flat_index: flat_index,
                        title_buf: tbuf,
                        author_buf: abuf,
                        field: fld,
                        caret,
                    });
                }
                ret
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

            Modal::Help { .. } => Intent::Redraw, // handled above (before the clone)

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

            // Snapshot is handled via the early-return at the top of this function
            // (needs ownership of `parent`).  This arm is unreachable but required
            // for exhaustiveness.
            Modal::Snapshot { .. } => Intent::Redraw,
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
                // 's' → save & close (modal closed here; draft consumed by dispatcher)
                KeyCode::Char('s') => {
                    self.modal = None;
                    // Keep settings_draft alive for the dispatcher to read.
                    Intent::SaveSettings
                }
                // Esc | 'q' | Ctrl-G → discard & close
                KeyCode::Esc | KeyCode::Char('q') => {
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
                // ── Maintenance hot keys (reuse the `:` command handlers) ──
                // `r` refresh mirrors, `o` reorganize (move-preview + y/n),
                // `c` cleanup leftover `.part` files (→ Trash).
                KeyCode::Char('r') => Intent::Command("refresh-mirrors".into()),
                KeyCode::Char('o') => Intent::Command("reorganize".into()),
                KeyCode::Char('c') => Intent::Command("cleanup".into()),
                // `?` — open Help on the Settings page; restore Settings on close.
                // The draft lives in `self.settings_draft`, untouched by Help.
                KeyCode::Char('?') => {
                    self.modal = Some(Modal::Help {
                        page: HelpPage::Settings,
                        parent: Some(Box::new(Modal::Settings)),
                    });
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
            6 => d.seq_per_group = !d.seq_per_group,
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
                6 => d.seq_per_group = !d.seq_per_group,
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
            5 => {
                if !v.is_empty() {
                    d.naming_template = v.to_string();
                }
            }
            7 => d.out_dir = v.to_string(),
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

    /// Every rendered row in the main book table, in render order. A book with
    /// two or more armed variations contributes a `Book` row followed by one
    /// `Variation` sub-row per additional armed copy. MUST mirror
    /// `render_book_table`'s stacking rule (both derive from `armed_variations`).
    pub fn rendered_rows(&self) -> Vec<RowRef> {
        let mut rows = Vec::with_capacity(self.flat.len());
        for (i, fb) in self.flat.iter().enumerate() {
            rows.push(RowRef::Book(i));
            let armed = armed_variations(&fb.book);
            if armed.len() >= 2 {
                for v in &armed[1..] {
                    rows.push(RowRef::Variation(i, v.md5.clone()));
                }
            }
        }
        rows
    }

    /// `true` when `r` is the row currently focused (book row vs. variation sub-row).
    fn row_is_focused(&self, r: &RowRef) -> bool {
        match (r, self.selected_var.as_ref()) {
            (RowRef::Book(i), None) => *i == self.selected,
            (RowRef::Variation(i, md5), Some(sel)) => *i == self.selected && md5 == sel,
            _ => false,
        }
    }

    /// Index of the focused row within `rows` (0 if not found).
    fn current_row_pos(&self, rows: &[RowRef]) -> usize {
        rows.iter()
            .position(|r| self.row_is_focused(r))
            .unwrap_or(0)
    }

    /// Move focus onto a specific rendered row, updating `selected`/`selected_var`.
    pub fn set_focus_row(&mut self, r: &RowRef) {
        match r {
            RowRef::Book(i) => {
                self.selected = *i;
                self.selected_var = None;
            }
            RowRef::Variation(i, md5) => {
                self.selected = *i;
                self.selected_var = Some(md5.clone());
            }
        }
    }

    /// `true` when the focused row is the FIRST rendered row.
    fn at_first_row(&self) -> bool {
        let rows = self.rendered_rows();
        rows.is_empty() || self.current_row_pos(&rows) == 0
    }

    /// `true` when the focused row is the LAST rendered row.
    fn at_last_row(&self) -> bool {
        let rows = self.rendered_rows();
        rows.is_empty() || self.current_row_pos(&rows) == rows.len() - 1
    }

    /// Detail-modal variation index for the focused variation sub-row (position
    /// of `selected_var`'s md5 within `book.versions`); 0 when a book row is
    /// focused or the variation can't be located.
    pub fn detail_variation_index(&self, flat_index: usize) -> usize {
        if let (Some(md5), Some(fb)) = (self.selected_var.as_ref(), self.flat.get(flat_index)) {
            if flat_index == self.selected {
                return fb
                    .book
                    .versions
                    .iter()
                    .position(|v| &v.md5 == md5)
                    .unwrap_or(0);
            }
        }
        0
    }

    /// Aggregate "All" view remap: given an aggregate group index, return the
    /// (owning list id, original group index) it came from. Used by the dispatch
    /// layer to route an action taken in All view to the OWNING list's
    /// orchestrator. `None` when not in aggregate view or the index is unknown.
    pub fn aggregate_origin(&self, agg_group: usize) -> Option<(String, usize)> {
        self.aggregate_origins.get(agg_group).cloned()
    }

    /// Drop a stale variation focus: clear `selected_var` when the selected book
    /// no longer renders that md5 as one of its armed sub-rows.
    fn validate_selected_var(&mut self) {
        let Some(md5) = self.selected_var.clone() else {
            return;
        };
        let still_valid = self
            .flat
            .get(self.selected)
            .map(|fb| {
                let armed = armed_variations(&fb.book);
                // Only ADDITIONAL armed copies (index >= 1) render as sub-rows.
                armed.len() >= 2 && armed[1..].iter().any(|v| v.md5 == md5)
            })
            .unwrap_or(false);
        if !still_valid {
            self.selected_var = None;
        }
    }

    /// True when the currently displayed list is the mutable **Manual** list
    /// (per-book add/remove affordances apply). False while the aggregate "All"
    /// view is active or when the active list is an immutable imported list.
    pub fn active_list_is_manual(&self) -> bool {
        if self.all_active {
            return false;
        }
        self.all_lists
            .get(self.active_list_idx)
            .map(|l| l.is_manual)
            .unwrap_or(false)
    }

    /// Switch to the PREVIOUS reading list in the global rotation (the `[`
    /// direction). The rotation includes the aggregate "All" stop as one extra
    /// position before the first real list: All → list0 → … → All. Returns the
    /// resulting [`Intent`] (a `SwitchList`, or `Redraw` when no lists exist).
    /// Shared by the `[` shortcut and `←` on the focused list strip.
    pub fn cycle_list_prev(&mut self) -> Intent {
        let n = self.all_lists.len();
        if n == 0 {
            return Intent::Redraw;
        }
        if self.all_active {
            self.all_active = false;
            self.active_list_idx = n - 1;
            Intent::SwitchList {
                id: self.all_lists[n - 1].id.clone(),
            }
        } else if self.active_list_idx == 0 {
            self.all_active = true;
            Intent::SwitchList {
                id: ALL_LIST_ID.to_string(),
            }
        } else {
            self.active_list_idx -= 1;
            Intent::SwitchList {
                id: self.all_lists[self.active_list_idx].id.clone(),
            }
        }
    }

    /// Switch to the NEXT reading list in the global rotation (the `]`
    /// direction). Mirrors [`cycle_list_prev`]. Shared by `]` and `→` on the
    /// focused list strip.
    pub fn cycle_list_next(&mut self) -> Intent {
        let n = self.all_lists.len();
        if n == 0 {
            return Intent::Redraw;
        }
        if self.all_active {
            self.all_active = false;
            self.active_list_idx = 0;
            Intent::SwitchList {
                id: self.all_lists[0].id.clone(),
            }
        } else if self.active_list_idx + 1 >= n {
            self.all_active = true;
            Intent::SwitchList {
                id: ALL_LIST_ID.to_string(),
            }
        } else {
            self.active_list_idx += 1;
            Intent::SwitchList {
                id: self.all_lists[self.active_list_idx].id.clone(),
            }
        }
    }

    /// `↑` within the Header pane: filter chips → list strip → STOP. From the
    /// chips the focus climbs to the list strip; from the list strip it is the
    /// top of everything, so this is a no-op (focus stays put).
    fn header_up(&mut self) {
        match self.header_row {
            HeaderRow::FilterChips => self.header_row = HeaderRow::ListStrip,
            HeaderRow::ListStrip => {} // top of everything — STOP.
        }
    }

    /// `↓` within the Header pane: list strip → filter chips → top of the book
    /// list. From the chips the focus drops into the List pane at its first row.
    fn header_down(&mut self) {
        match self.header_row {
            HeaderRow::ListStrip => self.header_row = HeaderRow::FilterChips,
            HeaderRow::FilterChips => {
                self.focus = Focus::List;
                self.selected = 0;
                self.selected_var = None;
            }
        }
    }

    fn move_selection(&mut self, delta: usize) {
        let rows = self.rendered_rows();
        if rows.is_empty() {
            return;
        }
        let pos = self.current_row_pos(&rows);
        let new = (pos + delta).min(rows.len() - 1);
        self.set_focus_row(&rows[new]);
    }

    fn move_selection_up(&mut self) {
        let rows = self.rendered_rows();
        if rows.is_empty() {
            return;
        }
        let pos = self.current_row_pos(&rows);
        let new = pos.saturating_sub(1);
        self.set_focus_row(&rows[new]);
    }

    /// Move down by one viewport height (Shift-Down / Shift-J).
    fn move_selection_page_down(&mut self) {
        let rows = self.rendered_rows();
        if rows.is_empty() {
            return;
        }
        // Use the book-table height from the last render as the page size.
        let page = self.last_rects.book_table.height.max(1) as usize;
        let pos = self.current_row_pos(&rows);
        let new = (pos + page).min(rows.len() - 1);
        self.set_focus_row(&rows[new]);
    }

    /// Move up by one viewport height (Shift-Up / Shift-K).
    fn move_selection_page_up(&mut self) {
        let rows = self.rendered_rows();
        if rows.is_empty() {
            return;
        }
        let page = self.last_rects.book_table.height.max(1) as usize;
        let pos = self.current_row_pos(&rows);
        let new = pos.saturating_sub(page);
        self.set_focus_row(&rows[new]);
    }

    /// Number of in-flight transfer rows the Activity pane can scroll through.
    /// Counts BOOKS that have at least one downloading version (matching the
    /// one-row-per-book display in `render_activity`), with a fallback to the
    /// live-telemetry transfer count.
    /// True when the Activity pane currently has at least one download leg
    /// (an in-flight transfer or a downloading variation). Drives whether the
    /// hint bar advertises the per-leg `p`/`c`/`r` keys.
    pub fn activity_has_legs(&self) -> bool {
        self.activity_row_count() > 0
    }

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
        // Bound by the full windowable content length (set by render) so the
        // queued list at the bottom of the pane is reachable when it overflows;
        // fall back to the leg count before the first render populates it.
        let n = self.activity_content_len.max(self.activity_row_count());
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

    /// The Activity-pane download legs in the SAME order they render: ViewModel
    /// legs (`state == "downloading"`) grouped by host (a `BTreeMap`, so hosts are
    /// alphabetical; legs within a host keep `flat` order). `activity_selected`
    /// indexes into this list. Empty when there are no live ViewModel legs — the
    /// caller then falls back to the telemetry-only path (sorted by md5).
    ///
    /// Single source of truth for leg ordering, shared by `focused_transfer_md5`
    /// (which `r`/`p`/`c` act on) and `build_leg_snapshot_modal` (the `Enter`
    /// snapshot) so the focused leg and the acted-on leg can never diverge.
    fn activity_legs(&self) -> Vec<(&FlatBook, String, &ViewVariation)> {
        let mut host_groups: std::collections::BTreeMap<String, Vec<(&FlatBook, &ViewVariation)>> =
            std::collections::BTreeMap::new();
        for fb in &self.flat {
            for v in &fb.book.versions {
                if v.state == "downloading" {
                    let host = v.host.as_deref().unwrap_or("unknown").to_string();
                    host_groups.entry(host).or_default().push((fb, v));
                }
            }
        }
        host_groups
            .into_iter()
            .flat_map(|(host, legs)| legs.into_iter().map(move |(fb, v)| (fb, host.clone(), v)))
            .collect()
    }

    /// Return the md5 of the transfer leg currently focused in the Activity pane.
    ///
    /// Resolves the leg via [`Self::activity_legs`] (host-grouped render order) so
    /// `activity_selected` maps to the leg the user actually sees highlighted;
    /// falls back to md5-sorted telemetry keys when no live ViewModel legs exist.
    pub fn focused_transfer_md5(&self) -> Option<String> {
        let legs = self.activity_legs();
        if !legs.is_empty() {
            return legs
                .get(self.activity_selected)
                .map(|(_, _, v)| v.md5.clone());
        }
        // Telemetry-only fallback: sorted by md5 (matches build_leg_snapshot_modal).
        let mut keys: Vec<&String> = self.transfers.keys().collect();
        keys.sort();
        keys.get(self.activity_selected).map(|k| (*k).clone())
    }

    /// Build a [`Modal::Snapshot`] for the Activity-pane leg at `activity_selected`.
    ///
    /// Uses the same [`Self::activity_legs`] ordering as `focused_transfer_md5`, so
    /// the snapshot and the `r`/`p`/`c` actions always target the same leg.
    /// Returns `None` when there are no active legs or the selection is out of range.
    fn build_leg_snapshot_modal(&self) -> Option<Modal> {
        let legs = self.activity_legs();
        let target = self.activity_selected;

        if !legs.is_empty() {
            let (fb, host, v) = legs.get(target)?;
            let telemetry = self.transfers.get(&v.md5);
            let lines = build_leg_snapshot_lines(&fb.book.title, host, v, telemetry);
            return Some(Modal::Snapshot {
                title: format!(" {} \u{00b7} {} ", v.fmt, &v.md5[..8.min(v.md5.len())]),
                lines,
                parent: None,
            });
        }

        // Telemetry-only path: sorted by md5 (same order as focused_transfer_md5).
        let mut keys: Vec<&String> = self.transfers.keys().collect();
        keys.sort();
        if let Some(md5) = keys.get(target) {
            let t = &self.transfers[*md5];
            let lines = build_transfer_snapshot_lines(t);
            return Some(Modal::Snapshot {
                title: format!(" leg \u{00b7} {} ", &md5[..8.min(md5.len())]),
                lines,
                parent: None,
            });
        }

        None
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
            let arg = &trimmed[space_pos + 1..];
            if cmd == "import" {
                return complete_path(arg);
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
    /// The md5s of the top-ranked candidate for each format in the active list's
    /// format preference, in preference order (one copy per preferred format).
    /// Falls back to the single best candidate when no version matches a
    /// preferred format. Used by the "fetch all preferred formats" actions.
    pub fn preferred_format_md5s(&self, book: &ViewBook) -> Vec<String> {
        let prefs: &[String] = self
            .view
            .as_ref()
            .map(|v| v.format_pref.as_slice())
            .unwrap_or(&[]);
        let mut out: Vec<String> = Vec::new();
        for pref in prefs {
            if let Some(v) = book
                .versions
                .iter()
                .find(|v| v.fmt.eq_ignore_ascii_case(pref))
            {
                if !out.contains(&v.md5) {
                    out.push(v.md5.clone());
                }
            }
        }
        // No preferred format present → fall back to the single best candidate.
        if out.is_empty() {
            if let Some(v) = book.versions.first() {
                out.push(v.md5.clone());
            }
        }
        out
    }

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
        // Drop variation focus if the focused sub-row no longer exists.
        self.validate_selected_var();
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
            StatusFilter::Queued => {
                book.versions.iter().any(|v| v.state == "queued")
                    && !book.versions.iter().any(|v| v.state == "downloading")
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
                // Queued: a Pending/queued variation with no active download.
                if book.versions.iter().any(|v| v.state == "queued") && !in_progress {
                    c.queued += 1;
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
    pub queued: usize,
    pub in_progress: usize,
    pub done: usize,
}

// ---------------------------------------------------------------------------
// Snapshot content builders (called from on_input / handle_modal_input)
// ---------------------------------------------------------------------------

/// Build the `(label, value)` rows for a **variation snapshot** popup.
pub fn build_variation_snapshot_lines(v: &ViewVariation) -> Vec<(String, String)> {
    let dash = "\u{2014}".to_string();
    let mut lines = vec![
        ("Title".to_string(), v.title.clone()),
        (
            "Author".to_string(),
            if v.author.is_empty() {
                dash.clone()
            } else {
                v.author.clone()
            },
        ),
        ("Format".to_string(), v.fmt.clone()),
        (
            "Size".to_string(),
            if v.size > 0 {
                format!("{} MB", v.size)
            } else {
                dash.clone()
            },
        ),
        (
            "Year".to_string(),
            v.year
                .map(|y| y.to_string())
                .unwrap_or_else(|| dash.clone()),
        ),
        (
            "Pages".to_string(),
            v.pages
                .map(|p| p.to_string())
                .unwrap_or_else(|| dash.clone()),
        ),
        (
            "Publisher".to_string(),
            if v.publisher.is_empty() {
                dash.clone()
            } else {
                v.publisher.clone()
            },
        ),
        (
            "Language".to_string(),
            if v.language.is_empty() {
                dash.clone()
            } else {
                v.language.clone()
            },
        ),
        (
            "Source host".to_string(),
            v.host.as_deref().unwrap_or("\u{2014}").to_string(),
        ),
        ("MD5".to_string(), v.md5.clone()),
        ("Match score".to_string(), format!("{:.2}", v.score)),
        ("State".to_string(), v.state.clone()),
    ];
    if v.state == "done" {
        if let Some(path) = &v.output_path {
            lines.push(("Output path".to_string(), path.clone()));
        }
    }
    lines
}

/// Build the `(label, value)` rows for a **history-event snapshot** popup.
pub fn build_history_snapshot_lines(ev: &ViewEvent) -> Vec<(String, String)> {
    let secs = ev.at_ms / 1000;
    let time_str = format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    );
    let mut lines = vec![
        ("Timestamp".to_string(), time_str),
        ("Kind".to_string(), ev.kind.clone()),
    ];
    // Wrap detail at ~46 chars so it fits inside the popup's value column.
    let wrapped = snap_wrap_text(&ev.detail, 46);
    for (i, chunk) in wrapped.iter().enumerate() {
        let label = if i == 0 {
            "Detail".to_string()
        } else {
            String::new()
        };
        lines.push((label, chunk.clone()));
    }
    lines
}

/// Build `(label, value)` rows for a **leg snapshot** (ViewModel data + optional
/// live telemetry).
/// The output path list-view `o` (open) / `R` (reveal) should target: the
/// SELECTED copy's file when an `↳ alt. copy` sub-row is focused (`selected_var`
/// holds its md5), else the first downloaded copy for a whole-book selection.
fn list_target_path(fb: &FlatBook, selected_var: Option<&str>) -> Option<String> {
    if let Some(md5) = selected_var {
        if let Some(p) = fb
            .book
            .versions
            .iter()
            .find(|v| v.md5 == md5)
            .and_then(|v| v.output_path.clone())
        {
            return Some(p);
        }
    }
    fb.book.versions.iter().find_map(|v| v.output_path.clone())
}

/// The output path `o` (open) / `R` (reveal) should target in the Detail modal:
/// the FOCUSED variation's own file when a specific copy is highlighted
/// (`Variations` sub-focus), else the first downloaded copy — used for `History`
/// focus, or when the focused copy isn't downloaded yet.
fn detail_target_path(fb: &FlatBook, sf: &DetailSubFocus, sel: usize) -> Option<String> {
    if matches!(sf, DetailSubFocus::Variations) {
        if let Some(p) = fb
            .book
            .versions
            .get(sel)
            .and_then(|v| v.output_path.clone())
        {
            return Some(p);
        }
    }
    fb.book.versions.iter().find_map(|v| v.output_path.clone())
}

fn build_leg_snapshot_lines(
    book_title: &str,
    host: &str,
    v: &ViewVariation,
    telemetry: Option<&ActiveTransfer>,
) -> Vec<(String, String)> {
    let mut lines = vec![
        ("Book".to_string(), book_title.to_string()),
        ("Host".to_string(), host.to_string()),
        ("MD5".to_string(), v.md5.clone()),
        ("Format".to_string(), v.fmt.clone()),
        ("State".to_string(), v.state.clone()),
        ("Progress".to_string(), format!("{}%", v.progress)),
    ];
    if let Some(t) = telemetry {
        let bytes_str = match (t.bytes_done, t.total_bytes) {
            (done, Some(total)) => {
                format!("{} / {}", snap_fmt_bytes(done), snap_fmt_bytes(total))
            }
            (done, None) => snap_fmt_bytes(done),
        };
        lines.push(("Bytes".to_string(), bytes_str));
        if let Some(speed) = t.speed_bps {
            lines.push(("Speed".to_string(), snap_fmt_speed(speed)));
        }
        if let Some(eta) = t.eta_secs {
            lines.push(("ETA".to_string(), format!("{}s", eta)));
        }
    } else {
        if let Some(db) = v.downloaded_bytes {
            let bytes_str = match v.total_bytes {
                Some(total) => format!("{} / {}", snap_fmt_bytes(db), snap_fmt_bytes(total)),
                None => snap_fmt_bytes(db),
            };
            lines.push(("Bytes".to_string(), bytes_str));
        }
        if let Some(eta) = v.eta_secs {
            lines.push(("ETA".to_string(), format!("{}s", eta)));
        }
    }
    if let Some(err) = &v.last_error {
        lines.push(("Error".to_string(), err.clone()));
    }
    lines
}

/// Build `(label, value)` rows for a **leg snapshot** using telemetry-only data
/// (when the ViewModel has no downloading versions).
fn build_transfer_snapshot_lines(t: &ActiveTransfer) -> Vec<(String, String)> {
    let mut lines = vec![
        ("Book".to_string(), t.title.clone()),
        ("Host".to_string(), t.host.clone()),
        ("MD5".to_string(), t.md5.clone()),
        ("State".to_string(), "downloading".to_string()),
    ];
    let bytes_str = match (t.bytes_done, t.total_bytes) {
        (done, Some(total)) => format!("{} / {}", snap_fmt_bytes(done), snap_fmt_bytes(total)),
        (done, None) => snap_fmt_bytes(done),
    };
    lines.push(("Bytes".to_string(), bytes_str));
    if let Some(speed) = t.speed_bps {
        lines.push(("Speed".to_string(), snap_fmt_speed(speed)));
    }
    if let Some(eta) = t.eta_secs {
        lines.push(("ETA".to_string(), format!("{}s", eta)));
    }
    lines
}

/// Word-wrap `text` to at most `width` chars per line.
fn snap_wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn snap_fmt_bytes(b: u64) -> String {
    if b >= 1_000_000 {
        format!("{:.1} MB", b as f64 / 1_000_000.0)
    } else if b >= 1_000 {
        format!("{} KB", b / 1_000)
    } else {
        format!("{} B", b)
    }
}

fn snap_fmt_speed(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.1} MB/s", bps as f64 / 1_000_000.0)
    } else if bps >= 1_000 {
        format!("{} KB/s", bps / 1_000)
    } else {
        format!("{} B/s", bps)
    }
}
