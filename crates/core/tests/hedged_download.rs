//! Headless validation harness for the speculative (hedged) download feature
//! (`docs/SPECULATIVE_DOWNLOAD.md`). This is the GATE: the hedging logic in the
//! scheduler must pass these scenarios before any app wiring.
//!
//! Like `download_queue.rs`, every test runs offline against a hand-rolled tokio
//! TCP "mock" HTTP server (no real mirrors). The mock adds a **trickle** mode:
//! many tiny body writes with a per-chunk sleep, so the *windowed* throughput
//! stays at/below the stall threshold (modeling a crawling — not erroring —
//! mirror), which is exactly what the stall detector measures.
//!
//! Scenarios (per the task brief):
//!   1. stall→hedge→fast-wins
//!   2. primary-wins-after-hedge-launched
//!   3. all-slow (no panic, completes, caps respected)
//!   4. disabled (default) → a stalled download does NOT hedge (one leg only)

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use libgen_core::download::{md5_hex, DownloadError, DownloadTarget, Resolver, ResolverChain};
use libgen_core::queue::{DownloadRequest, HedgeConfig, HostLimits, Progress, SchedulerBuilder};
use reqwest::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};

// ---------------------------------------------------------------------------
// Mock HTTP server (with a trickle mode for sustained-slow streams)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct PathConfig {
    body: Arc<Vec<u8>>,
    /// Total time to spread the body over, written as many small chunks with a
    /// per-chunk sleep. `ZERO` = serve at full speed in one write. A long
    /// `trickle` over a large body keeps the windowed rate below the threshold.
    trickle: Duration,
    /// Number of chunks to split the body into when trickling.
    chunks: usize,
}

impl PathConfig {
    fn new(body: Vec<u8>) -> Self {
        PathConfig {
            body: Arc::new(body),
            trickle: Duration::ZERO,
            chunks: 1,
        }
    }
    /// A sustained-slow path: spread the body over `total` with `chunks` writes.
    fn trickle(body: Vec<u8>, total: Duration, chunks: usize) -> Self {
        PathConfig {
            body: Arc::new(body),
            trickle: total,
            chunks: chunks.max(1),
        }
    }
}

#[derive(Default)]
struct PathStats {
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    total: AtomicUsize,
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

