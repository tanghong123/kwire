//! Tauri-specific engine glue: wires the generic [`libgen_engine::spawn_with`]
//! driver to a [`tauri::AppHandle`] as the event sink. This is the ONLY file in
//! `app/src-tauri` that imports tauri for engine purposes; the driver loop itself
//! lives in `libgen-engine` (no tauri dependency).

use std::sync::atomic::Ordering;

use tauri::{AppHandle, Emitter, Manager};

use libgen_core::model::DownloadList;
use libgen_core::orchestrator::Event;

use libgen_engine::{BookStatePayload, EngineEmitter};

use libgen_engine::LegTracker;

use crate::bridge;
use crate::commands::{LegProjection, ProgressEnvelope, ProgressPayload, QueryStagePayload};
use crate::state::AppState;

/// The production emitter: forwards engine events to the front end as the same
/// `query://book` / `download://progress` events the UI already consumes, plus
/// `engine://book` for per-book state.
///
/// Owns the shared [`LegTracker`] (the single source of truth for download legs):
/// every `Download` event is fed into it, and the affected md5's projected legs
/// ride along on the `download://progress` payload so the frontend renders the
/// primary/alt-copy split without reconstructing leg state in JavaScript.
struct TauriEmitter {
    app: AppHandle,
    legs: std::sync::Mutex<LegTracker>,
    clock: std::time::Instant,
}

impl EngineEmitter for TauriEmitter {
    fn emit_event(&self, list_id: &str, shape: &DownloadList, ev: &Event) {
        match ev {
            Event::QueryStage {
                group_path,
                book_index,
                title,
                stage,
            } => {
                let book_id = bridge::flat_id_in(shape, group_path, *book_index)
                    .unwrap_or_else(|| format!("bk{book_index}"));
                let _ = self.app.emit(
                    "query://book",
                    QueryStagePayload {
                        list_id: list_id.to_string(),
                        book_id,
                        title: title.clone(),
                        stage: stage.clone(),
                    },
                );
            }
            Event::Download(p) => {
                // Feed the shared tracker (sees EVERY variant incl. keep-alives /
                // LegEnded), then attach the affected md5's projected legs so the
                // UI renders the primary/alt-copy split without its own JS state.
                let now_ms = self.clock.elapsed().as_millis() as u64;
                let mut tracker = self.legs.lock().unwrap();
                tracker.note(p, now_ms);
                if let Some(payload) = ProgressPayload::from_progress(p) {
                    let legs = payload
                        .md5()
                        .map(|md5| LegProjection::project(&tracker, md5, now_ms))
                        .unwrap_or_default();
                    let _ = self
                        .app
                        .emit("download://progress", ProgressEnvelope { payload, legs });
                }
            }
            Event::Done => {
                let _ = self
                    .app
                    .emit("download://progress", ProgressPayload::AllDone);
            }
            // Planned / StatusChanged carry no extra UI signal beyond the above +
            // engine://book + the refreshed library, so they are not forwarded.
            _ => {}
        }
    }

    fn emit_book_state(&self, payload: BookStatePayload) {
        let _ = self.app.emit("engine://book", payload);
    }

    fn emit_refresh(&self) {
        let _ = self.app.emit("library://refresh", ());
    }
}

/// Spawn the long-lived engine driver task, wired to the Tauri [`AppHandle`]
/// (which owns the managed [`AppState`] and is the event sink). Idempotent: only
/// the first call spawns; subsequent calls are no-ops (guarded by
/// `engine_started`). The task re-fetches the managed state each tick from the
/// handle, so it shares the SAME `AppState` every command sees.
pub fn spawn(app: AppHandle) {
    let handles = {
        let state = app.state::<AppState>();
        if state.engine_started.swap(true, Ordering::SeqCst) {
            return;
        }
        state.engine_handles()
    };
    let emitter = TauriEmitter {
        app,
        legs: std::sync::Mutex::new(LegTracker::new()),
        clock: std::time::Instant::now(),
    };
    // Spawn on Tauri's managed async runtime — this runs from the `setup` hook,
    // which is NOT inside a Tokio runtime context, so a bare `tokio::spawn` would
    // panic ("no reactor running"). `tauri::async_runtime::spawn` works from any
    // context. (Workers spawned inside `run_engine` are fine — they run within
    // this task, which is on the runtime.)
    // Use `tauri::async_runtime::spawn` rather than `tokio::spawn` directly:
    // this function is called from the Tauri `setup` hook, which is NOT inside
    // a Tokio runtime context. `spawn_with` internally calls `tokio::spawn`,
    // which requires being on the runtime — the async block here ensures that.
    tauri::async_runtime::spawn(async move {
        libgen_engine::spawn_with(handles, emitter);
    });
}
