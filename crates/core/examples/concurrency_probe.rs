//! Empirical probe: is downloading many libgen.li-family books CONCURRENTLY more
//! reliable / faster when spread across multiple mirror front-ends
//! (libgen.li/bz/la/gl/vg) vs all from ONE mirror — given that all five
//! 307-redirect to the SAME CDN (`cdn3.booksdl.lc`)?
//!
//! The question this answers: is throttling per-mirror-hostname / per-get.php
//! session, or per-client-IP at the CDN? If multi-mirror buys nothing, the
//! shared-CDN backend is the bottleneck (per-IP). If multi-mirror measurably
//! helps, the front-door / get.php session is the limiter (per-hostname).
//!
//! Run (from repo root):
//!     cargo run -p libgen-core --example concurrency_probe --release
//!
//! Optional env knobs:
//!     PROBE_CAP_MB      bytes cap per download via Range (default 12)
//!     PROBE_LEVELS      comma concurrency levels (default "3,5,8")
//!     PROBE_REPEATS     repeats per cell (default 2)
//!     PROBE_MD5S        comma md5 list (default: harvested from the app DB)
//!     PROBE_SPACING_MS  ms to sleep between launching each download (default 0)
//!
//! This is a PROBE, not a stress test: total volume is modest (cap × N × cells),
//! it uses a browser-like UA, a 15s connect timeout, and a 30s headers timeout.
//! It only adds this example file; it does NOT touch production code.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use libgen_core::download::{LibgenLiResolver, Resolver};
use reqwest::header::RANGE;
use reqwest::{Client, StatusCode};
use tokio::io::AsyncWriteExt;

/// The five libgen.li-family mirrors that all front cdn3.booksdl.lc.
const MIRRORS: [&str; 5] = [
    "libgen.li",
    "libgen.bz",
    "libgen.la",
    "libgen.gl",
    "libgen.vg",
];

const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/120.0 Safari/537.36 Kwire/1.0";

/// Classify what went wrong, so the report can show error *classes* rather than
/// raw strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ErrClass {
    ResolveFailed,  // ads.php fetch / no get.php link / key scrape failed
    ConnectTimeout, // connect()/TLS never completed within connect_timeout
    NoHeaders,      // connected but no response headers within 30s
    Http502,
    Http522,
    HttpOther(u16),
    Got200NotPartial, // asked for Range, server streamed whole file (no 206)
    Stalled,          // headers OK then body trickled to a stop
    BodyError,        // stream/reset mid-transfer
}

impl std::fmt::Display for ErrClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrClass::ResolveFailed => write!(f, "resolve-failed"),
            ErrClass::ConnectTimeout => write!(f, "connect-timeout"),
            ErrClass::NoHeaders => write!(f, "no-headers-30s"),
            ErrClass::Http502 => write!(f, "HTTP-502"),
            ErrClass::Http522 => write!(f, "HTTP-522"),
            ErrClass::HttpOther(c) => write!(f, "HTTP-{c}"),
            ErrClass::Got200NotPartial => write!(f, "200-not-206"),
            ErrClass::Stalled => write!(f, "stalled"),
            ErrClass::BodyError => write!(f, "body-error"),
        }
    }
}

struct DlOutcome {
    md5: String,
    mirror: String,
    bytes: u64,
    secs: f64,
    /// 206 (Range honored), 200 (full), or None on failure.
    status: Option<u16>,
    err: Option<ErrClass>,
}

impl DlOutcome {
    fn mbps(&self) -> f64 {
        if self.secs > 0.0 {
            (self.bytes as f64 / 1e6) / self.secs
        } else {
            0.0
        }
    }
}

