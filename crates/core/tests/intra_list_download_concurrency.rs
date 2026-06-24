//! Regression test for the download half of the synchronization bug: the engine
//! used to hold the per-list orchestrator lock ACROSS `scheduler.run` + the whole
//! progress loop, so every book in a list downloaded one-at-a-time.
//!
//! This replicates the engine's lock-free download dance — `begin_download` (brief
//! lock → plan + spawn `scheduler.run`) → drain `rx` OFF-lock (`apply_progress`
//! under a BRIEF lock per tick) → `finish_download` (brief lock) — against an
//! `Arc<Mutex<Orchestrator>>` with a SLOW mock host, and asserts two books in ONE
//! list download CONCURRENTLY (total ≈ one book's time), not serially (≈ 2×). On
//! the lock-across-transfer regression this takes ~2× and fails.
//!
//! Plus a cancellation test: a `Downloading` book whose transfer is aborted via
//! `scheduler.cancel(md5)` settles promptly to `Progress::Cancelled` (and, for a
//! pause, the `.part` + resume_offset are kept).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libgen_core::download::{md5_hex, part_path, DirectUrlResolver};
use libgen_core::model::{
    BookInput, BookRequest, Candidate, DownloadJob, DownloadList, Format, Group, JobState,
    ListSettings, RequestStatus,
};
use libgen_core::orchestrator::{Event, Orchestrator};
use libgen_core::queue::{HostLimits, Progress, Scheduler, SchedulerBuilder};
use libgen_core::search::{MirrorConfig, SearchClient, Transport};
use libgen_core::store::Store;
use reqwest::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};

// ---------------------------------------------------------------------------
// Minimal mock HTTP host (Range-aware, programmably slow). Self-contained so the
// test has no extra mock-server dependency.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct HostBody {
    body: Arc<Vec<u8>>,
    delay: Duration,
}

struct MockHost {
    addr: SocketAddr,
    bodies: Arc<Mutex<HashMap<String, HostBody>>>,
}

impl MockHost {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies: Arc<Mutex<HashMap<String, HostBody>>> = Arc::new(Mutex::new(HashMap::new()));
        let bodies_cl = bodies.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let bodies = bodies_cl.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, bodies).await;
                });
            }
        });
        MockHost { addr, bodies }
    }

    fn template(&self) -> String {
        format!("http://{}/get/{{md5}}", self.addr)
    }

    async fn set(&self, md5: &str, body: Vec<u8>, delay: Duration) {
        self.bodies.lock().await.insert(
            format!("/get/{md5}"),
            HostBody {
                body: Arc::new(body),
                delay,
            },
        );
    }
}

