//! `libgen series-info "<title>" "<author>"` — look a book's series up on Open
//! Library and print the detected series + its ordered members.
//!
//! Live by default, or offline via `--replay <fixtures dir>` (and `--record`
//! to save live responses). Mirrors `query-books`' shape.

use anyhow::Result;
use clap::Args as ClapArgs;
use libgen_core::series::{GoodreadsClient, LibgenSeriesClient, Series, SeriesClient};
use serde::Serialize;
use std::path::PathBuf;

#[derive(ClapArgs)]
pub struct Args {
    /// Book title.
    pub title: String,
    /// Book author (used to disambiguate the work on Open Library).
    pub author: String,

    /// Replay recorded Open Library responses from this fixtures dir (offline).
    #[arg(long)]
    pub replay: Option<PathBuf>,

    /// Record live Open Library responses into this fixtures dir.
    #[arg(long)]
    pub record: Option<PathBuf>,
}

#[derive(Serialize)]
struct MemberOut {
    position: Option<u32>,
    title: String,
}

#[derive(Serialize)]
struct Output {
    title: String,
    author: String,
    /// `true` when the book is part of a known series.
    in_series: bool,
    /// Which source resolved it (`open_library` / `libgen` / `goodreads`).
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    series_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    series_name: Option<String>,
    members: Vec<MemberOut>,
}

pub async fn run(args: Args) -> Result<()> {
    // Consult the THREE equal sources in turn — OpenLibrary, libgen series.php,
    // Goodreads — and take the FIRST that yields a usable series (≥2 members).
    // Each source has its own transport; all share the same replay/record dir.
    let (resolved, source) = resolve_any(&args).await?;

    let out = match resolved {
        Some(s) => Output {
            title: args.title.clone(),
            author: args.author.clone(),
            in_series: true,
            source,
            series_key: Some(s.key),
            series_name: Some(s.name),
            members: s
                .members
                .into_iter()
                .map(|m| MemberOut {
                    position: m.position,
                    title: m.title,
                })
                .collect(),
        },
        None => Output {
            title: args.title.clone(),
            author: args.author.clone(),
            in_series: false,
            source: None,
            series_key: None,
            series_name: None,
            members: Vec::new(),
        },
    };

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Try the three sources in order; first with ≥2 members wins. Returns the
/// series and the name of the source that produced it.
async fn resolve_any(args: &Args) -> Result<(Option<Series>, Option<String>)> {
    // OpenLibrary.
    let ol = match (&args.replay, &args.record) {
        (Some(dir), _) => SeriesClient::replay(dir.clone()),
        (None, Some(dir)) => SeriesClient::recording(dir.clone()),
        (None, None) => SeriesClient::live(),
    };
    // A source erroring (e.g. a network blip, or a missing replay fixture) must
    // NOT block the others — treat it like "not found here" and move on.
    if let Ok(Some(s)) = ol.lookup(&args.title, &args.author).await {
        if s.members.len() >= 2 {
            return Ok((Some(s), Some("open_library".into())));
        }
    }

    // libgen series.php.
    let libgen = match (&args.replay, &args.record) {
        (Some(dir), _) => LibgenSeriesClient::replay(dir.clone()),
        (None, Some(dir)) => LibgenSeriesClient::recording(dir.clone()),
        (None, None) => LibgenSeriesClient::live(),
    };
    if let Ok(Some(s)) = libgen.lookup(&args.title, &args.author).await {
        if s.members.len() >= 2 {
            return Ok((Some(s), Some("libgen".into())));
        }
    }

    // Goodreads.
    let goodreads = match (&args.replay, &args.record) {
        (Some(dir), _) => GoodreadsClient::replay(dir.clone()),
        (None, Some(dir)) => GoodreadsClient::recording(dir.clone()),
        (None, None) => GoodreadsClient::live(),
    };
    if let Ok(Some(s)) = goodreads.lookup(&args.title, &args.author).await {
        if s.members.len() >= 2 {
            return Ok((Some(s), Some("goodreads".into())));
        }
    }

    Ok((None, None))
}
