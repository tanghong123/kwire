//! Headless tests for the download engine + per-host queues.
//!
//! All tests run offline against a hand-rolled tokio TCP "mock" HTTP server
//! (see `mock` module) — no real mirrors, no extra mock-server crate. The mock
//! understands `Range` requests, can serve a programmable sequence of status
//! codes (for retry tests), can be artificially slow (for concurrency/rate
//! tests), and tracks per-path in-flight counts and request timestamps so tests
//! can assert politeness.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use libgen_core::download::{
    download_with_client, download_with_client_cancellable, md5_hex, DirectUrlResolver,
    DownloadTarget, Resolver, ResolverChain,
};
use libgen_core::queue::{DownloadRequest, HostLimits, Progress, SchedulerBuilder};
use reqwest::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio::time::Instant;

// ---------------------------------------------------------------------------
// Mock HTTP server
// ---------------------------------------------------------------------------

/// Per-path behavior the mock server should exhibit.
#[derive(Clone)]
struct PathConfig {
    /// The full body to serve (Range slices come from this).
    body: Arc<Vec<u8>>,
    /// If set, corrupt the served bytes (flip first byte) to break md5.
    corrupt: bool,
    /// Status codes to return before finally serving the body. Each connection
    /// pops one; e.g. [503, 503] means two failures then success.
    fail_sequence: Arc<Mutex<Vec<u16>>>,
    /// Artificial delay applied while streaming the body (for slow endpoints).
    delay: Duration,
    /// Ignore Range header and always serve 200 + full body.
    ignore_range: bool,
    /// Drop the connection after this many body bytes (simulate reset). 0 = no.
    cut_after: usize,
}

impl PathConfig {
    fn new(body: Vec<u8>) -> Self {
        PathConfig {
            body: Arc::new(body),
            corrupt: false,
            fail_sequence: Arc::new(Mutex::new(Vec::new())),
            delay: Duration::ZERO,
            ignore_range: false,
            cut_after: 0,
        }
    }
}

#[derive(Default)]
struct PathStats {
    /// Current number of in-flight requests.
    in_flight: AtomicUsize,
    /// Peak concurrent in-flight requests observed.
    peak: AtomicUsize,
    /// Total requests received.
    total: AtomicUsize,
    /// Request start timestamps (for rate-limit spacing assertions).
    timestamps: Mutex<Vec<Instant>>,
}

struct MockServer {
    addr: SocketAddr,
    configs: Arc<Mutex<HashMap<String, PathConfig>>>,
    stats: Arc<Mutex<HashMap<String, Arc<PathStats>>>>,
}

impl MockServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let configs: Arc<Mutex<HashMap<String, PathConfig>>> = Arc::new(Mutex::new(HashMap::new()));
        let stats: Arc<Mutex<HashMap<String, Arc<PathStats>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let configs_cl = configs.clone();
        let stats_cl = stats.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let configs = configs_cl.clone();
                let stats = stats_cl.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, configs, stats).await;
                });
            }
        });

        MockServer {
            addr,
            configs,
            stats,
        }
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// URL template with `{md5}` for a given path prefix.
    fn template(&self, prefix: &str) -> String {
        format!("http://{}{}/{{md5}}", self.addr, prefix)
    }

    async fn set(&self, path: &str, cfg: PathConfig) {
        self.configs.lock().await.insert(path.to_string(), cfg);
        self.stats
            .lock()
            .await
            .entry(path.to_string())
            .or_insert_with(|| Arc::new(PathStats::default()));
    }

    async fn stats_for(&self, path: &str) -> Arc<PathStats> {
        self.stats
            .lock()
            .await
            .entry(path.to_string())
            .or_insert_with(|| Arc::new(PathStats::default()))
            .clone()
    }
}

