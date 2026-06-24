//! Integration test pinning the parser against the real Jeremy fixtures.
//!
//! Two invariants:
//!   1. Parsing the `.md` and the `.json` fixtures yields the *same*
//!      `DownloadList` (the formats are equivalent by construction).
//!   2. That `DownloadList` matches a checked-in golden snapshot. Run with
//!      `UPDATE_GOLDEN=1` to (re)write the snapshot after intentional changes.

use libgen_core::parse;
use libgen_core::DownloadList;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/core; fixtures live at the workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
}

fn read(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Each reading-list fixture: the file stem (md + json share it) and its golden
/// snapshot name.
const FIXTURES: &[(&str, &str)] = &[
    ("jeremy_public_domain_list", "jeremy.normalized.json"),
    ("avery_public_domain_list", "avery.normalized.json"),
];

#[test]
fn md_and_json_fixtures_are_equivalent() {
    let dir = fixtures_dir();
    for (stem, _) in FIXTURES {
        let md = parse::parse_markdown(&read(&dir.join(format!("{stem}.md"))))
            .unwrap_or_else(|e| panic!("parse {stem}.md: {e}"));
        let json = parse::parse_json(&read(&dir.join(format!("{stem}.json"))))
            .unwrap_or_else(|e| panic!("parse {stem}.json: {e}"));
        assert_eq!(
            md, json,
            "{stem}: markdown and json must parse to the same DownloadList"
        );
    }
}

#[test]
fn fixtures_match_golden_snapshot() {
    let dir = fixtures_dir();
    for (stem, golden) in FIXTURES {
        let list = parse::parse_json(&read(&dir.join(format!("{stem}.json"))))
            .unwrap_or_else(|e| panic!("parse {stem}.json: {e}"));

        let golden_path = dir.join("expected").join(golden);
        let actual = serde_json::to_string_pretty(&list).expect("serialize DownloadList");

        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
            // Trailing newline keeps the file POSIX-friendly and diff-stable.
            std::fs::write(&golden_path, format!("{actual}\n")).expect("write golden");
            continue;
        }

        let expected = read(&golden_path);
        let expected_list: DownloadList =
            serde_json::from_str(&expected).expect("deserialize golden snapshot");
        assert_eq!(
            list, expected_list,
            "{stem}: parsed list diverged from golden; rerun with UPDATE_GOLDEN=1 if intended"
        );
    }
}