async fn handle_conn(
    mut sock: TcpStream,
    bodies: Arc<Mutex<HashMap<String, HostBody>>>,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 64 * 1024 {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let mut range_start: Option<u64> = None;
    for line in lines {
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("range:") {
            if let Some(eq) = v.find('=') {
                let start = v[eq + 1..].split('-').next().unwrap_or("").trim();
                if let Ok(s) = start.parse::<u64>() {
                    range_start = Some(s);
                }
            }
        }
    }

    let cfg = bodies.lock().await.get(&path).cloned();
    let cfg = match cfg {
        Some(c) => c,
        None => {
            sock.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
            return Ok(());
        }
    };

    let body: Vec<u8> = (*cfg.body).clone();
    let total = body.len() as u64;
    let (status_line, slice): (String, Vec<u8>) = match range_start {
        Some(start) if start <= total => {
            let end = total.saturating_sub(1);
            (
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\n"
                ),
                body[start as usize..].to_vec(),
            )
        }
        _ => ("HTTP/1.1 200 OK\r\n".to_string(), body),
    };
    let header = format!(
        "{status_line}Content-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        slice.len()
    );
    sock.write_all(header.as_bytes()).await?;
    if cfg.delay.is_zero() {
        sock.write_all(&slice).await?;
    } else {
        let mid = slice.len() / 2;
        sock.write_all(&slice[..mid]).await?;
        sock.flush().await?;
        tokio::time::sleep(cfg.delay).await;
        sock.write_all(&slice[mid..]).await?;
    }
    sock.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Orchestrator + scheduler fixtures
// ---------------------------------------------------------------------------

/// A transport that never produces results (we drive downloads directly, not via
/// search) — the orchestrator just needs a search client to exist.
struct EmptyTransport;
#[async_trait::async_trait]
impl Transport for EmptyTransport {
    async fn get(&self, _url: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

fn mirror_config() -> MirrorConfig {
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

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

/// Build a scheduler routed at the mock host's direct-URL template, capped so two
/// books can download at once (so the test measures the per-list lock, not the
/// host cap).
fn scheduler_for(template: &str) -> Arc<Scheduler> {
    let resolver = DirectUrlResolver::new("mock", template.to_string(), client());
    let chain = libgen_core::download::ResolverChain::new(vec![Arc::new(resolver)]);
    Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 4,
                min_interval: Duration::from_millis(0),
                max_attempts: 2,
            })
            .build(),
    )
}

/// One book with a single `Matched` epub candidate whose job is `Pending` (ready
/// to download), keyed by `md5`/`size`.
fn book_with_pending(title: &str, md5: &str, size: u64) -> BookRequest {
    let mut req = BookRequest::new(BookInput {
        title: title.into(),
        authors: vec!["Some Author".into()],
        ..Default::default()
    });
    req.status = RequestStatus::Matched;
    req.selected = Some(md5.to_string());
    req.candidates = vec![Candidate {
        md5: md5.to_string(),
        title: title.into(),
        authors: vec!["Some Author".into()],
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

fn list_with(books: Vec<BookRequest>) -> DownloadList {
    let mut g = Group::new("Batch");
    g.books = books;
    DownloadList {
        title: "L".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    }
}

fn orch_for(list: &DownloadList, out_dir: &std::path::Path) -> Arc<Mutex<Orchestrator>> {
    let search = SearchClient::new(mirror_config(), Box::new(EmptyTransport));
    let store = Store::open_in_memory().unwrap();
    let orch = Orchestrator::new(store, list, search, out_dir).unwrap();
    Arc::new(Mutex::new(orch))
}

/// The engine's lock-free download for one book: brief lock (begin) → drain rx
/// OFF-lock (apply_progress under brief lock per tick) → brief lock (finish).
/// Mirrors `run_item`'s Download branch exactly.
async fn download_offlock(orch: &Arc<Mutex<Orchestrator>>, scheduler: &Arc<Scheduler>, bi: usize) {
    let (tx, mut rx) = mpsc::channel::<Event>(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let session = {
        let mut g = orch.lock().await;
        g.begin_download(scheduler, &[0], bi, None, &tx)
            .await
            .ok()
            .flatten()
    };
    if let Some(mut session) = session {
        let mut completed: Vec<String> = Vec::new();
        while let Some(prog) = session.rx.recv().await {
            if let Progress::Done { md5, .. } = &prog {
                completed.push(md5.clone());
            }
            {
                let mut g = orch.lock().await;
                let _ = g.apply_progress(&session.pending, &prog);
            }
            let _ = tx.send(Event::Download(prog)).await;
        }
        let _ = session.run.await;
        let mut g = orch.lock().await;
        let _ = g.finish_download(&[0], bi, &completed).await;
    }
    drop(tx);
    let _ = drain.await;
}

fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    static CTR: AtomicUsize = AtomicUsize::new(0);
    let unique = format!(
        "lgdl-dl-test-{}-{}",
        std::process::id(),
        CTR.fetch_add(1, Ordering::SeqCst)
    );
    p.push(unique);
    std::fs::create_dir_all(&p).unwrap();
    p
}

// ---------------------------------------------------------------------------
// Test 1 — intra-list DOWNLOAD concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_books_in_one_list_download_concurrently() {
    let delay = Duration::from_millis(200);
    let server = MockHost::start().await;

    // Two distinct blobs → two md5s, both served slowly by the same mock host.
    let blob_a = (0..3000).map(|i| (i % 251) as u8).collect::<Vec<u8>>();
    let blob_b = (0..3000)
        .map(|i| ((i + 7) % 251) as u8)
        .collect::<Vec<u8>>();
    let md5_a = md5_hex(&blob_a);
    let md5_b = md5_hex(&blob_b);
    server.set(&md5_a, blob_a.clone(), delay).await;
    server.set(&md5_b, blob_b.clone(), delay).await;
    let scheduler = scheduler_for(&server.template());

    // Baseline: one book alone.
    let out1 = tempdir();
    let one = orch_for(
        &list_with(vec![book_with_pending(
            "Book A",
            &md5_a,
            blob_a.len() as u64,
        )]),
        &out1,
    );
    let t0 = Instant::now();
    download_offlock(&one, &scheduler, 0).await;
    let single = t0.elapsed();

    // Two books concurrently in the SAME orchestrator.
    let out2 = tempdir();
    let two = orch_for(
        &list_with(vec![
            book_with_pending("Book A", &md5_a, blob_a.len() as u64),
            book_with_pending("Book B", &md5_b, blob_b.len() as u64),
        ]),
        &out2,
    );
    let t1 = Instant::now();
    let a = tokio::spawn({
        let two = Arc::clone(&two);
        let sched = Arc::clone(&scheduler);
        async move { download_offlock(&two, &sched, 0).await }
    });
    let b = tokio::spawn({
        let two = Arc::clone(&two);
        let sched = Arc::clone(&scheduler);
        async move { download_offlock(&two, &sched, 1).await }
    });
    let _ = tokio::join!(a, b);
    let concurrent = t1.elapsed();

    // Both books actually downloaded (Done).
    {
        let g = two.lock().await;
        let snap = g.snapshot().unwrap();
        for b in &snap.groups[0].books {
            assert_eq!(b.status, RequestStatus::Done, "book should be Done");
        }
    }

    // Concurrent should be close to a single book's time, NOT ~2×. Generous 1.6×
    // single absorbs store/scheduling jitter; serial (the bug) would be ~2×.
    assert!(
        concurrent < single.mul_f64(1.6),
        "intra-list downloads serialized: single={single:?}, two-concurrent={concurrent:?} \
         (expected concurrent ≈ single, not ~2×)"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — cancellation (pause keeps the partial)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_a_downloading_book_aborts_promptly_and_keeps_partial() {
    // A large, slow body so the transfer is reliably mid-flight when we cancel.
    let server = MockHost::start().await;
    let blob = (0..200_000).map(|i| (i % 251) as u8).collect::<Vec<u8>>();
    let md5 = md5_hex(&blob);
    server.set(&md5, blob.clone(), Duration::from_secs(5)).await;
    let scheduler = scheduler_for(&server.template());

    let out = tempdir();
    let orch = orch_for(
        &list_with(vec![book_with_pending(
            "Slow Book",
            &md5,
            blob.len() as u64,
        )]),
        &out,
    );

    // Begin the download (spawns scheduler.run); drive the drain on a task so the
    // transfer is in flight.
    let (tx, _rx0) = mpsc::channel::<Event>(256);
    let mut session = {
        let mut g = orch.lock().await;
        g.begin_download(&scheduler, &[0], 0, None, &tx)
            .await
            .unwrap()
            .expect("a pending download")
    };
    let dest = session.pending[0].destination.clone();

    // Wait until bytes have actually started flowing (a `.part` exists), so the
    // cancel hits an in-flight stream.
    let part = part_path(&dest);
    let mut waited = Duration::ZERO;
    while !part.exists() && waited < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }
    assert!(part.exists(), "download should have started a .part");

    // PAUSE-cancel (keep partial), as Stop does. Flip nothing under the orch lock
    // here — this mirrors the command calling scheduler.cancel/pause separately.
    let signalled = scheduler.pause(&md5).await;
    assert!(signalled, "an in-flight md5 should be signalable");

    // Drain: the transfer must abort promptly with Progress::Cancelled{paused}.
    let cancel_deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_cancelled = false;
    let mut kept_offset = 0u64;
    while let Ok(Some(prog)) =
        tokio::time::timeout_at(cancel_deadline.into(), session.rx.recv()).await
    {
        if let Progress::Cancelled {
            paused,
            resume_offset,
            ..
        } = &prog
        {
            assert!(*paused, "pause should keep the partial");
            kept_offset = *resume_offset;
            saw_cancelled = true;
        }
        let mut g = orch.lock().await;
        let _ = g.apply_progress(&session.pending, &prog);
    }
    let _ = session.run.await;
    assert!(saw_cancelled, "should have observed Progress::Cancelled");

    // The partial (.part) is kept and resume_offset reflects its length (> 0).
    assert!(part.exists(), "paused download keeps its .part");
    assert!(kept_offset > 0, "paused resume_offset should be preserved");

    // The persisted job is Paused with the kept resume_offset.
    let g = orch.lock().await;
    let snap = g.snapshot().unwrap();
    let job = snap.groups[0].books[0].candidates[0].job.as_ref().unwrap();
    assert_eq!(job.state, JobState::Paused, "job settles to Paused");
    assert_eq!(job.resume_offset, kept_offset);
}
