//! `libgen download-books` — resolve + ranged/resumable download + md5 verify,
//! driven by the per-host queue scheduler.
//!
//! Flags: --host-concurrency <n>, --rate <per-sec>, --out <dir>, --resume,
//! --mock <url-template> (point the resolver at a local mock server for headless
//! tests; the template contains a `{md5}` placeholder).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use libgen_core::download::{host_of, DirectUrlResolver, Resolver, ResolverChain};
use libgen_core::queue::{DownloadRequest, HostLimits, Progress, SchedulerBuilder};
use reqwest::Client;
use tokio::sync::mpsc;

#[derive(ClapArgs)]
pub struct Args {
    /// One or more md5s of files to download.
    #[arg(required = true)]
    pub md5: Vec<String>,

    /// Output directory.
    #[arg(long, default_value = "downloads")]
    pub out: String,

    /// Max concurrent downloads per host.
    #[arg(long, default_value_t = 2)]
    pub host_concurrency: usize,

    /// Max requests per second per host (rate limit). 0 = unlimited.
    #[arg(long, default_value_t = 0.0)]
    pub rate: f64,

    /// Max attempts per host before failover/failure.
    #[arg(long, default_value_t = 4)]
    pub max_attempts: u32,

    /// Resume from any existing `.part` files instead of restarting.
    #[arg(long)]
    pub resume: bool,

    /// Resolver URL template(s) with a `{md5}` placeholder, e.g.
    /// `http://127.0.0.1:9000/get/{md5}`. Repeat for failover mirrors. Points
    /// the resolver at a local mock server for headless tests.
    #[arg(long = "mock")]
    pub mock: Vec<String>,

    /// Real download site(s) to resolve against, in failover order. Supported:
    /// `libgen.li`, `libgen.vg`, `libgen.la` (shared ads.php/get.php CDN family),
    /// `libgen.pw`, `randombook.org` (independent libgen.download CDN), and
    /// `ipfs` (md5→CID via libgen.li, served by public IPFS gateways). Repeat for
    /// multiple. Mutually combinable with --mock (mocks are tried first).
    #[arg(long = "site")]
    pub site: Vec<String>,
}

pub async fn run(args: Args) -> Result<()> {
    // Real mirrors (libgen.li) gate on the User-Agent, and get.php 307-redirects
    // to a CDN, so a browser-like UA + redirect following (reqwest default) are
    // required.
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Kwire/1.0")
        .build()
        .context("building http client")?;

    // Build the resolver chain: --mock direct-URL resolvers first (for headless
    // tests), then real --site resolvers (failover order). At least one of the
    // two must be provided.
    if args.mock.is_empty() && args.site.is_empty() {
        anyhow::bail!(
            "no download source configured. Pass --site libgen.li for a real \
             mirror, or --mock <url-template-with-{{md5}}> for a mock/direct-get \
             server."
        );
    }

    let mut resolvers: Vec<Arc<dyn Resolver>> = args
        .mock
        .iter()
        .map(|tpl| {
            Arc::new(DirectUrlResolver::new(
                host_of(tpl),
                tpl.clone(),
                client.clone(),
            )) as Arc<dyn Resolver>
        })
        .collect();

    for site in &args.site {
        let resolver = build_site_resolver(site, &client)?;
        resolvers.push(resolver);
    }
    let chain = ResolverChain::new(resolvers);

    let min_interval = if args.rate > 0.0 {
        Duration::from_secs_f64(1.0 / args.rate)
    } else {
        Duration::ZERO
    };
    let limits = HostLimits {
        max_concurrency: args.host_concurrency.max(1),
        min_interval,
        max_attempts: args.max_attempts.max(1),
    };

    let scheduler = Arc::new(
        SchedulerBuilder::new(chain, client)
            .default_limits(limits)
            .build(),
    );

    let out_dir = PathBuf::from(&args.out);
    let requests: Vec<DownloadRequest> = args
        .md5
        .iter()
        .map(|md5| {
            let dest = out_dir.join(format!("{md5}.bin"));
            let resume_offset = if args.resume {
                let part = libgen_core::download::part_path(&dest);
                std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0)
            } else {
                0
            };
            DownloadRequest {
                md5: md5.clone(),
                dest,
                resume_offset,
                expected_size: None,
            }
        })
        .collect();

    let (tx, mut rx) = mpsc::channel::<Progress>(1024);

    // Print progress events as they arrive.
    let printer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                Progress::Resolved {
                    md5,
                    host,
                    total_bytes,
                    ..
                } => {
                    eprintln!(
                        "resolved {md5} -> host={host} total={}",
                        total_bytes
                            .map(|b| b.to_string())
                            .unwrap_or_else(|| "?".into())
                    );
                }
                Progress::Resuming {
                    md5, host, offset, ..
                } => {
                    eprintln!("resuming {md5} on host={host} from offset={offset}");
                }
                Progress::Bytes {
                    md5,
                    bytes_done,
                    total_bytes,
                    ..
                } => {
                    if let Some(total) = total_bytes {
                        eprintln!("  {md5}: {bytes_done}/{total} bytes");
                    } else {
                        eprintln!("  {md5}: {bytes_done} bytes");
                    }
                }
                Progress::Stalled {
                    md5,
                    host,
                    speed_bps,
                    ..
                } => {
                    eprintln!(
                        "  {md5}: stalled on {host} ({} B/s) — racing a mirror",
                        speed_bps.unwrap_or(0)
                    );
                }
                Progress::Retrying {
                    md5,
                    attempt,
                    backoff,
                    error,
                    ..
                } => {
                    eprintln!("  {md5}: retry #{attempt} in {backoff:?} ({error})");
                }
                Progress::FailingOver {
                    md5,
                    from_host,
                    error,
                    ..
                } => {
                    eprintln!("  {md5}: failing over from {from_host} ({error})");
                }
                Progress::Done {
                    md5,
                    path,
                    bytes_written,
                    ..
                } => {
                    eprintln!("done {md5}: {} ({bytes_written} bytes)", path.display());
                }
                Progress::Failed { md5, error } => {
                    eprintln!("FAILED {md5}: {error}");
                }
                Progress::Note { md5, detail } => {
                    eprintln!("note {md5}: {detail}");
                }
                Progress::LegEnded { md5, leg_id } => {
                    eprintln!("leg ended {md5} (leg {leg_id})");
                }
                Progress::Cancelled {
                    md5,
                    paused,
                    resume_offset,
                } => {
                    if paused {
                        eprintln!("paused {md5} at {resume_offset} bytes");
                    } else {
                        eprintln!("cancelled {md5}");
                    }
                }
            }
        }
    });

    let outcomes = scheduler.run(requests, tx).await;
    let _ = printer.await;

    let mut ok = 0usize;
    let mut failed = 0usize;
    for o in &outcomes {
        match &o.result {
            Ok(path) => {
                ok += 1;
                println!("{}\t{}", o.md5, path.display());
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}\tERROR: {e}", o.md5);
            }
        }
    }
    eprintln!("{ok} ok, {failed} failed");

    if failed > 0 {
        anyhow::bail!("{failed} download(s) failed");
    }
    Ok(())
}

/// Map a `--site` name to a concrete resolver via the shared core registry.
fn build_site_resolver(site: &str, client: &Client) -> Result<Arc<dyn Resolver>> {
    libgen_core::download::resolver_for_site(site, client)
}
