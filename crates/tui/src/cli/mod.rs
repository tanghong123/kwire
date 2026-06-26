//! One-shot CLI subcommands for `kwire search` and `kwire get`.
//!
//! These run entirely on the normal terminal (stdout/stderr); they never touch
//! raw mode, alternate screen, or ratatui.  The TUI is only started when the
//! binary is invoked with no subcommand.

pub mod cmd_get;
pub mod cmd_search;
pub mod emitter;

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Search mirrors for books and print ranked candidates.
    Search(cmd_search::Args),
    /// Download a book by MD5 or by title search.
    Get(cmd_get::Args),
}

pub async fn run(cmd: Commands) -> Result<()> {
    match cmd {
        Commands::Search(args) => cmd_search::run(args).await,
        Commands::Get(args) => cmd_get::run(args).await,
    }
}