/// Resolve `md5` on `mirror`, then download up to `cap_bytes` via Range, measuring
/// throughput. Returns a fully-classified outcome (never panics on network error).
async fn probe_one(
    client: Client,
    mirror: String,
    md5: String,
    cap_bytes: u64,
    tmp: PathBuf,
) -> DlOutcome {
    let start = Instant::now();
    // --- resolve (real production resolver: ads.php -> get.php?key=...) ---
    let resolver = LibgenLiResolver::new(format!("https://{mirror}"), client.clone());
    let target = match resolver.resolve(&md5).await {
        Ok(t) => t,
        Err(_e) => {
            return DlOutcome {
                md5,
                mirror,
                bytes: 0,
                secs: start.elapsed().as_secs_f64(),
                status: None,
                err: Some(ErrClass::ResolveFailed),
            };
        }
    };

    // --- download with a Range cap (measures real bytes, not just headers) ---
    let dl_start = Instant::now();
    let req = client
        .get(&target.url)
        .header(RANGE, format!("bytes=0-{}", cap_bytes - 1));

    let resp = match tokio::time::timeout(Duration::from_secs(30), req.send()).await {
        Err(_) => {
            return done(md5, mirror, 0, dl_start, None, ErrClass::NoHeaders);
        }
        Ok(Err(e)) => {
            let cls = if e.is_connect() || e.is_timeout() {
                ErrClass::ConnectTimeout
            } else {
                ErrClass::BodyError
            };
            return done(md5, mirror, 0, dl_start, None, cls);
        }
        Ok(Ok(r)) => r,
    };

    let status = resp.status();
    if !(status.is_success() || status == StatusCode::PARTIAL_CONTENT) {
        let cls = match status.as_u16() {
            502 => ErrClass::Http502,
            522 => ErrClass::Http522,
            other => ErrClass::HttpOther(other),
        };
        return done(md5, mirror, 0, dl_start, Some(status.as_u16()), cls);
    }
    // Asked for Range; a 200 means the CDN ignored it (still measures throughput,
    // but we flag it — relevant since the prod downloader fails over on this).
    let got_200_not_206 = status == StatusCode::OK;

    // Stream until cap_bytes, with an idle-stall guard.
    let part = tmp.join(format!("{mirror}-{md5}.part"));
    let mut file = match tokio::fs::File::create(&part).await {
        Ok(f) => f,
        Err(_) => {
            return done(
                md5,
                mirror,
                0,
                dl_start,
                Some(status.as_u16()),
                ErrClass::BodyError,
            )
        }
    };
    let mut stream = resp.bytes_stream();
    let mut written: u64 = 0;
    loop {
        let next = tokio::time::timeout(Duration::from_secs(20), stream.next()).await;
        match next {
            Err(_) => {
                let _ = file.flush().await;
                let _ = tokio::fs::remove_file(&part).await;
                // If we already got some bytes, it's a stall; otherwise no-headers-ish.
                return DlOutcome {
                    md5,
                    mirror,
                    bytes: written,
                    secs: dl_start.elapsed().as_secs_f64(),
                    status: Some(status.as_u16()),
                    err: Some(ErrClass::Stalled),
                };
            }
            Ok(None) => break, // server ended early (file smaller than cap)
            Ok(Some(Err(_e))) => {
                let _ = tokio::fs::remove_file(&part).await;
                return DlOutcome {
                    md5,
                    mirror,
                    bytes: written,
                    secs: dl_start.elapsed().as_secs_f64(),
                    status: Some(status.as_u16()),
                    err: Some(ErrClass::BodyError),
                };
            }
            Ok(Some(Ok(chunk))) => {
                let _ = file.write_all(&chunk).await;
                written += chunk.len() as u64;
                if written >= cap_bytes {
                    break;
                }
            }
        }
    }
    let _ = file.flush().await;
    let _ = tokio::fs::remove_file(&part).await;

    DlOutcome {
        md5,
        mirror,
        bytes: written,
        secs: dl_start.elapsed().as_secs_f64(),
        status: Some(status.as_u16()),
        // A 200 instead of 206 is noted as an err-class for the table, but the
        // download still "succeeded" in delivering bytes (counted as success).
        err: if got_200_not_206 {
            Some(ErrClass::Got200NotPartial)
        } else {
            None
        },
    }
}

fn done(
    md5: String,
    mirror: String,
    bytes: u64,
    start: Instant,
    status: Option<u16>,
    err: ErrClass,
) -> DlOutcome {
    DlOutcome {
        md5,
        mirror,
        bytes,
        secs: start.elapsed().as_secs_f64(),
        status,
        err: Some(err),
    }
}

/// True if the outcome delivered the capped bytes without a hard failure. A
/// 200-not-206 still counts as a success (bytes arrived); it's flagged separately.
fn is_success(o: &DlOutcome) -> bool {
    match &o.err {
        None => true,
        Some(ErrClass::Got200NotPartial) => o.bytes > 0,
        _ => false,
    }
}

struct CellResult {
    succeeded: usize,
    failed: usize,
    wall_secs: f64,
    total_bytes: u64,
    per_dl_mbps: Vec<f64>,
    errors: std::collections::HashMap<String, usize>,
    got_200: usize,
}

