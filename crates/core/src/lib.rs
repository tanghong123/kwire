//! libgen-core: UI-agnostic engine for parsing book lists, searching Library
//! Genesis, matching candidates, and downloading via per-host queues.
//!
//! Every front end (Tauri GUI, ratatui TUI, CLI harnesses) drives this library.
//! No module here may depend on a UI.

pub mod cover_gen;
pub mod covers;
pub mod download;
pub mod matching;
pub mod model;
pub mod naming;
pub mod orchestrator;
pub mod parse;
pub mod queue;
pub mod ranking;
pub mod search;
pub mod series;
pub mod slum;
pub mod speed;
pub mod store;

pub use matching::detect_title_language;
pub use model::{
    BookInput, BookRequest, Candidate, DownloadJob, DownloadList, Format, Group, JobState,
    ListSettings, RequestStatus,
};
