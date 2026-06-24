//! One-time migration: move orphaned `.part` files (left at OLD naming-scheme
//! paths) to each book's CURRENT correct destination, so in-progress downloads
//! resume at the right place. The DB's persisted `output_path` is already the
//! current-scheme path; we just relocate the biggest matching `.part` next to it.
//!
//! Usage: cargo run -p libgen-app --example migrate_parts -- <db> <out_dir> [--apply]
use libgen_core::download::part_path;
use libgen_core::model::JobState;
use libgen_core::orchestrator::{collect_files_recursive, strip_seq_prefix};
use libgen_core::store::Store;
use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    let db = std::env::args()
        .nth(1)
        .expect("usage: migrate_parts <db> <out_dir> [--apply]");
    let out_dir = PathBuf::from(std::env::args().nth(2).expect("out_dir required"));
    let apply = std::env::args().any(|a| a == "--apply");

    // 1. Current correct dest (output_path) for every still-unfinished variation.
    let store = Store::open(&db)?;
    let mut targets: Vec<(PathBuf, String)> = Vec::new(); // (dest, stable) — UNFINISHED
                                                          // Stable names that are already COMPLETED (a Done variation whose file exists):
                                                          // an abandoned .part for such a book is safe to delete.
    let mut done_ok: std::collections::HashSet<String> = Default::default();
    for sl in store.all_lists()? {
        let mut stack = vec![&sl.list.groups];
        while let Some(groups) = stack.pop() {
            for g in groups {
                for b in &g.books {
                    for c in &b.candidates {
                        if let Some(j) = &c.job {
                            if let Some(op) = j.output_path.as_deref().filter(|s| !s.is_empty()) {
                                let dest = PathBuf::from(op);
                                let stable = dest
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .map(|n| strip_seq_prefix(n).to_string())
                                    .unwrap_or_default();
                                if matches!(j.state, JobState::Done) {
                                    if dest.exists() {
                                        done_ok.insert(stable);
                                    }
                                } else {
                                    targets.push((dest, stable));
                                }
                            }
                        }
                    }
                }
                stack.push(&g.subgroups);
            }
        }
    }

    // 2. Index every .part on disk (skip hedge temps) by sequence-stripped name.
    let mut parts: std::collections::HashMap<String, Vec<(u64, PathBuf)>> = Default::default();
    collect_files_recursive(&out_dir, &mut |f: &Path| {
        if let Some(name) = f.file_name().and_then(|n| n.to_str()) {
            if name.ends_with(".part") && !name.contains(".hedge.") {
                let base = &name[..name.len() - ".part".len()];
                let len = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
                parts
                    .entry(strip_seq_prefix(base).to_string())
                    .or_default()
                    .push((len, f.to_path_buf()));
            }
        }
    });

    // 3. For each dest, adopt the BIGGEST matching orphan if larger than what's there.
    let mut moved = 0;
    println!(
        "{} — scanning {} unfinished targets",
        if apply { "APPLYING" } else { "DRY RUN" },
        targets.len()
    );
    for (dest, stable) in &targets {
        let target = part_path(dest);
        let cur = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        let best = parts
            .get(stable)
            .into_iter()
            .flatten()
            .filter(|(_, p)| p.as_path() != target.as_path())
            .max_by_key(|(sz, _)| *sz);
        if let Some((sz, src)) = best {
            if *sz > cur {
                println!(
                    "MOVE {:>11} bytes (dest had {})\n    from {}\n    to   {}",
                    sz,
                    cur,
                    src.display(),
                    target.display()
                );
                if apply {
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::rename(src, &target)?;
                    moved += 1;
                }
            }
        }
    }

    // 4. Cleanup: every .part NOT at a current dest is a superseded old-scheme
    //    leftover (the biggest was just moved into place) — delete it. Then prune
    //    the now-empty old folders so no stray <out>/Lift-Off remains.
    let known: std::collections::HashSet<PathBuf> =
        targets.iter().map(|(d, _)| part_path(d)).collect();
    let mut deleted = 0;
    let mut leftovers: Vec<PathBuf> = Vec::new();
    collect_files_recursive(&out_dir, &mut |f: &Path| {
        if let Some(name) = f.file_name().and_then(|n| n.to_str()) {
            if name.ends_with(".part") && !name.contains(".hedge.") && !known.contains(f) {
                leftovers.push(f.to_path_buf());
            }
        }
    });
    for f in &leftovers {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let stable =
            strip_seq_prefix(&name[..name.len().saturating_sub(".part".len())]).to_string();
        let sz = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        // SAFE delete: only remove an orphan that is genuinely superseded — either a
        // BIGGER partial for the same book exists, or the book already COMPLETED
        // (Done file on disk). Never delete the only/biggest copy of progress.
        let bigger_exists = parts
            .get(&stable)
            .into_iter()
            .flatten()
            .any(|(s, p)| p.as_path() != f && *s > sz);
        let superseded = bigger_exists || done_ok.contains(&stable);
        if superseded {
            println!("DELETE {:>11} bytes  {}", sz, f.display());
            if apply {
                std::fs::remove_file(f)?;
                deleted += 1;
            }
        } else {
            println!(
                "KEEP   {:>11} bytes (not superseded — left in place)  {}",
                sz,
                f.display()
            );
        }
    }
    if apply {
        prune_empty_dirs(&out_dir);
    }
    println!(
        "\n{}",
        if apply {
            format!("DONE — moved {moved}, deleted {deleted} leftover .part(s), pruned empty dirs")
        } else {
            "(dry run; pass --apply to move + clean up)".into()
        }
    );
    Ok(())
}

/// Remove empty directories under `root` (bottom-up), leaving `root` itself.
fn prune_empty_dirs(dir: &Path) -> bool {
    let mut empty = true;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if prune_empty_dirs(&p) {
                    let _ = std::fs::remove_dir(&p);
                } else {
                    empty = false;
                }
            } else {
                empty = false;
            }
        }
    } else {
        empty = false;
    }
    empty
}
