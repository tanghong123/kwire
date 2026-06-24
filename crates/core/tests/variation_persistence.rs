//! Task 5 investigation: do per-variation download picks survive an app restart?
//!
//! Bug report: "some changes of picking variations to download do not survive an
//! app restart." This test builds an orchestrator on a temp (on-disk) DB, queries
//! via the replay transport so candidates exist, requests a NON-best variation (and
//! an extra format on a Matched book), then DROPS the orchestrator and re-`attach`es
//! a fresh one on the same DB (simulating a restart). It runs the resume flow
//! (`reset_inflight_for_resume` + `query_all`) and asserts every requested
//! variation's `job` (Pending) SURVIVED with the right md5.

use libgen_core::model::{
    BookInput, BookRequest, DownloadList, Group, JobState, ListSettings, RequestStatus,
};
use libgen_core::orchestrator::Orchestrator;
use libgen_core::search::{MirrorConfig, SearchClient};
use libgen_core::store::Store;
use std::path::PathBuf;
use tokio::sync::mpsc;

fn config() -> MirrorConfig {
    let toml = r#"
        [[search_mirror]]
        host = "libgen.li"
        search_url = "https://libgen.li/index.php?req={query}&res={limit}"
        kind = "libgen_li_html"
        priority = 1
        [[search_mirror]]
        host = "libgen.is"
        search_url = "https://libgen.is/json.php?req={query}"
        kind = "libgen_json"
        priority = 2
    "#;
    MirrorConfig::from_toml(toml).unwrap()
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("search")
}

fn search() -> SearchClient {
    SearchClient::replay(config(), fixtures_dir())
}

fn list() -> DownloadList {
    let mut g = Group::new("Batch 1");
    g.books.push(BookRequest::new(BookInput {
        title: "Treasure Island".into(),
        authors: vec!["Robert Louis Stevenson".into()],
        ..Default::default()
    }));
    DownloadList {
        title: "Restart".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    }
}

async fn drain(mut rx: mpsc::Receiver<libgen_core::orchestrator::Event>) {
    while rx.recv().await.is_some() {}
}

#[tokio::test]
async fn variation_picks_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");

    // md5s we deliberately requested, to verify they survive.
    let best_md5: String;
    let extra_md5: String;
    let list_id: i64;

    {
        let mut orch =
            Orchestrator::new(Store::open(&db).unwrap(), &list(), search(), "/out").unwrap();
        list_id = orch.list_id();

        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        t.await.unwrap();

        // Treasure Island auto-matched: its best variation is auto-requested (Pending).
        let snap = orch.snapshot().unwrap();
        let book = &snap.groups[0].books[0];
        assert_eq!(
            book.status,
            RequestStatus::Matched,
            "Treasure Island should auto-match"
        );
        assert!(
            book.candidates.len() >= 2,
            "need an extra variation to pick"
        );
        best_md5 = book.candidates[0].md5.clone();
        // The best variation was auto-requested.
        assert_eq!(
            book.candidates[0].job.as_ref().map(|j| &j.state),
            Some(&JobState::Pending),
            "best variation auto-requested"
        );

        // Pick a NON-best (extra format) variation explicitly.
        extra_md5 = book.candidates[1].md5.clone();
        assert!(book.candidates[1].job.is_none(), "extra not yet requested");
        orch.request_variation(&[0], 0, &extra_md5).unwrap();

        // Confirm in-memory before the drop.
        let snap = orch.snapshot().unwrap();
        let book = &snap.groups[0].books[0];
        assert_eq!(
            book.candidates[1].job.as_ref().map(|j| &j.state),
            Some(&JobState::Pending),
            "extra variation requested before restart"
        );
        // orch drops here, closing the DB connection (simulated quit).
    }

    // ---- Simulated restart: fresh Store + attach on the same DB ----
    let mut orch2 = Orchestrator::attach(Store::open(&db).unwrap(), list_id, search(), "/out");

    // Resume flow the front end runs on boot.
    orch2.reset_inflight_for_resume().unwrap();
    let (tx, rx) = mpsc::channel(64);
    let t = tokio::spawn(drain(rx));
    orch2.query_all(&tx).await.unwrap();
    drop(tx);
    t.await.unwrap();

    // Assert both requested variations survived with their md5 + Pending job.
    let snap = orch2.snapshot().unwrap();
    let book = &snap.groups[0].books[0];

    let best = book
        .candidates
        .iter()
        .find(|c| c.md5 == best_md5)
        .expect("best md5 still present after restart");
    assert_eq!(
        best.job.as_ref().map(|j| &j.state),
        Some(&JobState::Pending),
        "best variation's Pending job must survive restart"
    );

    let extra = book
        .candidates
        .iter()
        .find(|c| c.md5 == extra_md5)
        .expect("extra md5 still present after restart");
    assert_eq!(
        extra.job.as_ref().map(|j| &j.state),
        Some(&JobState::Pending),
        "explicitly-requested extra variation's Pending job must survive restart"
    );

    // Exactly the two we requested are requested (nothing clobbered, nothing added).
    let requested: Vec<&str> = book
        .candidates
        .iter()
        .filter(|c| c.job.is_some())
        .map(|c| c.md5.as_str())
        .collect();
    assert_eq!(
        requested.len(),
        2,
        "exactly two requested variations survive"
    );
    assert!(requested.contains(&best_md5.as_str()));
    assert!(requested.contains(&extra_md5.as_str()));
}