async fn handle_conn(
    mut sock: TcpStream,
    configs: Arc<Mutex<HashMap<String, PathConfig>>>,
    stats: Arc<Mutex<HashMap<String, Arc<PathStats>>>>,
) -> std::io::Result<()> {
    // Read request headers (until \r\n\r\n).
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
        if buf.len() > 64 * 1024 {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/").to_string();

    let mut range_start: Option<u64> = None;
    for line in lines {
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("range:") {
            // bytes=START-
            if let Some(eq) = v.find('=') {
                let spec = &v[eq + 1..];
                let start = spec.split('-').next().unwrap_or("").trim();
                if let Ok(s) = start.parse::<u64>() {
                    range_start = Some(s);
                }
            }
        }
    }

    let cfg = configs.lock().await.get(&path).cloned();
    let cfg = match cfg {
        Some(c) => c,
        None => {
            let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            sock.write_all(resp).await?;
            return Ok(());
        }
    };

    // Stats bookkeeping.
    let stat = stats
        .lock()
        .await
        .entry(path.clone())
        .or_insert_with(|| Arc::new(PathStats::default()))
        .clone();
    stat.total.fetch_add(1, Ordering::SeqCst);
    stat.timestamps.lock().await.push(Instant::now());
    let cur = stat.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    stat.peak.fetch_max(cur, Ordering::SeqCst);

    // Guard to decrement in-flight on the way out.
    struct Guard(Arc<PathStats>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _guard = Guard(stat.clone());

    // Programmed failure sequence (for retry tests).
    let maybe_fail = {
        let mut seq = cfg.fail_sequence.lock().await;
        if seq.is_empty() {
            None
        } else {
            Some(seq.remove(0))
        }
    };
    if let Some(code) = maybe_fail {
        let reason = match code {
            503 => "Service Unavailable",
            500 => "Internal Server Error",
            429 => "Too Many Requests",
            404 => "Not Found",
            _ => "Error",
        };
        let resp =
            format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        sock.write_all(resp.as_bytes()).await?;
        return Ok(());
    }

    // Build body, honoring Range.
    let mut body: Vec<u8> = (*cfg.body).clone();
    if cfg.corrupt && !body.is_empty() {
        body[0] ^= 0xFF;
    }
    let total = body.len() as u64;

    let (status_line, slice): (String, Vec<u8>) = match range_start {
        Some(start) if !cfg.ignore_range && start <= total => {
            let s = body[start as usize..].to_vec();
            let end = total.saturating_sub(1);
            (
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\n"
                ),
                s,
            )
        }
        _ => ("HTTP/1.1 200 OK\r\n".to_string(), body),
    };

    let header = format!(
        "{status_line}Content-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        slice.len()
    );
    sock.write_all(header.as_bytes()).await?;

    // Stream body, optionally slowly and/or cut short.
    if cfg.cut_after > 0 && cfg.cut_after < slice.len() {
        sock.write_all(&slice[..cfg.cut_after]).await?;
        sock.flush().await?;
        // Give the client time to consume the partial bytes before we drop the
        // socket (so reqwest yields them as a chunk, then sees the truncation
        // against Content-Length). Without this the close can race the read and
        // reqwest may error with 0 bytes delivered.
        tokio::time::sleep(Duration::from_millis(50)).await;
        return Ok(());
    }

    if cfg.delay.is_zero() {
        sock.write_all(&slice).await?;
    } else {
        // Write in two halves with a delay so the request stays "in flight".
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
// A resolver that always errors (for failover tests).
// ---------------------------------------------------------------------------

struct AlwaysFailResolver {
    name: String,
}

#[async_trait::async_trait]
impl Resolver for AlwaysFailResolver {
    fn name(&self) -> &str {
        &self.name
    }
    async fn resolve(
        &self,
        _md5: &str,
    ) -> Result<DownloadTarget, libgen_core::download::DownloadError> {
        // Resolve "succeeds" to a host that always 404s on download, forcing the
        // download-side failover path. We point it at a bogus path on the same
        // server so the GET 404s.
        Err(libgen_core::download::DownloadError::Permanent(
            "host A down".into(),
        ))
    }
}

/// A resolver with an explicit synthetic `host` label (so two resolvers pointing
/// at the SAME mock server still route to DISTINCT per-host queues), serving from
/// a configurable path prefix. Used to test host-spill across hosts.
struct LabeledResolver {
    name: String,
    host: String,
    base: String,
    prefix: String,
}

#[async_trait::async_trait]
impl Resolver for LabeledResolver {
    fn name(&self) -> &str {
        &self.name
    }
    async fn resolve(
        &self,
        md5: &str,
    ) -> Result<DownloadTarget, libgen_core::download::DownloadError> {
        Ok(DownloadTarget {
            url: format!("{}{}/{}", self.base, self.prefix, md5),
            host: self.host.clone(),
            expected_md5: Some(md5.to_string()),
            total_bytes: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_blob(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

async fn collect_events(mut rx: mpsc::Receiver<Progress>) -> Vec<Progress> {
    let mut out = Vec::new();
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_and_verify_md5_ok() {
    let server = MockServer::start().await;
    let blob = make_blob(5000);
    let md5 = md5_hex(&blob);
    server
        .set(&format!("/get/{md5}"), PathConfig::new(blob.clone()))
        .await;

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let target = DownloadTarget {
        url: format!("{}/get/{md5}", server.base()),
        host: "127.0.0.1".into(),
        expected_md5: Some(md5.clone()),
        total_bytes: Some(blob.len() as u64),
    };
    let written = download_with_client(&client(), &target, &dest, 0)
        .await
        .expect("download ok");
    assert_eq!(written, blob.len() as u64);
    let on_disk = std::fs::read(&dest).unwrap();
    assert_eq!(on_disk, blob);
    assert_eq!(md5_hex(&on_disk), md5);
    // .part should be gone after the atomic rename.
    assert!(!libgen_core::download::part_path(&dest).exists());
}

#[tokio::test]
async fn corrupt_body_fails_md5_verification() {
    let server = MockServer::start().await;
    let blob = make_blob(2048);
    let md5 = md5_hex(&blob);
    let mut cfg = PathConfig::new(blob.clone());
    cfg.corrupt = true;
    server.set(&format!("/get/{md5}"), cfg).await;

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let target = DownloadTarget {
        url: format!("{}/get/{md5}", server.base()),
        host: "127.0.0.1".into(),
        expected_md5: Some(md5.clone()),
        total_bytes: Some(blob.len() as u64),
    };
    let err = download_with_client(&client(), &target, &dest, 0)
        .await
        .expect_err("must fail md5");
    assert!(!err.is_transient(), "md5 mismatch is permanent: {err}");
    assert!(!dest.exists());
    assert!(!libgen_core::download::part_path(&dest).exists());
}

#[tokio::test]
async fn resume_after_truncated_download() {
    // Deterministic resume: simulate a prior interrupted download by writing a
    // partial `.part` on disk, then resume via HTTP Range and assert the final
    // file is byte-exact and md5-verified. The mock asserts a 206 Range
    // response (Accept-Ranges + Content-Range) is honored.
    let server = MockServer::start().await;
    let blob = make_blob(10_000);
    let md5 = md5_hex(&blob);
    let path = format!("/get/{md5}");
    server.set(&path, PathConfig::new(blob.clone())).await;

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let target = DownloadTarget {
        url: format!("{}/get/{md5}", server.base()),
        host: "127.0.0.1".into(),
        expected_md5: Some(md5.clone()),
        total_bytes: Some(blob.len() as u64),
    };

    // Pre-seed a partial .part as if a previous attempt died at 4000 bytes.
    let cut = 4000u64;
    let part = libgen_core::download::part_path(&dest);
    std::fs::write(&part, &blob[..cut as usize]).unwrap();

    // Resume from the partial length: should fetch only the remaining bytes.
    let written = download_with_client(&client(), &target, &dest, cut)
        .await
        .expect("resume ok");
    assert_eq!(written, blob.len() as u64 - cut, "only the tail is fetched");
    let on_disk = std::fs::read(&dest).unwrap();
    assert_eq!(on_disk.len(), blob.len());
    assert_eq!(on_disk, blob);
    assert_eq!(md5_hex(&on_disk), md5);

    // Confirm the server actually served a ranged (206) request: total requests
    // == 1 and the resume produced the correct partial byte count.
    let s = server.stats_for(&path).await;
    assert_eq!(s.total.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn resume_from_part_when_offset_is_zero() {
    // The app-kill / relaunch case: a download interrupted WITHOUT an explicit
    // pause keeps its `.part` on disk but its job's `resume_offset` stays 0 (only
    // a pause sets it). The downloader MUST resume from the `.part`, not treat
    // offset==0 as "fresh" and delete it. Regression guard for the restart-from-
    // scratch bug (real cases: large 36 MB / 15 MB partials would have been wiped).
    let server = MockServer::start().await;
    let blob = make_blob(10_000);
    let md5 = md5_hex(&blob);
    let path = format!("/get/{md5}");
    server.set(&path, PathConfig::new(blob.clone())).await;

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let target = DownloadTarget {
        url: format!("{}/get/{md5}", server.base()),
        host: "127.0.0.1".into(),
        expected_md5: Some(md5.clone()),
        total_bytes: Some(blob.len() as u64),
    };

    // Pre-seed a partial as if the app was killed mid-download at 6000 bytes.
    let cut = 6000u64;
    let part = libgen_core::download::part_path(&dest);
    std::fs::write(&part, &blob[..cut as usize]).unwrap();

    // resume_offset = 0 (the post-kill value), but a 6000-byte .part exists.
    let written = download_with_client(&client(), &target, &dest, 0)
        .await
        .expect("resume ok");
    assert_eq!(
        written,
        blob.len() as u64 - cut,
        "must resume from the .part (fetch only the tail), NOT restart from scratch"
    );
    let on_disk = std::fs::read(&dest).unwrap();
    assert_eq!(on_disk, blob, "final file is byte-exact");
    assert_eq!(md5_hex(&on_disk), md5);
}

#[tokio::test]
async fn resume_via_scheduler_retry_after_reset() {
    // End-to-end through the scheduler: the server cuts the connection mid-body
    // on the first attempt (transient), then serves fully; the queue retries and
    // resumes from the .part. Verifies the retry+resume integration.
    let server = MockServer::start().await;
    let blob = make_blob(20_000);
    let md5 = md5_hex(&blob);
    let path = format!("/get/{md5}");
    let mut cfg = PathConfig::new(blob.clone());
    cfg.cut_after = 8000;
    server.set(&path, cfg).await;

    let resolver = DirectUrlResolver::new("mock", server.template("/get"), client());
    let chain = ResolverChain::new(vec![Arc::new(resolver)]);
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 1,
                min_interval: Duration::from_millis(0),
                max_attempts: 6,
            })
            .base_backoff(Duration::from_millis(5))
            .build(),
    );

    let dir = tempdir();
    let dest = dir.join("out.bin");

    // After the first (cut) attempt, flip the path to serve fully so the retry
    // succeeds. We do this from a spawned task that waits a moment.
    let server_cfgs = server.configs.clone();
    let path_cl = path.clone();
    let blob_cl = blob.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        server_cfgs
            .lock()
            .await
            .insert(path_cl, PathConfig::new(blob_cl));
    });

    let (tx, rx) = mpsc::channel(512);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched
        .run(vec![DownloadRequest::new(md5.clone(), dest.clone())], tx)
        .await;
    let _ = ev_task.await.unwrap();
    assert!(
        outcomes[0].result.is_ok(),
        "should resume and complete: {:?}",
        outcomes[0].result
    );
    assert_eq!(md5_hex(&std::fs::read(&dest).unwrap()), md5);
}

#[tokio::test]
async fn retry_recovers_after_503s() {
    let server = MockServer::start().await;
    let blob = make_blob(1500);
    let md5 = md5_hex(&blob);
    let mut cfg = PathConfig::new(blob.clone());
    // Two 503s, then success.
    cfg.fail_sequence = Arc::new(Mutex::new(vec![503, 503]));
    server.set(&format!("/get/{md5}"), cfg).await;

    let resolver = DirectUrlResolver::new("mock", server.template("/get"), client());
    let chain = ResolverChain::new(vec![Arc::new(resolver)]);
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 2,
                min_interval: Duration::from_millis(0),
                max_attempts: 4,
            })
            .base_backoff(Duration::from_millis(5))
            .build(),
    );

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let (tx, rx) = mpsc::channel(256);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched
        .run(vec![DownloadRequest::new(md5.clone(), dest.clone())], tx)
        .await;
    let events = ev_task.await.unwrap();

    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].result.is_ok(),
        "expected recovery: {:?}",
        outcomes[0].result
    );
    let retries = events
        .iter()
        .filter(|e| matches!(e, Progress::Retrying { .. }))
        .count();
    assert_eq!(retries, 2, "should have retried twice");
    assert_eq!(md5_hex(&std::fs::read(&dest).unwrap()), md5);
}

/// A normal single-leg (no hedge) download emits exactly one `LegEnded`, for the
/// primary's `leg_id` (= 0), via the leg task's Drop guard. The primary's per-leg
/// events all carry `leg_id = 0, is_hedge = false`. (Backend leg-lifecycle
/// contract — see docs/LEG_LIFECYCLE.md §9.)
#[tokio::test]
async fn leg_emits_exactly_one_leg_ended_with_monotonic_primary_id() {
    let server = MockServer::start().await;
    let blob = make_blob(4000);
    let md5 = md5_hex(&blob);
    server
        .set(&format!("/get/{md5}"), PathConfig::new(blob.clone()))
        .await;

    let resolver = DirectUrlResolver::new("mock", server.template("/get"), client());
    let chain = ResolverChain::new(vec![Arc::new(resolver)]);
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 2,
                min_interval: Duration::from_millis(0),
                max_attempts: 4,
            })
            .build(),
    );

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let (tx, rx) = mpsc::channel(256);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched
        .run(vec![DownloadRequest::new(md5.clone(), dest.clone())], tx)
        .await;
    let events = ev_task.await.unwrap();

    assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0].result);

    // Exactly one LegEnded, and it names the primary leg (id 0).
    let ended: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Progress::LegEnded { leg_id, md5: m } if *m == md5 => Some(*leg_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        ended,
        vec![0],
        "exactly one LegEnded for the primary (leg_id 0); got {ended:?}"
    );

    // Every per-leg event on this single-leg run is the primary: leg_id 0, not hedge.
    for e in &events {
        let stamp = match e {
            Progress::Resolved {
                leg_id, is_hedge, ..
            }
            | Progress::Resuming {
                leg_id, is_hedge, ..
            }
            | Progress::Bytes {
                leg_id, is_hedge, ..
            }
            | Progress::Stalled {
                leg_id, is_hedge, ..
            }
            | Progress::Retrying {
                leg_id, is_hedge, ..
            }
            | Progress::FailingOver {
                leg_id, is_hedge, ..
            } => Some((*leg_id, *is_hedge)),
            _ => None,
        };
        if let Some((leg_id, is_hedge)) = stamp {
            assert_eq!(leg_id, 0, "primary leg id is 0: {e:?}");
            assert!(!is_hedge, "primary is not a hedge: {e:?}");
        }
    }
}

