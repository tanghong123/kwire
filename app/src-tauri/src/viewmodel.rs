//! View model: project the engine's nested [`DownloadList`] into the shape the
//! front end renders. The reviewed UX (see `app/ui/index.html`) is built around
//! **per-variation download state** — each book exposes its kept candidate
//! variations (distinct md5s), and EACH variation carries its own download
//! `state`/`progress`/`output_path`, so one book can have its epub `done` while
//! its pdf is still `downloading`. The book's coarse status is the `acquisition()`
//! roll-up over those variations.
//!
//! Keeping this mapping here means the frontend consumes one stable JSON shape
//! whether it is fed by the real engine (under Tauri) or by the bundled demo
//! data (over file://) — the two are kept deliberately congruent.

use serde::Serialize;

use libgen_core::model::{
    Acquisition, BookRequest, Candidate, DownloadJob, DownloadList, Format, JobState, ListSettings,
    RequestStatus,
};

/// Top-level model handed to the UI. Mirrors one entry of the frontend's `LISTS`
/// array (the GUI loads a single list at a time today, but the shape is the one
/// the multi-list UI already speaks).
#[derive(Debug, Clone, Serialize)]
pub struct ViewModel {
    /// Stable list id the UI keys the sidebar/aggregate on.
    pub id: String,
    pub title: String,
    pub subtitle: String,
    /// Ranked preferred formats, most-preferred first (the list's `format_pref`).
    pub format_pref: Vec<String>,
    /// The full per-list settings the Settings sheet edits (a JSON-friendly
    /// mirror of the engine's [`ListSettings`]). `format_pref` above is the same
    /// data surfaced separately for the legacy toolbar/format-rank consumer.
    pub settings: ViewListSettings,
    /// True only for the singleton mutable **Manual** list (the list's
    /// `settings.is_manual`). The UI shows the per-book add/remove affordances
    /// only when this is set; imported reading lists are immutable.
    pub is_manual: bool,
    pub groups: Vec<ViewGroup>,
}

/// JSON-friendly mirror of the engine's [`ListSettings`] for the Settings sheet.
/// Every field round-trips through `set_settings`.
#[derive(Debug, Clone, Serialize)]
pub struct ViewListSettings {
    /// Ranked preferred formats, most-preferred first.
    pub format_pref: Vec<String>,
    /// Preferred language (empty string = any).
    pub language: String,
    /// Filename template, e.g. "{seq:02} - {authors} - {title}.{ext}".
    pub naming_template: String,
    /// Confidence at/above which a single best candidate auto-downloads (0..=1).
    pub auto_threshold: f32,
    /// Confidence below which nothing is offered (treated as not-found) (0..=1).
    pub near_threshold: f32,
    /// Sequence numbering scope: true = per-group, false = per-list.
    pub seq_per_group: bool,
    /// How many top-ranked variations to keep per request.
    pub keep_top: usize,
}