/// Run one cell: `c` concurrent downloads of `md5s` (cycled), either all on
/// `single` mirror or round-robin across all MIRRORS.
async fn run_cell(
    client: &Client,
    md5s: &[String],
    c: usize,
    multi: bool,
    cap_bytes: u64,
    tmp: &PathBuf,
    spacing_ms: u64,
) -> CellResult {
    let mut futs = FuturesUnordered::new();
    let wall = Instant::now();
    for i in 0..c {
        let md5 = md5s[i % md5s.len()].clone();
        let mirror = if multi {
            MIRRORS[i % MIRRORS.len()].to_string()
        } else {
            MIRRORS[0].to_string() // libgen.li only
        };
        let client = client.clone();
        let tmp = tmp.clone();
        futs.push(tokio::spawn(probe_one(client, mirror, md5, cap_bytes, tmp)));
        if spacing_ms > 0 {
            tokio::time::sleep(Duration::from_millis(spacing_ms)).await;
        }
    }

    let mut res = CellResult {
        succeeded: 0,
        failed: 0,
        wall_secs: 0.0,
        total_bytes: 0,
        per_dl_mbps: Vec::new(),
        errors: std::collections::HashMap::new(),
        got_200: 0,
    };
    while let Some(joined) = futs.next().await {
        let o = match joined {
            Ok(o) => o,
            Err(_) => continue,
        };
        if is_success(&o) {
            res.succeeded += 1;
            res.total_bytes += o.bytes;
            res.per_dl_mbps.push(o.mbps());
        } else {
            res.failed += 1;
        }
        if matches!(o.err, Some(ErrClass::Got200NotPartial)) {
            res.got_200 += 1;
        }
        if let Some(e) = &o.err {
            // Don't count 200-not-206 as an "error" line if the dl still succeeded.
            if !(matches!(e, ErrClass::Got200NotPartial) && is_success(&o)) {
                *res.errors.entry(e.to_string()).or_insert(0) += 1;
            }
        }
    }
    res.wall_secs = wall.elapsed().as_secs_f64();
    res
}

fn agg_mbps(r: &CellResult) -> f64 {
    if r.wall_secs > 0.0 {
        (r.total_bytes as f64 / 1e6) / r.wall_secs
    } else {
        0.0
    }
}

fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Harvest large, known-good md5s from the app's SQLite DB (DONE candidates
/// preferred, then by descending size). Falls back to a small hard-coded set if
/// the DB isn't present.
fn harvest_md5s() -> Vec<(String, u64, String)> {
    use rusqlite::Connection;
    let db = dirs_app_db();
    let mut out: Vec<(String, u64, String)> = Vec::new();
    if let Ok(conn) = Connection::open(&db) {
        if let Ok(mut stmt) = conn.prepare("SELECT json FROM candidate") {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                let mut seen = std::collections::HashSet::new();
                let mut all: Vec<(String, u64, String, bool)> = Vec::new();
                for row in rows.flatten() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&row) {
                        let md5 = v.get("md5").and_then(|x| x.as_str()).unwrap_or("");
                        let sz = v.get("size_bytes").and_then(|x| x.as_u64()).unwrap_or(0);
                        let title = v
                            .get("title")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .chars()
                            .take(36)
                            .collect::<String>();
                        let done = v
                            .get("job")
                            .and_then(|j| j.get("state"))
                            .and_then(|s| s.as_str())
                            == Some("done");
                        if md5.len() == 32 && sz >= 2_000_000 {
                            all.push((md5.to_string(), sz, title, done));
                        }
                    }
                }
                // DONE first, then descending size; dedup.
                for want_done in [true, false] {
                    let mut sized: Vec<&(String, u64, String, bool)> =
                        all.iter().filter(|r| r.3 == want_done).collect();
                    sized.sort_by(|a, b| b.1.cmp(&a.1));
                    for r in sized {
                        if seen.insert(r.0.clone()) {
                            out.push((r.0.clone(), r.1, r.2.clone()));
                        }
                    }
                }
            }
        }
    }
    out
}

fn dirs_app_db() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/Kwire/library.sqlite3")
}

