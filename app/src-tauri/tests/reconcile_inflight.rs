//! Integration tests for the **session-gated stuck-download reconciliation**
//! (`commands::reconcile_completed_inflight`), the reusable unit shared by the
//! startup integrity scan AND the running engine's in-session sweep.
//!
//! It judges each in-flight-but-not-`Done` variation that has NO live transport:
//!   * complete file (full size + md5 verifies) → `Done` (the lost `Progress::Done`
//!     reconciled in — the original "completed-but-stuck" bug);
//!   * partial/absent file → re-queue (`Pending`, `attempts += 1`, keep the
//!     `.part`/`resume_offset`) so the engine's drive loop resumes it;
//!   * a variation already at the attempt cap → `Failed` (stop a dead source from
//!     thrashing; becomes user-retryable);
//!   * a variation WITH a live transport (in the `live_md5s` set) → never touched.

use std::collections::HashSet;
use std::path::PathBuf;

use libgen_app_lib::commands::{self, testsupport, RECONCILE_MAX_ATTEMPTS};
use libgen_app_lib::state::AppState;

use libgen_core::download::md5_hex;
use libgen_core::model::{
    BookInput, BookRequest, Candidate, DownloadJob, DownloadList, Format, Group, JobState,
    ListSettings, RequestStatus,
};

