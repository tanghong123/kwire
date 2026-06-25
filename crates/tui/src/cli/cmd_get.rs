//! `kwire get <arg…>` — download by MD5 or by title search + best-match pick.

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
        return download_by_md5(&query, &args.site, &args.out, client).await;
    }

    // Title search path.
    let (title, author) = parse_title_author(&query, args.author.as_deref());

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
        println!("No candidates found. Try: kwire search \"{}\"", title);
        return Ok(());
    }

    // Rank with the matcher.
    let settings = ListSettings::default();
    let outcome = matching::evaluate(&input, candidates, &settings);

    if outcome.ranked.is_empty() {
        println!(
            "No matching candidates found. Try: kwire search \"{}\"",
            title
        );
        return Ok(());
    }

    let best = &outcome.ranked[0];

    // Print chosen candidate.
    let authors_str = if best.authors.is_empty() {
        String::from("(unknown author)")
    } else {
        best.authors.join(", ")
    };
    let fmt_str = best
        .extension
        .as_ref()
        .map(|e| e.ext().to_ascii_uppercase())
        .unwrap_or_else(|| "?".to_string());
    let size_str = best
        .size_bytes
        .map(super::cmd_search::human_size)
        .unwrap_or_else(|| "?".to_string());

    println!(
        "Chosen: {} · {} · {} · {} · {}",
        best.title, authors_str, best.md5, fmt_str, size_str
    );

    download_by_md5(&best.md5, &args.site, &args.out, client).await
}

/// Download a single md5 using `--site`, saving to `--out`.
async fn download_by_md5(md5: &str, site: &str, out: &str, client: Client) -> Result<()> {
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
    // Use md5.bin as the temp dest; the file gets renamed after md5 verification.
    let dest = out_dir.join(format!("{md5}.bin"));
    let req = DownloadRequest {
        md5: md5.to_string(),
        dest,
        resume_offset: 0,
        expected_size: None,
    };

    let (tx, mut rx) = mpsc::channel::<Progress>(256);

    // Spawn a simple progress printer.
    let printer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                Progress::Done {
                    md5,
                    path,
                    bytes_written,
                    ..
                } => {
                    println!("Saved: {} ({} bytes)", path.display(), bytes_written);
                    let _ = md5; // silence unused warning
                }
                Progress::Failed { md5, error } => {
                    eprintln!("Failed {md5}: {error}");
                }
                Progress::Resolved {
                    total_bytes, host, ..
                } => {
                    let sz = total_bytes
                        .map(super::cmd_search::human_size)
                        .unwrap_or_else(|| "?".to_string());
                    eprintln!("Resolving via {host} ({sz})…");
                }
                _ => {}
            }
        }
    });

    let outcomes = scheduler.run(vec![req], tx).await;
    let _ = printer.await;

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
}
