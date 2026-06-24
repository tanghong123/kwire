//! Standalone harness for the DOWNLOAD WORKER POOL (docs/DOWNLOAD_SCHEDULING.md).
//!
//! Drives the REAL execution engine headlessly (`testsupport::spawn_engine` →
//! tick → download-worker pool → `begin_download`/`apply_progress`) against a
//! mock host, with full control over when downloads start (goals are `Idle`
//! until `start`). It asserts the properties the app couldn't show reliably:
//!   * every book actually reaches `Done` (state transitions are persisted),
//!   * no more than `G` (`max_concurrent_downloads`) download at once — the pool
//!     bounds global concurrency, books beyond `G` wait QUEUED (no spawned-and-
//!     parked "waiting for a slot"),
//!   * with `N > G` books and a high per-host cap, the pool reaches `G` in
//!     parallel (it isn't accidentally serialized).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libgen_app_lib::commands::testsupport;
use libgen_app_lib::state::AppState;

use libgen_core::download::{md5_hex, DirectUrlResolver, ResolverChain};
use libgen_core::model::{
    BookInput, BookRequest, Candidate, DownloadJob, DownloadList, Format, Group, JobState,
    ListSettings, RequestStatus,
};
use libgen_core::queue::{HostLimits, Scheduler, SchedulerBuilder};
use reqwest::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Mock host: serves blobs slowly, tracks peak concurrent transfers.
// ---------------------------------------------------------------------------

struct MockHost {
    addr: SocketAddr,
    bodies: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>,
    peak: Arc<AtomicUsize>,
}

impl MockHost {
    async fn start(delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (b, i, p) = (bodies.clone(), inflight.clone(), peak.clone());
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let (b, i, p) = (b.clone(), i.clone(), p.clone());
                tokio::spawn(async move {
                    let _ = serve(sock, b, i, p, delay).await;
                });
            }
        });
        MockHost { addr, bodies, peak }
    }

    fn template(&self) -> String {
        format!("http://{}/get/{{md5}}", self.addr)
    }

    async fn set(&self, md5: &str, body: Vec<u8>) {
        self.bodies
            .lock()
            .await
            .insert(format!("/get/{md5}"), Arc::new(body));
    }
}

