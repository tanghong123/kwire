//! Shared application state held by Tauri (`.manage`).
//!
//! The app keeps a [`Library`] of one [`Orchestrator`] per persisted list (the
//! multi-list sidebar), all backed by the SAME on-disk SQLite database file so
//! state survives a relaunch (resume-on-launch). Each orchestrator owns its own
//! [`Store`] (SQLite connection) and the [`SearchClient`] for its list. Because
//! the orchestrators are driven from async commands and hold non-`Sync` SQLite
//! connections, the whole library is guarded by a [`tokio::sync::Mutex`].
//!
//! A single download [`Scheduler`] is shared across every list (it is `Send +
//! Sync`), so pause/cancel can signal in-flight downloads regardless of which
//! list owns them. Engine configuration (mirrors file, output dir, db path,
//! optional replay dir) is resolved once at startup and reused for every load.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};

use libgen_core::orchestrator::Orchestrator;
use libgen_core::queue::{HostLimits, Scheduler};

/// Engine configuration, resolved from the environment / sensible defaults.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path to `mirrors.toml`.
    pub mirrors: PathBuf,
    /// Where downloads (and planned destinations) are written.
    pub out_dir: PathBuf,
    /// Path to the persistent SQLite database. Lists loaded here survive a
    /// relaunch so the app can resume downloads on the next start.
    pub db_path: PathBuf,
    /// If set, search runs offline against this recorded-fixtures dir instead of
    /// hitting the live mirrors. Handy for tests/demos.
    pub replay_dir: Option<PathBuf>,
    /// Global app settings the Settings sheet edits, loaded from (and saved to)
    /// `app-config.json` in the DB's directory.
    pub app: AppSettings,
}

/// Global, app-wide settings persisted as a small JSON file next to the DB.
/// These are the knobs the Settings sheet's "App settings" section edits:
/// the default download folder, the download-site failover order, and
/// concurrency/politeness limits used when building the scheduler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppSettings {
    /// Default download folder. Empty means "use the built-in default".
    pub out_dir: String,
    /// Global cap on TOTAL concurrent downloads across all hosts/lists (the `G`
    /// from docs/DOWNLOAD_SCHEDULING.md). Books beyond `G` stay queued. This is the
    /// only download-concurrency knob: the download chain is the fixed libgen+
    /// family (all one CDN), so per-host concurrency/rate selection was removed.
    #[serde(default = "default_max_concurrent_downloads")]
    pub max_concurrent_downloads: usize,
    /// Concurrent search queries (`Orchestrator::with_query_concurrency`).
    pub query_concurrency: usize,
    /// Max retry attempts per host (`HostLimits.max_attempts`).
    pub max_attempts: u32,
    /// Speculative (hedged) download: when ON, a download that stalls (crawls or
    /// hangs without erroring) races a second transport from a different mirror;
    /// the first verified copy wins. OFF by default — see
    /// `docs/SPECULATIVE_DOWNLOAD.md`.
    #[serde(default)]
    pub hedge_enabled: bool,
}

/// Default global download cap (`G`). Five concurrent downloads balances
/// throughput against politeness; tune in Settings.
pub(crate) fn default_max_concurrent_downloads() -> usize {
    5
}

impl Default for AppSettings {
    fn default() -> Self {
        let def = HostLimits::default();
        AppSettings {
            out_dir: String::new(),
            max_concurrent_downloads: default_max_concurrent_downloads(),
            query_concurrency: 8,
            max_attempts: def.max_attempts,
            hedge_enabled: false,
        }
    }
}

impl AppSettings {
    /// Per-host politeness limits. Per-host concurrency and rate are no longer
    /// user-configurable — but since the libgen+ download chain all fronts ONE CDN
    /// host, the per-host concurrency cap IS effectively the global one, so it
    /// tracks `max_concurrent_downloads` (otherwise the single host would bottleneck
    /// below the user's chosen cap). Rate keeps the built-in default spacing.
    pub fn host_limits(&self) -> HostLimits {
        HostLimits {
            max_concurrency: self.max_concurrent_downloads.max(1),
            max_attempts: self.max_attempts.max(1),
            ..HostLimits::default()
        }
    }

