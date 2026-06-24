//! Integration tests for Stage 3 (the goal-driven engine + per-orchestrator
//! locking). These drive the real engine headlessly against replay search
//! fixtures (no network), through the crate's `#[doc(hidden)] pub` test surface,
//! exactly as the Tauri commands do — verifying:
//!
//!   1. The library is NEVER frozen: while one orchestrator is locked for a long
//!      op, `state.library.lock()` AND access to the OTHER list both succeed
//!      promptly (the core bug being fixed).
//!   2. Launch is paused: after `resume_on_launch`, every book's goal is `Idle`
//!      and no transient in-flight states remain.
//!   3. The engine drives a list to its goal: with goal = Complete the engine
//!      reaches `Matched`/`NotFound` for each book WITHOUT any direct `query_all`
//!      call (everything flows through the driver).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use libgen_app_lib::commands::testsupport;
use libgen_app_lib::engine::{self, NoopEmitter};
use libgen_app_lib::state::AppState;

use libgen_core::model::{
    BookInput, BookRequest, DownloadList, Goal, Group, ListSettings, RequestStatus,
};

fn repo_root() -> PathBuf {
    // tests run from `app/src-tauri`; the workspace root is two levels up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn fixtures_dir() -> PathBuf {
    repo_root().join("fixtures").join("search")
}

fn mirrors_path() -> PathBuf {
    repo_root().join("mirrors.toml")
}

/// A list of fixture-backed books plus one with no fixture (→ NotFound).
fn list_with(title: &str, books: &[(&str, &str)]) -> DownloadList {
    let mut g = Group::new("Batch");
    for (t, a) in books {
        g.books.push(BookRequest::new(BookInput {
            title: (*t).into(),
            authors: if a.is_empty() {
                vec![]
            } else {
                vec![(*a).into()]
            },
            ..Default::default()
        }));
    }
    DownloadList {
        title: title.into(),
        settings: ListSettings::default(),
        groups: vec![g],
    }
}

/// Build a test AppState over a fresh temp DB + the replay fixtures.
fn make_state(dir: &std::path::Path) -> AppState {
    testsupport::app_state(dir.join("library.sqlite3"), fixtures_dir(), mirrors_path())
}