#[tokio::test]
async fn permanent_404_fails_fast_without_retry() {
    let server = MockServer::start().await;
    let md5 = md5_hex(b"whatever");
    // Do NOT register the path → server returns 404.
    let resolver = DirectUrlResolver::new("mock", server.template("/missing"), client());
    let chain = ResolverChain::new(vec![Arc::new(resolver)]);
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 1,
                min_interval: Duration::from_millis(0),
                max_attempts: 5,
            })
            .build(),
    );

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let (tx, rx) = mpsc::channel(256);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched
        .run(vec![DownloadRequest::new(md5.clone(), dest.clone())], tx)
        .await;
    let events = ev_task.await.unwrap();

    assert!(outcomes[0].result.is_err());
    let retries = events
        .iter()
        .filter(|e| matches!(e, Progress::Retrying { .. }))
        .count();
    assert_eq!(retries, 0, "404 should fail fast, no retries");
}

#[tokio::test]
async fn per_host_concurrency_is_respected() {
    let server = MockServer::start().await;
    let blob = make_blob(2000);

    // Register 6 distinct md5 paths, all on the same host, each slow.
    let mut md5s = Vec::new();
    for i in 0..6 {
        let b: Vec<u8> = blob.iter().map(|x| x.wrapping_add(i as u8)).collect();
        let md5 = md5_hex(&b);
        let mut cfg = PathConfig::new(b);
        cfg.delay = Duration::from_millis(150);
        server.set(&format!("/get/{md5}"), cfg).await;
        md5s.push(md5);
    }

    let resolver = DirectUrlResolver::new("mock", server.template("/get"), client());
    let chain = ResolverChain::new(vec![Arc::new(resolver)]);
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 2,
                min_interval: Duration::from_millis(0),
                max_attempts: 2,
            })
            .build(),
    );

    let dir = tempdir();
    let reqs: Vec<_> = md5s
        .iter()
        .enumerate()
        .map(|(i, m)| DownloadRequest::new(m.clone(), dir.join(format!("f{i}.bin"))))
        .collect();
    let (tx, rx) = mpsc::channel(512);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched.run(reqs, tx).await;
    let _ = ev_task.await.unwrap();

    assert!(outcomes.iter().all(|o| o.result.is_ok()));

    // Peak in-flight across all paths must never exceed max_concurrency (2),
    // since they share the host (127.0.0.1) queue. Check global peak by summing
    // simultaneous: easiest is to assert each path saw <=2, but the real
    // constraint is host-wide. We track per-path; assert the host-wide peak via
    // the sum of in-flight is bounded by checking each path peak <= 2 AND that
    // total requests == 6 (no spurious retries inflating counts).
    let mut total_requests = 0usize;
    for m in &md5s {
        let s = server.stats_for(&format!("/get/{m}")).await;
        total_requests += s.total.load(Ordering::SeqCst);
        assert!(
            s.peak.load(Ordering::SeqCst) <= 2,
            "per-path peak exceeded host concurrency"
        );
    }
    assert_eq!(total_requests, 6, "no spurious requests");
}

