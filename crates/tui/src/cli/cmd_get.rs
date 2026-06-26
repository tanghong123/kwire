//! `kwire get <arg…>` — download by MD5 or by title search + best-match pick.
//!
//! Progress is streamed live: a `\r`-updated progress line when stdout is a
//! TTY, periodic newlines when piped.  Lifecycle events (connecting, done, …)
//! go to stderr so stdout stays clean.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use libgen_core::download::{Resolver, ResolverChain};
use libgen_core::matching;
use libgen_core::model::{BookInput, Format, ListSettings};
use libgen_core::queue::{DownloadRequest, HostLimits, Progress, SchedulerBuilder};
use libgen_engine::{build_search, Config};
use reqwest::Client;
use tokio::sync::mpsc;

use super::emitter::CliEmitter;

#[derive(ClapArgs)]
pub struct GetArgs {
    /// MD5 hash (32 hex chars) or title/query to search for.
    #[arg(required = true, num_args = 1..)]
    pub arg: Vec<String>,

    /// Override the author field (ignored when arg is an MD5).
    #[arg(long)]
    pub author: Option<String>,

    /// Prefer a specific file format (e.g. epub, pdf).
    #[arg(long)]
    pub format: Option<String>,

    /// Download site (default: libgen.li).
    #[arg(long, default_value = "libgen.li")]
    pub site: String,

    /// Output directory for downloaded files.
    #[arg(long, default_value = ".")]
    pub out: String,

    /// Maximum search candidates to rank.
    #[arg(long, default_value_t = 25)]
    pub limit: usize,
}

// Public re-export so mod.rs can name it simply.
pub use GetArgs as Args;

pub async fn run(args: GetArgs) -> Result<()> {
    let query = args.arg.join(" ");

    // Build HTTP client used for both search and download.
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Kwire/1.0")
        .build()
        .context("building http client")?;

    // Detect whether arg is a bare MD5 (32 lowercase hex chars).
    if is_md5(&query) {
        let emitter = CliEmitter::new();
        return download_by_md5(&query, &args.site, &args.out, client, &emitter).await;
    }

    // Title search path.
    let (title, author) = parse_title_author(&query, args.author.as_deref());

    eprintln!("searching  {title}");

    let cfg = Config::from_env();
    let search_client = build_search(&cfg).map_err(|e| anyhow::anyhow!(e))?;

    let mut format_pref = vec![];
    if let Some(ref fmt_str) = args.format {
        format_pref.push(Format::parse(fmt_str));
    }

    let input = BookInput {
        title: title.clone(),
        authors: if author.is_empty() {
            vec![]
        } else {
            vec![author.clone()]
        },
        format_pref,
        ..Default::default()
    };

    let candidates = search_client
        .search(&input)
        .await
        .context("searching mirrors")?;

    if candidates.is_empty() {
        eprintln!("no candidates found — try: kwire search \"{title}\"");
        return Ok(());
    }

    // Rank with the matcher.
    let settings = ListSettings::default();
    let outcome = matching::evaluate(&input, candidates, &settings);

    if outcome.ranked.is_empty() {
        eprintln!("no matching candidates — try: kwire search \"{title}\"");
        return Ok(());
    }

    let best = &outcome.ranked[0];

    // Print the chosen candidate's metadata (two-line format, same as `search`).
    println!("{}", super::cmd_search::format_candidate(1, best));

    let emitter = CliEmitter::new();
    download_by_md5(&best.md5, &args.site, &args.out, client, &emitter).await
}

/// Download a single md5 using `--site`, saving to `--out`.
/// Progress is streamed live through `emitter`.
async fn download_by_md5(
    md5: &str,
    site: &str,
    out: &str,
    client: Client,
    emitter: &CliEmitter,
) -> Result<()> {
    let resolver = build_site_resolver(site, &client)
        .with_context(|| format!("building resolver for site {site:?}"))?;
    let chain = ResolverChain::new(vec![resolver]);

    let limits = HostLimits::default();
    let scheduler = Arc::new(
        SchedulerBuilder::new(chain, client)
            .default_limits(limits)
            .build(),
    );

    let out_dir = PathBuf::from(out);
    let dest = out_dir.join(format!("{md5}.bin"));
    let req = DownloadRequest {
        md5: md5.to_string(),
        dest,
        resume_offset: 0,
        expected_size: None,
    };

    let (tx, mut rx) = mpsc::channel::<Progress>(256);

    // Drain progress events on this task, streaming them live through the emitter.
    // We collect the events here so we don't need to move `emitter` into a spawn.
    let run_handle = {
        let scheduler = Arc::clone(&scheduler);
        tokio::spawn(async move { scheduler.run(vec![req], tx).await })
    };

    while let Some(ev) = rx.recv().await {
        emitter.print_progress(&ev);
    }

    let outcomes = run_handle.await.context("scheduler task panicked")?;

    for o in &outcomes {
        if let Err(e) = &o.result {
            anyhow::bail!("download failed: {e}");
        }
    }

    Ok(())
}

