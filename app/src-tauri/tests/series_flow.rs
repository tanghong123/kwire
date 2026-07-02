//! Live, headless reproduction of the desktop "download series" flow that a user
//! reported failing:
//!   1. add a manual book titled "a series of unfortunate events" (nothing else),
//!   2. query it and pick a copy,
//!   3. invoke download-series → "book does not belong to any series".
//!
//! The bug: the reverse series lookup was seeded from the book's INPUT title (the
//! bare series name), which matches only box sets and never resolves. The fix
//! seeds from the best real-member CANDIDATE instead. This drives the actual
//! Tauri command core (`testsupport::download_series`) end to end.
//!
//! Network test (hits real search mirrors + Open Library / libgen / Goodreads),
//! so it is `#[ignore]` by default. Run it explicitly:
//!   cargo test -p libgen-app --test series_flow -- --ignored --nocapture

use std::path::PathBuf;
use std::time::{Duration, Instant};

use libgen_app_lib::commands::testsupport;
use libgen_app_lib::engine::{self, NoopEmitter};
use libgen_app_lib::state::AppState;
use libgen_core::model::{DownloadList, Goal};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}
fn mirrors_path() -> PathBuf {
    repo_root().join("mirrors.toml")
}

async fn wait_until<P: Fn(&DownloadList) -> bool>(
    state: &AppState,
    id: &str,
    timeout: Duration,
    pred: P,
) -> bool {
    let start = Instant::now();
    loop {
        if let Some(snap) = testsupport::snapshot(state, id).await {
            if pred(&snap) {
                return true;
            }
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Hermetic (offline) guard: a book whose candidates all carry the series name
/// must seed from a real member — not the series-name input, not a box set. The
/// full case-3 ranking (majority-keyword ignore) is unit-tested in
/// `libgen_core::series`; this checks the desktop mapping end of it.
#[test]
fn series_seeds_prefer_member_over_box_set_and_series_name_input() {
    use libgen_core::model::{BookInput, BookRequest, Candidate};
    let cand = |title: &str, author: &str, score: f32| -> Candidate {
        serde_json::from_value(serde_json::json!({
            "md5": "x", "title": title, "authors": [author], "score": score,
        }))
        .unwrap()
    };
    let mut book = BookRequest::new(BookInput {
        title: "a series of unfortunate events".into(),
        authors: vec![],
        ..Default::default()
    });
    book.candidates = vec![
        cand("A Series of Unfortunate Events Collection", "Lemony, Lemony A", 0.62),
        cand("A Series Of Unfortunate Events 10 Slippery Slope", "Snicket, Lemony", 0.49),
        cand("A Series of Unfortunate Events: The Beatrice Letters", "Lemony Snicket", 0.48),
    ];
    let seeds = testsupport::series_seeds(&book);
    assert_eq!(seeds[0].0, "A Series Of Unfortunate Events 10 Slippery Slope");
    assert_eq!(seeds[0].1, "Snicket, Lemony");
    assert!(
        !seeds.iter().any(|(t, _)| t.contains("Collection")
            || *t == "a series of unfortunate events"),
        "no box set / series-name seed: {seeds:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits live mirrors + Open Library; run with --ignored"]
async fn download_series_from_series_name_add_resolves_via_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let state =
        testsupport::app_state_live(tmp.path().join("library.sqlite3"), mirrors_path());
    engine::spawn_with(state.engine_handles(), NoopEmitter);

    // 1) Add the exact title the user typed — no author.
    let lib = testsupport::add_manual_book(&state, "a series of unfortunate events", None)
        .await
        .expect("add ok");
    let list = lib
        .lists
        .iter()
        .find(|l| l.title == "Manual")
        .expect("Manual list");
    let list_id = list.id.clone();
    let book_id = list.groups[0].books[0].id.clone();

    // 2) Discover candidates (goal = Match) and wait for them to arrive.
    testsupport::set_goal(&state, &list_id, Goal::Match)
        .await
        .unwrap();
    let got = wait_until(&state, &list_id, Duration::from_secs(90), |snap| {
        !snap.groups[0].books[0].candidates.is_empty()
    })
    .await;
    assert!(got, "the query should discover candidates");

    // Show exactly what the flow sees — the input vs. the candidates vs. the seed
    // the fix now derives (this is "where the problem was").
    let snap = testsupport::snapshot(&state, &list_id).await.unwrap();
    let book = &snap.groups[0].books[0];
    eprintln!(
        "INPUT   title={:?} authors={:?}",
        book.input.title, book.input.authors
    );
    for c in &book.candidates {
        eprintln!(
            "CAND    score={:.2} title={:?} authors={:?}",
            c.score, c.title, c.authors
        );
    }
    let seeds = testsupport::series_seeds(book);
    eprintln!("SEEDS -> {seeds:?}");
    assert_ne!(
        seeds[0].0.to_lowercase(),
        "a series of unfortunate events",
        "the primary seed must NOT be the bare series name (never reverse-resolves)"
    );

    // 3) Download series — must now succeed and create a "(series)" list.
    let lib2 = testsupport::download_series(&state, Some(list_id), book_id)
        .await
        .expect("download_series should succeed via a member candidate");
    let series_list = lib2
        .lists
        .iter()
        .find(|l| l.title.contains("(series)"))
        .unwrap_or_else(|| {
            panic!(
                "expected a (series) list; got {:?}",
                lib2.lists.iter().map(|l| &l.title).collect::<Vec<_>>()
            )
        });
    let n: usize = series_list.groups.iter().map(|g| g.books.len()).sum();
    eprintln!("CREATED series list {:?} with {n} books", series_list.title);
    assert!(n >= 2, "the series should have >= 2 members, got {n}");
}