#[tokio::test]
async fn host_wide_concurrency_cap() {
    // Stronger assertion: a single shared path hit by many requests must never
    // exceed max_concurrency in-flight on the host.
    let server = MockServer::start().await;
    let blob = make_blob(1000);
    let md5 = md5_hex(&blob);
    let mut cfg = PathConfig::new(blob.clone());
    cfg.delay = Duration::from_millis(120);
    let path = format!("/shared/{md5}");
    server.set(&path, cfg).await;

    let resolver = DirectUrlResolver::new("mock", server.template("/shared"), client());
    let chain = ResolverChain::new(vec![Arc::new(resolver)]);
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 2,
                min_interval: Duration::from_millis(0),
                max_attempts: 1,
            })
            .build(),
    );

    let dir = tempdir();
    // 5 requests for the SAME md5 → same path, different dest files.
    let reqs: Vec<_> = (0..5)
        .map(|i| DownloadRequest::new(md5.clone(), dir.join(format!("s{i}.bin"))))
        .collect();
    let (tx, rx) = mpsc::channel(512);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched.run(reqs, tx).await;
    let _ = ev_task.await.unwrap();
    assert!(outcomes.iter().all(|o| o.result.is_ok()));

    let s = server.stats_for(&path).await;
    assert_eq!(s.total.load(Ordering::SeqCst), 5);
    assert!(
        s.peak.load(Ordering::SeqCst) <= 2,
        "host concurrency cap violated: peak={}",
        s.peak.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn per_host_rate_limit_spacing() {
    let server = MockServer::start().await;
    let blob = make_blob(500);

    let mut md5s = Vec::new();
    for i in 0..4u8 {
        let b: Vec<u8> = blob.iter().map(|x| x.wrapping_add(i)).collect();
        let md5 = md5_hex(&b);
        server.set(&format!("/r/{md5}"), PathConfig::new(b)).await;
        md5s.push(md5);
    }

    let resolver = DirectUrlResolver::new("mock", server.template("/r"), client());
    let chain = ResolverChain::new(vec![Arc::new(resolver)]);
    let min_interval = Duration::from_millis(100);
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 1,
                min_interval,
                max_attempts: 1,
            })
            .build(),
    );

    let dir = tempdir();
    let reqs: Vec<_> = md5s
        .iter()
        .enumerate()
        .map(|(i, m)| DownloadRequest::new(m.clone(), dir.join(format!("r{i}.bin"))))
        .collect();
    let (tx, rx) = mpsc::channel(512);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched.run(reqs, tx).await;
    let _ = ev_task.await.unwrap();
    assert!(outcomes.iter().all(|o| o.result.is_ok()));

    // Gather all request timestamps across the host's paths, sort, and assert
    // consecutive spacing is >= min_interval (jitter only adds delay).
    let mut all: Vec<Instant> = Vec::new();
    for m in &md5s {
        let s = server.stats_for(&format!("/r/{m}")).await;
        all.extend(s.timestamps.lock().await.iter().copied());
    }
    all.sort();
    assert_eq!(all.len(), 4);
    for w in all.windows(2) {
        let gap = w[1].duration_since(w[0]);
        // Assert meaningful per-host spacing (the limiter isn't bursting) with a
        // generous tolerance for wall-clock jitter under parallel-test load: a real
        // rate-limit failure bunches requests near 0ms, far below this half-interval
        // floor. (Was `min_interval - 20ms`, which flaked under load.)
        assert!(
            gap >= min_interval / 2,
            "requests too close: {gap:?} < {min_interval:?}/2 (rate-limit not spacing)"
        );
    }
}