#[tokio::main]
async fn main() {
    let cap_mb: u64 = std::env::var("PROBE_CAP_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    let cap_bytes = cap_mb * 1_000_000;
    let levels: Vec<usize> = std::env::var("PROBE_LEVELS")
        .unwrap_or_else(|_| "3,5,8".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let repeats: usize = std::env::var("PROBE_REPEATS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let spacing_ms: u64 = std::env::var("PROBE_SPACING_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // md5 set: env override, else harvest from DB.
    let md5s: Vec<String>;
    if let Ok(env) = std::env::var("PROBE_MD5S") {
        md5s = env.split(',').map(|s| s.trim().to_string()).collect();
        println!("Using {} md5s from PROBE_MD5S env.", md5s.len());
    } else {
        let harvested = harvest_md5s();
        let max_needed = *levels.iter().max().unwrap_or(&8);
        let take = harvested.len().min(max_needed.max(10));
        println!(
            "Harvested {} large candidates from the app DB; using {}:",
            harvested.len(),
            take
        );
        for (md5, sz, title) in harvested.iter().take(take) {
            println!("  {md5}  {:7.2} MB  {title}", *sz as f64 / 1e6);
        }
        md5s = harvested.into_iter().take(take).map(|r| r.0).collect();
    }
    if md5s.is_empty() {
        eprintln!("No md5s available (DB missing & PROBE_MD5S unset). Aborting.");
        std::process::exit(1);
    }

    let client = Client::builder()
        .user_agent(BROWSER_UA)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        // Mirror the production pool: a generous per-host connection budget so the
        // single-mirror case isn't artificially connection-starved.
        .pool_max_idle_per_host(16)
        .build()
        .expect("client");

    let tmp = std::env::temp_dir().join("libgen_concurrency_probe");
    let _ = std::fs::create_dir_all(&tmp);

    println!(
        "\nProbe: cap={cap_mb}MB/dl  levels={levels:?}  repeats={repeats}  spacing={spacing_ms}ms\n\
         Mirrors (multi): {}\n\
         Single mirror : {}\n",
        MIRRORS.join(", "),
        MIRRORS[0]
    );
    println!(
        "{:<8} {:<14} {:>4} {:>4} {:>8} {:>10} {:>10} {:>6}  error-classes",
        "concur", "mode", "ok", "fail", "wall_s", "agg_MB/s", "med_MB/s", "200s"
    );
    println!("{}", "-".repeat(96));

    for &c in &levels {
        for multi in [false, true] {
            let mode = if multi { "multi-mirror" } else { "single(li)" };
            // Repeat each cell and report variance.
            let mut agg_samples = Vec::new();
            let mut ok_samples = Vec::new();
            for rep in 0..repeats {
                let r = run_cell(&client, &md5s, c, multi, cap_bytes, &tmp, spacing_ms).await;
                let mut errs: Vec<String> =
                    r.errors.iter().map(|(k, v)| format!("{k}×{v}")).collect();
                errs.sort();
                let errstr = if errs.is_empty() {
                    "-".into()
                } else {
                    errs.join(" ")
                };
                println!(
                    "{:<8} {:<14} {:>4} {:>4} {:>8.1} {:>10.2} {:>10.2} {:>6}  {}",
                    if rep == 0 {
                        format!("C={c}")
                    } else {
                        String::new()
                    },
                    if rep == 0 {
                        mode.to_string()
                    } else {
                        format!("  (rep{})", rep + 1)
                    },
                    r.succeeded,
                    r.failed,
                    r.wall_secs,
                    agg_mbps(&r),
                    median(&r.per_dl_mbps),
                    r.got_200,
                    errstr,
                );
                agg_samples.push(agg_mbps(&r));
                ok_samples.push(r.succeeded as f64);
                // Be polite between cells: short cooldown.
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            if repeats > 1 {
                let mean_agg: f64 = agg_samples.iter().sum::<f64>() / agg_samples.len() as f64;
                let mean_ok: f64 = ok_samples.iter().sum::<f64>() / ok_samples.len() as f64;
                println!(
                    "{:<8} {:<14} {:>4} {:>4} {:>8} {:>10.2} {:>10} {:>6}  (mean of {} reps)",
                    "",
                    "  =mean=",
                    format!("{:.1}", mean_ok),
                    "",
                    "",
                    mean_agg,
                    "",
                    "",
                    repeats
                );
            }
            println!();
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
    println!(
        "Done. Single mirror = {}; multi round-robins {}.\n\
         Compare agg_MB/s and ok counts across the two modes at each C to judge\n\
         whether spreading across front-doors helps even though they share cdn3.booksdl.lc.",
        MIRRORS[0],
        MIRRORS.join("/")
    );
}