#[tokio::test]
async fn requery_resets_only_unsettled_books() {
    use libgen_core::model::Candidate;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("rq.db");

    // Treasure Island (has a real fixture → matches) + a book with no fixture (not_found).
    let mut g = Group::new("Batch 1");
    g.books.push(BookRequest::new(BookInput {
        title: "Treasure Island".into(),
        authors: vec!["Robert Louis Stevenson".into()],
        ..Default::default()
    }));
    g.books.push(BookRequest::new(BookInput {
        title: "A Book With No Recorded Fixture Anywhere".into(),
        ..Default::default()
    }));
    let dl = DownloadList {
        title: "L".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    };

    let mut orch = Orchestrator::new(Store::open(&db).unwrap(), &dl, search(), "/out").unwrap();
    let (tx, rx) = mpsc::channel(64);
    let t = tokio::spawn(drain(rx));
    orch.query_all(&tx).await.unwrap();
    drop(tx);
    let _ = t.await;

    // Treasure Island auto-matched, so its best variation carries a (Pending) job — queued
    // for download but NOT started, so no bandwidth used. Re-query MUST refresh
    // it (this is how matched-by-an-old-algorithm books get re-matched).
    let before = orch.snapshot().unwrap();
    assert_eq!(before.groups[0].books[0].status, RequestStatus::Matched);
    assert!(before.groups[0].books[0]
        .candidates
        .iter()
        .any(|c: &Candidate| matches!(
            &c.job,
            Some(j) if j.state == JobState::Pending
        )));

    let reset = orch.requery_unsettled().unwrap();
    assert_eq!(
        reset, 2,
        "matched-but-Pending AND not-found are both re-queried"
    );

    let after = orch.snapshot().unwrap();
    for bi in 0..2 {
        let b = &after.groups[0].books[bi];
        assert_eq!(b.status, RequestStatus::Queued, "book {bi} reset to Queued");
        assert!(b.candidates.is_empty(), "book {bi} candidates cleared");
    }
}

#[tokio::test]
async fn requery_preserves_in_flight_and_downloaded_books() {
    use libgen_core::model::{Candidate, DownloadJob};
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("rq2.db");
    let mut orch = Orchestrator::new(Store::open(&db).unwrap(), &list(), search(), "/out").unwrap();
    let lid = orch.list_id();
    let (tx, rx) = mpsc::channel(64);
    let t = tokio::spawn(drain(rx));
    orch.query_all(&tx).await.unwrap();
    drop(tx);
    let _ = t.await;

    // Promote Treasure Island' best variation to Done (an "already downloaded" book), via a
    // second connection to the same DB.
    let md5 = {
        let mut book = orch.snapshot().unwrap().groups[0].books[0].clone();
        let md5 = book.candidates[0].md5.clone();
        book.candidates[0].job = Some(DownloadJob {
            state: JobState::Done,
            ..Default::default()
        });
        let mut store2 = Store::open(&db).unwrap();
        store2.update_request(lid, &[0], 0, &book).unwrap();
        md5
    };

    let reset = orch.requery_unsettled().unwrap();
    assert_eq!(reset, 0, "a downloaded book is never re-queried");
    let after = orch.snapshot().unwrap();
    let h = &after.groups[0].books[0];
    assert_eq!(
        h.status,
        RequestStatus::Matched,
        "downloaded book untouched"
    );
    assert!(h.candidates.iter().any(
        |c: &Candidate| c.md5 == md5 && matches!(&c.job, Some(j) if j.state == JobState::Done)
    ));
}