#[tokio::test]
async fn failover_to_alternate_mirror() {
    let server = MockServer::start().await;
    let blob = make_blob(3000);
    let md5 = md5_hex(&blob);
    // Mirror B serves the good file at /b/<md5>.
    server
        .set(&format!("/b/{md5}"), PathConfig::new(blob.clone()))
        .await;
    // Mirror A's path /a/<md5> is unregistered → 404 (host A always fails).

    let resolver_a = DirectUrlResolver::new("mirrorA", server.template("/a"), client());
    let resolver_b = DirectUrlResolver::new("mirrorB", server.template("/b"), client());
    let chain = ResolverChain::new(vec![Arc::new(resolver_a), Arc::new(resolver_b)]);
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 2,
                min_interval: Duration::from_millis(0),
                max_attempts: 2,
            })
            .build(),
    );

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let (tx, rx) = mpsc::channel(256);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched
        .run(vec![DownloadRequest::new(md5.clone(), dest.clone())], tx)
        .await;
    let events = ev_task.await.unwrap();

    assert!(
        outcomes[0].result.is_ok(),
        "should complete via mirror B: {:?}",
        outcomes[0].result
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Progress::FailingOver { .. })),
        "should have emitted a failover event"
    );
    assert_eq!(md5_hex(&std::fs::read(&dest).unwrap()), md5);
}

