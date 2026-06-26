//! Pure action type returned by [`crate::app::AppState::on_input`].
//!
//! `Intent` is the bridge between raw terminal input and the rest of the app.
//! Every variant either drives the engine, changes UI state, or terminates the
//! session — nothing else. Keeping it as a plain enum means `on_input` is
//! trivially testable without any I/O.

/// What the event loop should do after `on_input` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// No-op redraw: navigation, filter change, focus change, or anything else
    /// that is pure UI state (already applied inside `on_input`).
    Redraw,

    /// Select a specific candidate variation for download.
    Select {
        group_path: Vec<usize>,
        book_index: usize,
        md5: String,
    },

    /// Re-queue a failed/not-found book.
    Retry {
        group_path: Vec<usize>,
        book_index: usize,
    },

    /// A `:command line` was entered; the event loop calls `app.run_command`.
    Command(String),

    /// Terminate the event loop and restore the terminal.
    Quit,

    /// Open the book-detail modal for the given flat index.
    OpenDetail { flat_index: usize },

    /// Open the variation-picker modal for the given flat index.
    OpenPicker { flat_index: usize },

    /// Open the help screen.
    OpenHelp,

    /// Pause a downloading variation.
    Pause {
        group_path: Vec<usize>,
        book_index: usize,
    },

    /// Cancel a downloading variation.
    Cancel {
        group_path: Vec<usize>,
        book_index: usize,
    },

    /// Open a file with the system default application.
    OpenFile(String),

    /// Reveal a file in Finder (macOS) / file manager.
    RevealFile(String),

    /// Switch the active reading list (emitted by ←/→ in the list strip).
    SwitchList { id: String },

    /// Persist the staged settings draft to the engine and app config.
    /// The draft is still in `AppState::settings_draft`; the dispatcher reads
    /// it, calls the engine, then clears it.
    SaveSettings,

    /// Close the Settings modal without persisting any changes.
    DiscardSettings,

    /// The user confirmed the delete-list modal; delete list with this id.
    ConfirmDelete { id: String },

    /// Re-query a single book with a user-supplied corrected title.
    ReQueryBook {
        group_path: Vec<usize>,
        book_index: usize,
        /// The corrected title to search for.
        title: String,
    },

    /// Edit a book's title and/or authors and re-queue it for discovery.
    EditBook {
        group_path: Vec<usize>,
        book_index: usize,
        title: String,
        authors: Vec<String>,
    },

    /// Remove a book from its list (user confirmed).
    RemoveBook {
        group_path: Vec<usize>,
        book_index: usize,
    },

    /// Mark a book as not-found (user gives up on it).
    MarkNotFound {
        group_path: Vec<usize>,
        book_index: usize,
    },

    /// Pause a specific in-flight transfer by md5 (Activity pane `p`).
    PauseTransfer { md5: String },

    /// Cancel a specific in-flight transfer by md5 (Activity pane `c`).
    CancelTransfer { md5: String },

    /// Resume/retry a paused or cancelled transfer by md5 (Activity pane `r`).
    ResumeTransfer { md5: String },

    /// Apply the previewed reorganize: move already-downloaded files on disk into
    /// the canonical layout for the current naming/folder/sub-grouping scheme.
    ApplyReorganize,
}
