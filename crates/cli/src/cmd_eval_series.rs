//! `libgen eval-series <list.json> --out <dir> [--replay <dir>]` — the
//! THREE-SOURCE series evaluation harness.
//!
//! For EACH book in the list, run ALL THREE series sources INDEPENDENTLY
//! (OpenLibrary, libgen `series.php`, Goodreads) and write, for human review:
//!   - `<out>/raw/<slug>.<source>.<n>.{html,json}` — every fetched page (the
//!     oracle loop: keep the raw source so the parser can be validated against
//!     the page's ACTUAL content).
//!   - `<out>/<slug>.json` — the per-source structured result.
//!   - append to `<out>/comparison.tsv` — one row per book, counts + agreement.
//!   - `<out>/RELIABILITY.md` — totals per source, agreement, median count,
//!     notable failures.
//!
//! The harness is resilient: if a source errors, the error is recorded and the
//! run continues. A polite delay separates books. Live by default; fully offline
//! with `--replay <dir>` (which replays the `<out>/raw` layout the harness
//! itself records).

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use libgen_core::search::Transport;
use libgen_core::series::{
    GoodreadsClient, LibgenSeriesClient, Series, SeriesClient, SeriesMember,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

#[derive(ClapArgs)]
pub struct Args {
    /// Path to a JSON list of books (see [`InputList`]): either a bare array of
    /// `{title, author}` or `{books:[…]}`, or a parsed `DownloadList`.
    pub list: PathBuf,

    /// Output directory for raw pages, per-book JSON, comparison.tsv, RELIABILITY.md.
    #[arg(long)]
    pub out: PathBuf,

    /// Replay recorded responses from this dir (offline). When omitted, the
    /// harness runs LIVE and records the raw pages it fetches into `<out>/raw`.
    #[arg(long)]
    pub replay: Option<PathBuf>,

    /// Polite delay between books, in milliseconds (live runs only).
    #[arg(long, default_value_t = 1500)]
    pub delay_ms: u64,
}

/// A book to evaluate.
#[derive(Debug, Clone, Deserialize)]
struct EvalBook {
    title: String,
    #[serde(default)]
    author: String,
}

/// Accepted input shapes for the book list.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum InputList {
    Array(Vec<EvalBook>),
    Wrapped { books: Vec<EvalBook> },
}

impl InputList {
    fn books(self) -> Vec<EvalBook> {
        match self {
            InputList::Array(v) => v,
            InputList::Wrapped { books } => books,
        }
    }
}

/// Per-source result for one book.
#[derive(Debug, Clone, Serialize, Default)]
struct SourceResult {
    found: bool,
    count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    series_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    series_name: Option<String>,
    members: Vec<MemberOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    err: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct MemberOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    position: Option<u32>,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    md5: Option<String>,
}