fn repo_root() -> PathBuf {
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
fn make_state(dir: &std::path::Path) -> AppState {
    testsupport::app_state(dir.join("library.sqlite3"), fixtures_dir(), mirrors_path())
}

/// A stuck `Downloading` variation: 50 of `total` bytes, stale speed. `output_path`
/// points at `path` (which may or may not exist on disk). `attempts` seeds the cap.
fn inflight_book(
    title: &str,
    md5: &str,
    path: &std::path::Path,
    total: u64,
    attempts: u32,
) -> BookRequest {
    let mut b = BookRequest::new(BookInput {
        title: title.into(),
        ..Default::default()
    });
    b.status = RequestStatus::Downloading;
    b.selected = Some(md5.to_string());
    b.candidates = vec![Candidate {
        md5: md5.to_string(),
        title: title.into(),
        authors: vec![],
        year: None,
        publisher: None,
        language: None,
        pages: None,
        extension: Some(Format::Epub),
        size_bytes: None,
        source_host: None,
        cover_url: None,
        score: 1.0,
        job: Some(DownloadJob {
            state: JobState::Downloading,
            attempts,
            bytes_done: 50,
            total_bytes: Some(total),
            resume_offset: 50,
            speed_bps: Some(1_600_000),
            output_path: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        }),
    }];
    b
}

async fn load_one(
    state: &AppState,
    book: BookRequest,
) -> (
    String,
    std::sync::Arc<tokio::sync::Mutex<libgen_core::orchestrator::Orchestrator>>,
) {
    let mut g = Group::new("Batch");
    g.books.push(book);
    let list = DownloadList {
        title: "L".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    };
    let id = testsupport::load(state, &list).await.unwrap();
    let orch = {
        let lib = state.library.lock().await;
        lib.arc_for(&id).unwrap()
    };
    (id, orch)
}

fn job_of(snap: &DownloadList, title: &str) -> DownloadJob {
    snap.groups[0]
        .books
        .iter()
        .find(|b| b.input.title == title)
        .unwrap()
        .candidates[0]
        .job
        .clone()
        .unwrap()
}

/// COMPLETE branch: a sessionless stuck job whose file is full-size + md5-verifies
/// is promoted to `Done`/`md5_verified`; idempotent on a second pass.
#[tokio::test]
async fn complete_sessionless_download_is_promoted_to_done() {
    let dir = std::env::temp_dir().join(format!("kwire-reconcile-done-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let good_path = dir.join("good.epub");
    let good_bytes = vec![3u8; 120];
    std::fs::write(&good_path, &good_bytes).unwrap();
    let good_md5 = md5_hex(&good_bytes);

    let state = make_state(&dir);
    let (id, orch) = load_one(&state, inflight_book("Good", &good_md5, &good_path, 120, 0)).await;

    let live: HashSet<String> = HashSet::new();
    let fixed = commands::reconcile_completed_inflight(&orch, &live, "test").await;
    assert_eq!(fixed, 1, "the complete file's job is reconciled to Done");

    let snap = testsupport::snapshot(&state, &id).await.unwrap();
    let good = job_of(&snap, "Good");
    assert_eq!(good.state, JobState::Done, "complete download promoted");
    assert!(good.md5_verified, "md5_verified set");
    assert_eq!(good.bytes_done, 120, "bytes_done reconciled to total");
    assert_eq!(good.speed_bps, None, "stale live readout cleared");

    // Idempotent: now Done, so nothing left to reconcile.
    assert_eq!(
        commands::reconcile_completed_inflight(&orch, &live, "test").await,
        0
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// (a) NOT-DONE / partial: a sessionless stuck job whose file is incomplete (here
/// absent on disk) is RE-QUEUED — `state → Pending`, `attempts += 1`, the
/// `resume_offset`/`.part` KEPT — under the attempt cap.
#[tokio::test]
async fn sessionless_partial_download_is_requeued_with_attempts_bumped() {
    let dir = std::env::temp_dir().join(format!("kwire-reconcile-requeue-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // File never written → the final file is absent, so the job is incomplete.
    let absent = dir.join("absent.epub");
    let md5 = md5_hex(&vec![5u8; 200]);

    let state = make_state(&dir);
    let (id, orch) = load_one(&state, inflight_book("Partial", &md5, &absent, 200, 1)).await;

    let live: HashSet<String> = HashSet::new();
    let fixed = commands::reconcile_completed_inflight(&orch, &live, "test").await;
    assert_eq!(fixed, 1, "the sessionless partial is re-queued");

    let snap = testsupport::snapshot(&state, &id).await.unwrap();
    let job = job_of(&snap, "Partial");
    assert_eq!(job.state, JobState::Pending, "re-queued to Pending");
    assert_eq!(job.attempts, 2, "attempts bumped 1 → 2");
    assert_eq!(job.resume_offset, 50, "resume_offset KEPT for resume");
    assert_eq!(job.speed_bps, None, "stale live readout cleared");
    let _ = std::fs::remove_dir_all(&dir);
}

/// (b) NOT-DONE / over the cap: a sessionless stuck job whose `attempts` has reached
/// the cap is marked `Failed` (thrash guard) instead of re-queued.
#[tokio::test]
async fn sessionless_partial_over_attempt_cap_is_failed() {
    let dir = std::env::temp_dir().join(format!("kwire-reconcile-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let absent = dir.join("absent.epub");
    let md5 = md5_hex(&vec![6u8; 300]);

    let state = make_state(&dir);
    let (id, orch) = load_one(
        &state,
        inflight_book("Dead", &md5, &absent, 300, RECONCILE_MAX_ATTEMPTS),
    )
    .await;

    let live: HashSet<String> = HashSet::new();
    let fixed = commands::reconcile_completed_inflight(&orch, &live, "test").await;
    assert_eq!(fixed, 1, "the capped variation is failed, not re-queued");

    let snap = testsupport::snapshot(&state, &id).await.unwrap();
    let job = job_of(&snap, "Dead");
    assert_eq!(job.state, JobState::Failed, "marked Failed at the cap");
    assert!(
        job.last_error
            .as_deref()
            .unwrap_or("")
            .contains("did not complete"),
        "user-facing reason set: {:?}",
        job.last_error
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// (c) LIVENESS: a variation WITH a live transport (its md5 in `live_md5s`) is never
/// touched, even though its file is incomplete and quiet.
#[tokio::test]
async fn variation_with_live_session_is_left_untouched() {
    let dir = std::env::temp_dir().join(format!("kwire-reconcile-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let absent = dir.join("absent.epub");
    let md5 = md5_hex(&vec![7u8; 400]);

    let state = make_state(&dir);
    let (id, orch) = load_one(&state, inflight_book("Live", &md5, &absent, 400, 0)).await;

    // The engine reports this md5 as having a live transport.
    let mut live: HashSet<String> = HashSet::new();
    live.insert(md5.clone());

    let fixed = commands::reconcile_completed_inflight(&orch, &live, "test").await;
    assert_eq!(fixed, 0, "a live transfer is never reconciled");

    let snap = testsupport::snapshot(&state, &id).await.unwrap();
    let job = job_of(&snap, "Live");
    assert_eq!(
        job.state,
        JobState::Downloading,
        "the live (slow-but-alive) job is left exactly as-is"
    );
    assert_eq!(job.attempts, 0, "attempts not bumped for a live job");
    let _ = std::fs::remove_dir_all(&dir);
}
