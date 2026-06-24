//! `libgen run-list` — drive the whole Phase-5 pipeline end to end, headlessly.
//!
//! parse → persist (SQLite) → query (replay/live) → match → compute destination
//! paths (naming/foldering) → print the planned per-book status + destination
//! filename. By default this is a **dry run** (no bytes fetched); pass `--mock`
//! to actually resolve + download through the scheduler against a direct-get
//! mirror (or a local mock server).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use libgen_core::download::{host_of, DirectUrlResolver, Resolver, ResolverChain};
use libgen_core::orchestrator::{Event, Orchestrator};
use libgen_core::parse;
use libgen_core::queue::{Progress, SchedulerBuilder};
use libgen_core::search::{MirrorConfig, SearchClient};
use libgen_core::store::Store;
use reqwest::Client;
use tokio::sync::mpsc;

#[derive(ClapArgs)]
pub struct Args {
    /// Path to a .md or .json reading list.
    pub file: PathBuf,

    /// Replay recorded search responses from this fixtures dir (offline).
    #[arg(long)]
    pub replay: Option<PathBuf>,

    /// Output directory for downloads / planned destinations.
    #[arg(long, default_value = "downloads")]
    pub out: PathBuf,

    /// Path to mirrors.toml (default: ./mirrors.toml).
    #[arg(long, default_value = "mirrors.toml")]
    pub mirrors: PathBuf,

    /// SQLite database path. Defaults to an ephemeral in-memory DB.
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Force JSON parsing regardless of file extension.
    #[arg(long)]
    pub json: bool,

    /// Actually download via a direct-get resolver template (with `{md5}`),
    /// e.g. `http://127.0.0.1:9000/get/{md5}`. Repeat for failover mirrors.
    /// When omitted (and no --site), the command is a dry run (plan + print only).
    #[arg(long = "mock")]
    pub mock: Vec<String>,

    /// Actually download via a real download site (e.g. `libgen.li`), in
    /// failover order. Repeat for multiple. Combinable with --mock.
    #[arg(long = "site")]
    pub site: Vec<String>,

    /// Request ONE best variation of EACH preferred format per matched book
    /// (e.g. the top epub AND the top pdf), instead of just the single best
    /// copy. Each requested variation downloads to its own file.
    #[arg(long = "all-formats")]
    pub all_formats: bool,

    /// Resume an existing persisted list (of the same title) in `--db` instead
    /// of re-querying: attach, reset any in-flight jobs to pending (keeping
    /// resume offsets), and continue its pending/paused downloads. Implies a
    /// download pass; combine with `--site`/`--mock` for the resolver(s).
    #[arg(long = "resume")]
    pub resume: bool,
}