    /// Speculative-download config derived from these settings. Only `enabled`
    /// is user-exposed; the thresholds keep the design defaults (off by default).
    pub fn hedge_config(&self) -> libgen_core::queue::HedgeConfig {
        libgen_core::queue::HedgeConfig {
            enabled: self.hedge_enabled,
            ..Default::default()
        }
    }

    /// Load from `path`, returning defaults when the file is missing/invalid.
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist to `path`, creating the parent directory as needed.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into());
        std::fs::write(path, json)
    }
}

impl Config {
    /// Resolve config from env vars, falling back to repo-relative defaults so a
    /// plain `cargo tauri dev` from the repo "just works".
    pub fn from_env() -> Self {
        let mirrors = std::env::var_os("LIBGEN_MIRRORS")
            .map(PathBuf::from)
            .unwrap_or_else(default_mirrors);
        let out_dir = std::env::var_os("LIBGEN_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(default_out_dir);
        let db_path = std::env::var_os("LIBGEN_DB")
            .map(PathBuf::from)
            .unwrap_or_else(default_db_path);
        let replay_dir = std::env::var_os("LIBGEN_REPLAY").map(PathBuf::from);
        let cfg_path = app_config_path(&db_path);
        let app = AppSettings::load(&cfg_path);
        Config {
            mirrors,
            out_dir,
            db_path,
            replay_dir,
            app,
        }
    }

    /// Path to the global `app-config.json`, kept in the DB's directory.
    pub fn config_path(&self) -> PathBuf {
        app_config_path(&self.db_path)
    }

    /// Path to the cached SLUM availability snapshot (written by `refresh_mirrors`,
    /// read at scheduler/search build time to auto-order mirrors). Next to the DB.
    pub fn slum_cache_path(&self) -> PathBuf {
        self.db_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
            .join("slum-cache.json")
    }

    /// The output directory downloads are written to: the app-config override
    /// when set, else the env/default `out_dir`.
    pub fn effective_out_dir(&self) -> PathBuf {
        let o = self.app.out_dir.trim();
        if o.is_empty() {
            self.out_dir.clone()
        } else {
            PathBuf::from(o)
        }
    }
}

/// The global app-config JSON lives next to the DB file (its parent dir).
fn app_config_path(db_path: &std::path::Path) -> PathBuf {
    db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("app-config.json")
}

/// Best-effort default location for `mirrors.toml`: alongside the workspace
/// root (two levels up from `app/src-tauri` at dev time), else CWD.
fn default_mirrors() -> PathBuf {
    let candidates = [
        PathBuf::from("mirrors.toml"),
        PathBuf::from("../../mirrors.toml"),
        repo_root().join("mirrors.toml"),
    ];
    candidates
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("mirrors.toml"))
}

fn default_out_dir() -> PathBuf {
    // ~/Downloads (the app writes per-list subfolders under it); else ./downloads.
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Downloads");
    }
    PathBuf::from("downloads")
}

/// Default persistent DB location: an app-data dir under `$HOME`, else CWD.
/// The parent directory is created on demand by [`Config::open_store`].
fn default_db_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Kwire")
            .join("library.sqlite3");
    }
    PathBuf::from("library.sqlite3")
}

/// Workspace root inferred from this crate's manifest dir (`app/src-tauri`).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// One loaded list: its orchestrator plus the stable UI id the sidebar keys on.
///
/// The orchestrator is held behind its OWN `Arc<Mutex<…>>` (a *per-orchestrator*
/// lock) so the execution engine and the commands lock the shared [`Library`]
/// only briefly — to find/clone the `Arc` — then drop it and lock just the one
/// orchestrator for the (possibly network-bound) work. This is what keeps the
/// library lock short-lived: no command or engine step ever holds the library
/// guard across network I/O. See `docs/EXECUTION_MODEL.md` §A / §6.
pub struct LoadedList {
    /// Stable id the UI uses (`"list{list_id}"`), derived from the persisted row.
    pub id: String,
    pub orch: Arc<Mutex<Orchestrator>>,
}

impl LoadedList {
    /// Wrap an orchestrator into a loaded list with its per-orchestrator lock.
    pub fn new(id: String, orch: Orchestrator) -> Self {
        LoadedList {
            id,
            orch: Arc::new(Mutex::new(orch)),
        }
    }
}