/// Poll a list's snapshot until `pred` holds for it, or `timeout` elapses.
async fn wait_for_list<P>(state: &AppState, id: &str, timeout: Duration, mut pred: P) -> bool
where
    P: FnMut(&DownloadList) -> bool,
{
    let start = Instant::now();
    loop {
        if let Some(list) = testsupport::snapshot(state, id).await {
            if pred(&list) {
                return true;
            }
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// (1) The library is never frozen. Hold ONE orchestrator's lock for a long time
/// (simulating a live network op) and assert that the library lock AND the OTHER
/// list's orchestrator lock are both still acquirable promptly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_never_frozen_while_one_orch_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(make_state(dir.path()));

    let id_a = testsupport::load(
        &state,
        &list_with("A", &[("Treasure Island", "Robert Louis Stevenson")]),
    )
    .await
    .unwrap();
    let id_b = testsupport::load(
        &state,
        &list_with("B", &[("Anne of Green Gables", "L. M. Montgomery")]),
    )
    .await
    .unwrap();

    // Grab list A's orchestrator handle, then HOLD its lock for a long time on a
    // background task (a stand-in for a long live op running off the library lock).
    let orch_a = {
        let lib = state.library.lock().await;
        lib.arc_for(&id_a).unwrap()
    };
    let held = {
        let orch_a = orch_a.clone();
        tokio::spawn(async move {
            let _guard = orch_a.lock().await;
            tokio::time::sleep(Duration::from_secs(3)).await;
        })
    };
    // Give the holder a moment to actually take the lock.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The library lock must be acquirable promptly even while A is busy.
    let t0 = Instant::now();
    {
        let _lib = state.library.lock().await;
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "library lock was starved by a busy orchestrator: {:?}",
            t0.elapsed()
        );
    }

    // The OTHER list's orchestrator must also be acquirable promptly.
    let orch_b = {
        let lib = state.library.lock().await;
        lib.arc_for(&id_b).unwrap()
    };
    let t1 = Instant::now();
    {
        let guard_b = orch_b.lock().await;
        assert!(
            t1.elapsed() < Duration::from_millis(500),
            "list B was blocked by list A being busy: {:?}",
            t1.elapsed()
        );
        // And it is genuinely usable while A is held.
        assert!(guard_b.snapshot().is_ok());
    }

    // A is still held (the long op hasn't finished) the whole time above.
    assert!(!held.is_finished(), "the long op should still be in flight");
    held.await.unwrap();
}

/// (2) Launch is paused. After resume, every book's goal is Idle and no transient
/// in-flight statuses remain. We seed a persisted list with an in-flight status,
/// then resume against the same DB.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_on_launch_is_paused() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.sqlite3");

    // Seed a list directly in the store with an in-flight (Querying) book and a
    // non-Idle goal, to prove resume normalizes both.
    {
        let mut store = libgen_core::store::Store::open(&db).unwrap();
        let mut list = list_with(
            "Seeded",
            &[
                ("Treasure Island", "Robert Louis Stevenson"),
                ("Anne of Green Gables", "L. M. Montgomery"),
            ],
        );
        list.groups[0].books[0].status = RequestStatus::Querying;
        list.groups[0].books[0].goal = Goal::Complete;
        list.groups[0].books[1].status = RequestStatus::Downloading;
        list.groups[0].books[1].goal = Goal::Complete;
        store.insert_list(&list).unwrap();
    }

    let state = testsupport::app_state(db, fixtures_dir(), mirrors_path());
    testsupport::resume(&state);

    let lib_ids = {
        let lib = state.library.lock().await;
        lib.lists.iter().map(|l| l.id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(lib_ids.len(), 1, "one list resumed");
    let snap = testsupport::snapshot(&state, &lib_ids[0]).await.unwrap();
    for b in &snap.groups[0].books {
        assert_eq!(b.goal, Goal::Idle, "every book parked at Idle on launch");
        assert_ne!(
            b.status,
            RequestStatus::Querying,
            "no transient Querying remains"
        );
        assert_ne!(
            b.status,
            RequestStatus::Downloading,
            "no transient Downloading remains"
        );
    }
}

/// (3) The engine drives a list to its goal. Spawn the real engine, load a list,
/// set goal = `Match` (discover-only — deterministic offline, no download
/// attempts against an unreachable resolver), and assert each book reaches the
/// right resolved discovery state — entirely through the driver (no direct
/// `query_all` call). The download leg of the goal is covered by the
/// orchestrator's `download_one`/scheduler unit + queue tests (a live/mock HTTP
/// server is out of scope for this offline harness).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_drives_list_to_goal() {
    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(make_state(dir.path()));

    // Start the engine against this state's shared handles.
    engine::spawn_with(state.engine_handles(), NoopEmitter);

    let id = testsupport::load(
        &state,
        &list_with(
            "Goal",
            &[
                ("Treasure Island", "Robert Louis Stevenson"),
                ("The Adventures of Tom Sawyer", "Mark Twain"),
                ("Anne of Green Gables", "L. M. Montgomery"),
                ("A Book With No Recorded Fixture Anywhere", ""),
            ],
        ),
    )
    .await
    .unwrap();

    // Set goal = Match (discover only) and wake the engine.
    testsupport::set_goal(&state, &id, Goal::Match)
        .await
        .unwrap();

    // The engine should discover every book. Wait until none remain Queued/
    // Querying (i.e. discovery has settled for all four books).
    let settled = wait_for_list(&state, &id, Duration::from_secs(20), |list| {
        list.groups[0]
            .books
            .iter()
            .all(|b| !matches!(b.status, RequestStatus::Queued | RequestStatus::Querying))
    })
    .await;
    assert!(settled, "engine settled discovery for every book");

    let snap = testsupport::snapshot(&state, &id).await.unwrap();
    let books = &snap.groups[0].books;
    // The three fixture-backed books auto-match; the last has no fixture.
    assert_eq!(books[0].status, RequestStatus::Matched, "Treasure Island");
    assert_eq!(books[1].status, RequestStatus::Matched, "Tom Sawyer");
    assert_eq!(books[2].status, RequestStatus::Matched, "Anne");
    assert_eq!(books[3].status, RequestStatus::NotFound, "no-fixture");
}