pub async fn run(args: Args) -> Result<()> {
    // ---- parse ----
    let content = std::fs::read_to_string(&args.file)
        .with_context(|| format!("reading {}", args.file.display()))?;
    let is_json = args.json
        || args
            .file
            .extension()
            .map(|e| e.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
    let list = parse::parse_auto(&content, is_json).context("parsing reading list")?;

    // ---- persist (open store) ----
    let store = match &args.db {
        Some(path) => {
            Store::open(path).with_context(|| format!("opening db {}", path.display()))?
        }
        None => Store::open_in_memory().context("opening in-memory db")?,
    };

    // ---- search client (replay or live) ----
    let config = MirrorConfig::load(&args.mirrors)
        .with_context(|| format!("loading mirrors from {}", args.mirrors.display()))?;
    let search = match &args.replay {
        Some(dir) => SearchClient::replay(config, dir.clone()),
        None => SearchClient::new(config, Box::new(libgen_core::search::LiveTransport::new())),
    };

    // ---- reuse an existing same-title list (resume / dedupe-by-title) ----
    // If a DB already holds a list with this title, attach to it instead of
    // inserting a duplicate. Otherwise insert fresh.
    let existing_id = find_list_by_title(&store, &list.title)?;
    let resuming = args.resume;

    let mut orch = match existing_id {
        Some(id) => {
            println!("(reusing existing list id={id} '{}' in db)", list.title);
            Orchestrator::attach(store, id, search, args.out.clone())
        }
        None => {
            if resuming {
                anyhow::bail!(
                    "--resume given but no existing list titled '{}' in the db",
                    list.title
                );
            }
            Orchestrator::new(store, &list, search, args.out.clone())
                .context("building orchestrator")?
        }
    };

    if resuming {
        // Resume-on-launch: reset any in-flight jobs to pending (keeping resume
        // offsets) and re-pend paused/cancelled work, then continue downloads.
        let reset = orch
            .reset_inflight_for_resume()
            .context("resetting in-flight jobs for resume")?;
        orch.resume_all().context("re-pending paused work")?;
        println!("(resume: reset {reset} in-flight job(s) to pending)");
    } else {
        // ---- query + match (persisted) — only queued books are searched ----
        let (tx, rx) = mpsc::channel::<Event>(1024);
        let ev_task = tokio::spawn(async move {
            let mut rx = rx;
            while rx.recv().await.is_some() {}
        });
        orch.query_all(&tx).await.context("query/match pass")?;
        drop(tx);
        let _ = ev_task.await;
    }

    // ---- optionally request one best variation of EACH preferred format ----
    // (query_all already auto-requested the single best per matched book).
    if args.all_formats {
        request_all_formats(&mut orch).context("requesting all preferred formats")?;
    }

    // ---- compute destinations (naming/foldering) ----
    let planned = orch.plan_downloads().context("planning destinations")?;

    // ---- report ----
    println!("List: {}", list.title);
    println!("Output root: {}", args.out.display());
    println!();
    print_status_tree(&orch.snapshot()?);
    println!();
    println!("Planned downloads ({}):", planned.len());
    for p in &planned {
        let rel = p
            .destination
            .strip_prefix(&args.out)
            .unwrap_or(&p.destination);
        println!(
            "  [{}] {}  ->  {}",
            &p.md5[..8.min(p.md5.len())],
            p.title,
            rel.display()
        );
    }

    // ---- optional real download ----
    if !args.mock.is_empty() || !args.site.is_empty() {
        println!();
        println!(
            "Downloading via {} resolver(s)...",
            args.mock.len() + args.site.len()
        );
        download(&mut orch, &args.mock, &args.site).await?;
    } else {
        println!();
        println!("(dry run — pass --site libgen.li or --mock <url-template> to download)");
    }

    Ok(())
}

/// Find the id of the first persisted list whose title matches `title`, so a
/// repeated `run-list --db <path>` reuses it instead of inserting a duplicate.
fn find_list_by_title(store: &Store, title: &str) -> Result<Option<i64>> {
    let all = store.all_lists().context("listing persisted lists")?;
    Ok(all
        .into_iter()
        .find(|s| s.list.title == title)
        .map(|s| s.id))
}

/// For every matched book, request one best variation of EACH preferred format
/// (the list's `format_pref`, overridden per-book by `input.format_pref`). The
/// top-ranked candidate of each format is requested via `request_variation`
/// (idempotent with the auto-requested best). Returns nothing; persists through
/// the orchestrator.
fn request_all_formats(orch: &mut Orchestrator) -> Result<()> {
    use libgen_core::model::{Format, Group, RequestStatus};

    let list = orch.snapshot()?;
    let default_prefs = list.settings.format_pref.clone();

    // Collect (group_path, book_index, md5) requests to issue, so we don't
    // borrow the snapshot while mutating.
    let mut to_request: Vec<(Vec<usize>, usize, String)> = Vec::new();

    fn walk(
        groups: &[Group],
        path: &mut Vec<usize>,
        default_prefs: &[Format],
        out: &mut Vec<(Vec<usize>, usize, String)>,
    ) {
        for (gi, g) in groups.iter().enumerate() {
            path.push(gi);
            for (bi, b) in g.books.iter().enumerate() {
                if b.status != RequestStatus::Matched {
                    continue;
                }
                let prefs: Vec<Format> = if b.input.format_pref.is_empty() {
                    default_prefs.to_vec()
                } else {
                    b.input.format_pref.clone()
                };
                // Candidates are already rank-ordered; the first per format is the
                // best of that format.
                for fmt in &prefs {
                    if let Some(c) = b
                        .candidates
                        .iter()
                        .find(|c| c.extension.as_ref() == Some(fmt))
                    {
                        out.push((path.clone(), bi, c.md5.clone()));
                    }
                }
            }
            walk(&g.subgroups, path, default_prefs, out);
            path.pop();
        }
    }
    walk(
        &list.groups,
        &mut Vec::new(),
        &default_prefs,
        &mut to_request,
    );

    for (group_path, book_index, md5) in to_request {
        orch.request_variation(&group_path, book_index, &md5)?;
    }
    Ok(())
}

/// Print each book's status in a small indented tree.
fn print_status_tree(list: &libgen_core::model::DownloadList) {
    fn rec(groups: &[libgen_core::model::Group], depth: usize) {
        for g in groups {
            println!("{}# {}", "  ".repeat(depth), g.name);
            for b in &g.books {
                println!(
                    "{}- {}  [{}]",
                    "  ".repeat(depth + 1),
                    b.input.title,
                    status_label(&b.status)
                );
            }
            rec(&g.subgroups, depth + 1);
        }
    }
    rec(&list.groups, 0);
}

fn status_label(s: &libgen_core::model::RequestStatus) -> String {
    use libgen_core::model::RequestStatus::*;
    match s {
        Queued => "queued".into(),
        Querying => "querying".into(),
        Matched => "matched".into(),
        NeedsSelection => "needs_selection".into(),
        NotFound => "not_found".into(),
        Ready => "ready".into(),
        Downloading => "downloading".into(),
        Verifying => "verifying".into(),
        Done => "done".into(),
        Failed { error } => format!("failed: {error}"),
        Paused => "paused".into(),
        Cancelled => "cancelled".into(),
    }
}

async fn download(orch: &mut Orchestrator, mock: &[String], site: &[String]) -> Result<()> {
    // Real mirrors gate on the User-Agent and 307-redirect to a CDN, so use a
    // browser-like UA and reqwest's default redirect-following.
    let client = Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Kwire/1.0")
        .build()
        .context("building http client")?;
    let mut resolvers: Vec<Arc<dyn Resolver>> = mock
        .iter()
        .map(|tpl| {
            Arc::new(DirectUrlResolver::new(
                host_of(tpl),
                tpl.clone(),
                client.clone(),
            )) as Arc<dyn Resolver>
        })
        .collect();
    for s in site {
        resolvers.push(libgen_core::download::resolver_for_site(s, &client)?);
    }
    let chain = ResolverChain::new(resolvers);
    let scheduler = Arc::new(SchedulerBuilder::new(chain, client).build());

    let (tx, mut rx) = mpsc::channel::<Event>(1024);
    let printer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let Event::Download(p) = ev {
                match p {
                    Progress::Done { md5, path, .. } => {
                        println!("  done {}: {}", &md5[..8.min(md5.len())], path.display());
                    }
                    Progress::Failed { md5, error } => {
                        eprintln!("  FAILED {}: {error}", &md5[..8.min(md5.len())]);
                    }
                    _ => {}
                }
            }
        }
    });
    orch.start_downloads(&scheduler, &tx).await?;
    drop(tx);
    let _ = printer.await;
    Ok(())
}
