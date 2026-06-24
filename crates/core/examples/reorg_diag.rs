//! Diagnose a "Reorganize now stays clickable" report: prints, per list, the
//! (file-on-disk → current-plan) path pairs that the reorganizer would move — so we
//! can see WHY (e.g. a seq number that drifted between the file and the plan).
//!
//! Usage: cargo run -p libgen-core --example reorg_diag -- <db-path>
//!        LIBGEN_OUT=<out_dir> to override the download folder (default ~/Downloads)

use libgen_core::orchestrator::Orchestrator;
use libgen_core::search::{MirrorConfig, SearchClient};
use libgen_core::store::Store;

fn tail(p: &str, n: usize) -> String {
    let parts: Vec<&str> = p.split('/').collect();
    let start = parts.len().saturating_sub(n);
    parts[start..].join("/")
}

fn main() -> anyhow::Result<()> {
    let db = std::env::args()
        .nth(1)
        .expect("usage: reorg_diag <db-path>");
    let out_dir = std::env::var("LIBGEN_OUT")
        .unwrap_or_else(|_| format!("{}/Downloads", std::env::var("HOME").unwrap_or_default()));
    println!("db={db}\nout_dir={out_dir}");

    let lists: Vec<(i64, String)> = {
        let store = Store::open(&db)?;
        store
            .all_lists()?
            .iter()
            .map(|sl| (sl.id, sl.list.title.clone()))
            .collect()
    };

    for (id, title) in lists {
        let store = Store::open(&db)?;
        let search = SearchClient::replay(MirrorConfig::from_toml("")?, std::env::temp_dir());
        let mut orch = Orchestrator::attach(store, id, search, &out_dir);
        let diffs = orch.reorganize_plan_diff()?;
        println!(
            "\n== list {id}: {title} —  {} file(s) would move ==",
            diffs.len()
        );
        for (src, dest) in diffs.iter().take(8) {
            println!("  FROM .../{}", tail(src, 2));
            println!("    TO .../{}", tail(dest, 2));
        }
        if diffs.len() > 8 {
            println!("  … and {} more", diffs.len() - 8);
        }
    }
    Ok(())
}
