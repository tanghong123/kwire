//! Integration tests for the mutable **Manual** list commands
//! (`add_manual_book` / `remove_book`), driven headlessly through the crate's
//! `#[doc(hidden)] pub` test surface — exactly as the Tauri commands do.
//!
//! Verifies the contract the UI relies on:
//!   * `add_manual_book` twice APPENDS two books to ONE Manual list (find-or-
//!     create the singleton; not rejected as a duplicate title, not replaced),
//!   * the created list carries `is_manual = true` (the per-list viewmodel flag),
//!   * `remove_book` removes one book from the Manual list,
//!   * `remove_book` ERRORS on a non-manual (imported) list.

use std::path::PathBuf;

use libgen_app_lib::commands::testsupport;
use libgen_app_lib::state::AppState;

use libgen_core::model::{
    BookInput, BookRequest, DownloadList, Group, ListSettings, RequestStatus,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}
fn fixtures_dir() -> PathBuf {
    repo_root().join("fixtures").join("search")
}
fn mirrors_path() -> PathBuf {
    repo_root().join("mirrors.toml")
}
fn make_state(dir: &std::path::Path) -> AppState {
    testsupport::app_state(dir.join("library.sqlite3"), fixtures_dir(), mirrors_path())
}

/// The Manual list's UI id + its `is_manual` flag, from the library view.
fn manual_view(lib: &libgen_app_lib::viewmodel::ViewLibrary) -> Option<(&str, bool, usize)> {
    lib.lists.iter().find(|l| l.title == "Manual").map(|l| {
        (
            l.id.as_str(),
            l.is_manual,
            l.groups.iter().map(|g| g.books.len()).sum(),
        )
    })
}

#[tokio::test]
async fn add_manual_book_twice_appends_two_books_to_one_list() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());

    let lib = testsupport::add_manual_book(&state, "Treasure Island", None)
        .await
        .expect("first add ok");
    let (_, is_manual, n) = manual_view(&lib).expect("Manual list created");
    assert!(is_manual, "created list is flagged mutable (is_manual)");
    assert_eq!(n, 1, "one book after first add");

    // Second add must NOT be rejected (the old `load_list` duplicate-title path) and
    // must NOT replace the list — it appends to the SAME Manual list.
    let lib =
        testsupport::add_manual_book(&state, "The Adventures of Tom Sawyer", Some("Mark Twain"))
            .await
            .expect("second add ok");

    // Still exactly ONE list titled "Manual".
    let manuals = lib.lists.iter().filter(|l| l.title == "Manual").count();
    assert_eq!(manuals, 1, "still one Manual list");
    let (_, _, n) = manual_view(&lib).unwrap();
    assert_eq!(n, 2, "two books after second add");

    let snap = testsupport::snapshot(&state, manual_view(&lib).unwrap().0)
        .await
        .unwrap();
    let titles: Vec<&str> = snap.groups[0]
        .books
        .iter()
        .map(|b| b.input.title.as_str())
        .collect();
    assert_eq!(
        titles,
        vec!["Treasure Island", "The Adventures of Tom Sawyer"]
    );
}

#[tokio::test]
async fn add_manual_book_rejects_empty_title() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    assert!(testsupport::add_manual_book(&state, "   ", None)
        .await
        .is_err());
}

#[tokio::test]
async fn remove_book_removes_one_from_manual_list() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());

    testsupport::add_manual_book(&state, "Treasure Island", None)
        .await
        .unwrap();
    let lib = testsupport::add_manual_book(&state, "Anne of Green Gables", None)
        .await
        .unwrap();
    let id = manual_view(&lib).unwrap().0.to_string();

    // Remove the FIRST book (flat id "bk0").
    let lib = testsupport::remove_book(&state, &id, "bk0")
        .await
        .expect("remove ok");
    let (_, _, n) = manual_view(&lib).unwrap();
    assert_eq!(n, 1, "one book left after removing one");

    let snap = testsupport::snapshot(&state, &id).await.unwrap();
    let titles: Vec<&str> = snap.groups[0]
        .books
        .iter()
        .map(|b| b.input.title.as_str())
        .collect();
    assert_eq!(titles, vec!["Anne of Green Gables"]);
}

#[tokio::test]
async fn remove_book_errors_on_non_manual_list() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());

    // An ordinary imported (immutable) list.
    let mut g = Group::new("Batch");
    g.books.push(BookRequest::new(BookInput {
        title: "Treasure Island".into(),
        ..Default::default()
    }));
    let imported = DownloadList {
        title: "Imported".into(),
        settings: ListSettings::default(), // is_manual == false
        groups: vec![g],
    };
    let id = testsupport::load(&state, &imported).await.unwrap();

    let res = testsupport::remove_book(&state, &id, "bk0").await;
    assert!(res.is_err(), "removing from an immutable list must error");

    // The book is still there (nothing removed).
    let snap = testsupport::snapshot(&state, &id).await.unwrap();
    assert_eq!(snap.groups[0].books.len(), 1);
    assert_eq!(snap.groups[0].books[0].status, RequestStatus::Queued);
}
