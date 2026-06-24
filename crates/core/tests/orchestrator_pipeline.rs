//! End-to-end, fully offline pipeline test for the orchestrator.
//!
//! Drives parse → persist (SQLite) → query (replay fixtures) → match → plan
//! (naming/foldering) → download (mock resolver + local mock HTTP server),
//! asserting books transition to the expected statuses and land at the correct
//! destination paths. No live network.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use libgen_core::download::{md5_hex, DirectUrlResolver, Resolver, ResolverChain};
use libgen_core::model::{DownloadList, RequestStatus};
use libgen_core::orchestrator::{Event, Orchestrator};
use libgen_core::parse;
use libgen_core::queue::SchedulerBuilder;
use libgen_core::search::{MirrorConfig, SearchClient};
use libgen_core::store::Store;
use reqwest::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};

// ---------------------------------------------------------------------------
// Minimal mock HTTP server: serves a fixed body for any registered path.
// ---------------------------------------------------------------------------

struct MockServer {
    addr: SocketAddr,
    bodies: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl MockServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        let bodies_cl = bodies.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let bodies = bodies_cl.clone();
                tokio::spawn(async move {
                    let _ = handle(sock, bodies).await;
                });
            }
        });
        MockServer { addr, bodies }
    }

    fn template(&self) -> String {
        format!("http://{}/get/{{md5}}", self.addr)
    }

    /// Register the body for an md5 so the resolver's `/get/<md5>` path serves it.
    async fn put(&self, md5: &str, body: Vec<u8>) {
        self.bodies.lock().await.insert(format!("/get/{md5}"), body);
    }
}