async fn serve(
    mut sock: TcpStream,
    bodies: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>,
    inflight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    delay: Duration,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let path = head
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let body = bodies.lock().await.get(&path).cloned();
    let body = match body {
        Some(b) => b,
        None => {
            sock.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
            return Ok(());
        }
    };
    let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
    peak.fetch_max(now, Ordering::SeqCst);
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(header.as_bytes()).await;
    let mid = body.len() / 2;
    let _ = sock.write_all(&body[..mid]).await;
    let _ = sock.flush().await;
    tokio::time::sleep(delay).await; // hold the transfer so concurrency is observable
    let _ = sock.write_all(&body[mid..]).await;
    let _ = sock.flush().await;
    inflight.fetch_sub(1, Ordering::SeqCst);
    Ok(())
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    static CTR: AtomicUsize = AtomicUsize::new(0);
    p.push(format!(
        "lgdl-pool-{}-{}",
        std::process::id(),
        CTR.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// One book already `Matched` with a single selected `Pending` epub variation,
/// keyed by `md5`/`size` (ready to download once its goal is raised to Complete).
fn book(title: &str, md5: &str, size: u64) -> BookRequest {
    let mut req = BookRequest::new(BookInput {
        title: title.into(),
        authors: vec!["Author".into()],
        ..Default::default()
    });
    req.status = RequestStatus::Matched;
    req.selected = Some(md5.to_string());
    req.candidates = vec![Candidate {
        md5: md5.to_string(),
        title: title.into(),
        authors: vec!["Author".into()],
        year: None,
        publisher: None,
        language: None,
        pages: None,
        extension: Some(Format::Epub),
        size_bytes: Some(size),
        source_host: None,
        cover_url: None,
        score: 1.0,
        job: Some(DownloadJob {
            state: JobState::Pending,
            ..Default::default()
        }),
    }];
    req
}

fn mock_scheduler(template: &str) -> Arc<Scheduler> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let resolver = DirectUrlResolver::new("mock", template.to_string(), client.clone());
    let chain = ResolverChain::new(vec![Arc::new(resolver)]);
    Arc::new(
        SchedulerBuilder::new(chain, client)
            // Per-host cap HIGH so the GLOBAL worker pool (G) is the binding limit.
            .default_limits(HostLimits {
                max_concurrency: 32,
                min_interval: Duration::from_millis(0),
                max_attempts: 3,
            })
            .build(),
    )
}

async fn count_done(state: &AppState, id: &str) -> usize {
    match testsupport::snapshot(state, id).await {
        Some(list) => list
            .groups
            .iter()
            .flat_map(|g| &g.books)
            .filter(|b| b.status == RequestStatus::Done)
            .count(),
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// The harness test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn worker_pool_downloads_all_books_bounded_by_g() {
    const N: usize = 12;
    const G: usize = 4;

    let server = MockHost::start(Duration::from_millis(120)).await;

    // N distinct blobs → N md5s, all served by the one mock host.
    let mut books = Vec::new();
    for i in 0..N {
        let blob: Vec<u8> = (0..2000).map(|b| ((b + i * 7) % 251) as u8).collect();
        let md5 = md5_hex(&blob);
        server.set(&md5, blob.clone()).await;
        books.push(book(&format!("Book {i}"), &md5, blob.len() as u64));
    }
    let mut g = Group::new("Batch");
    g.books = books;
    let list = DownloadList {
        title: "Pool".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    };

    // Headless app state with a fixed worker pool of G + the mock-host scheduler.
    let dir = tempdir();
    let state = testsupport::app_state(
        dir.join("library.sqlite3"),
        repo_root().join("fixtures").join("search"),
        repo_root().join("mirrors.toml"),
    );
    testsupport::set_max_concurrent_downloads(&state, G);
    testsupport::set_scheduler(&state, mock_scheduler(&server.template())).await;
    let id = testsupport::load(&state, &list).await.unwrap();

    // Start the engine; nothing downloads yet (goals are Idle). Then START.
    testsupport::spawn_engine(&state);
    testsupport::start(&state, &id).await.unwrap();

    // Poll until every book is Done (or time out).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if count_done(&state, &id).await == N {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out: only {}/{N} books reached Done",
            count_done(&state, &id).await
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Every book downloaded + persisted as Done.
    assert_eq!(count_done(&state, &id).await, N);

    // The worker pool never exceeded G concurrent transfers, and DID parallelize
    // up to G (so books beyond G genuinely waited queued, not all-at-once).
    let peak = server.peak.load(Ordering::SeqCst);
    assert!(peak <= G, "worker pool exceeded G: peak={peak}, G={G}");
    assert!(
        peak >= 2,
        "expected the pool to parallelize up to G={G}, but peak was only {peak}"
    );
}

// A book with variation A requested (Pending) and variation B merely available
// (a candidate with NO job) — so B can be requested mid-flight.
fn book_a_pending_b_available(title: &str, md5a: &str, md5b: &str, size: u64) -> BookRequest {
    let mut req = BookRequest::new(BookInput {
        title: title.into(),
        authors: vec!["Author".into()],
        ..Default::default()
    });
    req.status = RequestStatus::Matched;
    req.selected = Some(md5a.to_string());
    let mk = |md5: &str, with_job: bool| Candidate {
        md5: md5.to_string(),
        title: title.into(),
        authors: vec!["Author".into()],
        year: None,
        publisher: None,
        language: None,
        pages: None,
        extension: Some(Format::Epub),
        size_bytes: Some(size),
        source_host: None,
        cover_url: None,
        score: 1.0,
        job: with_job.then(|| DownloadJob {
            state: JobState::Pending,
            ..Default::default()
        }),
    };
    req.candidates = vec![mk(md5a, true), mk(md5b, false)];
    req
}

async fn done_variations(state: &AppState, id: &str) -> usize {
    match testsupport::snapshot(state, id).await {
        Some(list) => list.groups[0].books[0]
            .candidates
            .iter()
            .filter(|c| {
                c.job
                    .as_ref()
                    .map(|j| j.state == JobState::Done)
                    .unwrap_or(false)
            })
            .count(),
        None => 0,
    }
}

/// The fix: two variations of ONE book download in PARALLEL even when the second is
/// requested AFTER the first is already in flight. Before the per-variation dispatch
/// change the book's in-flight key blocked the second variation until the first
/// finished (serial) — so the transfers would never overlap (peak == 1).
#[tokio::test]
async fn second_variation_requested_midflight_downloads_in_parallel() {
    // Slow host so the first transfer is comfortably still running when we request
    // the second — the overlap is what we measure.
    let server = MockHost::start(Duration::from_millis(600)).await;
    let blob_a: Vec<u8> = (0..3000).map(|b| (b % 251) as u8).collect();
    let blob_b: Vec<u8> = (0..3000).map(|b| ((b + 123) % 251) as u8).collect();
    let md5a = md5_hex(&blob_a);
    let md5b = md5_hex(&blob_b);
    server.set(&md5a, blob_a.clone()).await;
    server.set(&md5b, blob_b.clone()).await;

    let mut g = Group::new("Batch");
    g.books = vec![book_a_pending_b_available("Peter Pan", &md5a, &md5b, 3000)];
    let list = DownloadList {
        title: "P".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    };

    let dir = tempdir();
    let state = testsupport::app_state(
        dir.join("library.sqlite3"),
        repo_root().join("fixtures").join("search"),
        repo_root().join("mirrors.toml"),
    );
    // G >= 2 so per-variation dispatch CAN parallelize (G bounds concurrent downloads).
    testsupport::set_max_concurrent_downloads(&state, 4);
    testsupport::set_scheduler(&state, mock_scheduler(&server.template())).await;
    let id = testsupport::load(&state, &list).await.unwrap();
    testsupport::spawn_engine(&state);
    testsupport::start(&state, &id).await.unwrap();

    // Wait until variation A is actually transferring (the host has a live request),
    // then request variation B MID-FLIGHT.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if server.peak.load(Ordering::SeqCst) >= 1 {
            break;
        }
        assert!(Instant::now() < deadline, "variation A never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    testsupport::request_variation_at(&state, &id, &[0], 0, &md5b)
        .await
        .unwrap();

    // Both variations complete...
    loop {
        if done_variations(&state, &id).await == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out: only {}/2 variations Done",
            done_variations(&state, &id).await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // ...and they OVERLAPPED: peak concurrency reached 2, proving the mid-flight
    // second variation dispatched in parallel rather than waiting for the first.
    let peak = server.peak.load(Ordering::SeqCst);
    assert!(
        peak >= 2,
        "two variations of one book did not download in parallel (peak={peak}) — \
         the second was serialized behind the first"
    );
}
