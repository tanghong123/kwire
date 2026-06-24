//! `libgen` — CLI harnesses over libgen-core. Each subcommand is a permanent
//! front door used to develop and headlessly test one engine module.

mod cmd_download;
mod cmd_eval_series;
mod cmd_parse;
mod cmd_query;
mod cmd_run;
mod cmd_series;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "libgen", about = "Kwire engine harnesses")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse a Markdown/JSON reading list into the normalized model (no network).
    ParseList(cmd_parse::Args),
    /// Search mirrors for a book request and print ranked candidates.
    QueryBooks(cmd_query::Args),
    /// Look up a book's series on Open Library and print its ordered members.
    SeriesInfo(cmd_series::Args),
    /// Evaluate ALL THREE series sources (OL, libgen, Goodreads) over a book list.
    EvalSeries(cmd_eval_series::Args),
    /// Resolve + download a candidate (resumable, md5-verified).
    DownloadBooks(cmd_download::Args),
    /// Run the full pipeline for a list: parse → persist → query → match → plan.
    RunList(cmd_run::Args),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::ParseList(a) => cmd_parse::run(a).await,
        Command::QueryBooks(a) => cmd_query::run(a).await,
        Command::SeriesInfo(a) => cmd_series::run(a).await,
        Command::EvalSeries(a) => cmd_eval_series::run(a).await,
        Command::DownloadBooks(a) => cmd_download::run(a).await,
        Command::RunList(a) => cmd_run::run(a).await,
    }
}
