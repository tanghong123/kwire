//! `libgen parse-list` — owned by the parser work-stream.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::path::PathBuf;

#[derive(ClapArgs)]
pub struct Args {
    /// Path to a .md or .json reading list.
    pub file: PathBuf,
    /// Force JSON parsing regardless of extension.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let content = std::fs::read_to_string(&args.file)
        .with_context(|| format!("reading {}", args.file.display()))?;
    let is_json = args.json
        || args
            .file
            .extension()
            .map(|e| e.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
    let list = libgen_core::parse::parse_auto(&content, is_json)?;
    println!("{}", serde_json::to_string_pretty(&list)?);
    Ok(())
}