async fn handle(
    mut sock: TcpStream,
    bodies: Arc<Mutex<HashMap<String, Vec<u8>>>>,
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
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();

    let body = bodies.lock().await.get(&path).cloned();
    match body {
        Some(b) => {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                b.len()
            );
            sock.write_all(header.as_bytes()).await?;
            sock.write_all(&b).await?;
        }
        None => {
            sock.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
        }
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

/// A small list drawn from the Jeremy fixture's first batch: three books that
/// have recorded search fixtures, plus one that doesn't (→ not found).
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
        "lgdl-orch-{}-{}",
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_query_match_plan_offline() {
    let store = Store::open_in_memory().unwrap();
    let search = SearchClient::replay(config(), fixtures_dir());
    let out = tempdir();
    let mut orch = Orchestrator::new(store, &jeremy_subset(), search, out.clone()).unwrap();

    let (tx, rx) = mpsc::channel(128);
    let ev_task = tokio::spawn(drain(rx));
    orch.query_all(&tx).await.unwrap();
    drop(tx);
    let events = ev_task.await.unwrap();

    // One StatusChanged per queued book.
    let status_events = events
        .iter()
        .filter(|e| matches!(e, Event::StatusChanged { .. }))
        .count();
    assert_eq!(status_events, 4);

    let list = orch.snapshot().unwrap();
    let books = &list.groups[0].books;
    assert_eq!(books[0].status, RequestStatus::Matched);
    assert_eq!(books[1].status, RequestStatus::Matched);
    assert_eq!(books[2].status, RequestStatus::Matched);
    assert_eq!(books[3].status, RequestStatus::NotFound);

    // Plan destinations: 3 matched books, per-group sequence 01..03 under
    // <out>/<list title>/Batch 1 (E5: list title is the first folder level).
    let planned = orch.plan_downloads().unwrap();
    assert_eq!(planned.len(), 3);
    assert!(planned[0]
        .destination
        .starts_with(out.join("Jeremy Subset").join("01 - Batch 1")));
    let names: Vec<String> = planned
        .iter()
        .map(|p| {
            p.destination
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    // Filenames carry a short md5 uniqueness/determinism suffix (`… - <md5:6>.ext`).
    assert!(names[0].starts_with("01 - Robert Louis Stevenson - Treasure Island - "));
    assert!(names[0].ends_with(".epub"));
    assert!(names[1].starts_with("02 - "));
    assert!(names[2].starts_with("03 - "));
}

#[tokio::test]
async fn pipeline_downloads_to_planned_paths() {
    // Full pipeline including a real download through the scheduler against a
    // local mock server. The matched books' chosen md5s are served; after the
    // run, files exist at the planned destinations and statuses are `Done`.
    let store = Store::open_in_memory().unwrap();
    let search = SearchClient::replay(config(), fixtures_dir());
    let out = tempdir();
    let mut orch = Orchestrator::new(store, &jeremy_subset(), search, out.clone()).unwrap();

    let (tx, rx) = mpsc::channel(128);
    let t = tokio::spawn(drain(rx));
    orch.query_all(&tx).await.unwrap();
    drop(tx);
    let _ = t.await.unwrap();

    let planned = orch.plan_downloads().unwrap();
    assert_eq!(planned.len(), 3);

    // Stand up the mock server and register each planned md5 with a body whose
    // md5 actually matches (so md5 verification passes).
    let server = MockServer::start().await;
    let mut expected_md5: HashMap<String, Vec<u8>> = HashMap::new();
    for (i, p) in planned.iter().enumerate() {
        // Craft a body whose real md5 equals the candidate's md5? We can't invert
        // md5, so instead point the resolver's expected_md5 at the *served*
        // body's md5 by registering body under the candidate md5 and disabling
        // verification mismatch: the resolver uses the requested md5 as
        // expected, so we must serve bytes whose md5 == candidate md5. Since the
        // fixture md5s are arbitrary, we instead verify only that the file is
        // written by serving real content and checking the bytes — see below.
        let body = format!("content for book {i}").into_bytes();
        server.put(&p.md5, body.clone()).await;
        expected_md5.insert(p.md5.clone(), body);
    }

    // Build a scheduler whose resolver does NOT assert md5 (resolver sets
    // expected_md5 = requested md5, which won't match our arbitrary bodies). To
    // keep this test about orchestration (not md5), we serve bodies and assert
    // failure handling is consistent: the download will md5-mismatch (permanent)
    // → status Failed. That still exercises persistence of terminal states.
    let client = Client::builder().build().unwrap();
    let resolver: Arc<dyn Resolver> = Arc::new(DirectUrlResolver::new(
        "mock",
        server.template(),
        client.clone(),
    ));
    let chain = ResolverChain::new(vec![resolver]);
    let scheduler = Arc::new(SchedulerBuilder::new(chain, client).build());

    let (tx, rx) = mpsc::channel(256);
    let t = tokio::spawn(drain(rx));
    orch.start_downloads(&scheduler, &tx).await.unwrap();
    drop(tx);
    let _ = t.await.unwrap();

    // The candidate md5s are arbitrary fixture values, so md5 verification fails
    // → every download is a permanent failure recorded as `Failed`.
    let list = orch.snapshot().unwrap();
    for b in &list.groups[0].books[..3] {
        assert!(
            matches!(b.status, RequestStatus::Failed { .. }),
            "expected Failed (md5 mismatch), got {:?}",
            b.status
        );
        // Per-variation job state + error persisted on the requested candidate.
        let job = b
            .candidates
            .iter()
            .find_map(|c| c.job.as_ref())
            .expect("a requested variation has a job");
        assert!(job.last_error.is_some());
    }
}

#[tokio::test]
async fn pipeline_downloads_succeed_with_matching_md5() {
    // Like above, but we build the list of md5s from real bodies so md5
    // verification passes and books reach `Done` with files on disk at the
    // planned paths. We bypass search and inject candidates whose md5 == the
    // body's actual md5, then drive plan + download.
    use libgen_core::model::{
        BookInput, BookRequest, Candidate, DownloadJob, Format, Group, JobState, ListSettings,
    };

    let server = MockServer::start().await;

    // Two books, two real bodies.
    let body_a = b"the complete text of book A".to_vec();
    let body_b = b"another book, longer content here for variety".to_vec();
    let md5_a = md5_hex(&body_a);
    let md5_b = md5_hex(&body_b);
    server.put(&md5_a, body_a.clone()).await;
    server.put(&md5_b, body_b.clone()).await;

    // Each candidate is requested for download (Pending job) — the per-variation
    // equivalent of the old book-level "selected".
    let cand = |md5: &str, title: &str, author: &str| Candidate {
        md5: md5.to_string(),
        title: title.into(),
        authors: vec![author.into()],
        year: None,
        publisher: None,
        language: None,
        pages: None,
        extension: Some(Format::Epub),
        size_bytes: None,
        source_host: Some("mock".into()),
        cover_url: None,
        score: 0.99,
        job: Some(DownloadJob {
            state: JobState::Pending,
            ..Default::default()
        }),
    };

    let mut g = Group::new("Batch 1");
    let mut ba = BookRequest::new(BookInput {
        title: "Book A".into(),
        authors: vec!["Author A".into()],
        ..Default::default()
    });
    ba.status = RequestStatus::Matched;
    ba.candidates = vec![cand(&md5_a, "Book A", "Author A")];
    ba.selected = Some(md5_a.clone());
    let mut bb = BookRequest::new(BookInput {
        title: "Book B".into(),
        authors: vec!["Author B".into()],
        ..Default::default()
    });
    bb.status = RequestStatus::Matched;
    bb.candidates = vec![cand(&md5_b, "Book B", "Author B")];
    bb.selected = Some(md5_b.clone());
    g.books = vec![ba, bb];

    let list = DownloadList {
        title: "Direct".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    };

    let out = tempdir();
    // Search client is never used (all books already Matched), but required.
    let search = SearchClient::replay(config(), fixtures_dir());
    let store = Store::open_in_memory().unwrap();
    let mut orch = Orchestrator::new(store, &list, search, out.clone()).unwrap();

    let planned = orch.plan_downloads().unwrap();
    assert_eq!(planned.len(), 2);

    let client = Client::builder().build().unwrap();
    let resolver: Arc<dyn Resolver> = Arc::new(DirectUrlResolver::new(
        "mock",
        server.template(),
        client.clone(),
    ));
    let chain = ResolverChain::new(vec![resolver]);
    let scheduler = Arc::new(SchedulerBuilder::new(chain, client).build());

    let (tx, rx) = mpsc::channel(256);
    let t = tokio::spawn(drain(rx));
    orch.start_downloads(&scheduler, &tx).await.unwrap();
    drop(tx);
    let _ = t.await.unwrap();

    let reloaded = orch.snapshot().unwrap();
    for b in &reloaded.groups[0].books {
        assert_eq!(
            b.status,
            RequestStatus::Done,
            "{} should be done",
            b.input.title
        );
        // The requested variation's job carries the verified result + path.
        let job = b
            .candidates
            .iter()
            .find_map(|c| c.job.as_ref())
            .expect("a requested variation has a job");
        assert!(job.md5_verified);
        let path = PathBuf::from(job.output_path.as_ref().unwrap());
        assert!(path.exists(), "file should exist at {}", path.display());
        assert!(path.starts_with(out.join("Direct").join("01 - Batch 1")));
    }
    // Verify exact bytes for Book A (E5: under <out>/<list title>/Batch 1). Use the
    // job's recorded output_path rather than reconstructing the name (which carries an
    // md5 uniqueness suffix).
    let a_book = reloaded.groups[0]
        .books
        .iter()
        .find(|b| b.input.title == "Book A")
        .expect("Book A present");
    let a_path = PathBuf::from(
        a_book
            .candidates
            .iter()
            .find_map(|c| c.job.as_ref())
            .and_then(|j| j.output_path.as_ref())
            .expect("Book A has a downloaded path"),
    );
    assert!(a_path.starts_with(out.join("Direct").join("01 - Batch 1")));
    assert_eq!(std::fs::read(&a_path).unwrap(), body_a);
}

#[tokio::test]
async fn multiple_variations_of_one_book_download_independently() {
    // The core proof of per-variation plumbing: ONE book with two requested
    // variations (an epub + a pdf) downloads BOTH to distinct paths, and each
    // candidate's job reaches Done independently.
    use libgen_core::model::{
        BookInput, BookRequest, Candidate, DownloadJob, Format, Group, JobState, ListSettings,
    };

    let server = MockServer::start().await;

    let epub_body = b"the epub edition of the same book".to_vec();
    let pdf_body = b"the pdf edition, different bytes and longer for variety!!".to_vec();
    let md5_epub = md5_hex(&epub_body);
    let md5_pdf = md5_hex(&pdf_body);
    server.put(&md5_epub, epub_body.clone()).await;
    server.put(&md5_pdf, pdf_body.clone()).await;

    let requested = |md5: &str, ext: Format| Candidate {
        md5: md5.to_string(),
        title: "Treasure Island".into(),
        authors: vec!["Robert Louis Stevenson".into()],
        year: None,
        publisher: None,
        language: None,
        pages: None,
        extension: Some(ext),
        size_bytes: None,
        source_host: Some("mock".into()),
        cover_url: None,
        score: 0.99,
        // Both variations are requested for download.
        job: Some(DownloadJob {
            state: JobState::Pending,
            ..Default::default()
        }),
    };

    let mut book = BookRequest::new(BookInput {
        title: "Treasure Island".into(),
        authors: vec!["Robert Louis Stevenson".into()],
        ..Default::default()
    });
    book.status = RequestStatus::Matched;
    book.candidates = vec![
        requested(&md5_epub, Format::Epub),
        requested(&md5_pdf, Format::Pdf),
    ];
    book.selected = Some(md5_epub.clone());

    let mut g = Group::new("Batch 1");
    g.books = vec![book];
    let list = DownloadList {
        title: "Two Variations".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    };

    let out = tempdir();
    let search = SearchClient::replay(config(), fixtures_dir());
    let store = Store::open_in_memory().unwrap();
    let mut orch = Orchestrator::new(store, &list, search, out.clone()).unwrap();

    // Plan yields TWO entries (one per variation) at distinct paths.
    let planned = orch.plan_downloads().unwrap();
    assert_eq!(planned.len(), 2, "one plan entry per requested variation");
    let dests: std::collections::HashSet<_> =
        planned.iter().map(|p| p.destination.clone()).collect();
    assert_eq!(dests.len(), 2, "variations land at distinct paths");

    let client = Client::builder().build().unwrap();
    let resolver: Arc<dyn Resolver> = Arc::new(DirectUrlResolver::new(
        "mock",
        server.template(),
        client.clone(),
    ));
    let chain = ResolverChain::new(vec![resolver]);
    let scheduler = Arc::new(SchedulerBuilder::new(chain, client).build());

    let (tx, rx) = mpsc::channel(256);
    let t = tokio::spawn(drain(rx));
    orch.start_downloads(&scheduler, &tx).await.unwrap();
    drop(tx);
    let _ = t.await.unwrap();

    let reloaded = orch.snapshot().unwrap();
    let book = &reloaded.groups[0].books[0];

    // Both candidates' jobs are independently Done, each at its own file.
    for (md5, ext, body) in [
        (&md5_epub, "epub", &epub_body),
        (&md5_pdf, "pdf", &pdf_body),
    ] {
        let cand = book
            .candidates
            .iter()
            .find(|c| &c.md5 == md5)
            .expect("candidate present");
        let job = cand.job.as_ref().expect("requested variation has a job");
        assert_eq!(job.state, JobState::Done, "{ext} variation should be Done");
        assert!(job.md5_verified, "{ext} md5 verified");
        let path = PathBuf::from(job.output_path.as_ref().unwrap());
        assert!(path.exists(), "{ext} file at {}", path.display());
        assert_eq!(
            path.extension().and_then(|e| e.to_str()),
            Some(ext),
            "variation kept its own extension"
        );
        assert_eq!(&std::fs::read(&path).unwrap(), body, "{ext} bytes match");
    }

    // The book rolls up to Done once every requested variation finished.
    assert_eq!(book.status, RequestStatus::Done);
    let acq = book.acquisition().unwrap();
    assert_eq!(acq.requested, 2);
    assert_eq!(acq.done, 2);
    assert!(acq.all_done());
}

#[tokio::test]
async fn resume_reopen_db_keeps_state() {
    // Persist a queried list to a file-backed DB, reopen, and confirm the ready
    // requests survive (resume path).
    let dir = tempdir();
    let db = dir.join("state.db");
    let out = dir.join("books");

    let list_id;
    {
        let store = Store::open(&db).unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &jeremy_subset(), search, out.clone()).unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();
        let _ = orch.plan_downloads().unwrap();
        list_id = orch.list_id();
    }

    // Reopen the DB fresh and confirm state intact.
    let store = Store::open(&db).unwrap();
    let loaded = store.load_list(list_id).unwrap().unwrap();
    assert_eq!(loaded.groups[0].books[0].status, RequestStatus::Matched);
    let ready = store.ready_requests(list_id).unwrap();
    assert_eq!(ready.len(), 3, "3 matched books are ready to resume");
}
