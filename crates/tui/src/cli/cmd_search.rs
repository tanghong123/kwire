//! `kwire search <query…>` — one-shot search, streams ranked candidates live.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use libgen_core::model::{BookInput, Candidate, Format};
use libgen_engine::{build_search, Config};

#[derive(ClapArgs)]
pub struct SearchArgs {
    /// Search query (title and/or author keywords).
    #[arg(required = true, num_args = 1..)]
    pub query: Vec<String>,

    /// Maximum number of candidates to display.
    #[arg(long, default_value_t = 10)]
    pub limit: usize,

    /// Filter by file extension (e.g. epub, pdf).
    #[arg(long)]
    pub format: Option<String>,

    /// Filter by language (e.g. English).
    #[arg(long)]
    pub language: Option<String>,
}

// Public re-export so mod.rs can name it simply (no name clash with clap::Args).
pub use SearchArgs as Args;

pub async fn run(args: SearchArgs) -> Result<()> {
    let query = args.query.join(" ");
    let cfg = Config::from_env();
    let client = build_search(&cfg).map_err(|e| anyhow::anyhow!(e))?;

    let input = BookInput {
        title: query.clone(),
        ..Default::default()
    };

    // Activity line: let the user know we're hitting the network.
    eprintln!("searching  {query}");

    let mut candidates = client.search(&input).await.context("searching mirrors")?;

    // Optional format filter.
    if let Some(ref fmt_str) = args.format {
        let want = Format::parse(fmt_str);
        candidates.retain(|c| c.extension.as_ref() == Some(&want));
    }

    // Optional language filter.
    if let Some(ref lang) = args.language {
        let lang_lc = lang.to_ascii_lowercase();
        candidates.retain(|c| {
            c.language
                .as_deref()
                .map(|l| l.to_ascii_lowercase().contains(&lang_lc))
                .unwrap_or(false)
        });
    }

    let total = candidates.len();
    candidates.truncate(args.limit);
    let showing = candidates.len();

    if candidates.is_empty() {
        eprintln!("no candidates found for {query:?}");
        return Ok(());
    }

    // Activity line: how many came back vs. how many we're showing.
    eprintln!("found {total}  showing {showing}");

    // Stream each candidate to stdout live (one per iteration).
    for (i, c) in candidates.iter().enumerate() {
        println!("{}", format_candidate(i + 1, c));
    }

    Ok(())
}

/// Format one candidate as a two-line entry.
///
/// Line 1: `N. FMT  Title — Authors`
/// Line 2: `   size · year · Npg · score · source`
///
/// Fields that are unavailable are omitted from line 2.  `FMT` (the file
/// format) is omitted when unknown; authors are omitted when empty.
pub fn format_candidate(index: usize, c: &Candidate) -> String {
    // ── Line 1 ─────────────────────────────────────────────────────────────
    let fmt_prefix = c
        .extension
        .as_ref()
        .map(|e| format!("{}  ", e.ext().to_ascii_uppercase()))
        .unwrap_or_default();

    let authors = c.authors.join(", ");
    let line1 = if authors.is_empty() {
        format!("{index}. {fmt_prefix}{}", c.title)
    } else {
        format!("{index}. {fmt_prefix}{} — {authors}", c.title)
    };

    // ── Line 2 ─────────────────────────────────────────────────────────────
    let mut meta: Vec<String> = Vec::new();

    if let Some(sz) = c.size_bytes {
        meta.push(human_size(sz));
    }
    if let Some(y) = c.year {
        meta.push(y.to_string());
    }
    if let Some(p) = c.pages {
        meta.push(format!("{p}pg"));
    }

    // Score: always include — useful for judging match quality.
    meta.push(format!("{:.2}", c.score));

    if let Some(ref src) = c.source_host {
        meta.push(src.clone());
    }
    // md5 — the identifier to pass to `kwire get <md5>`.
    if !c.md5.is_empty() {
        meta.push(format!("md5 {}", c.md5));
    }

    let line2 = if meta.is_empty() {
        String::new()
    } else {
        format!("   {}", meta.join(" · "))
    };

    format!("{line1}\n{line2}")
}