/// The set of loaded lists plus which one is active.
#[derive(Default)]
pub struct Library {
    /// One orchestrator per persisted list, in load order.
    pub lists: Vec<LoadedList>,
    /// The active list id, or `"__all__"` for the aggregate, or empty if none.
    pub current: String,
}

impl Library {
    /// Stable UI id for a persisted list row id.
    pub fn id_for(list_id: i64) -> String {
        format!("list{list_id}")
    }

    /// Clone the per-orchestrator `Arc<Mutex<…>>` handle for a list id. Callers
    /// take this under a BRIEF library lock, drop the library guard, then lock the
    /// returned handle for the actual (possibly network-bound) work — keeping the
    /// library lock short-lived.
    pub fn arc_for(&self, id: &str) -> Option<Arc<Mutex<Orchestrator>>> {
        self.lists
            .iter()
            .find(|l| l.id == id)
            .map(|l| Arc::clone(&l.orch))
    }

    /// Clone every loaded list's `(id, orch-arc)` so the caller can iterate work
    /// after dropping the library lock.
    pub fn all_arcs(&self) -> Vec<(String, Arc<Mutex<Orchestrator>>)> {
        self.lists
            .iter()
            .map(|l| (l.id.clone(), Arc::clone(&l.orch)))
            .collect()
    }
}

/// The whole managed application state. The download [`Scheduler`] is shared by
/// every list so pause/cancel can reach in-flight downloads.
///
/// The shared mutable pieces (`library`, `scheduler`, `config`) are each behind an
/// `Arc<Mutex<…>>` so the long-lived execution engine task can clone exactly the
/// handles it needs (via [`AppState::engine_handles`]) and own them `'static`,
/// sharing the SAME state every command locks. Field access from commands is
/// unchanged (`state.library.lock().await`, etc.).
#[derive(Default)]
pub struct AppState {
    pub library: Arc<Mutex<Library>>,
    /// Lazily-built shared scheduler (created on the first download), retained so
    /// pause/cancel/resume commands can signal active downloads.
    pub scheduler: Arc<Mutex<Option<Arc<Scheduler>>>>,
    /// The resolved engine + app configuration. Held here so the Settings
    /// commands can read/update the global app settings at runtime (and so a
    /// changed scheduler is rebuilt with the new sites/limits). A plain
    /// `std::sync::Mutex` (never held across an `.await`) so the small,
    /// synchronous reads in `build_library` don't taint command futures as
    /// non-`Send`.
    pub config: Arc<std::sync::Mutex<Config>>,
    /// Wake handle for the execution engine (see `crate::engine`). Commands that
    /// change a book's goal/state call `wake_engine()` after persisting so the
    /// long-lived driver task re-plans promptly instead of waiting for its idle
    /// timer. Shared (`Arc`) so the engine task and every command share it.
    pub engine_wake: Arc<Notify>,
    /// Set once the engine task has been spawned (in `lib.rs` setup), so tests /
    /// re-entrant setup don't launch it twice.
    pub engine_started: std::sync::atomic::AtomicBool,
}

/// The `'static`, cloneable handles the engine task needs — exactly the shared
/// mutable state, with no borrow of the managed `AppState`. Built once when the
/// engine is spawned (or in a test) and moved into the driver task.
#[derive(Clone)]
pub struct EngineHandles {
    pub library: Arc<Mutex<Library>>,
    pub scheduler: Arc<Mutex<Option<Arc<Scheduler>>>>,
    pub config: Arc<std::sync::Mutex<Config>>,
    pub engine_wake: Arc<Notify>,
}

impl AppState {
    /// Wake the execution engine to re-plan now (after a goal/state change).
    pub fn wake_engine(&self) {
        self.engine_wake.notify_one();
    }

    /// Clone the shared handles the engine task owns.
    pub fn engine_handles(&self) -> EngineHandles {
        EngineHandles {
            library: Arc::clone(&self.library),
            scheduler: Arc::clone(&self.scheduler),
            config: Arc::clone(&self.config),
            engine_wake: Arc::clone(&self.engine_wake),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config::from_env()
    }
}