    let stat = stats
        .lock()
        .await
        .entry(path.clone())
        .or_insert_with(|| Arc::new(PathStats::default()))
        .clone();
    stat.total.fetch_add(1, Ordering::SeqCst);
    let cur = stat.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    stat.peak.fetch_max(cur, Ordering::SeqCst);
    struct Guard(Arc<PathStats>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _guard = Guard(stat.clone());

    let body: Vec<u8> = (*cfg.body).clone();
    let total = body.len() as u64;

    let (status_line, slice): (String, Vec<u8>) = match range_start {
        Some(start) if start <= total && start > 0 => {
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

    if cfg.trickle.is_zero() {
        sock.write_all(&slice).await?;
    } else {
        // Spread the body over `trickle` as `chunks` small writes so the windowed
        // throughput (what the stall detector measures) stays low while the
        // connection stays alive (no error).
        let n = cfg.chunks.min(slice.len().max(1));
        let chunk = slice.len().div_ceil(n.max(1));
        let per = cfg.trickle / (n as u32).max(1);
        let mut off = 0;
        while off < slice.len() {
            let end = (off + chunk).min(slice.len());
            sock.write_all(&slice[off..end]).await?;
            sock.flush().await?;
            tokio::time::sleep(per).await;
            off = end;
        }
    }
    sock.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// A resolver with an explicit synthetic `host` label routing to a path prefix,
// so two resolvers against the SAME mock server reach DISTINCT per-host queues.
// ---------------------------------------------------------------------------

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
    async fn resolve(&self, md5: &str) -> Result<DownloadTarget, DownloadError> {
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
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

/// A small hedge config with short timings so tests run fast: a 200ms stall
/// window, an 8 KB/s-equivalent threshold, and a low `min_hedge_file_bytes`.
fn fast_hedge(enabled: bool) -> HedgeConfig {
    HedgeConfig {
        enabled,
        stall_window: Duration::from_millis(200),
        stall_min_bps: 50_000,
        min_hedge_file_bytes: 1024,
        max_legs_per_book: 2,
        max_concurrent_hedges: 2,
    }
}

async fn collect_events(mut rx: mpsc::Receiver<Progress>) -> Vec<Progress> {
    let mut out = Vec::new();
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let unique = format!("lgdl-hedge-{}-{}", std::process::id(), fastrand_u64());
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
    static CTR: AtomicUsize = AtomicUsize::new(0);
    nanos ^ ((CTR.fetch_add(1, Ordering::SeqCst) as u64) << 32)
}

/// No temp/.part files for `dest` are left behind (the hedge temps are siblings
/// named `<dest>.hedge.*`).
fn no_leftover_temps(dest: &std::path::Path) -> bool {
    let dir = dest.parent().unwrap();
    let stem = dest.file_name().unwrap().to_string_lossy().to_string();
    let mut leftovers = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == stem {
            continue; // the final file is fine
        }
        if name.starts_with(&stem) {
            // any sibling .part / .hedge.* of this dest is a leftover
            leftovers.push(name);
        }
    }
    if !leftovers.is_empty() {
        eprintln!("leftover temps for {dest:?}: {leftovers:?}");
    }
    leftovers.is_empty()
}

// ---------------------------------------------------------------------------
// Scenario 1: stall → hedge → fast-wins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stall_then_hedge_fast_wins() {
    let server = MockServer::start().await;
    let blob = make_blob(40_000);
    let md5 = md5_hex(&blob);

    // Host A: crawls — 40 KB spread over ~2s in 40 chunks → ~20 KB/s windowed,
    // below the 50 KB/s threshold, and it never errors.
    server
        .set(
            &format!("/a/{md5}"),
            PathConfig::trickle(blob.clone(), Duration::from_millis(2000), 40),
        )
        .await;
    // Host B: serves fast.
    server
        .set(&format!("/b/{md5}"), PathConfig::new(blob.clone()))
        .await;

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
            .hedge(fast_hedge(true))
            .build(),
    );

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let (tx, rx) = mpsc::channel(512);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched
        .run(vec![DownloadRequest::new(md5.clone(), dest.clone())], tx)
        .await;
    let events = ev_task.await.unwrap();

    // The variation completed.
    assert!(
        outcomes[0].result.is_ok(),
        "variation should complete: {:?}",
        outcomes[0].result
    );
    // A Stalled event fired for host A.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Progress::Stalled { host, .. } if host == "hostA")),
        "expected a Stalled event for the crawling host A"
    );
    // The winner is host B (fast).
    let done_host = events.iter().find_map(|e| match e {
        Progress::Done { host, .. } => Some(host.clone()),
        _ => None,
    });
    assert_eq!(done_host.as_deref(), Some("hostB"), "host B should win");

    // Final file present, byte-exact, md5-verified.
    let on_disk = std::fs::read(&dest).unwrap();
    assert_eq!(on_disk, blob);
    assert_eq!(md5_hex(&on_disk), md5);
    // No stray .part / hedge temp.
    assert!(no_leftover_temps(&dest));

    // Neither host exceeded its concurrency cap.
    let sa = server.stats_for(&format!("/a/{md5}")).await;
    let sb = server.stats_for(&format!("/b/{md5}")).await;
    assert!(sa.peak.load(Ordering::SeqCst) <= 2, "host A cap exceeded");
    assert!(sb.peak.load(Ordering::SeqCst) <= 2, "host B cap exceeded");
    // Host B served the winning bytes.
    assert!(sb.total.load(Ordering::SeqCst) >= 1, "host B should be hit");
}

// ---------------------------------------------------------------------------
// Scenario 2: primary wins after a hedge was launched
// ---------------------------------------------------------------------------

#[tokio::test]
async fn primary_wins_after_hedge_launched() {
    let server = MockServer::start().await;
    let blob = make_blob(40_000);
    let md5 = md5_hex(&blob);

    // Host A: trickles slowly enough to TRIP the stall detector, then completes
    // (it crawls for a while → a hedge launches → but A still finishes first).
    // 40 KB over ~600ms in 40 chunks ⇒ windowed rate dips below threshold during
    // the early window, but A finishes shortly after.
    server
        .set(
            &format!("/a/{md5}"),
            PathConfig::trickle(blob.clone(), Duration::from_millis(600), 40),
        )
        .await;
    // Host B (the hedge target): much slower, so A wins the race.
    server
        .set(
            &format!("/b/{md5}"),
            PathConfig::trickle(blob.clone(), Duration::from_millis(5000), 50),
        )
        .await;

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
            .hedge(HedgeConfig {
                stall_window: Duration::from_millis(150),
                ..fast_hedge(true)
            })
            .build(),
    );

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let (tx, rx) = mpsc::channel(512);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched
        .run(vec![DownloadRequest::new(md5.clone(), dest.clone())], tx)
        .await;
    let events = ev_task.await.unwrap();

