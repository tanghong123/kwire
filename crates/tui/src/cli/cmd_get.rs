//! `kwire get <arg…>` — download by MD5 or by title search + best-match pick.
//!
//! Progress is streamed live: a `\r`-updated progress line when stdout is a
//! TTY, periodic newlines when piped.  Lifecycle events (connecting, done, …)
//! go to stderr so stdout stays clean.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use libgen_core::matching;
use libgen_core::model::{BookInput, Format, ListSettings, RequestStatus};
use libgen_core::queue::{DownloadRequest, Progress};
use libgen_engine::{build_scheduler, build_search, Config};
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

    /// Download site(s) to pin (comma-separated). Default: the full
    /// libgen-family failover chain, auto-ordered by live health.
    #[arg(long)]
    pub site: Option<String>,

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

    // App config drives the download scheduler (failover chain + politeness) and
    // the search client.
    let cfg = Config::from_env();
    let site = args.site.as_deref();

    // Detect whether arg is a bare MD5 (32 lowercase hex chars).
    if is_md5(&query) {
        // Bare md5: no candidate metadata → no known size, no format label.
        return download_by_md5(&query, site, &args.out, &cfg, None, None).await;
    }

    // Title search path.
    eprintln!("searching  {query}");

    let search_client = build_search(&cfg).map_err(|e| anyhow::anyhow!(e))?;

    let mut format_pref = vec![];
    if let Some(ref fmt_str) = args.format {
        format_pref.push(Format::parse(fmt_str));
    }

    let input = BookInput {
        title: query.clone(),
        authors: match &args.author {
            Some(a) => vec![a.clone()],
            None => vec![],
        },
        format_pref,
        ..Default::default()
    };

    let mut candidates = search_client
        .search(&input)
        .await
        .context("searching mirrors")?;

    if candidates.is_empty() {
        eprintln!("no candidates found — try: kwire search \"{query}\"");
        return Ok(());
    }

    let settings = ListSettings::default();

    if args.author.is_some() {
        // STRUCTURED path: rank with the SAME match algorithm as the desktop
        // (matching::evaluate), then act on its confidence decision.
        let outcome = matching::evaluate(&input, candidates, &settings);
        match outcome.status {
            // Confident match → auto-download the best, exactly like the desktop.
            RequestStatus::Matched => {
                let best = &outcome.ranked[0];
                println!("{}", super::cmd_search::format_candidate(1, best));
                let label = best.extension.as_ref().map(|f| f.ext().to_uppercase());
                download_by_md5(&best.md5, site, &args.out, &cfg, best.size_bytes, label).await
            }
            // No confident match → degrade to search: show ranked candidates.
            RequestStatus::NeedsSelection => {
                eprintln!("no confident match — pick one and run:  kwire get <md5>");
                for (i, c) in outcome.ranked.iter().take(args.limit).enumerate() {
                    println!("{}", super::cmd_search::format_candidate(i + 1, c));
                }
                Ok(())
            }
            // Nothing usable.
            _ => {
                eprintln!("no matching candidates — try: kwire search \"{query}\"");
                Ok(())
            }
        }
    } else {
        // FREEFORM path: the whole query is "title + author", matched against each
        // candidate's title+author combined. Sort by that score (size desc, md5
        // tie-break), then apply the SAME confidence bands the backend uses.
        for c in &mut candidates {
            c.score = matching::freeform_query_match(&query, c);
        }
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.size_bytes.unwrap_or(0).cmp(&a.size_bytes.unwrap_or(0)))
                .then_with(|| a.md5.cmp(&b.md5))
        });

        let auto = settings.auto_threshold;
        let near = settings.near_threshold;
        let top = candidates.first().map(|c| c.score).unwrap_or(0.0);

        if candidates.is_empty() || top < near {
            eprintln!("no matching candidates — try: kwire search \"{query}\"");
            Ok(())
        } else if top >= auto {
            // Confident match → auto-download the best.
            let best = &candidates[0];
            println!("{}", super::cmd_search::format_candidate(1, best));
            let label = best.extension.as_ref().map(|f| f.ext().to_uppercase());
            download_by_md5(&best.md5, site, &args.out, &cfg, best.size_bytes, label).await
        } else {
            // Middling → degrade to a pick-one list, download nothing.
            eprintln!("no confident match — pick one and run:  kwire get <md5>");
            for (i, c) in candidates.iter().take(args.limit).enumerate() {
                println!("{}", super::cmd_search::format_candidate(i + 1, c));
            }
            Ok(())
        }
    }
}

/// Download a single md5, saving to `--out`. `site` pins specific mirror(s)
/// (comma-separated); `None` uses the full libgen-family failover chain.
/// Progress is streamed live through `emitter`.
async fn download_by_md5(
    md5: &str,
    site: Option<&str>,
    out: &str,
    cfg: &Config,
    expected_size: Option<u64>,
    format_label: Option<String>,
) -> Result<()> {
    // The chronicle lines ("EPUB started on …") carry the format label when the
    // caller knows it (title-search path); the bare-md5 path omits it.
    let emitter = CliEmitter::with_label(format_label);
    // Use the engine's scheduler builder: full failover chain (auto-ordered by
    // SLUM health) when no `--site` is pinned, and a DOWNLOAD client with only a
    // connect timeout (no overall timeout — large streaming bodies must not be
    // killed mid-flight).
    let scheduler = Arc::new(build_scheduler(site, cfg).context("building download scheduler")?);

    let out_dir = PathBuf::from(out);
    let dest = out_dir.join(format!("{md5}.bin"));
    // Keep a copy of the destination so we can inspect the saved file (page
    // check) after the download completes — `dest` itself is moved into the req.
    let dest_clone = dest.clone();
    let req = DownloadRequest {
        md5: md5.to_string(),
        dest,
        resume_offset: 0,
        expected_size,
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

    // The download succeeded, which means the download layer already verified the
    // md5 (a mismatch deletes the .part and permanent-errors). Now apply the SAME
    // post-download page check the desktop/TUI do — warn when a file has
    // suspiciously few pages (a sample, the wrong file, or a corrupt download).
    // The saved file is `<md5>.bin`, so page_count sniffs the format by magic
    // bytes. Print to stderr so stdout stays clean.
    match libgen_core::pagecount::page_count(&dest_clone) {
        Some(pages) if pages < libgen_core::pagecount::LOW_PAGE_THRESHOLD => {
            eprintln!("⚠ md5 verified · {pages} pages — suspiciously few (sample/wrong/corrupt?)");
        }
        Some(pages) => {
            eprintln!("✓ md5 verified · {pages} pages");
        }
        None => {
            eprintln!("✓ md5 verified");
        }
    }

    Ok(())
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
///
/// No longer used by the default `get` path (replaced by the freeform
/// title+author match), but kept (and still unit-tested) for the comma-split
/// semantics.
#[allow(dead_code)]
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
        let emitter = CliEmitter::for_test(false);
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
        let emitter = CliEmitter::for_test(false);
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