fn build_site_resolver(site: &str, client: &Client) -> Result<Arc<dyn Resolver>> {
    libgen_core::download::resolver_for_site(site, client)
}

// ---------------------------------------------------------------------------
// Helpers — these are the ones the spec asks us to unit-test
// ---------------------------------------------------------------------------

/// True when `s` is exactly 32 lowercase-or-upper hex chars (a bare MD5).
pub fn is_md5(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Split `"title, author"` on the LAST comma into `(title, author)`.
/// `--author` wins when supplied.  Returns `(title, "")` when there is no
/// comma and no explicit author.
pub fn parse_title_author(query: &str, explicit_author: Option<&str>) -> (String, String) {
    if let Some(a) = explicit_author {
        return (query.trim().to_string(), a.trim().to_string());
    }
    if let Some(idx) = query.rfind(',') {
        let title = query[..idx].trim().to_string();
        let author = query[idx + 1..].trim().to_string();
        (title, author)
    } else {
        (query.trim().to_string(), String::new())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- MD5 detection ---

    #[test]
    fn detects_valid_lowercase_md5() {
        assert!(is_md5("1df204c78842ffe549166ffcb984babc"));
    }

    #[test]
    fn detects_valid_uppercase_md5() {
        // All-upper hex is still 32 hex chars — valid MD5 representation.
        assert!(is_md5("1DF204C78842FFE549166FFcb984babc"));
    }

    #[test]
    fn rejects_short_hex() {
        assert!(!is_md5("1df204c78842ffe549166ffcb984bab"));
    }

    #[test]
    fn rejects_non_hex_32_chars() {
        assert!(!is_md5("this is a normal search query ok"));
    }

    #[test]
    fn rejects_title_query() {
        assert!(!is_md5("Treasure Island"));
    }

    // --- title / author split ---

    #[test]
    fn splits_on_last_comma() {
        let (title, author) = parse_title_author("Treasure Island, Robert Louis Stevenson", None);
        assert_eq!(title, "Treasure Island");
        assert_eq!(author, "Robert Louis Stevenson");
    }

    #[test]
    fn last_comma_wins_with_multiple_commas() {
        // "A, B, C" → title="A, B", author="C"
        let (title, author) = parse_title_author("A, B, C", None);
        assert_eq!(title, "A, B");
        assert_eq!(author, "C");
    }

    #[test]
    fn no_comma_returns_empty_author() {
        let (title, author) = parse_title_author("Treasure Island", None);
        assert_eq!(title, "Treasure Island");
        assert_eq!(author, "");
    }

    #[test]
    fn explicit_author_wins_over_comma_split() {
        let (title, author) =
            parse_title_author("Treasure Island, Whoever", Some("Robert Louis Stevenson"));
        assert_eq!(title, "Treasure Island, Whoever");
        assert_eq!(author, "Robert Louis Stevenson");
    }

    #[test]
    fn explicit_author_wins_with_no_comma() {
        let (title, author) = parse_title_author("Treasure Island", Some("Stevenson"));
        assert_eq!(title, "Treasure Island");
        assert_eq!(author, "Stevenson");
    }

    // --- CliEmitter integration: progress formatting used by download_by_md5 ---

    #[test]
    fn emitter_progress_line_contains_percent() {
        // Validate that the progress-line formatter (via the emitter module)
        // produces the expected format for a Bytes event.
        use super::super::emitter::format_progress_line;
        let line = format_progress_line(500, Some(1000), Some(512 * 1024), Some(10));
        assert!(line.contains("50%"), "pct: {line:?}");
        assert!(line.contains("512 KB/s"), "speed: {line:?}");
        assert!(line.contains("10s"), "eta: {line:?}");
    }

    #[test]
    fn emitter_print_progress_done_does_not_panic() {
        let emitter = CliEmitter { is_tty: false };
        let p = Progress::Done {
            md5: "a".repeat(32),
            host: "libgen.li".into(),
            path: PathBuf::from("/tmp/out.epub"),
            bytes_written: 4096,
        };
        // Must not panic; output goes to stdout/stderr which we ignore in tests.
        emitter.print_progress(&p);
    }

    #[test]
    fn emitter_print_progress_bytes_non_tty_does_not_panic() {
        let emitter = CliEmitter { is_tty: false };
        // At 50 % → multiple of 10, so this will println! on non-TTY.
        let p = Progress::Bytes {
            md5: "b".repeat(32),
            leg_id: 0,
            is_hedge: false,
            host: "libgen.li".into(),
            bytes_done: 500,
            total_bytes: Some(1000),
            speed_bps: Some(1024),
            eta_secs: Some(5),
        };
        emitter.print_progress(&p);
    }
}
