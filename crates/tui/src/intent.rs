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
}
