//! Verifies the GLOBAL download concurrency cap (`G`, the
//! `SchedulerBuilder::global_concurrency` knob — docs/DOWNLOAD_SCHEDULING.md §3):
//! no more than `G` download legs transfer at once, even when the per-host caps
//! would allow more. A mock host records the PEAK number of simultaneous
//! connections; with `G = 2` and a high per-host cap, the peak must never exceed
//! 2 though 5 books are submitted at once.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use libgen_core::download::{md5_hex, DirectUrlResolver, ResolverChain};
use libgen_core::queue::{DownloadRequest, HostLimits, Progress, SchedulerBuilder};
use reqwest::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};

/// A mock host that serves slow bodies and tracks the peak concurrent connections.
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
                    let _ = handle_conn(sock, b, i, p, delay).await;
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

async fn handle_conn(
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

    // Count this transfer as in-flight for the duration of the (slow) body write.
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
    tokio::time::sleep(delay).await; // hold the slot so transfers overlap
    let _ = sock.write_all(&body[mid..]).await;
    let _ = sock.flush().await;

    inflight.fetch_sub(1, Ordering::SeqCst);
    Ok(())
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

#[tokio::test]
async fn global_cap_limits_total_concurrent_downloads() {
    const G: usize = 2;
    const BOOKS: usize = 5;

    let server = MockHost::start(Duration::from_millis(250)).await;

    // BOOKS distinct blobs (distinct md5s) all served by the one mock host.
    let mut reqs = Vec::new();
    let out = std::env::temp_dir().join(format!("lgdl-gcap-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    for i in 0..BOOKS {
        let blob = (0..4000)
            .map(|b| ((b + i) % 251) as u8)
            .collect::<Vec<u8>>();
        let md5 = md5_hex(&blob);
        server.set(&md5, blob.clone()).await;
        reqs.push(DownloadRequest::new(
            md5.clone(),
            out.join(format!("{i}.epub")),
        ));
    }

    // Per-host cap HIGH (8) so the GLOBAL cap (2) is the binding constraint.
    let resolver = DirectUrlResolver::new("mock", server.template(), client());
    let chain = ResolverChain::new(vec![Arc::new(resolver)]);
    let scheduler = Arc::new(
        SchedulerBuilder::new(chain, client())
            .default_limits(HostLimits {
                max_concurrency: 8,
                min_interval: Duration::from_millis(0),
                max_attempts: 2,
            })
            .global_concurrency(G)
            .build(),
    );

    let (tx, mut rx) = mpsc::channel::<Progress>(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let outcomes = scheduler.run(reqs, tx).await;
    let _ = drain.await;

    // All books downloaded.
    assert_eq!(outcomes.len(), BOOKS);
    assert!(
        outcomes.iter().all(|o| o.result.is_ok()),
        "all downloads should succeed: {:?}",
        outcomes
            .iter()
            .map(|o| (&o.md5, &o.result))
            .collect::<Vec<_>>()
    );
    // The crux: never more than G transfers at once, despite the per-host cap of 8
    // and BOOKS submitted together.
    let peak = server.peak.load(Ordering::SeqCst);
    assert!(
        peak <= G,
        "global cap exceeded: peak concurrent transfers = {peak}, G = {G}"
    );
    // And it actually parallelized up to G (not accidentally serialized).
    assert!(
        peak >= 2,
        "expected the cap to allow {G} at once, saw peak {peak}"
    );
}
