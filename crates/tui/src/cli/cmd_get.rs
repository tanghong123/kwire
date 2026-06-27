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

use super::emitter::{CliEmitter, CursorGuard};

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
        // Bare md5: no candidate metadata → no known size, no format label, and
        // the saved file falls back to `<md5>.bin`.
        return download_by_md5(&query, site, &args.out, &cfg, None, None, None).await;
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

    // Stream per-mirror activity (which mirror, its outcome) before the verdict,
    // and remember which hosts were tried for a precise exhaustion message.
    let observer = super::emitter::CliSearchObserver::new();
    let mut candidates = search_client
        .search_observed(&input, &observer)
        .await
        .context("searching mirrors")?;

    if candidates.is_empty() {
        let tried = observer.tried_hosts();
        if tried.is_empty() {
            eprintln!("no candidates found — try: kwire search \"{query}\"");
        } else {
            eprintln!(
                "no candidates on any mirror (tried: {}) — try: kwire search \"{query}\"",
                tried.join(", ")
            );
        }
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
                download_by_md5(
                    &best.md5,
                    site,
                    &args.out,
                    &cfg,
                    best.size_bytes,
                    label,
                    Some(best),
                )
                .await
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
            download_by_md5(
                &best.md5,
                site,
                &args.out,
                &cfg,
                best.size_bytes,
                label,
                Some(best),
            )
            .await
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
    candidate: Option<&libgen_core::model::Candidate>,
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
    // Build a PROPER, human-readable filename via the SHARED core naming builder
    // (the same one the desktop/TUI use) when we have candidate metadata: e.g.
    // `Walter Isaacson - Steve Jobs - 3a7029.epub` (Author - Title - <md5:6>.ext,
    // with the real format extension). The bare-md5 path has no metadata, so it
    // falls back to `<md5>.bin` (the page check then sniffs the format).
    let filename = cli_filename(md5, candidate);
    let dest = out_dir.join(filename);
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

    // Hide the terminal cursor for the duration of the live progress bar and
    // restore it on Drop — covers normal completion, the `?` error returns below,
    // and panic unwinds. (No-op when stdout isn't a TTY.)
    let _cursor = CursorGuard::hide(emitter.is_tty);
    // Ctrl-C kills the process WITHOUT running destructors, so the guard's Drop
    // wouldn't fire and the cursor would stay hidden. Restore it explicitly on
    // SIGINT, then exit with the conventional 130, but only when we're a TTY.
    if emitter.is_tty {
        tokio::spawn(async {
            if tokio::signal::ctrl_c().await.is_ok() {
                // \x1b[?25h → show cursor.
                print!("\u{1b}[?25h");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                std::process::exit(130);
            }
        });
    }

    // Drain progress events on this task, streaming them live through the emitter.
    // We collect the events here so we don't need to move `emitter` into a spawn.
    let run_handle = {
        let scheduler = Arc::clone(&scheduler);
        tokio::spawn(async move { scheduler.run(vec![req], tx).await })
    };

    while let Some(ev) = rx.recv().await {
        emitter.print_progress(&ev);
    }

    // Restore the cursor BEFORE the saved/verified chronicle lines print (rather
    // than waiting for the function-end Drop), so those lines render with a
    // visible cursor.
    drop(_cursor);

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
    // Format-aware: PDF reports "N pages", EPUB "N sections" (reflowable — the
    // count is spine sections), and the low flag is per-format (EPUB by readable
    // text length, not section count). Print to stderr so stdout stays clean.
    match libgen_core::pagecount::page_stats(&dest_clone) {
        Some(s) if s.low => {
            eprintln!(
                "⚠ md5 verified · {} {} — suspiciously short (sample/wrong/corrupt?)",
                s.count,
                s.unit.label()
            );
        }
        Some(s) => {
            eprintln!("✓ md5 verified · {} {}", s.count, s.unit.label());
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

/// Build the saved filename for a `kwire get` download.
///
/// With candidate metadata → the SHARED core naming builder ([`naming::filename`])
/// under a CLI template `{authors} - {title}.{ext}`, which also appends the
/// ` - <md5:6>` uniqueness tag and the REAL format extension → e.g.
/// `Walter Isaacson - Steve Jobs - 3a7029.epub`. Without metadata (bare-md5
/// path) → `<md5>.bin` so the post-download page check can sniff the format.
fn cli_filename(md5: &str, candidate: Option<&libgen_core::model::Candidate>) -> String {
    use libgen_core::naming::{filename, NameContext};
    match candidate {
        Some(c) => {
            let settings = ListSettings {
                naming_template: "{authors} - {title}.{ext}".to_string(),
                ..Default::default()
            };
            let ctx = NameContext {
                seq: 1,
                candidate: c,
                title: &c.title,
                authors: &c.authors,
            };
            filename(&settings, &ctx)
        }
        None => format!("{md5}.bin"),
    }
}

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

    // --- saved filename ---

    #[test]
    fn cli_filename_builds_proper_name_with_format_ext() {
        use libgen_core::model::{Candidate, Format};
        let md5 = "3a70291df204c78842ffe549166ffcb9";
        let c = Candidate {
            md5: md5.to_string(),
            title: "Steve Jobs".to_string(),
            authors: vec!["Walter Isaacson".to_string()],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: Some(Format::Epub),
            size_bytes: None,
            source_host: None,
            cover_url: None,
            score: 1.0,
            job: None,
        };
        let name = cli_filename(md5, Some(&c));
        // Author - Title - <md5:6>.ext, with the REAL format extension (not .bin).
        assert_eq!(name, "Walter Isaacson - Steve Jobs - 3a7029.epub");
    }

    #[test]
    fn cli_filename_falls_back_to_bin_without_metadata() {
        let md5 = "3a70291df204c78842ffe549166ffcb9";
        assert_eq!(cli_filename(md5, None), format!("{md5}.bin"));
    }

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
        // Leading arg is the animated spinner frame (a braille glyph).
        let line =
            format_progress_line("\u{280B}", 500, Some(1000), Some(512 * 1024), Some(10), 80);
        assert!(line.contains("50%"), "pct: {line:?}");
        assert!(line.contains("512 KB/s"), "speed: {line:?}");
        assert!(line.contains("eta 10s"), "eta: {line:?}");
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