impl From<&ListSettings> for ViewListSettings {
    fn from(s: &ListSettings) -> Self {
        ViewListSettings {
            format_pref: s.format_pref.iter().map(|f| f.ext()).collect(),
            language: s.language.clone().unwrap_or_default(),
            naming_template: s.naming_template.clone(),
            auto_threshold: s.auto_threshold,
            near_threshold: s.near_threshold,
            seq_per_group: s.seq_per_group,
            keep_top: s.keep_top,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ViewGroup {
    pub name: String,
    pub books: Vec<ViewBook>,
    pub collapsed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ViewBook {
    pub id: String,
    pub title: String,
    pub author: String,
    /// The book's (possibly back-filled) publication year, when known.
    pub year: Option<u16>,
    /// Effective page count of the book's CHOSEN downloading/done copy: that
    /// copy's actual counted pages if known, else its mirror-reported `pages`;
    /// `None` when no copy is acquiring. A derived display value (NOT stored on
    /// `BookInput`). See [`libgen_core::model::effective_pages`].
    pub pages: Option<u32>,
    /// Names of the book's [`BookInput`] metadata fields currently AUTO-FILLED
    /// from the downloading/downloaded copy (a subset of `["authors", "year"]`),
    /// so the UI can mark them as auto-filled rather than user-entered. Mirrors
    /// [`libgen_core::model::BookRequest::backfilled`].
    pub backfilled: Vec<String>,
    pub seq: usize,
    /// Discovery state of the request: queued | matched | needs_selection |
    /// not_found. Drives the "Needs you" / "Not found" treatment independently
    /// of the per-variation download roll-up.
    pub discovery: String,
    /// The kept candidate variations (distinct md5s) the user can download,
    /// each with its own state. Empty when discovery is `not_found`.
    pub versions: Vec<ViewVariation>,
    /// Roll-up of requested variations (those with a download `job`), or `None`
    /// when the book is still purely in discovery. Mirrors `acquisition()`.
    pub acquisition: Option<ViewAcquisition>,
    /// `true` when this DOWNLOADED book's on-disk copy is no longer the best
    /// match — a better-ranked candidate exists, so the UI should surface a
    /// "review / replace" affordance. Set by `reverify_downloads` on Re-query.
    pub review: bool,
    /// The md5 of the recommended (top-ranked) candidate to replace the current
    /// copy with, present only when `review` is true; `null` otherwise. Feed this
    /// to the `replace_download` command.
    pub recommended_md5: Option<String>,
    /// Chronological event log for this book + its variations (born → discovered →
    /// selected → downloading/retry/failover → done/failed), for the detail
    /// timeline. Oldest first.
    pub history: Vec<ViewEvent>,
}

/// One entry in a book's [`ViewBook::history`] timeline.
#[derive(Debug, Clone, Serialize)]
pub struct ViewEvent {
    pub at_ms: u64,
    pub md5: Option<String>,
    pub fmt: Option<String>,
    pub kind: String,
    pub detail: String,
}

/// One kept candidate variation, with its own per-variation download state.
#[derive(Debug, Clone, Serialize)]
pub struct ViewVariation {
    pub md5: String,
    /// The candidate's OWN title/author as returned by the mirror, so the user
    /// can judge whether this result actually matches what they wanted.
    pub title: String,
    pub author: String,
    /// Format extension (e.g. "epub", "pdf").
    pub fmt: String,
    /// Best-effort size in MB for display (0 when unknown).
    pub size: u64,
    /// Raw size in bytes, if known.
    pub size_bytes: Option<u64>,
    pub year: Option<u16>,
    pub publisher: String,
    /// Candidate language as reported by the mirror ("" when unknown).
    pub language: String,
    /// Page count, when the mirror exposed one.
    pub pages: Option<u32>,
    /// Pages (PDF) or spine sections (EPUB) counted from the FINISHED file after
    /// its md5 verified — distinct from `pages` (mirror-reported metadata). `None`
    /// when unchecked/unsupported/unparseable. The UI shows this as the trustworthy
    /// "actual" page count.
    pub counted_pages: Option<u32>,
    /// True when `counted_pages` is known and below
    /// [`libgen_core::pagecount::LOW_PAGE_THRESHOLD`] (currently 10): the download
    /// completed and verified but is suspiciously short, so the UI can warn it may
    /// be a sample/wrong/corrupt file. Non-fatal — the variation is still `done`.
    pub low_pages: bool,
    pub host: Option<String>,
    /// One of: available | queued | downloading | done | failed — the UI's
    /// per-variation vocabulary, derived from the candidate's `job`.
    pub state: String,
    /// Download progress percent (0..=100).
    pub progress: u32,
    /// Bytes downloaded so far (populated only while/after downloading).
    pub downloaded_bytes: Option<u64>,
    /// Total bytes for the download, when known.
    pub total_bytes: Option<u64>,
    /// Smoothed download speed in bytes/sec (populated only while downloading).
    pub speed_bps: Option<u64>,
    /// Estimated seconds remaining (populated only while downloading and speed
    /// is non-zero; `None` once done, not downloading, or speed is zero).
    pub eta_secs: Option<u64>,
    /// Final on-disk path once written (for the Reveal action).
    pub output_path: Option<String>,
    /// Matcher confidence 0.0..=1.0.
    pub score: f32,
    /// Cover image URL from the download site (libgen), if the row carried one.
    pub cover_url: Option<String>,
    /// Why a failed variation failed (e.g. "downloaded file is missing (data
    /// lost)"), so the UI can show the reason. Only set for the `failed` state.
    pub last_error: Option<String>,
}

/// JSON-friendly mirror of the engine's [`Acquisition`] roll-up.
#[derive(Debug, Clone, Serialize)]
pub struct ViewAcquisition {
    pub requested: usize,
    pub done: usize,
    pub active: usize,
    pub failed: usize,
}

impl From<Acquisition> for ViewAcquisition {
    fn from(a: Acquisition) -> Self {
        ViewAcquisition {
            requested: a.requested,
            done: a.done,
            active: a.active,
            failed: a.failed,
        }
    }
}

/// Map an engine [`RequestStatus`] onto the UI's coarse discovery vocabulary.
/// Note: download progress lives per-variation (see [`variation_state`]); this
/// only describes whether the request has been found/needs a choice.
///
/// Exact strings the UI consumes:
///   * `"queued"`        — waiting to be queried (not yet searched),
///   * `"querying"`      — search in flight right now,
///   * `"matched"`       — discovered (matched/ready/downloading/…),
///   * `"needs_selection"` — ambiguous; user must pick,
///   * `"not_found"`     — nothing found (or cancelled).
pub(crate) fn discovery_str(status: &RequestStatus) -> &'static str {
    match status {
        RequestStatus::NeedsSelection => "needs_selection",
        RequestStatus::NotFound | RequestStatus::Cancelled => "not_found",
        RequestStatus::Queued => "queued",
        // Transient: the book is being searched in this query pass.
        RequestStatus::Querying => "querying",
        // Matched/Ready/and any download-phase status are "discovered": the
        // per-variation states carry the download story from here.
        _ => "matched",
    }
}

/// Map a candidate's optional [`DownloadJob`] onto the UI's per-variation state.
/// A variation with no job is "available" (kept but not requested for download).
fn variation_state(job: Option<&DownloadJob>) -> &'static str {
    match job.map(|j| &j.state) {
        None => "available",
        Some(JobState::Pending) => "queued",
        Some(JobState::Resolving) | Some(JobState::Downloading) | Some(JobState::Verifying) => {
            "downloading"
        }
        Some(JobState::Done) => "done",
        Some(JobState::Failed) => "failed",
        Some(JobState::Paused) => "paused",
        Some(JobState::Cancelled) => "cancelled",
    }
}

fn fmt_str(f: Option<&Format>) -> String {
    f.map(|f| f.ext()).unwrap_or_else(|| "epub".into())
}

fn progress_pct(job: Option<&DownloadJob>) -> u32 {
    match job {
        Some(j) if j.state == JobState::Done => 100,
        Some(j) => match j.total_bytes {
            Some(total) if total > 0 => {
                (((j.bytes_done as f64 / total as f64) * 100.0).round() as u32).min(100)
            }
            _ => 0,
        },
        None => 0,
    }
}

fn view_variation(c: &Candidate) -> ViewVariation {
    let job = c.job.as_ref();
    // Speed/ETA/byte counts are live download telemetry: surface them only while a
    // download is actually in flight, so a finished/paused/available variation
    // doesn't show a stale rate.
    let downloading = matches!(
        job.map(|j| &j.state),
        Some(JobState::Resolving) | Some(JobState::Downloading) | Some(JobState::Verifying)
    );
    ViewVariation {
        md5: c.md5.clone(),
        title: c.title.clone(),
        author: c.authors.join(", "),
        fmt: fmt_str(c.extension.as_ref()),
        size: c
            .size_bytes
            .map(|b| (b / (1024 * 1024)).max(1))
            .unwrap_or(0),
        size_bytes: c.size_bytes,
        year: c.year,
        publisher: c.publisher.clone().unwrap_or_default(),
        language: c.language.clone().unwrap_or_default(),
        pages: c.pages,
        counted_pages: job.and_then(|j| j.page_count),
        low_pages: job
            .and_then(|j| j.page_count)
            .map(|n| n < libgen_core::pagecount::LOW_PAGE_THRESHOLD)
            .unwrap_or(false),
        host: job.and_then(|j| j.host.clone()),
        state: variation_state(job).to_string(),
        progress: progress_pct(job),
        downloaded_bytes: if downloading {
            job.map(|j| j.bytes_done)
        } else {
            None
        },
        total_bytes: if downloading {
            job.and_then(|j| j.total_bytes)
        } else {
            None
        },
        speed_bps: if downloading {
            job.and_then(|j| j.speed_bps)
        } else {
            None
        },
        eta_secs: if downloading {
            job.and_then(|j| j.eta_secs)
        } else {
            None
        },
        output_path: job.and_then(|j| j.output_path.clone()),
        score: c.score,
        cover_url: c.cover_url.clone(),
        last_error: job
            .filter(|j| j.state == JobState::Failed)
            .and_then(|j| j.last_error.clone()),
    }
}

/// The multi-list library handed to the UI: every persisted list projected,
/// plus which one is currently active. Mirrors the frontend's `LISTS` array +
/// `CURRENT` selection (the sidebar + "All downloads" aggregate).
#[derive(Debug, Clone, Serialize)]
pub struct ViewLibrary {
    pub lists: Vec<ViewModel>,
    /// Active list id, or `"__all__"` for the aggregate.
    pub current: String,
    /// Global app settings (download folder, sites, concurrency/politeness).
    pub config: ViewAppConfig,
    /// Configured search-mirror hosts (read-only; `mirrors.toml` is hand-edited).
    pub search_mirrors: Vec<String>,
}

/// JSON-friendly mirror of the global [`crate::state::AppSettings`] for the
/// Settings sheet's "App settings" section.
#[derive(Debug, Clone, Serialize)]
pub struct ViewAppConfig {
    /// Default download folder (where finished files are written).
    pub out_dir: String,
    /// Global cap on total concurrent downloads (`G`).
    pub max_concurrent_downloads: usize,
    /// Concurrent search queries (`Orchestrator::with_query_concurrency`).
    pub query_concurrency: usize,
    /// Max retry attempts per host (`HostLimits.max_attempts`).
    pub max_attempts: u32,
    /// Speculative (hedged) download: race a slow mirror against a fresh one.
    /// OFF by default.
    pub hedge_enabled: bool,
}

/// One site's live availability (from open-slum.org), for the Settings sheet's
/// "Mirror health" panel and the on-demand refresh. A JSON-friendly mirror of
/// [`libgen_core::slum::SlumSite`].
#[derive(Debug, Clone, Serialize)]
pub struct ViewSiteHealth {
    /// Bare host (e.g. `annas-archive.gl`).
    pub host: String,
    /// Human-readable monitor name (e.g. `Anna's Archive GL`).
    pub name: String,
    /// SLUM group (e.g. `Anna's Archive`, `Library Genesis+`).
    pub group: String,
    /// Whether the latest heartbeat reports the site up.
    pub up: bool,
    /// Latest heartbeat round-trip in milliseconds, when reported.
    pub ping_ms: Option<u32>,
    /// Rolling 24h uptime ratio (0.0–1.0), when reported.
    pub uptime_24h: Option<f64>,
}

/// Build the full [`ViewModel`] from a persisted [`DownloadList`], tagging it
/// with the stable UI `id` the sidebar keys on.
///
/// Groups (and nested subgroups) are flattened into a single ordered list of
/// rendered sections — exactly the depth-first order [`crate::bridge::positions`]
/// uses — so a book's flat `bkN` id matches the row the UI draws.
pub fn build_with_id(id: String, list: &DownloadList) -> ViewModel {
    let mut groups = Vec::new();
    let mut flat = 0usize;
    flatten_groups(&list.groups, &mut flat, &mut groups);
    let total = flat;

    ViewModel {
        id,
        title: if list.title.is_empty() {
            "Reading list".into()
        } else {
            list.title.clone()
        },
        subtitle: format!("{total} book(s)"),
        format_pref: format_pref_strings(&list.settings),
        settings: ViewListSettings::from(&list.settings),
        is_manual: list.settings.is_manual,
        groups,
    }
}

/// Convenience: project a list under the default `"loaded"` id (used by the
/// crate's e2e test, which mirrors a single-list command path).
#[cfg(test)]
pub fn build(list: &DownloadList) -> ViewModel {
    build_with_id("loaded".into(), list)
}

/// The list's ranked preferred formats as plain extension strings.
fn format_pref_strings(settings: &ListSettings) -> Vec<String> {
    settings.format_pref.iter().map(|f| f.ext()).collect()
}

/// Depth-first flatten: emit each group's books (assigning consecutive flat
/// indices), then recurse into its subgroups, matching `bridge::positions`.
fn flatten_groups(
    groups: &[libgen_core::model::Group],
    flat: &mut usize,
    out: &mut Vec<ViewGroup>,
) {
    for (gi, g) in groups.iter().enumerate() {
        let mut seq = 0usize;
        let books = g
            .books
            .iter()
            .map(|req| {
                let idx = *flat;
                *flat += 1;
                seq += 1;
                build_book(req, idx, seq)
            })
            .collect();
        out.push(ViewGroup {
            // Prefix the (sub)group with its order among siblings — the group
            // order matters, and this matches the numbered folder it downloads to.
            name: format!("{:02}. {}", gi + 1, g.name),
            books,
            collapsed: false,
        });
        flatten_groups(&g.subgroups, flat, out);
    }
}

fn build_book(req: &BookRequest, flat_index: usize, seq: usize) -> ViewBook {
    let discovery = discovery_str(&req.status).to_string();
    let versions = if req.status == RequestStatus::NotFound {
        Vec::new()
    } else {
        req.candidates.iter().map(view_variation).collect()
    };
    // When flagged for review, recommend a candidate to replace the downloaded
    // copy. Prefer the best-ranked candidate of the SAME FORMAT as the on-disk
    // copy (don't push the user to a different format when a good same-format
    // copy exists); fall back to the top-ranked candidate overall.
    let recommended_md5 = if req.review {
        let downloaded = req.candidates.iter().find(|c| {
            matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Done))
                && c.job
                    .as_ref()
                    .and_then(|j| j.output_path.as_ref())
                    .is_some()
        });
        let want_fmt = downloaded.and_then(|c| c.extension.as_ref());
        let same_fmt = want_fmt.and_then(|fmt| {
            req.candidates
                .iter()
                .find(|c| {
                    c.extension.as_ref() == Some(fmt)
                        && downloaded.map(|d| d.md5.as_str()) != Some(c.md5.as_str())
                })
                .map(|c| c.md5.clone())
        });
        same_fmt.or_else(|| req.candidates.first().map(|c| c.md5.clone()))
    } else {
        None
    };
    ViewBook {
        id: format!("bk{flat_index}"),
        title: req.input.title.clone(),
        author: req.input.authors.join(", "),
        year: libgen_core::model::effective_year(req),
        pages: libgen_core::model::effective_pages(req),
        backfilled: req.backfilled.clone(),
        seq,
        discovery,
        versions,
        acquisition: req.acquisition().map(Into::into),
        review: req.review,
        recommended_md5,
        history: req
            .history
            .iter()
            .map(|e| ViewEvent {
                at_ms: e.at_ms,
                md5: e.md5.clone(),
                fmt: e.fmt.clone(),
                kind: e.kind.clone(),
                detail: e.detail.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod unit {
    use super::*;
    use libgen_core::model::{Candidate, DownloadJob, Format, JobState, RequestStatus};

    #[test]
    fn querying_maps_to_querying_and_queued_to_queued() {
        // The two states the query progress UI distinguishes: "queued for query"
        // vs "being queried".
        assert_eq!(discovery_str(&RequestStatus::Queued), "queued");
        assert_eq!(discovery_str(&RequestStatus::Querying), "querying");
        assert_eq!(
            discovery_str(&RequestStatus::NeedsSelection),
            "needs_selection"
        );
        assert_eq!(discovery_str(&RequestStatus::NotFound), "not_found");
        assert_eq!(discovery_str(&RequestStatus::Matched), "matched");
    }

    fn cand() -> Candidate {
        Candidate {
            md5: "a".repeat(32),
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            year: Some(2000),
            publisher: Some("Cassell".into()),
            language: Some("English".into()),
            pages: Some(233),
            extension: Some(Format::Epub),
            size_bytes: Some(2 * 1024 * 1024),
            source_host: Some("libgen.li".into()),
            cover_url: None,
            score: 0.99,
            job: None,
        }
    }

    #[test]
    fn variation_projects_language_and_pages() {
        let v = view_variation(&cand());
        assert_eq!(v.language, "English");
        assert_eq!(v.pages, Some(233));
        // Empty language when the mirror didn't report one.
        let mut c = cand();
        c.language = None;
        c.pages = None;
        let v = view_variation(&c);
        assert_eq!(v.language, "");
        assert_eq!(v.pages, None);
    }

    #[test]
    fn book_pages_prefer_counted_then_reported() {
        use libgen_core::model::{BookInput, BookRequest};
        // Chosen (done) copy with both a counted page_count and reported pages:
        // the actual counted value wins.
        let mut req = BookRequest::new(BookInput {
            title: "T".into(),
            // The book's year lives on `input` (set by back-fill or the user); the
            // viewmodel surfaces it as `book.year`.
            year: Some(2000),
            ..Default::default()
        });
        let mut c = cand(); // reports pages = Some(233)
        c.job = Some(DownloadJob {
            state: JobState::Done,
            page_count: Some(231),
            output_path: Some("/x".into()),
            ..Default::default()
        });
        req.candidates = vec![c];
        req.selected = Some("a".repeat(32));
        let book = build_book(&req, 0, 1);
        assert_eq!(book.pages, Some(231), "counted pages preferred");
        // Year is surfaced on the book.
        assert_eq!(book.year, Some(2000));

        // No counted value → fall back to the candidate's reported pages.
        req.candidates[0].job.as_mut().unwrap().page_count = None;
        let book = build_book(&req, 0, 1);
        assert_eq!(book.pages, Some(233), "falls back to reported pages");

        // No acquiring copy → no page count.
        req.candidates[0].job = None;
        let book = build_book(&req, 0, 1);
        assert_eq!(book.pages, None);
    }

    #[test]
    fn speed_eta_surface_only_while_downloading() {
        // Available (no job): no telemetry.
        let v = view_variation(&cand());
        assert_eq!(v.speed_bps, None);
        assert_eq!(v.eta_secs, None);
        assert_eq!(v.downloaded_bytes, None);

        // Actively downloading: telemetry surfaces.
        let mut c = cand();
        c.job = Some(DownloadJob {
            state: JobState::Downloading,
            bytes_done: 1024,
            total_bytes: Some(4096),
            speed_bps: Some(512),
            eta_secs: Some(6),
            ..Default::default()
        });
        let v = view_variation(&c);
        assert_eq!(v.downloaded_bytes, Some(1024));
        assert_eq!(v.total_bytes, Some(4096));
        assert_eq!(v.speed_bps, Some(512));
        assert_eq!(v.eta_secs, Some(6));

        // Done: stale telemetry is suppressed (job cleared it, and the view gates
        // on state too).
        let mut c = cand();
        c.job = Some(DownloadJob {
            state: JobState::Done,
            bytes_done: 4096,
            total_bytes: Some(4096),
            speed_bps: None,
            eta_secs: None,
            ..Default::default()
        });
        let v = view_variation(&c);
        assert_eq!(v.speed_bps, None);
        assert_eq!(v.eta_secs, None);
    }
}

#[cfg(test)]
mod e2e {
    //! End-to-end check that the GUI's data path works: drive the real engine
    //! (replay search, no network) exactly as the `query_and_match` command does,
    //! then project to the `ViewModel` the frontend renders, and assert it
    //! reflects per-variation state. This verifies the command layer's logic
    //! without needing a live Tauri window.
    use super::build;
    use libgen_core::model::{BookInput, BookRequest, DownloadList, Group, ListSettings};
    use libgen_core::orchestrator::{Event, Orchestrator};
    use libgen_core::search::{MirrorConfig, SearchClient};
    use libgen_core::store::Store;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("fixtures")
            .join("search")
    }

    fn config() -> MirrorConfig {
        MirrorConfig::from_toml(
            r#"
            [[search_mirror]]
            host = "libgen.li"
            search_url = "https://libgen.li/index.php?req={query}&res={limit}"
            kind = "libgen_li_html"
            priority = 1
            [[search_mirror]]
            host = "libgen.is"
            search_url = "https://libgen.is/json.php?req={query}"
            kind = "libgen_json"
            priority = 2
        "#,
        )
        .unwrap()
    }

    fn small_list() -> DownloadList {
        let mut g = Group::new("Batch 1");
        for (t, a) in [
            ("Treasure Island", "Robert Louis Stevenson"),
            ("Anne of Green Gables", "L. M. Montgomery"),
        ] {
            g.books.push(BookRequest::new(BookInput {
                title: t.into(),
                authors: vec![a.into()],
                ..Default::default()
            }));
        }
        DownloadList {
            title: "Mini".into(),
            settings: ListSettings::default(),
            groups: vec![g],
        }
    }

    #[tokio::test]
    async fn viewmodel_reflects_per_variation_state_after_query() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/tmp/x").unwrap();

        let (tx, rx) = mpsc::channel::<Event>(64);
        let drain = tokio::spawn(async move {
            let mut rx = rx;
            while rx.recv().await.is_some() {}
        });
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = drain.await;

        // Project exactly as the command layer does.
        let vm = build(&orch.snapshot().unwrap());

        // Format preference is surfaced to the UI (default epub/pdf).
        assert!(!vm.format_pref.is_empty());

        let book = vm.groups[0]
            .books
            .iter()
            .find(|b| b.title == "Treasure Island")
            .expect("Treasure Island projected");
        assert_eq!(book.discovery, "matched");
        assert!(!book.versions.is_empty(), "kept variations projected");

        // One-best default: exactly one variation auto-requested (state != available),
        // and the acquisition roll-up reflects it.
        let requested: Vec<_> = book
            .versions
            .iter()
            .filter(|v| v.state != "available")
            .collect();
        assert_eq!(requested.len(), 1, "one best variation auto-requested");
        let acq = book
            .acquisition
            .as_ref()
            .expect("acquisition roll-up present");
        assert_eq!(acq.requested, 1);
    }
}
