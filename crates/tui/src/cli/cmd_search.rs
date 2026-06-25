//! `kwire search <query…>` — one-shot search, prints numbered candidates.

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

    candidates.truncate(args.limit);

    if candidates.is_empty() {
        println!("No candidates found for {:?}.", query);
        return Ok(());
    }

    for (i, c) in candidates.iter().enumerate() {
        println!("{}", format_candidate(i + 1, c));
    }

    Ok(())
}

/// Format one candidate as the two-line display described in the spec.
pub fn format_candidate(index: usize, c: &Candidate) -> String {
    // Line 1: `N. <title> — <authors>`
    let authors = if c.authors.is_empty() {
        String::new()
    } else {
        c.authors.join(", ")
    };
    let line1 = if authors.is_empty() {
        format!("{}. {}", index, c.title)
    } else {
        format!("{}. {} — {}", index, c.title, authors)
    };

    // Line 2: `  <md5>  <year> · <pages>p · <format> · <size>`
    let mut meta: Vec<String> = Vec::new();
    if let Some(y) = c.year {
        meta.push(y.to_string());
    }
    if let Some(p) = c.pages {
        meta.push(format!("{}p", p));
    }
    if let Some(ref ext) = c.extension {
        meta.push(ext.ext().to_ascii_uppercase());
    }
    if let Some(sz) = c.size_bytes {
        meta.push(human_size(sz));
    }

    let line2 = if meta.is_empty() {
        format!("  {}", c.md5)
    } else {
        format!("  {}  {}", c.md5, meta.join(" · "))
    };

    format!("{}\n{}", line1, line2)
}

/// Format bytes as a human-readable string (e.g. "4.8 MB").
pub fn human_size(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
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
            source_host: None,
            cover_url: None,
            score: 0.0,
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
        );
        let s = format_candidate(1, &c);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "1. Treasure Island — Robert Louis Stevenson");
        assert!(lines[1].contains("1df204c78842ffe549166ffcb984babc"));
        assert!(lines[1].contains("1883"));
        assert!(lines[1].contains("312p"));
        assert!(lines[1].contains("EPUB"));
        assert!(lines[1].contains("2.0 MB"));
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
        );
        let s = format_candidate(2, &c);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "2. Unknown Book");
        assert_eq!(lines[1], "  aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
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
        );
        let s = format_candidate(3, &c);
        let lines: Vec<&str> = s.lines().collect();
        assert!(lines[1].contains("2021"));
        assert!(!lines[1].contains('p'), "no pages field should appear");
        assert!(lines[1].contains("PDF"));
        assert!(lines[1].contains("500 KB"));
    }

    #[test]
    fn human_size_formatting() {
        assert_eq!(super::human_size(2 * 1024 * 1024), "2.0 MB");
        assert_eq!(super::human_size(512 * 1024), "512 KB");
        assert_eq!(super::human_size(999), "999 B");
        assert_eq!(super::human_size(1536 * 1024), "1.5 MB");
    }
}
