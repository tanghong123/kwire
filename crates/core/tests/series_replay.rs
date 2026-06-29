//! Offline integration test: drive `SeriesClient` in replay mode against
//! recorded Open Library responses, asserting the parse + ordering recipe end to
//! end. Runs with NO network. Mirrors how `query_replay.rs` loads fixtures.

use libgen_core::series::{GoodreadsClient, LibgenSeriesClient, SeriesClient};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../crates/core ; fixtures live at the repo root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("series")
}

fn client() -> SeriesClient {
    SeriesClient::replay(fixtures_dir())
}

#[tokio::test]
async fn oz_open_library() {
    oz_returns_ordered_members_from_replay().await;
}

#[tokio::test]
async fn alice_libgen_tpb() {
    // Source B (libgen series.php). From the Alice SEARCH page the resolver
    // finds TWO series links — 364378 (TPB: real titles + a volume column) and
    // 364379 (Strip: by-year, NO titles). It MUST pick the TPB/book series.
    let series = LibgenSeriesClient::replay(fixtures_dir())
        .lookup("Alice's Adventures in Wonderland", "Lewis Carroll")
        .await
        .expect("libgen lookup ok")
        .expect("Alice resolves via libgen series.php");

    // The TPB id, NOT the Strip id (364379).
    assert_eq!(
        series.key, "libgen:364378",
        "must pick the TPB series, not Strip"
    );

    // ≥ 6 ordered volumes, each with a real title and a downloadable md5.
    assert!(
        series.members.len() >= 6,
        "expected ≥6 TPB volumes, got {:?}",
        series.members.iter().map(|m| &m.title).collect::<Vec<_>>()
    );
    let titles: Vec<&str> = series.members.iter().map(|m| m.title.as_str()).collect();
    // Ground-truth titles present (the wide title cell, NOT the year).
    for t in [
        "Alice's Adventures in Wonderland",
        "Through the Looking-Glass",
        "The Hunting of the Snark",
        "Sylvie and Bruno",
        "The Nursery Alice",
    ] {
        assert!(titles.contains(&t), "missing TPB volume {t}: {titles:?}");
    }
    // No member title is a bare year (the regex-vs-cell trap the doc warns about).
    for t in &titles {
        assert!(
            t.parse::<u32>().is_err(),
            "a year leaked in as a title: {t}"
        );
    }
    // Ordered ascending by volume number, all present (1..=N).
    let positions: Vec<Option<u32>> = series.members.iter().map(|m| m.position).collect();
    assert_eq!(
        positions,
        (1..=series.members.len() as u32)
            .map(Some)
            .collect::<Vec<_>>(),
        "members must be in volume order"
    );
    // Members carry downloadable md5s from the cover path.
    assert!(
        series.members.iter().all(|m| m.md5.is_some()),
        "every libgen member should carry a cover md5"
    );
    // Every member carries an author (the row's author cell, else the seed) so
    // the seeded list's libgen query is never title-only.
    assert!(
        series
            .members
            .iter()
            .all(|m| m.author.as_deref().map(|a| !a.is_empty()) == Some(true)),
        "every libgen member should carry an author: {:?}",
        series
            .members
            .iter()
            .map(|m| (&m.title, &m.author))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn oz_libgen_rejects_unrelated_co_listed_series() {
    // Honest-evaluation regression: libgen's `series.php` has NO genuine "Wizard
    // of Oz" series. A plain "The Wonderful Wizard of Oz" search surfaces only
    // UNRELATED series ("Flying Machine Stories", "Kansas Local Histories")
    // whose name is unrelated and whose member overlap is incidental. The
    // resolver must REJECT both rather than return 341 bogus members.
    let res = LibgenSeriesClient::replay(fixtures_dir())
        .lookup("The Wonderful Wizard of Oz", "L. Frank Baum")
        .await
        .expect("libgen lookup ok");
    assert!(
        res.is_none(),
        "libgen must not return an unrelated co-listed series for Oz: {:?}",
        res.map(|s| (s.key, s.name, s.members.len()))
    );
}

#[tokio::test]
async fn alice_goodreads() {
    // Source C (Goodreads). autocomplete → book → series, server-rendered.
    let series = GoodreadsClient::replay(fixtures_dir())
        .lookup("Alice's Adventures in Wonderland", "Lewis Carroll")
        .await
        .expect("goodreads lookup ok")
        .expect("Alice resolves via Goodreads");

    assert_eq!(series.key, "goodreads:146183");
    assert!(
        series.members.len() >= 6,
        "expected ≥6 Goodreads volumes, got {}",
        series.members.len()
    );
    // The human-curated #N order: real book per position, in sequence.
    assert_eq!(series.members[0].title, "Alice's Adventures in Wonderland");
    assert_eq!(series.members[1].title, "Through the Looking-Glass");
    assert_eq!(series.members[2].title, "The Hunting of the Snark");
    let positions: Vec<Option<u32>> = series.members.iter().take(6).map(|m| m.position).collect();
    assert_eq!(positions, (1..=6).map(Some).collect::<Vec<_>>());
    // Box sets / bundles are filtered out of the member list.
    for m in &series.members {
        let l = m.title.to_lowercase();
        assert!(
            !l.contains("box set") && !l.contains("book set") && !l.contains("bundle"),
            "a box set / bundle leaked into Goodreads members: {}",
            m.title
        );
    }
}

async fn oz_returns_ordered_members_from_replay() {
    let series = client()
        .lookup("The Wonderful Wizard of Oz", "L. Frank Baum")
        .await
        .expect("lookup ok")
        .expect("Oz is in a series");

    assert_eq!(series.key, "OL329664L");
    assert_eq!(series.name, "The Wonderful Wizard of Oz");
    // The validated recipe yields 14 Oz members.
    assert_eq!(
        series.members.len(),
        14,
        "got {:?}",
        series.members.iter().map(|m| &m.title).collect::<Vec<_>>()
    );

    // Ordered by series position 1..=14 (every member has one).
    let positions: Vec<Option<u32>> = series.members.iter().map(|m| m.position).collect();
    assert_eq!(
        positions,
        (1..=14).map(Some).collect::<Vec<_>>(),
        "members must be in reading order"
    );

    // Position 1 is the seed book; position 2 is "The Marvelous Land of Oz"; a
    // later entry carries its per-book subtitle as "Title: Subtitle".
    assert_eq!(series.members[0].title, "The Wonderful Wizard of Oz");
    assert_eq!(series.members[1].title, "The Marvelous Land of Oz");
    assert!(
        series
            .members
            .iter()
            .any(|m| m.title == "Ozma of Oz: The Royal Book"),
        "subtitle members render as 'Title: Subtitle': {:?}",
        series.members.iter().map(|m| &m.title).collect::<Vec<_>>()
    );

    // Every member inherits the seed author so the seeded list's libgen query
    // carries author corroboration (never title-only).
    assert!(
        series
            .members
            .iter()
            .all(|m| m.author.as_deref() == Some("L. Frank Baum")),
        "members must inherit the seed author: {:?}",
        series
            .members
            .iter()
            .map(|m| (&m.title, &m.author))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn standalone_book_is_not_in_a_series() {
    // The work JSON for this title has no `series` field → Ok(None).
    let res = client()
        .lookup("Anne of Green Gables", "L. M. Montgomery")
        .await
        .expect("lookup ok");
    assert!(res.is_none(), "a standalone book is not part of a series");
}

#[tokio::test]
async fn untagged_series_recovered_by_title_prefix_fallback() {
    // Uncle Wiggily has NO `series` field on Open Library, and the seed title
    // doesn't even appear in the limit=5 work search → the primary path yields
    // nothing. The TITLE-PREFIX fallback then reconstructs the series from
    // sibling titles, dropping box sets / collections.
    let series = client()
        .lookup("Uncle Wiggily: The Bedtime Stories", "Howard R. Garis")
        .await
        .expect("lookup ok")
        .expect("untagged series recovered via fallback");

    // Synthetic prefix key + the derived series name.
    assert_eq!(series.key, "prefix:uncle-wiggily");
    assert_eq!(series.name, "Uncle Wiggily");

    let titles: Vec<&str> = series.members.iter().map(|m| m.title.as_str()).collect();

    // Box sets / collections / number-range bundles are filtered out.
    for t in &titles {
        let l = t.to_lowercase();
        assert!(
            !l.contains("box set") && !l.contains("boxed set") && !l.contains("collection"),
            "box set / collection leaked into members: {t}"
        );
    }
    // The non-prefix sibling ("The Little Book of Bedtime") is dropped.
    assert!(
        !titles.iter().any(|t| t.contains("Little Book of Bedtime")),
        "non-prefix sibling should be dropped: {titles:?}"
    );

    // Real individual volumes are recovered (≥ 2 required to call it a series).
    assert!(series.members.len() >= 2, "got {titles:?}");
    assert!(titles.iter().any(|t| t.contains("Airship")));
    assert!(titles.iter().any(|t| t.contains("Travels")));

    // Members carry 1-based positions in derived order.
    let positions: Vec<Option<u32>> = series.members.iter().map(|m| m.position).collect();
    assert_eq!(
        positions,
        (1..=series.members.len() as u32)
            .map(Some)
            .collect::<Vec<_>>(),
        "members must carry sequential 1-based positions"
    );
}

#[tokio::test]
async fn standalone_with_subtitle_is_not_a_series_via_fallback() {
    // "Walden: Life in the Woods" is a standalone. The primary path finds
    // no series; the fallback searches for the "Walden" prefix but only ONE
    // plausible volume survives (the lone "Walden"; "A Week on the Concord
    // River" does not start with the prefix) → below the ≥2 threshold
    // → Ok(None). Guards against "<Title>: A <thing>" false positives.
    let res = client()
        .lookup("Walden: Life in the Woods", "Henry David Thoreau")
        .await
        .expect("lookup ok");
    assert!(
        res.is_none(),
        "a standalone with a subtitle must not be detected as a series: {res:?}"
    );
}

#[tokio::test]
async fn zero_members_falls_back_to_the_single_book() {
    // This work HAS a series key, but the members search returns zero docs and
    // the HTML series page has no /works/ links → fall back to just the seed.
    let series = client()
        .lookup("A Solitary Volume", "Sole Author")
        .await
        .expect("lookup ok")
        .expect("series key present → Some");
    assert_eq!(series.key, "OL999999L");
    assert_eq!(
        series.members.len(),
        1,
        "fallback is the requesting book only"
    );
    assert_eq!(series.members[0].title, "A Solitary Volume");
}