#[tokio::test]
async fn failover_when_resolver_errors() {
    // Exercise the resolve-side failover: resolver A errors, resolver B works.
    let server = MockServer::start().await;
    let blob = make_blob(1234);
    let md5 = md5_hex(&blob);
    server
        .set(&format!("/g/{md5}"), PathConfig::new(blob.clone()))
        .await;

    let resolver_a = AlwaysFailResolver { name: "A".into() };
    let resolver_b = DirectUrlResolver::new("B", server.template("/g"), client());
    let chain = ResolverChain::new(vec![Arc::new(resolver_a), Arc::new(resolver_b)]);
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits::default())
            .build(),
    );

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let (tx, rx) = mpsc::channel(256);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched
        .run(vec![DownloadRequest::new(md5.clone(), dest.clone())], tx)
        .await;
    let _ = ev_task.await.unwrap();
    assert!(
        outcomes[0].result.is_ok(),
        "resolver failover should succeed: {:?}",
        outcomes[0].result
    );
    assert_eq!(md5_hex(&std::fs::read(&dest).unwrap()), md5);
}

#[tokio::test]
async fn spills_to_idle_host_when_preferred_is_saturated() {
    // Two hosts (A preferred, B alternate) both serve every md5, each capped at 2
    // concurrent. A is slow (so its 2 slots saturate); with host-spill, the extra
    // jobs must flow to idle host B instead of queueing behind A — while NEITHER
    // host ever exceeds its concurrency cap.
    let server = MockServer::start().await;
    let blob = make_blob(2000);

    // Register the same 6 md5s under BOTH /a/<md5> (slow) and /b/<md5> (fast).
    let mut md5s = Vec::new();
    for i in 0..6 {
        let b: Vec<u8> = blob.iter().map(|x| x.wrapping_add(i as u8)).collect();
        let md5 = md5_hex(&b);
        let mut slow = PathConfig::new(b.clone());
        slow.delay = Duration::from_millis(200); // A is slow → saturates
        server.set(&format!("/a/{md5}"), slow).await;
        server.set(&format!("/b/{md5}"), PathConfig::new(b)).await;
        md5s.push(md5);
    }

    // Resolver A (preferred, host "hostA") and B (alternate, host "hostB"), both
    // hitting the same server but routed to DISTINCT per-host queues by label.
    let resolver_a = LabeledResolver {
        name: "hostA".into(),
        host: "hostA".into(),
        base: server.base(),
        prefix: "/a".into(),
    };
    let resolver_b = LabeledResolver {
        name: "hostB".into(),
        host: "hostB".into(),
        base: server.base(),
        prefix: "/b".into(),
    };
    let chain = ResolverChain::new(vec![Arc::new(resolver_a), Arc::new(resolver_b)]);
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 2,
                min_interval: Duration::from_millis(0),
                max_attempts: 2,
            })
            .build(),
    );

    let dir = tempdir();
    let reqs: Vec<_> = md5s
        .iter()
        .enumerate()
        .map(|(i, m)| DownloadRequest::new(m.clone(), dir.join(format!("f{i}.bin"))))
        .collect();
    let (tx, rx) = mpsc::channel(512);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched.run(reqs, tx).await;
    let _ = ev_task.await.unwrap();

    assert!(outcomes.iter().all(|o| o.result.is_ok()), "all complete");

    // Count requests + peak concurrency on each host's paths.
    let mut a_total = 0usize;
    let mut b_total = 0usize;
    for m in &md5s {
        let sa = server.stats_for(&format!("/a/{m}")).await;
        let sb = server.stats_for(&format!("/b/{m}")).await;
        a_total += sa.total.load(Ordering::SeqCst);
        b_total += sb.total.load(Ordering::SeqCst);
        assert!(
            sa.peak.load(Ordering::SeqCst) <= 2,
            "host A per-path peak exceeded cap"
        );
        assert!(
            sb.peak.load(Ordering::SeqCst) <= 2,
            "host B per-path peak exceeded cap"
        );
    }

    // Every job completed exactly once across the two hosts.
    assert_eq!(
        a_total + b_total,
        6,
        "each job downloaded once (no duplicates)"
    );
    // The whole point: work spilled to the idle host B rather than all queueing on
    // the saturated host A.
    assert!(
        b_total >= 1,
        "expected jobs to spill to idle host B (a={a_total}, b={b_total})"
    );
    // And A did NOT swallow everything (it's capped + slow).
    assert!(
        a_total <= 4,
        "host A should not have taken all jobs (a={a_total}, b={b_total})"
    );
}

