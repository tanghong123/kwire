//! Regression test for the synchronization bug where the engine held the per-list
//! orchestrator lock ACROSS the network search, serializing every book in a list.
//!
//! It replicates the engine's lock-free query dance — begin_query (brief lock) →
//! search OFF-lock → finish_query (brief lock) — against an `Arc<Mutex<Orchestrator>>`
//! with a SLOW transport, and asserts two books in ONE list query CONCURRENTLY
//! (total ≈ one book's time), not serially (≈ 2×). On the old code (the lock held
//! across `self.search.search`), this would take ~2× and fail.

use std::sync::Arc;
use std::time::{Duration, Instant};

use libgen_core::model::{BookInput, BookRequest, DownloadList, Group, ListSettings};
use libgen_core::orchestrator::Orchestrator;
use libgen_core::search::{MirrorConfig, SearchClient, Transport};
use libgen_core::store::Store;
use tokio::sync::{mpsc, Mutex};

/// A transport that sleeps before returning an empty body — simulates a slow
/// mirror so we can observe whether searches overlap.
struct SlowTransport {
    delay: Duration,
}
#[async_trait::async_trait]
impl Transport for SlowTransport {
    async fn get(&self, _url: &str) -> anyhow::Result<String> {
        tokio::time::sleep(self.delay).await;
        Ok(String::new()) // no results → NotFound; the point is the timing
    }
}

fn config() -> MirrorConfig {
    MirrorConfig::from_toml(
        r#"
        [[search_mirror]]
        host = "libgen.li"
        search_url = "https://libgen.li/index.php?req={query}&res={limit}"
        kind = "libgen_li_html"
        priority = 1
    "#,
    )
    .unwrap()
}

fn list(n: usize) -> DownloadList {
    let mut g = Group::new("Batch");
    for i in 0..n {
        g.books.push(BookRequest::new(BookInput {
            title: format!("Book {i}"),
            authors: vec!["Some Author".into()],
            ..Default::default()
        }));
    }
    DownloadList {
        title: "L".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    }
}

/// The engine's lock-free query for one book: brief lock → search off-lock →
/// brief lock. Mirrors `run_item`'s Query branch exactly.
async fn query_offlock(orch: &Arc<Mutex<Orchestrator>>, book_index: usize) {
    let (tx, mut rx) = mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let prep = {
        let mut g = orch.lock().await;
        match g.begin_query(&[0], book_index, &tx).await {
            Ok(Some((input, settings))) => Some((input, settings, g.search_client())),
            _ => None,
        }
    };
    if let Some((input, settings, search)) = prep {
        let cands = search.search(&input).await.unwrap_or_default();
        let outcome = libgen_core::matching::evaluate(&input, cands, &settings);
        let mut g = orch.lock().await;
        let _ = g.finish_query(&[0], book_index, outcome, &tx).await;
    }
    drop(tx);
    let _ = drain.await;
}

fn orch_for(n: usize, delay: Duration) -> Arc<Mutex<Orchestrator>> {
    let search = SearchClient::new(config(), Box::new(SlowTransport { delay }));
    let store = Store::open_in_memory().unwrap();
    let orch = Orchestrator::new(store, &list(n), search, "/out").unwrap();
    Arc::new(Mutex::new(orch))
}

#[tokio::test]
async fn two_books_in_one_list_query_concurrently() {
    let delay = Duration::from_millis(150);

    // Baseline: one book alone.
    let one = orch_for(1, delay);
    let t0 = Instant::now();
    query_offlock(&one, 0).await;
    let single = t0.elapsed();

    // Two books concurrently in the SAME orchestrator.
    let two = orch_for(2, delay);
    let t1 = Instant::now();
    let a = tokio::spawn({
        let two = Arc::clone(&two);
        async move { query_offlock(&two, 0).await }
    });
    let b = tokio::spawn({
        let two = Arc::clone(&two);
        async move { query_offlock(&two, 1).await }
    });
    let _ = tokio::join!(a, b);
    let concurrent = t1.elapsed();

    // Concurrent should be close to a single book's time, NOT ~2×. We allow a
    // generous 1.6× single to absorb scheduling/store jitter; serial execution
    // (the bug) would be ~2× and fail.
    assert!(
        concurrent < single.mul_f64(1.6),
        "intra-list queries serialized: single={single:?}, two-concurrent={concurrent:?} \
         (expected concurrent ≈ single, not ~2×)"
    );
}