    assert!(
        outcomes[0].result.is_ok(),
        "variation should complete: {:?}",
        outcomes[0].result
    );
    // Exactly one Done event (no double final file).
    let dones = events
        .iter()
        .filter(|e| matches!(e, Progress::Done { .. }))
        .count();
    assert_eq!(dones, 1, "exactly one winner");
    // The winner is host A (the primary recovered/finished first).
    let done_host = events.iter().find_map(|e| match e {
        Progress::Done { host, .. } => Some(host.clone()),
        _ => None,
    });
    assert_eq!(done_host.as_deref(), Some("hostA"), "primary A should win");

    let on_disk = std::fs::read(&dest).unwrap();
    assert_eq!(md5_hex(&on_disk), md5);
    // No leftover temp from the cancelled hedge leg.
    assert!(no_leftover_temps(&dest));

    let sa = server.stats_for(&format!("/a/{md5}")).await;
    let sb = server.stats_for(&format!("/b/{md5}")).await;
    assert!(sa.peak.load(Ordering::SeqCst) <= 2, "host A cap exceeded");
    assert!(sb.peak.load(Ordering::SeqCst) <= 2, "host B cap exceeded");
}

// ---------------------------------------------------------------------------
// Scenario 3: all slow — both crawl; no panic, eventual completion, caps held.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_slow_completes_without_panic() {
    let server = MockServer::start().await;
    let blob = make_blob(30_000);
    let md5 = md5_hex(&blob);

    // Both hosts crawl, but finish within the test timeout. A is a touch faster
    // so it eventually wins; neither errors.
    server
        .set(
            &format!("/a/{md5}"),
            PathConfig::trickle(blob.clone(), Duration::from_millis(1200), 30),
        )
        .await;
    server
        .set(
            &format!("/b/{md5}"),
            PathConfig::trickle(blob.clone(), Duration::from_millis(2400), 30),
        )
        .await;

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
            .hedge(fast_hedge(true))
            .build(),
    );

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let (tx, rx) = mpsc::channel(512);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched
        .run(vec![DownloadRequest::new(md5.clone(), dest.clone())], tx)
        .await;
    let _ = ev_task.await.unwrap();

    assert!(
        outcomes[0].result.is_ok(),
        "all-slow should still complete: {:?}",
        outcomes[0].result
    );
    assert_eq!(md5_hex(&std::fs::read(&dest).unwrap()), md5);
    assert!(no_leftover_temps(&dest));

    let sa = server.stats_for(&format!("/a/{md5}")).await;
    let sb = server.stats_for(&format!("/b/{md5}")).await;
    assert!(sa.peak.load(Ordering::SeqCst) <= 2, "host A cap exceeded");
    assert!(sb.peak.load(Ordering::SeqCst) <= 2, "host B cap exceeded");
}

