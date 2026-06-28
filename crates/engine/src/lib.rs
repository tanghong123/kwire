//! `libgen-engine`: the UI-agnostic concurrency driver shared between the Tauri
//! desktop app and any future front end (TUI, CLI, etc.). Depends only on
//! `libgen-core`; no tauri/ratatui imports.

pub mod bridge;
pub mod engine;
pub mod legs;
pub mod state;
pub mod viewmodel;

// Re-export the key types both frontends need.
pub use engine::{
    build_scheduler, build_search, ensure_scheduler_from, open_store, reconcile_completed_inflight,
    spawn_with, BookStatePayload, EngineEmitter, NoopEmitter, RECONCILE_MAX_ATTEMPTS,
};
pub use legs::{LegTracker, LegView, LEG_TTL_MS};
pub use state::{
    default_max_concurrent_downloads, AppSettings, AppState, Config, EngineHandles, Library,
    LoadedList,
};
pub use viewmodel::{
    ViewAcquisition, ViewAppConfig, ViewBook, ViewEvent, ViewGroup, ViewLibrary, ViewListSettings,
    ViewModel, ViewSiteHealth, ViewVariation,
};