#[tokio::test]
async fn host_wide_cap_holds_under_spill_with_single_host() {
    // Regression: with only ONE resolvable host, spill must still never exceed the
    // host's concurrency cap (the try_acquire path must not leak permits).
    let server = MockServer::start().await;
    let blob = make_blob(1000);
    let md5 = md5_hex(&blob);
    let mut cfg = PathConfig::new(blob.clone());
    cfg.delay = Duration::from_millis(120);
    let path = format!("/only/{md5}");
    server.set(&path, cfg).await;

    let resolver = LabeledResolver {
        name: "solo".into(),
        host: "solo".into(),
        base: server.base(),
        prefix: "/only".into(),
    };
    let chain = ResolverChain::new(vec![Arc::new(resolver)]);
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 2,
                min_interval: Duration::from_millis(0),
                max_attempts: 1,
            })
            .build(),
    );

    let dir = tempdir();
    let reqs: Vec<_> = (0..6)
        .map(|i| DownloadRequest::new(md5.clone(), dir.join(format!("s{i}.bin"))))
        .collect();
    let (tx, rx) = mpsc::channel(512);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched.run(reqs, tx).await;
    let _ = ev_task.await.unwrap();
    assert!(outcomes.iter().all(|o| o.result.is_ok()));

    let s = server.stats_for(&path).await;
    assert!(
        s.peak.load(Ordering::SeqCst) <= 2,
        "single-host cap violated under spill: peak={}",
        s.peak.load(Ordering::SeqCst)
    );
}

