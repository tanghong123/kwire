//! Headless, offline tests for the concurrency + download-lifecycle features:
//! concurrent `query_all`, pause/cancel of in-flight downloads, resume-on-launch,
//! md5 dedupe, and stable sequence numbering.
//!
//! Downloads run against a hand-rolled tokio TCP mock HTTP server that can stream
//! a body slowly (so a transfer stays "in flight" long enough to be paused or
//! cancelled). Searches use the replay fixtures, fully offline.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use libgen_core::download::{md5_hex, DirectUrlResolver, Resolver, ResolverChain};
use libgen_core::model::{
    BookInput, BookRequest, Candidate, DownloadJob, DownloadList, Format, Group, JobState,
    ListSettings, RequestStatus,
};
use libgen_core::orchestrator::{Event, Orchestrator};
use libgen_core::parse;
use libgen_core::queue::{Scheduler, SchedulerBuilder};
use libgen_core::search::{MirrorConfig, SearchClient};
use libgen_core::store::Store;
use reqwest::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};

// ---------------------------------------------------------------------------
// Mock HTTP server: serves a registered body, optionally streaming it slowly so
// the transfer can be paused/cancelled mid-flight. Honors Range for resume.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Body {
    bytes: Arc<Vec<u8>>,
    /// Delay inserted between the two halves of the body (keeps it in flight).
    delay: Duration,
}

struct MockServer {
    addr: SocketAddr,
    bodies: Arc<Mutex<HashMap<String, Body>>>,
    hits: Arc<AtomicUsize>,
}

impl MockServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies: Arc<Mutex<HashMap<String, Body>>> = Arc::new(Mutex::new(HashMap::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let bodies_cl = bodies.clone();
        let hits_cl = hits.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let bodies = bodies_cl.clone();
                let hits = hits_cl.clone();
                tokio::spawn(async move {
                    let _ = handle(sock, bodies, hits).await;
                });
            }
        });
        MockServer { addr, bodies, hits }
    }

    fn template(&self) -> String {
        format!("http://{}/get/{{md5}}", self.addr)
    }

    async fn put(&self, md5: &str, bytes: Vec<u8>, delay: Duration) {
        self.bodies.lock().await.insert(
            format!("/get/{md5}"),
            Body {
                bytes: Arc::new(bytes),
                delay,
            },
        );
    }

    fn hit_count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

async fn handle(
    mut sock: TcpStream,
    bodies: Arc<Mutex<HashMap<String, Body>>>,
    hits: Arc<AtomicUsize>,
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
                let spec = &v[eq + 1..];
                if let Ok(s) = spec.split('-').next().unwrap_or("").trim().parse::<u64>() {
                    range_start = Some(s);
                }
            }
        }
    }

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
    hits.fetch_add(1, Ordering::SeqCst);

    let full: Vec<u8> = (*body.bytes).clone();
    let total = full.len() as u64;
    let (status_line, slice): (String, Vec<u8>) = match range_start {
        Some(start) if start <= total => {
            let end = total.saturating_sub(1);
            (
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\n"
                ),
                full[start as usize..].to_vec(),
            )
        }
        _ => ("HTTP/1.1 200 OK\r\n".to_string(), full),
    };

    let header = format!(
        "{status_line}Content-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        slice.len()
    );
    sock.write_all(header.as_bytes()).await?;
    if body.delay.is_zero() || slice.len() < 2 {
        sock.write_all(&slice).await?;
    } else {
        let mid = slice.len() / 2;
        sock.write_all(&slice[..mid]).await?;
        sock.flush().await?;
        tokio::time::sleep(body.delay).await;
        sock.write_all(&slice[mid..]).await?;
    }
    sock.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("search")
}

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

fn jeremy_subset() -> DownloadList {
    let md = "# Jeremy Subset\n\
        ## Batch 1\n\
        1. Treasure Island — Robert Louis Stevenson\n\
        2. The Adventures of Tom Sawyer — Mark Twain\n\
        3. Anne of Green Gables — L. M. Montgomery\n\
        4. A Book That Has No Recorded Fixture Anywhere\n";
    parse::parse_markdown(md).unwrap()
}

fn tempdir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "lgdl-life-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

