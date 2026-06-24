//! `libgen query-books` — search mirrors for a book and print ranked candidates.
//!
//! Reads a JSON `BookInput` from a file (or "-" for stdin), queries the
//! configured mirrors (live, or offline via `--replay`), scores candidates with
//! `matching::evaluate`, and prints the ranked result as pretty JSON.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use libgen_core::matching;
use libgen_core::model::{BookInput, ListSettings};
use libgen_core::search::{MirrorConfig, SearchClient};
use serde::Serialize;
use std::io::Read;
use std::path::PathBuf;

#[derive(ClapArgs)]
pub struct Args {
    /// Path to a JSON BookInput (or "-" for stdin).
    pub input: PathBuf,

    /// Replay recorded HTTP responses from this fixtures dir (offline).
    #[arg(long)]
    pub replay: Option<PathBuf>,

    /// Record live HTTP responses into this fixtures dir while searching.
    #[arg(long)]
    pub record: Option<PathBuf>,

    /// Path to mirrors.toml (default: ./mirrors.toml).
    #[arg(long, default_value = "mirrors.toml")]
    pub mirrors: PathBuf,

    /// Max candidates per mirror.
    #[arg(long, default_value_t = 25)]
    pub limit: usize,
}

#[derive(Serialize)]
struct Output<'a> {
    status: String,
    input: &'a BookInput,
    candidates: Vec<libgen_core::model::Candidate>,
}

pub async fn run(args: Args) -> Result<()> {
    let raw = read_input(&args.input)?;
    let input: BookInput =
        serde_json::from_str(&raw).context("decoding JSON BookInput from input")?;

    let config = MirrorConfig::load(&args.mirrors)
        .with_context(|| format!("loading mirrors from {}", args.mirrors.display()))?;

    let client = match (&args.replay, &args.record) {
        (Some(dir), _) => SearchClient::replay(config, dir.clone()),
        (None, Some(dir)) => SearchClient::recording(config, dir.clone()),
        (None, None) => {
            SearchClient::new(config, Box::new(libgen_core::search::LiveTransport::new()))
        }
    }
    .with_limit(args.limit);

    let candidates = client.search(&input).await.context("searching mirrors")?;

    // Use list defaults for thresholds/format prefs; request prefs override.
    let settings = ListSettings::default();
    let outcome = matching::evaluate(&input, candidates, &settings);

    let status = match &outcome.status {
        libgen_core::model::RequestStatus::Matched => "matched",
        libgen_core::model::RequestStatus::NeedsSelection => "needs_selection",
        libgen_core::model::RequestStatus::NotFound => "not_found",
        other => {
            // evaluate only ever returns the three above.
            tracing::warn!(?other, "unexpected match status");
            "unknown"
        }
    };

    let out = Output {
        status: status.to_string(),
        input: &input,
        candidates: outcome.ranked,
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn read_input(path: &PathBuf) -> Result<String> {
    if path.as_os_str() == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading BookInput from stdin")?;
        Ok(s)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    }
}