// ---------------------------------------------------------------------------
// Scenario 4: disabled (default) → a stalled download does NOT hedge.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disabled_does_not_hedge() {
    let server = MockServer::start().await;
    let blob = make_blob(30_000);
    let md5 = md5_hex(&blob);

    // Host A crawls (would trip a stall if hedging were on); host B is fast and
    // would win if a hedge ran. With hedging OFF, B must NEVER be hit.
    server
        .set(
            &format!("/a/{md5}"),
            PathConfig::trickle(blob.clone(), Duration::from_millis(1200), 30),
        )
        .await;
    server
        .set(&format!("/b/{md5}"), PathConfig::new(blob.clone()))
        .await;

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
    // Default HedgeConfig is disabled; pass it explicitly to be unambiguous.
    let sched = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 2,
                min_interval: Duration::from_millis(0),
                max_attempts: 2,
            })
            .hedge(fast_hedge(false))
            .build(),
    );

    let dir = tempdir();
    let dest = dir.join("out.bin");
    let (tx, rx) = mpsc::channel(512);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched
        .run(vec![DownloadRequest::new(md5.clone(), dest.clone())], tx)
        .await;
    let events = ev_task.await.unwrap();

    assert!(
        outcomes[0].result.is_ok(),
        "single-leg download should still complete: {:?}",
        outcomes[0].result
    );
    // No Stalled event is acted on into a hedge: host A wins, host B untouched.
    let done_host = events.iter().find_map(|e| match e {
        Progress::Done { host, .. } => Some(host.clone()),
        _ => None,
    });
    assert_eq!(done_host.as_deref(), Some("hostA"), "only host A runs");

    let sa = server.stats_for(&format!("/a/{md5}")).await;
    let sb = server.stats_for(&format!("/b/{md5}")).await;
    assert_eq!(
        sb.total.load(Ordering::SeqCst),
        0,
        "host B must NOT be hit when hedging is disabled (one leg only)"
    );
    assert_eq!(sa.total.load(Ordering::SeqCst), 1, "exactly one leg ran");
    assert_eq!(md5_hex(&std::fs::read(&dest).unwrap()), md5);
    assert!(no_leftover_temps(&dest));
}

// ---------------------------------------------------------------------------
// Caps: many stalled books, a global hedge cap of 1 → host B (the only hedge
// target) never exceeds its concurrency cap; un-started books still complete.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn global_hedge_cap_and_per_host_caps_hold() {
    let server = MockServer::start().await;
    let blob = make_blob(30_000);

    // 4 distinct books, all crawling on host A and fast on host B.
    let mut md5s = Vec::new();
    for i in 0..4 {
        let b: Vec<u8> = blob.iter().map(|x| x.wrapping_add(i as u8)).collect();
        let md5 = md5_hex(&b);
        server
            .set(
                &format!("/a/{md5}"),
                PathConfig::trickle(b.clone(), Duration::from_millis(1500), 30),
            )
            .await;
        server.set(&format!("/b/{md5}"), PathConfig::new(b)).await;
        md5s.push(md5);
    }

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
            .hedge(HedgeConfig {
                max_concurrent_hedges: 1, // global cap of ONE extra leg at a time
                ..fast_hedge(true)
            })
            .build(),
    );

    let dir = tempdir();
    let reqs: Vec<_> = md5s
        .iter()
        .enumerate()
        .map(|(i, m)| DownloadRequest::new(m.clone(), dir.join(format!("f{i}.bin"))))
        .collect();
    let (tx, rx) = mpsc::channel(1024);
    let ev_task = tokio::spawn(collect_events(rx));
    let outcomes = sched.run(reqs, tx).await;
    let _ = ev_task.await.unwrap();

    // Every book completed (un-started books are never starved by hedges).
    assert!(
        outcomes.iter().all(|o| o.result.is_ok()),
        "all books complete: {:?}",
        outcomes.iter().map(|o| &o.result).collect::<Vec<_>>()
    );
    // Per-host concurrency caps held throughout (the real guarantee): host B is
    // the hedge lane and must never exceed its cap of 2 even under the global cap.
    for (i, m) in md5s.iter().enumerate() {
        let sa = server.stats_for(&format!("/a/{m}")).await;
        let sb = server.stats_for(&format!("/b/{m}")).await;
        assert!(
            sa.peak.load(Ordering::SeqCst) <= 2,
            "host A cap exceeded for book {i}"
        );
        assert!(
            sb.peak.load(Ordering::SeqCst) <= 2,
            "host B cap exceeded for book {i}"
        );
        assert_eq!(
            md5_hex(&std::fs::read(dir.join(format!("f{i}.bin"))).unwrap()),
            *m
        );
        assert!(no_leftover_temps(&dir.join(format!("f{i}.bin"))));
    }
}

// ---------------------------------------------------------------------------
// Default config is OFF.
// ---------------------------------------------------------------------------

#[test]
fn hedge_config_default_is_disabled() {
    let cfg = HedgeConfig::default();
    assert!(!cfg.enabled, "speculative download must default to OFF");
    assert_eq!(cfg.max_legs_per_book, 2);
    assert_eq!(cfg.max_concurrent_hedges, 2);
    assert_eq!(cfg.stall_window, Duration::from_secs(15));
    assert_eq!(cfg.stall_min_bps, 8 * 1024);
    assert_eq!(cfg.min_hedge_file_bytes, 256 * 1024);
}
