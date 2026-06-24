//! Does a settings change persist across the re-attach that the GUI's set_config
//! does after every Save? (Suspected cause of "saving settings has no effect".)
use libgen_core::model::{BookInput, BookRequest, DownloadList, Format, Group, ListSettings};
use libgen_core::orchestrator::Orchestrator;
use libgen_core::search::{MirrorConfig, SearchClient};
use libgen_core::store::Store;

fn list() -> DownloadList {
    let mut g = Group::new("B");
    g.books.push(BookRequest::new(BookInput {
        title: "Treasure Island".into(),
        ..Default::default()
    }));
    DownloadList {
        title: "L".into(),
        settings: ListSettings::default(),
        groups: vec![g],
    }
}
fn search() -> SearchClient {
    SearchClient::replay(MirrorConfig::default(), std::path::PathBuf::from("/none"))
}

#[test]
fn settings_survive_reattach() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    let id;
    {
        let mut orch =
            Orchestrator::new(Store::open(&db).unwrap(), &list(), search(), "/out").unwrap();
        id = orch.list_id();
        let mut s = orch.snapshot().unwrap().settings;
        s.format_pref = vec![Format::Pdf, Format::Epub];
        s.keep_top = 9;
        s.auto_threshold = 0.42;
        orch.update_settings(s).unwrap();
        // same orchestrator sees it
        assert_eq!(
            orch.snapshot().unwrap().settings.keep_top,
            9,
            "same-conn read"
        );
    }
    // NEW store connection + attach (what set_config does)
    let orch2 = Orchestrator::attach(Store::open(&db).unwrap(), id, search(), "/out");
    let s = orch2.snapshot().unwrap().settings;
    assert_eq!(s.keep_top, 9, "keep_top lost across reattach");
    assert_eq!(
        s.format_pref,
        vec![Format::Pdf, Format::Epub],
        "format_pref lost"
    );
    assert!((s.auto_threshold - 0.42).abs() < 1e-6, "threshold lost");
}
