//! Offline integration test: drive `SearchClient` in replay mode against
//! recorded fixtures, then assert that parsing + `matching::evaluate` produce
//! the expected candidates and outcome. Runs with NO network.

use libgen_core::matching;
use libgen_core::model::{BookInput, Format, ListSettings, RequestStatus};
use libgen_core::search::{MirrorConfig, SearchClient};
use std::path::PathBuf;

/// Mirror config mirroring the repo's `mirrors.toml`: libgen.li HTML first,
/// then a libgen.is JSON endpoint. Inline so the test is self-contained.
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
    MirrorConfig::from_toml(toml).expect("inline mirrors config")
}

fn fixtures_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../crates/core ; fixtures live at the repo root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("search")
}

fn client() -> SearchClient {
    SearchClient::replay(config(), fixtures_dir())
}

#[tokio::test]
async fn treasure_island_html_fixture_parses_and_auto_matches() {
    let input = BookInput {
        title: "Treasure Island".into(),
        authors: vec!["Robert Louis Stevenson".into()],
        year: Some(1883),
        language: Some("English".into()),
        ..Default::default()
    };

    let candidates = client()
        .search(&input)
        .await
        .expect("search treasure island");
    assert!(
        candidates.len() >= 10,
        "expected a full result table, got {}",
        candidates.len()
    );

    // Every parsed candidate has a 32-hex md5 and a source host.
    for c in &candidates {
        assert_eq!(c.md5.len(), 32, "md5 should be 32 hex chars: {:?}", c.md5);
        assert!(c.md5.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_eq!(c.source_host.as_deref(), Some("libgen.li"));
    }

    let outcome = matching::evaluate(&input, candidates, &ListSettings::default());
    assert_eq!(outcome.status, RequestStatus::Matched);

    let top = &outcome.ranked[0];
    assert_eq!(top.title.to_lowercase(), "treasure island");
    assert_eq!(top.authors, vec!["Robert Louis Stevenson".to_string()]);
    assert_eq!(top.extension, Some(Format::Epub));
    assert!(top.score >= ListSettings::default().auto_threshold);
    // Sorted descending.
    for w in outcome.ranked.windows(2) {
        assert!(w[0].score >= w[1].score, "candidates must be score-sorted");
    }
}

#[tokio::test]
async fn tom_sawyer_html_fixture_auto_matches_epub() {
    let input = BookInput {
        title: "The Adventures of Tom Sawyer".into(),
        authors: vec!["Mark Twain".into()],
        ..Default::default()
    };
    let candidates = client().search(&input).await.expect("search tom sawyer");
    assert!(!candidates.is_empty());

    let outcome = matching::evaluate(&input, candidates, &ListSettings::default());
    assert_eq!(outcome.status, RequestStatus::Matched);
    assert_eq!(
        outcome.ranked[0].title.to_lowercase(),
        "the adventures of tom sawyer"
    );
    // Top pick must be in a preferred format (default: epub/pdf).
    let prefs = ListSettings::default().format_pref;
    assert!(prefs.contains(outcome.ranked[0].extension.as_ref().unwrap()));
}

#[tokio::test]
async fn looser_strategy_finds_book_strict_query_misses() {
    // The strict "<title> <full author>" query for The Time Machine returns one
    // journal-article row (fixture: the-time-machine-an-invention-h-g-wells.html)
    // whose title cell stacks an issue marker; the multi-strategy search can also
    // widen to "The Time Machine Wells" (fixture: the-time-machine-wells.html).
    // Either way the matcher must surface a usable candidate (author in title).
    let input = BookInput {
        title: "The Time Machine: An Invention".into(),
        authors: vec!["H. G. Wells".into()],
        ..Default::default()
    };
    let candidates = client().search(&input).await.expect("search time machine");

    // The search surfaces the real Time Machine edition.
    let hits = candidates
        .iter()
        .filter(|c| c.title.to_lowercase().contains("time machine"))
        .count();
    assert!(
        hits >= 1,
        "search should surface the real Time Machine edition, got {hits}"
    );

    // Author-in-title → a confident match, never dropped to not-found or an empty
    // dead-end.
    let outcome = matching::evaluate(&input, candidates, &ListSettings::default());
    assert!(
        matches!(
            outcome.status,
            RequestStatus::Matched | RequestStatus::NeedsSelection
        ),
        "expected an actionable status, got {:?}",
        outcome.status
    );
    assert!(!outcome.ranked.is_empty(), "must carry candidates");
}

#[tokio::test]
async fn garbage_request_not_found() {
    // Hit the real "Treasure Island" fixture (so the parse path is exercised),
    // then overwrite candidate text with unrelated garbage → the request title no
    // longer matches anything → NotFound.
    let input = BookInput {
        title: "Treasure Island".into(),
        authors: vec!["Robert Louis Stevenson".into()],
        ..Default::default()
    };
    let candidates = client().search(&input).await.expect("search");
    // Replace candidate titles with garbage to simulate irrelevant results
    // while keeping the parse path real.
    let garbage: Vec<_> = candidates
        .into_iter()
        .map(|mut c| {
            c.title = "Treatise on Nothing in Particular".into();
            c.authors = vec!["Anonymous".into()];
            c
        })
        .collect();
    let outcome = matching::evaluate(&input, garbage, &ListSettings::default());
    assert_eq!(outcome.status, RequestStatus::NotFound);
}

#[tokio::test]
async fn missing_fixture_yields_empty_not_error() {
    // No fixture exists for this query on any mirror; search returns an empty
    // result (failover exhausted) rather than an error, and evaluate -> NotFound.
    let input = BookInput {
        title: "A Book That Has No Recorded Fixture Anywhere".into(),
        ..Default::default()
    };
    let candidates = client().search(&input).await.expect("search");
    assert!(candidates.is_empty());
    let outcome = matching::evaluate(&input, candidates, &ListSettings::default());
    assert_eq!(outcome.status, RequestStatus::NotFound);
}

// ===========================================================================
// Regression suite from REAL books the user hit during testing.
//
// Each replays that book's recorded live search and asserts the matching
// OUTCOME, so a future matcher change can't silently regress it. The shared
// invariant `assert_actionable_has_candidates` enforces the rule the user
// called out: an actionable status (Matched / NeedsSelection) must ALWAYS carry
// at least one candidate — never an empty "Needs you" dead-end.
// ===========================================================================

use libgen_core::model::Candidate;

async fn example_outcome(title: &str, author: &str) -> (RequestStatus, Vec<Candidate>) {
    let input = BookInput {
        title: title.into(),
        authors: vec![author.into()],
        ..Default::default()
    };
    let cands = client().search(&input).await.unwrap_or_default();
    let out = matching::evaluate(&input, cands, &ListSettings::default());
    (out.status, out.ranked)
}

/// The invariant the user demanded: a status that asks the user to act MUST have
/// something to act on.
fn assert_actionable_has_candidates(status: &RequestStatus, cands: &[Candidate]) {
    if matches!(
        status,
        RequestStatus::Matched | RequestStatus::NeedsSelection
    ) {
        assert!(
            !cands.is_empty(),
            "status {status:?} surfaced with ZERO candidates — an empty \
             Needs-you/Matched dead-end (should be NotFound)"
        );
    }
}

fn has_title_token(cands: &[Candidate], token: &str) -> bool {
    cands.iter().any(|c| c.title.to_lowercase().contains(token))
}

#[tokio::test]
async fn example_time_machine_auto_matches_with_author_in_title() {
    // The only libgen hit is a review row whose title is fully the request +
    // "by H. G. Wells"; title + author-in-title is confident → auto-match (no
    // needless "Needs you" confirm).
    let (status, cands) = example_outcome("The Time Machine: An Invention", "H. G. Wells").await;
    assert_eq!(status, RequestStatus::Matched);
    assert_actionable_has_candidates(&status, &cands);
    assert!(
        has_title_token(&cands, "time machine"),
        "{:?}",
        titles(&cands)
    );

    // Regression (journal-article title cell): this row's title must be the
    // ARTICLE title "The Time Machine: An Invention by H. G. Wells" (md5
    // 2c3befd4…), NOT the issue marker "vol. 69 iss. 3" that sits in the bold.
    assert!(
        cands.iter().any(|c| c
            .title
            .to_lowercase()
            .contains("the time machine: an invention")),
        "the journal-article title must be extracted, got {:?}",
        titles(&cands)
    );
    assert!(
        !cands
            .iter()
            .any(|c| c.title.to_lowercase().starts_with("vol.")),
        "an issue-marker leaked as a title: {:?}",
        titles(&cands)
    );
    assert!(
        cands.iter().any(|c| c.md5.starts_with("2c3befd4")),
        "the specific Boletim pdf must be present: {:?}",
        cands.iter().map(|c| &c.md5).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn example_heidi_needs_selection_with_candidates() {
    let (status, cands) = example_outcome(
        "Heidi: Her Years of Wandering and Learning",
        "Johanna Spyri",
    )
    .await;
    assert_eq!(status, RequestStatus::NeedsSelection);
    assert_actionable_has_candidates(&status, &cands);
    assert!(has_title_token(&cands, "heidi"), "{:?}", titles(&cands));
}

#[tokio::test]
async fn example_wind_in_the_willows_needs_selection_with_candidate() {
    let (status, cands) = example_outcome("The Wind in the Willows", "Kenneth Grahame").await;
    assert_eq!(status, RequestStatus::NeedsSelection);
    assert_actionable_has_candidates(&status, &cands);
    assert!(has_title_token(&cands, "willows"), "{:?}", titles(&cands));
}

#[tokio::test]
async fn example_jungle_book_matches() {
    // Found cleanly → auto-matched (so it must NOT be a "Check download" / needs
    // case on a fresh query).
    let (status, cands) = example_outcome("The Jungle Book", "Rudyard Kipling").await;
    assert_eq!(status, RequestStatus::Matched);
    assert_actionable_has_candidates(&status, &cands);
}

#[tokio::test]
async fn example_oz_matches_an_oz_book() {
    let (status, cands) = example_outcome("The Wonderful Wizard of Oz", "L. Frank Baum").await;
    assert_eq!(status, RequestStatus::Matched);
    assert_actionable_has_candidates(&status, &cands);
    assert!(
        has_title_token(&cands, "wizard of oz"),
        "{:?}",
        titles(&cands)
    );
}

#[tokio::test]
async fn example_alice_exact_base_title_ranks_first() {
    // Ranking regression: the exact base title "Alice's Adventures in Wonderland
    // by Lewis Carroll" (author embedded in the title, sparse author field) must
    // rank #1 and auto-match — NOT be out-ranked by a different series volume
    // ("Wonderland Revisited: Another Alice's Adventures in Wonderland").
    let (status, cands) =
        example_outcome("Alice's Adventures in Wonderland", "Lewis Carroll").await;
    assert_eq!(status, RequestStatus::Matched);
    assert_actionable_has_candidates(&status, &cands);
    assert_eq!(
        cands[0].title,
        "Alice's Adventures in Wonderland by Lewis Carroll",
        "exact base title must rank #1, got {:?}",
        titles(&cands)
    );
}

fn titles(cands: &[Candidate]) -> Vec<&str> {
    cands.iter().map(|c| c.title.as_str()).collect()
}