impl From<&SeriesMember> for MemberOut {
    fn from(m: &SeriesMember) -> Self {
        MemberOut {
            position: m.position,
            title: m.title.clone(),
            md5: m.md5.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct PerSource {
    ol: SourceResult,
    libgen: SourceResult,
    goodreads: SourceResult,
}

#[derive(Debug, Clone, Serialize)]
struct BookOut {
    title: String,
    author: String,
    per_source: PerSource,
}

pub async fn run(args: Args) -> Result<()> {
    let raw = std::fs::read_to_string(&args.list)
        .with_context(|| format!("reading list {}", args.list.display()))?;
    let books = parse_list(&raw)?;

    let out_dir = &args.out;
    let raw_dir = out_dir.join("raw");
    std::fs::create_dir_all(&raw_dir).with_context(|| format!("creating {}", raw_dir.display()))?;

    // Fresh comparison.tsv with a header.
    let tsv_path = out_dir.join("comparison.tsv");
    std::fs::write(&tsv_path, "slug\ttitle\tol\tlibgen\tgoodreads\tagreement\n")
        .with_context(|| format!("writing {}", tsv_path.display()))?;

    let mut all: Vec<BookOut> = Vec::new();
    let n = books.len();
    for (i, book) in books.into_iter().enumerate() {
        let slug = slugify(&book.title);
        eprintln!("[{}/{}] {} — {}", i + 1, n, book.title, book.author);

        let per_source = eval_book(&book, &slug, &raw_dir, args.replay.as_deref()).await;

        // Per-book JSON.
        let book_out = BookOut {
            title: book.title.clone(),
            author: book.author.clone(),
            per_source,
        };
        let book_json = out_dir.join(format!("{slug}.json"));
        std::fs::write(&book_json, serde_json::to_string_pretty(&book_out)?)
            .with_context(|| format!("writing {}", book_json.display()))?;

        // Append a comparison row.
        append_tsv(&tsv_path, &slug, &book_out)?;

        all.push(book_out);

        // Polite delay between books (live only).
        if args.replay.is_none() && i + 1 < n {
            tokio::time::sleep(Duration::from_millis(args.delay_ms)).await;
        }
    }

    // RELIABILITY.md summary.
    let report = build_reliability(&all);
    let rel_path = out_dir.join("RELIABILITY.md");
    std::fs::write(&rel_path, report).with_context(|| format!("writing {}", rel_path.display()))?;

    eprintln!(
        "eval-series complete: {} books → {}",
        all.len(),
        out_dir.display()
    );
    Ok(())
}

/// Parse the input list, accepting an array / `{books}` of `{title, author}`, or
/// a `DownloadList` (each group's books, first author joined).
fn parse_list(raw: &str) -> Result<Vec<EvalBook>> {
    if let Ok(list) = serde_json::from_str::<InputList>(raw) {
        let books = list.books();
        if !books.is_empty() {
            return Ok(books);
        }
    }
    // Fall back to a DownloadList shape.
    let dl: libgen_core::model::DownloadList =
        serde_json::from_str(raw).context("parsing book list (array, {books}, or DownloadList)")?;
    let mut out = Vec::new();
    collect_group_books(&dl.groups, &mut out);
    Ok(out)
}

fn collect_group_books(groups: &[libgen_core::model::Group], out: &mut Vec<EvalBook>) {
    for g in groups {
        for b in &g.books {
            out.push(EvalBook {
                title: b.input.title.clone(),
                author: b.input.authors.first().cloned().unwrap_or_default(),
            });
        }
        collect_group_books(&g.subgroups, out);
    }
}

/// Run all three sources for one book, recording raw pages under `raw_dir`.
async fn eval_book(
    book: &EvalBook,
    slug: &str,
    raw_dir: &Path,
    replay: Option<&Path>,
) -> PerSource {
    // OpenLibrary.
    let ol = {
        let t = make_transport(raw_dir, slug, "ol", replay);
        let client = SeriesClient::new(box_ol(t));
        to_source_result(client.lookup(&book.title, &book.author).await)
    };
    // libgen series.php.
    let libgen = {
        let t = make_transport(raw_dir, slug, "libgen", replay);
        let client = LibgenSeriesClient::new(t);
        to_source_result(client.lookup(&book.title, &book.author).await)
    };
    // Goodreads.
    let goodreads = {
        let t = make_transport(raw_dir, slug, "goodreads", replay);
        let client = GoodreadsClient::new(t);
        to_source_result(client.lookup(&book.title, &book.author).await)
    };
    PerSource {
        ol,
        libgen,
        goodreads,
    }
}

/// Build the transport for one source: replay from the recorded raw layout when
/// `--replay` is set; otherwise live + record each fetched page into `raw_dir`.
fn make_transport(
    raw_dir: &Path,
    slug: &str,
    source: &str,
    replay: Option<&Path>,
) -> Box<dyn Transport> {
    match replay {
        Some(dir) => Box::new(EvalReplayTransport::new(dir.to_path_buf(), slug, source)),
        None => Box::new(EvalRecordingTransport::new(
            Box::new(libgen_core::search::LiveTransport::new()),
            raw_dir.to_path_buf(),
            slug,
            source,
        )),
    }
}

/// The OL client wants an `OlTransport`; bridge a `search::Transport` to it so all
/// three sources share one recording/replay transport family.
fn box_ol(t: Box<dyn Transport>) -> Box<dyn libgen_core::series::OlTransport> {
    Box::new(OlBridge(t))
}

struct OlBridge(Box<dyn Transport>);

#[async_trait::async_trait]
impl libgen_core::series::OlTransport for OlBridge {
    async fn get(&self, url: &str) -> Result<String> {
        self.0.get(url).await
    }
}

/// Map a source lookup `Result<Option<Series>>` into a [`SourceResult`].
fn to_source_result(r: Result<Option<Series>>) -> SourceResult {
    match r {
        Ok(Some(s)) => SourceResult {
            found: true,
            count: s.members.len(),
            series_key: Some(s.key),
            series_name: Some(s.name),
            members: s.members.iter().map(MemberOut::from).collect(),
            err: None,
        },
        Ok(None) => SourceResult {
            found: false,
            ..Default::default()
        },
        Err(e) => SourceResult {
            found: false,
            err: Some(format!("{e:#}")),
            ..Default::default()
        },
    }
}

/// Append one comparison row: per-source counts + an agreement verdict.
fn append_tsv(path: &Path, slug: &str, b: &BookOut) -> Result<()> {
    use std::io::Write;
    let ps = &b.per_source;
    let cell = |s: &SourceResult| -> String {
        if let Some(e) = &s.err {
            format!("ERR({})", e.chars().take(20).collect::<String>())
        } else if s.found {
            s.count.to_string()
        } else {
            "0".to_string()
        }
    };
    let agreement = agreement_verdict(ps);
    let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
    writeln!(
        f,
        "{slug}\t{}\t{}\t{}\t{}\t{}",
        b.title,
        cell(&ps.ol),
        cell(&ps.libgen),
        cell(&ps.goodreads),
        agreement
    )?;
    Ok(())
}

/// A short agreement verdict across the sources that resolved (found ≥1 member):
/// "none" (no source resolved), "only:<src>" (one), or "agree"/"disagree" when
/// ≥2 resolved (member counts within 1 = agree).
fn agreement_verdict(ps: &PerSource) -> String {
    let mut resolved: Vec<(&str, usize)> = Vec::new();
    if ps.ol.found {
        resolved.push(("ol", ps.ol.count));
    }
    if ps.libgen.found {
        resolved.push(("libgen", ps.libgen.count));
    }
    if ps.goodreads.found {
        resolved.push(("goodreads", ps.goodreads.count));
    }
    match resolved.len() {
        0 => "none".to_string(),
        1 => format!("only:{}", resolved[0].0),
        _ => {
            let max = resolved.iter().map(|(_, c)| *c).max().unwrap();
            let min = resolved.iter().map(|(_, c)| *c).min().unwrap();
            if max - min <= 1 {
                "agree".to_string()
            } else {
                "disagree".to_string()
            }
        }
    }
}

/// Build the RELIABILITY.md report from all per-book results.
fn build_reliability(all: &[BookOut]) -> String {
    let total = all.len();
    let mut s = String::new();
    s.push_str("# Series resolver reliability\n\n");
    s.push_str(&format!("Books evaluated: **{total}**\n\n"));

    let stats = |pick: &dyn Fn(&BookOut) -> &SourceResult, name: &str| -> String {
        let results: Vec<&SourceResult> = all.iter().map(pick).collect();
        let resolved = results.iter().filter(|r| r.found).count();
        let errored = results.iter().filter(|r| r.err.is_some()).count();
        let mut counts: Vec<usize> = results
            .iter()
            .filter(|r| r.found)
            .map(|r| r.count)
            .collect();
        counts.sort_unstable();
        let median = if counts.is_empty() {
            0.0
        } else if counts.len() % 2 == 1 {
            counts[counts.len() / 2] as f64
        } else {
            (counts[counts.len() / 2 - 1] + counts[counts.len() / 2]) as f64 / 2.0
        };
        format!("| **{name}** | {resolved}/{total} | {errored} | {median} |\n")
    };

    s.push_str("## Per-source totals\n\n");
    s.push_str("| Source | Resolved | Errored | Median members |\n");
    s.push_str("|---|---|---|---|\n");
    s.push_str(&stats(&|b| &b.per_source.ol, "OpenLibrary"));
    s.push_str(&stats(&|b| &b.per_source.libgen, "libgen series.php"));
    s.push_str(&stats(&|b| &b.per_source.goodreads, "Goodreads"));
    s.push('\n');

    // Agreement breakdown.
    let mut agree = 0;
    let mut disagree = 0;
    let mut only = 0;
    let mut none = 0;
    for b in all {
        match agreement_verdict(&b.per_source).as_str() {
            "agree" => agree += 1,
            "disagree" => disagree += 1,
            "none" => none += 1,
            _ => only += 1,
        }
    }
    s.push_str("## Cross-source agreement\n\n");
    s.push_str(&format!(
        "- agree (≥2 sources, counts within 1): **{agree}**\n"
    ));
    s.push_str(&format!(
        "- disagree (≥2 sources, counts differ): **{disagree}**\n"
    ));
    s.push_str(&format!("- only one source resolved: **{only}**\n"));
    s.push_str(&format!("- no source resolved: **{none}**\n\n"));

    // Notable failures / disagreements, per book.
    s.push_str("## Per-book detail\n\n");
    s.push_str("| Book | OL | libgen | Goodreads | verdict |\n");
    s.push_str("|---|---|---|---|---|\n");
    for b in all {
        let cell = |r: &SourceResult| -> String {
            if let Some(e) = &r.err {
                format!("ERR: {}", e.chars().take(40).collect::<String>())
            } else if r.found {
                format!("{}", r.count)
            } else {
                "—".to_string()
            }
        };
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            b.title,
            cell(&b.per_source.ol),
            cell(&b.per_source.libgen),
            cell(&b.per_source.goodreads),
            agreement_verdict(&b.per_source),
        ));
    }
    s.push('\n');

    // Explicit failures list.
    let mut failures: Vec<String> = Vec::new();
    for b in all {
        for (name, r) in [
            ("OL", &b.per_source.ol),
            ("libgen", &b.per_source.libgen),
            ("goodreads", &b.per_source.goodreads),
        ] {
            if let Some(e) = &r.err {
                failures.push(format!("- **{}** [{}]: {}", b.title, name, e));
            } else if !r.found {
                failures.push(format!("- **{}** [{}]: not resolved", b.title, name));
            }
        }
    }
    if !failures.is_empty() {
        s.push_str("## Notable failures\n\n");
        for f in failures {
            s.push_str(&f);
            s.push('\n');
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Eval transports: record raw pages under `<out>/raw`, replay them back.
// ---------------------------------------------------------------------------

/// Records each fetched page under `<raw>/<slug>.<source>.<n>.{html,json}`, in
/// fetch order, while delegating the actual GET to a live transport. The
/// per-(slug,source) sequence counter makes the raw filenames deterministic so a
/// later `--replay` can serve the SAME sequence of GETs back.
struct EvalRecordingTransport {
    inner: Box<dyn Transport>,
    raw: PathBuf,
    slug: String,
    source: String,
    n: Mutex<usize>,
}

impl EvalRecordingTransport {
    fn new(inner: Box<dyn Transport>, raw: PathBuf, slug: &str, source: &str) -> Self {
        EvalRecordingTransport {
            inner,
            raw,
            slug: slug.to_string(),
            source: source.to_string(),
            n: Mutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Transport for EvalRecordingTransport {
    async fn get(&self, url: &str) -> Result<String> {
        let body = self.inner.get(url).await?;
        let seq = {
            let mut g = self.n.lock().unwrap();
            let v = *g;
            *g += 1;
            v
        };
        let ext = ext_for(&body);
        let path = self
            .raw
            .join(format!("{}.{}.{}.{}", self.slug, self.source, seq, ext));
        let _ = std::fs::write(&path, &body);
        Ok(body)
    }
}

/// Replays the raw pages an [`EvalRecordingTransport`] wrote, in the same fetch
/// order (by the per-(slug,source) sequence counter).
struct EvalReplayTransport {
    raw: PathBuf,
    slug: String,
    source: String,
    n: Mutex<usize>,
}

impl EvalReplayTransport {
    fn new(raw: PathBuf, slug: &str, source: &str) -> Self {
        // `--replay <dir>` may point at the out dir or directly at its `raw`.
        let raw = if raw.join("raw").is_dir() {
            raw.join("raw")
        } else {
            raw
        };
        EvalReplayTransport {
            raw,
            slug: slug.to_string(),
            source: source.to_string(),
            n: Mutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Transport for EvalReplayTransport {
    async fn get(&self, _url: &str) -> Result<String> {
        let seq = {
            let mut g = self.n.lock().unwrap();
            let v = *g;
            *g += 1;
            v
        };
        for ext in ["json", "html", "txt"] {
            let path = self
                .raw
                .join(format!("{}.{}.{}.{}", self.slug, self.source, seq, ext));
            if path.exists() {
                return std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()));
            }
        }
        anyhow::bail!(
            "no recorded eval page for {}.{}.{} in {}",
            self.slug,
            self.source,
            seq,
            self.raw.display()
        )
    }
}

fn ext_for(body: &str) -> &'static str {
    let t = body.trim_start();
    if t.starts_with('[') || t.starts_with('{') {
        "json"
    } else {
        "html"
    }
}

/// Filesystem-safe slug (lowercase alphanumerics, runs collapsed to a dash).
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("book");
    }
    out
}