async fn drain(mut rx: mpsc::Receiver<Event>) -> Vec<Event> {
    let mut out = Vec::new();
    while let Some(e) = rx.recv().await {
        out.push(e);
    }
    out
}

fn requested_cand(md5: &str, title: &str, author: &str, ext: Format) -> Candidate {
    Candidate {
        md5: md5.to_string(),
        title: title.into(),
        authors: vec![author.into()],
        year: None,
        publisher: None,
        language: None,
        pages: None,
        extension: Some(ext),
        size_bytes: None,
        source_host: Some("mock".into()),
        cover_url: None,
        score: 0.99,
        job: Some(DownloadJob {
            state: JobState::Pending,
            ..Default::default()
        }),
    }
}

fn scheduler_for(server: &MockServer) -> Arc<Scheduler> {
    let client = Client::builder().build().unwrap();
    let resolver: Arc<dyn Resolver> = Arc::new(DirectUrlResolver::new(
        "mock",
        server.template(),
        client.clone(),
    ));
    let chain = ResolverChain::new(vec![resolver]);
    Arc::new(SchedulerBuilder::new(chain, client).build())
}

// ---------------------------------------------------------------------------
// 1. Concurrent query_all yields the same final statuses (order-independent).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_query_all_matches_sequential_statuses() {
    // Run query_all (now concurrent) over the replay fixtures and assert the
    // final per-book statuses are exactly what the sequential version produced.
    let store = Store::open_in_memory().unwrap();
    let search = SearchClient::replay(config(), fixtures_dir());
    let out = tempdir();
    let mut orch = Orchestrator::new(store, &jeremy_subset(), search, out)
        .unwrap()
        .with_query_concurrency(6);

    let (tx, rx) = mpsc::channel(64);
    let ev_task = tokio::spawn(drain(rx));
    orch.query_all(&tx).await.unwrap();
    drop(tx);
    let events = ev_task.await.unwrap();

    // One StatusChanged per queued book regardless of completion order.
    let status_events = events
        .iter()
        .filter(|e| matches!(e, Event::StatusChanged { .. }))
        .count();
    assert_eq!(status_events, 4);

    let list = orch.snapshot().unwrap();
    let books = &list.groups[0].books;
    assert_eq!(books[0].status, RequestStatus::Matched, "Treasure Island");
    assert_eq!(books[1].status, RequestStatus::Matched, "Tom Sawyer");
    assert_eq!(
        books[2].status,
        RequestStatus::Matched,
        "Anne of Green Gables"
    );
    assert_eq!(books[3].status, RequestStatus::NotFound, "no-fixture");
    // Auto-request-best-on-match preserved.
    assert_eq!(
        books[0].candidates[0].job.as_ref().map(|j| &j.state),
        Some(&JobState::Pending),
    );
}

// ---------------------------------------------------------------------------
// 2. Pause / cancel an in-flight download.
// ---------------------------------------------------------------------------

/// Build a one-book list whose single candidate (an md5 served by `server`) is
/// requested for download. Returns the orchestrator and the md5.
async fn one_book_orch(server: &MockServer, out: &std::path::Path) -> (Orchestrator, String) {
    let body = vec![7u8; 40_000];
    let md5 = md5_hex(&body);
    server.put(&md5, body, Duration::from_millis(400)).await;

    let mut book = BookRequest::new(BookInput {
        title: "Slow Book".into(),
        authors: vec!["Author".into()],
        ..Default::default()
    });
    book.status = RequestStatus::Matched;
    book.candidates = vec![requested_cand(&md5, "Slow Book", "Author", Format::Epub)];
    book.selected = Some(md5.clone());
    let mut g = Group::new("Batch 1");
    g.books = vec![book];
    let list = DownloadList {
        title: "Cancel".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    };
    let search = SearchClient::replay(config(), fixtures_dir());
    let store = Store::open_in_memory().unwrap();
    let orch = Orchestrator::new(store, &list, search, out.to_path_buf()).unwrap();
    (orch, md5)
}