/// Format bytes as a human-readable string (e.g. `"4.8 MB"`).
pub fn human_size(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use libgen_core::model::{Candidate, Format};

    fn make_candidate(
        title: &str,
        authors: &[&str],
        md5: &str,
        year: Option<u16>,
        pages: Option<u32>,
        extension: Option<Format>,
        size_bytes: Option<u64>,
        score: f32,
        source_host: Option<&str>,
    ) -> Candidate {
        Candidate {
            md5: md5.to_string(),
            title: title.to_string(),
            authors: authors.iter().map(|s| s.to_string()).collect(),
            year,
            publisher: None,
            language: None,
            pages,
            extension,
            size_bytes,
            source_host: source_host.map(str::to_string),
            cover_url: None,
            score,
            job: None,
        }
    }

    #[test]
    fn format_candidate_full_fields() {
        let c = make_candidate(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            "1df204c78842ffe549166ffcb984babc",
            Some(1883),
            Some(312),
            Some(Format::Epub),
            Some(2 * 1024 * 1024),
            0.95_f32,
            Some("libgen.li"),
        );
        let s = format_candidate(1, &c);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        // Line 1: format + title + author
        assert!(lines[0].starts_with("1. EPUB"), "line1: {}", lines[0]);
        assert!(lines[0].contains("Treasure Island"), "title: {}", lines[0]);
        assert!(
            lines[0].contains("Robert Louis Stevenson"),
            "author: {}",
            lines[0]
        );
        // Line 2: metadata (no md5; pages use "pg" suffix)
        assert!(lines[1].contains("2.0 MB"), "size: {}", lines[1]);
        assert!(lines[1].contains("1883"), "year: {}", lines[1]);
        assert!(lines[1].contains("312pg"), "pages: {}", lines[1]);
        assert!(lines[1].contains("0.95"), "score: {}", lines[1]);
        assert!(lines[1].contains("libgen.li"), "source: {}", lines[1]);
        // md5 is shown for `kwire get <md5>`
        assert!(
            s.contains("md5 1df204c78842ffe549166ffcb984babc"),
            "md5 must be in output: {s}"
        );
    }

    #[test]
    fn format_candidate_minimal_fields() {
        // Only title + md5, no optional fields.
        let c = make_candidate(
            "Unknown Book",
            &[],
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            None,
            None,
            None,
            None,
            0.0_f32,
            None,
        );
        let s = format_candidate(2, &c);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "2. Unknown Book");
        // Line 2 has only the score (0.00) and no extra fields.
        assert!(lines[1].contains("0.00"), "score: {}", lines[1]);
    }

    #[test]
    fn format_candidate_no_pages() {
        // Has year + format + size but no pages.
        let c = make_candidate(
            "A Book",
            &["Author One"],
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            Some(2021),
            None,
            Some(Format::Pdf),
            Some(500 * 1024),
            0.75_f32,
            None,
        );
        let s = format_candidate(3, &c);
        let lines: Vec<&str> = s.lines().collect();
        // Line 1 has "PDF" from the format prefix.
        assert!(lines[0].contains("PDF"), "line1: {}", lines[0]);
        assert!(lines[1].contains("2021"), "year: {}", lines[1]);
        assert!(
            !lines[1].contains("pg"),
            "no pages field should appear: {}",
            lines[1]
        );
        assert!(lines[1].contains("500 KB"), "size: {}", lines[1]);
        assert!(lines[1].contains("0.75"), "score: {}", lines[1]);
    }

    #[test]
    fn format_candidate_no_format_extension() {
        // When extension is None, no FMT prefix on line 1.
        let c = make_candidate(
            "Mystery Book",
            &["Someone"],
            "cccccccccccccccccccccccccccccccc",
            None,
            None,
            None,
            None,
            0.5_f32,
            None,
        );
        let s = format_candidate(4, &c);
        let line1 = s.lines().next().unwrap();
        assert_eq!(line1, "4. Mystery Book — Someone");
    }

    #[test]
    fn human_size_formatting() {
        assert_eq!(human_size(2 * 1024 * 1024), "2.0 MB");
        assert_eq!(human_size(512 * 1024), "512 KB");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1536 * 1024), "1.5 MB");
    }
}