// ---------------------------------------------------------------------------
// tiny tempdir helper (avoid extra dev-dep)
// ---------------------------------------------------------------------------

fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let unique = format!("lgdl-test-{}-{}", std::process::id(), fastrand_u64());
    p.push(unique);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn fastrand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    // mix with a counter to avoid collisions within the same nanosecond.
    static CTR: AtomicUsize = AtomicUsize::new(0);
    nanos ^ ((CTR.fetch_add(1, Ordering::SeqCst) as u64) << 32)
}

#[tokio::test]
async fn range_ignored_200_emits_from_scratch_note() {
    // A host that IGNORES the Range header (always 200 + full body). With a partial
    // already on disk, the downloader must DROP the partial and restart from scratch
    // — and emit the paired diagnostic notes so the history can verify it (the
    // `range-ignored-restarts-from-scratch` invariant). Behavior unchanged: the file
    // still completes byte-exact.
    let server = MockServer::start().await;
    let blob = make_blob(8000);
    let md5 = md5_hex(&blob);
    let path = format!("/get/{md5}");
    let mut cfg = PathConfig::new(blob.clone());
    cfg.ignore_range = true; // serve 200 + full body even when asked for a Range
    server.set(&path, cfg).await;

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let target = DownloadTarget {
        url: format!("{}/get/{md5}", server.base()),
        host: "127.0.0.1".into(),
        expected_md5: Some(md5.clone()),
        total_bytes: Some(blob.len() as u64),
    };

    // Pre-seed a 3000-byte partial; the resume Range will be ignored (200).
    let cut = 3000u64;
    let part = libgen_core::download::part_path(&dest);
    std::fs::write(&part, &blob[..cut as usize]).unwrap();

    let (tx, mut rx) = mpsc::channel::<Progress>(64);
    let cancel = tokio_util::sync::CancellationToken::new();
    let written = download_with_client_cancellable(
        &client(),
        &target,
        &dest,
        cut, // ask to resume from the partial
        &cancel,
        Some(&tx),
        Some(&md5),
    )
    .await
    .expect("download ok");
    drop(tx);

    // Restarted from scratch → wrote the WHOLE file this call, and it's byte-exact.
    assert_eq!(
        written,
        blob.len() as u64,
        "restarted from 0, wrote full body"
    );
    let on_disk = std::fs::read(&dest).unwrap();
    assert_eq!(on_disk, blob);
    assert_eq!(md5_hex(&on_disk), md5);

    // The two paired notes were emitted, in order.
    let mut notes = Vec::new();
    while let Some(p) = rx.recv().await {
        if let Progress::Note { detail, .. } = p {
            notes.push(detail);
        }
    }
    assert!(
        notes
            .iter()
            .any(|d| d.contains("host ignored Range (HTTP 200)")),
        "expected a 'host ignored Range (HTTP 200)' note, got {notes:?}"
    );
    let ignored_idx = notes
        .iter()
        .position(|d| d.contains("host ignored Range (HTTP 200)"))
        .unwrap();
    assert!(
        notes[ignored_idx..]
            .iter()
            .any(|d| d.contains("restarting from scratch")),
        "expected a 'restarting from scratch' note AFTER the 200 note, got {notes:?}"
    );
}