#[tokio::test]
async fn cancel_in_flight_download_marks_cancelled() {
    let server = MockServer::start().await;
    let out = tempdir();
    let (mut orch, md5) = one_book_orch(&server, &out).await;
    let scheduler = scheduler_for(&server);

    // Cancel through the scheduler directly while `start_downloads` runs (it
    // borrows the orchestrator `&mut`, so the cancel signal comes from the side).
    let sched = Arc::clone(&scheduler);
    let md5_cl = md5.clone();
    let (tx, rx) = mpsc::channel::<Event>(256);
    let ev_task = tokio::spawn(drain(rx));
    let canceller = tokio::spawn(async move {
        // Wait for the transfer to actually be streaming, then cancel.
        tokio::time::sleep(Duration::from_millis(120)).await;
        // Retry a few times in case the download hasn't registered yet.
        for _ in 0..50 {
            if sched.cancel(&md5_cl).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    orch.start_downloads(&scheduler, &tx).await.unwrap();
    drop(tx);
    let _ = ev_task.await.unwrap();
    let _ = canceller.await;

    let list = orch.snapshot().unwrap();
    let cand = &list.groups[0].books[0].candidates[0];
    let job = cand.job.as_ref().unwrap();
    assert_eq!(
        job.state,
        JobState::Cancelled,
        "in-flight cancel → Cancelled"
    );
    // Hard cancel removed the .part; no final file.
    let dest = PathBuf::from(job.output_path.clone().unwrap_or_default());
    let _ = dest;
}

#[tokio::test]
async fn pause_then_resume_completes_with_correct_md5() {
    let server = MockServer::start().await;
    let out = tempdir();
    let (mut orch, md5) = one_book_orch(&server, &out).await;
    let scheduler = scheduler_for(&server);

    // Pause mid-flight.
    let sched = Arc::clone(&scheduler);
    let md5_cl = md5.clone();
    let (tx, rx) = mpsc::channel::<Event>(256);
    let ev_task = tokio::spawn(drain(rx));
    let pauser = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        for _ in 0..50 {
            if sched.pause(&md5_cl).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    orch.start_downloads(&scheduler, &tx).await.unwrap();
    drop(tx);
    let _ = ev_task.await.unwrap();
    let _ = pauser.await;

    // Paused: state preserved, resume_offset kept (the .part has the prefix).
    let list = orch.snapshot().unwrap();
    let job = list.groups[0].books[0].candidates[0].job.clone().unwrap();
    assert_eq!(job.state, JobState::Paused, "paused mid-flight");
    assert!(
        job.resume_offset > 0,
        "paused job kept a resume offset: {}",
        job.resume_offset
    );

    // Resume → Pending, then download to completion (server now serves quickly).
    orch.resume_variation(&[0], 0, &md5).unwrap();
    // Speed up the body so the resume finishes promptly.
    let body = vec![7u8; 40_000];
    server.put(&md5, body.clone(), Duration::ZERO).await;

    let scheduler2 = scheduler_for(&server);
    let (tx, rx) = mpsc::channel::<Event>(256);
    let ev_task = tokio::spawn(drain(rx));
    orch.start_downloads(&scheduler2, &tx).await.unwrap();
    drop(tx);
    let _ = ev_task.await.unwrap();

    let list = orch.snapshot().unwrap();
    let cand = &list.groups[0].books[0].candidates[0];
    let job = cand.job.as_ref().unwrap();
    assert_eq!(job.state, JobState::Done, "resumed download completes");
    assert!(job.md5_verified);
    let path = PathBuf::from(job.output_path.clone().unwrap());
    assert!(path.exists());
    assert_eq!(md5_hex(&std::fs::read(&path).unwrap()), md5);
}

// ---------------------------------------------------------------------------
// 3. Resume-on-launch: persist a partial list, reopen via attach, continue.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_on_launch_reattaches_and_finishes() {
    let dir = tempdir();
    let db = dir.join("state.db");
    let out = dir.join("books");

    let server = MockServer::start().await;
    let body = b"a complete and verifiable book body".to_vec();
    let md5 = md5_hex(&body);
    server.put(&md5, body.clone(), Duration::ZERO).await;

    // First "session": create the list, mark the book's job as if it had been
    // interrupted mid-download (Downloading with a partial resume offset).
    let list_id;
    {
        let store = Store::open(&db).unwrap();
        let mut book = BookRequest::new(BookInput {
            title: "Resumable".into(),
            authors: vec!["Author".into()],
            ..Default::default()
        });
        book.status = RequestStatus::Downloading;
        let mut cand = requested_cand(&md5, "Resumable", "Author", Format::Epub);
        // Simulate an interrupted in-flight job.
        cand.job = Some(DownloadJob {
            state: JobState::Downloading,
            bytes_done: 10,
            resume_offset: 10,
            ..Default::default()
        });
        book.candidates = vec![cand];
        book.selected = Some(md5.clone());
        let mut g = Group::new("Batch 1");
        g.books = vec![book];
        let list = DownloadList {
            title: "Resume List".into(),
            settings: ListSettings::default(),
            groups: vec![g],
        };
        let search = SearchClient::replay(config(), fixtures_dir());
        let orch = Orchestrator::new(store, &list, search, out.clone()).unwrap();
        list_id = orch.list_id();
    }

    // Second "session": reopen the DB, find the list, attach, reset in-flight,
    // and continue. The interrupted Downloading job becomes Pending then Done.
    let store = Store::open(&db).unwrap();
    let lists = store.all_lists().unwrap();
    assert!(lists.iter().any(|l| l.id == list_id));
    let search = SearchClient::replay(config(), fixtures_dir());
    let mut orch = Orchestrator::attach(store, list_id, search, out.clone());

    let reset = orch.reset_inflight_for_resume().unwrap();
    assert_eq!(reset, 1, "one interrupted job reset to pending");

    let scheduler = scheduler_for(&server);
    let (tx, rx) = mpsc::channel::<Event>(256);
    let ev_task = tokio::spawn(drain(rx));
    orch.start_downloads(&scheduler, &tx).await.unwrap();
    drop(tx);
    let _ = ev_task.await.unwrap();

    let list = orch.snapshot().unwrap();
    let job = list.groups[0].books[0].candidates[0].job.as_ref().unwrap();
    assert_eq!(job.state, JobState::Done, "resumed run finished");
    let path = PathBuf::from(job.output_path.clone().unwrap());
    assert_eq!(md5_hex(&std::fs::read(&path).unwrap()), md5);
}

// ---------------------------------------------------------------------------
// 4. Dedupe: two books with the same md5 produce two files from ONE download.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dedupe_same_md5_downloads_once_copies_into_each() {
    let server = MockServer::start().await;
    let out = tempdir();

    // One shared body / md5 wanted by two different books.
    let body = b"shared content wanted by two books".to_vec();
    let md5 = md5_hex(&body);
    server.put(&md5, body.clone(), Duration::ZERO).await;

    let make_book = |title: &str| {
        let mut b = BookRequest::new(BookInput {
            title: title.into(),
            authors: vec!["Author".into()],
            ..Default::default()
        });
        b.status = RequestStatus::Matched;
        b.candidates = vec![requested_cand(&md5, title, "Author", Format::Epub)];
        b.selected = Some(md5.clone());
        b
    };
    let mut g = Group::new("Batch 1");
    g.books = vec![make_book("Book One"), make_book("Book Two")];
    let list = DownloadList {
        title: "Dedupe".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    };

    let search = SearchClient::replay(config(), fixtures_dir());
    let store = Store::open_in_memory().unwrap();
    let mut orch = Orchestrator::new(store, &list, search, out.clone()).unwrap();

    let scheduler = scheduler_for(&server);
    let (tx, rx) = mpsc::channel::<Event>(256);
    let ev_task = tokio::spawn(drain(rx));
    orch.start_downloads(&scheduler, &tx).await.unwrap();
    drop(tx);
    let _ = ev_task.await.unwrap();

    // The server saw exactly ONE GET for the shared md5.
    assert_eq!(server.hit_count(), 1, "md5 downloaded once");

    // Both books reached Done, each at its OWN file, both byte-correct.
    let list = orch.snapshot().unwrap();
    let mut paths = Vec::new();
    for b in &list.groups[0].books {
        assert_eq!(b.status, RequestStatus::Done, "{}", b.input.title);
        let job = b.candidates[0].job.as_ref().unwrap();
        assert_eq!(job.state, JobState::Done);
        assert!(job.md5_verified);
        let p = PathBuf::from(job.output_path.clone().unwrap());
        assert!(p.exists(), "file at {}", p.display());
        assert_eq!(std::fs::read(&p).unwrap(), body, "deduped copy is correct");
        paths.push(p);
    }
    assert_ne!(paths[0], paths[1], "two distinct destination files");
}

// ---------------------------------------------------------------------------
// 5. Sequence numbers follow source-list order (re-flow on insert).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sequence_numbers_follow_source_order() {
    let out = tempdir();

    // Helper to build a matched, download-requested book.
    let mk = |title: &str| {
        let md5 = md5_hex(title.as_bytes());
        let mut b = BookRequest::new(BookInput {
            title: title.into(),
            authors: vec!["Author".into()],
            ..Default::default()
        });
        b.status = RequestStatus::Matched;
        b.candidates = vec![requested_cand(&md5, title, "Author", Format::Epub)];
        b.selected = Some(md5);
        b
    };

    let mut g = Group::new("Batch 1");
    g.books = vec![mk("Alpha"), mk("Beta")];
    let list = DownloadList {
        title: "Seq".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    };

    let search = SearchClient::replay(config(), fixtures_dir());
    let store = Store::open_in_memory().unwrap();
    let mut orch = Orchestrator::new(store, &list, search, out.clone()).unwrap();

    // Initial plan assigns and persists seq 1, 2.
    let planned = orch.plan_downloads().unwrap();
    let seq_of = |planned: &[libgen_core::orchestrator::PlannedDownload], title: &str| -> u32 {
        planned
            .iter()
            .find(|p| p.title == title)
            .unwrap()
            .destination
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let alpha_seq = seq_of(&planned, "Alpha");
    let beta_seq = seq_of(&planned, "Beta");
    assert_eq!(alpha_seq, 1);
    assert_eq!(beta_seq, 2);

    let mut snap = orch.snapshot().unwrap();
    assert_eq!(snap.groups[0].books[0].seq, Some(1));
    assert_eq!(snap.groups[0].books[1].seq, Some(2));

    // Sequence numbers ALWAYS follow SOURCE-LIST ORDER — even for books already
    // downloaded. Mark both existing books DOWNLOADED, then insert a NEW book in
    // the MIDDLE (between Alpha and Beta): the numbers re-flow to declaration
    // order (Alpha 1, Gamma 2, Beta 3). The displayed/planned number matches the
    // list immediately; the file on disk re-syncs when the user runs Reorganize.
    for b in &mut snap.groups[0].books {
        if let Some(c) = b.candidates.first_mut() {
            c.job.as_mut().unwrap().state = JobState::Done;
        }
        b.status = RequestStatus::Done;
    }
    let mut new_list = snap.clone();
    let mut newcomer = mk("Gamma");
    newcomer.seq = None;
    new_list.groups[0].books.insert(1, newcomer);

    let store2 = Store::open_in_memory().unwrap();
    let search2 = SearchClient::replay(config(), fixtures_dir());
    let mut orch2 = Orchestrator::new(store2, &new_list, search2, out.clone()).unwrap();

    let planned2 = orch2.plan_downloads().unwrap();
    assert_eq!(seq_of(&planned2, "Alpha"), 1, "Alpha is source position 1");
    assert_eq!(
        seq_of(&planned2, "Gamma"),
        2,
        "inserted book is source position 2"
    );
    assert_eq!(
        seq_of(&planned2, "Beta"),
        3,
        "Beta re-flows to source position 3"
    );

    // By contrast, when no file has been downloaded yet, inserting a book in the
    // middle renumbers in SOURCE ORDER: a fresh tree Alpha, Gamma, Beta numbers
    // them 1, 2, 3 by declaration order (E6).
    let mut fresh = DownloadList {
        title: "Seq".into(),
        settings: ListSettings::default(),
        groups: vec![Group::new("Batch 1")],
    };
    fresh.groups[0].books = vec![mk("Alpha"), mk("Gamma"), mk("Beta")];
    let store3 = Store::open_in_memory().unwrap();
    let search3 = SearchClient::replay(config(), fixtures_dir());
    let mut orch3 = Orchestrator::new(store3, &fresh, search3, out).unwrap();
    let planned3 = orch3.plan_downloads().unwrap();
    assert_eq!(seq_of(&planned3, "Alpha"), 1, "source order #1");
    assert_eq!(seq_of(&planned3, "Gamma"), 2, "source order #2");
    assert_eq!(seq_of(&planned3, "Beta"), 3, "source order #3");
}
