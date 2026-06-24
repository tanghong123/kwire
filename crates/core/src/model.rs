//! Shared domain model. This is the contract between every engine module and
//! every front end. Keep it stable; subsystem-specific types live in their own
//! modules (e.g. mirror config in `search`).

use serde::{Deserialize, Serialize};

/// A whole reading list = one destination folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DownloadList {
    pub title: String,
    #[serde(default)]
    pub settings: ListSettings,
    /// Top-level groups. A flat list is represented as a single implicit group
    /// (or books placed in a root group named after the list).
    pub groups: Vec<Group>,
}

/// A (possibly nested) group of books → maps to a subfolder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Group {
    pub name: String,
    #[serde(default)]
    pub books: Vec<BookRequest>,
    /// Nested subgroups → nested subfolders.
    #[serde(default)]
    pub subgroups: Vec<Group>,
}

impl Group {
    pub fn new(name: impl Into<String>) -> Self {
        Group {
            name: name.into(),
            books: Vec::new(),
            subgroups: Vec::new(),
        }
    }
}

/// User-supplied metadata describing a wanted book.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BookInput {
    pub title: String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Preferred formats, most-preferred first. Empty = inherit list default.
    #[serde(default)]
    pub format_pref: Vec<Format>,
}

/// One tracked request: the input plus its lifecycle state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BookRequest {
    pub input: BookInput,
    #[serde(default)]
    pub status: RequestStatus,
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    /// md5 of the chosen candidate, once decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<DownloadJob>,
    /// Stable, persisted sequence number assigned to this book the first time it
    /// is planned for download. Reused on every subsequent plan so inserting a
    /// new book mid-list does NOT renumber existing books' files. `None` until
    /// the book has been assigned a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u32>,
    /// Set by [`crate::orchestrator::Orchestrator::reverify_downloads`] when a
    /// downloaded (`Done`) variation is NOT the top-ranked fresh candidate — i.e.
    /// a better-matching copy now exists, so the user should review/replace it.
    /// `false` when the downloaded copy is still the best match (or untouched).
    #[serde(default)]
    pub review: bool,
    /// When the user ACCEPTS the current downloaded copy ("Accept current copy"),
    /// this records the md5 of the *recommended replacement they declined*. A
    /// later re-verify suppresses the review flag ONLY while its recommendation is
    /// still this same md5 — if a genuinely DIFFERENT (newly-better) copy is
    /// recommended, the book surfaces for review again so the user can consider
    /// it. `None` = no standing decision. Persisted; survives relaunch + Reverify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_dismissed: Option<String>,
    /// Pending trash-on-replace bookkeeping: when a `replace_download` is in
    /// flight, this records the OLD downloaded file so that, once the recommended
    /// copy finishes, the old file can be moved to Trash. Cleared on completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash_on_replace: Option<TrashPending>,
    /// How far the execution engine should drive this book (see
    /// `docs/EXECUTION_MODEL.md`). The book's `status` is its CURRENT state; this
    /// is the GOAL. The engine advances `status` toward `goal` doing all network
    /// I/O off the library lock. Defaults to `Idle` (do nothing) for old rows.
    #[serde(default)]
    pub goal: Goal,
    /// md5s the user has explicitly DISMISSED (removed) for this book. A re-query
    /// must not re-surface them as candidates — removing a copy means "don't offer
    /// this one again", not "drop it until the next search puts it back".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dismissed: Vec<String>,
    /// Append-only chronicle of key events in this book's (and its variations')
    /// life — discovered, selected, downloading from a host, retry/backoff,
    /// failover, done, failed, accepted, … — so the journey is inspectable in the
    /// UI and for diagnosis. Capped (oldest dropped) to bound growth.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<BookEvent>,
}

/// One entry in a book's [`BookRequest::history`] chronicle. `md5`/`fmt` tag the
/// variation an event pertains to (absent for book-level events). `kind` is a
/// short stable label (e.g. `"downloading"`, `"retry"`, `"failover"`, `"done"`,
/// `"failed"`); `detail` is human-readable context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BookEvent {
    /// Unix epoch milliseconds when the event was recorded.
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fmt: Option<String>,
    pub kind: String,
    pub detail: String,
}

/// The target the execution engine drives a [`BookRequest`] toward. `Match`
/// means "discover only" (reach `Matched`/`NeedsSelection`/`NotFound`);
/// `Complete` means "discover AND download to `Done`". Ordering matters:
/// `Idle < Match < Complete` — a higher goal subsumes the work of a lower one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Goal {
    /// Park the book — the engine takes no action (e.g. paused).
    #[default]
    Idle,
    /// Drive discovery only: query + match, then stop for review.
    Match,
    /// Drive all the way: discover, then download every chosen variation.
    Complete,
}

/// Records the previously-downloaded file to move to Trash once its replacement
/// (the recommended copy) finishes downloading. See
/// [`crate::orchestrator::Orchestrator::replace_download`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrashPending {
    /// md5 of the old (currently-Done) variation being replaced.
    pub old_md5: String,
    /// On-disk path of the old downloaded file to trash on success.
    pub old_path: String,
}

impl BookRequest {
    pub fn new(input: BookInput) -> Self {
        BookRequest {
            input,
            status: RequestStatus::Queued,
            candidates: Vec::new(),
            selected: None,
            job: None,
            seq: None,
            review: false,
            review_dismissed: None,
            trash_on_replace: None,
            goal: Goal::default(),
            dismissed: Vec::new(),
            history: Vec::new(),
        }
    }

    /// Append a timestamped event to this book's [`history`](Self::history)
    /// chronicle (oldest dropped past a cap). `md5`/`fmt` tag the variation an
    /// event is about (pass `None` for a book-level event).
    pub fn log_event(
        &mut self,
        md5: Option<String>,
        fmt: Option<String>,
        kind: &str,
        detail: impl Into<String>,
    ) {
        const MAX: usize = 300;
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.history.push(BookEvent {
            at_ms,
            md5,
            fmt,
            kind: kind.to_string(),
            detail: detail.into(),
        });
        if self.history.len() > MAX {
            let excess = self.history.len() - MAX;
            self.history.drain(0..excess);
        }
    }

    /// Roll-up of per-variation download state across requested variations
    /// (those with a `job`). Returns `None` if nothing has been requested for
    /// download yet (the request is still in discovery). This is what the book
    /// row summarizes (e.g. "Downloading 1/2").
    pub fn acquisition(&self) -> Option<Acquisition> {
        let states: Vec<&JobState> = self
            .candidates
            .iter()
            .filter_map(|c| c.job.as_ref().map(|j| &j.state))
            .collect();
        if states.is_empty() {
            return None;
        }
        let mut a = Acquisition {
            requested: states.len(),
            done: 0,
            active: 0,
            queued: 0,
            downloading: 0,
            failed: 0,
            paused: 0,
            cancelled: 0,
        };
        for s in states {
            match s {
                JobState::Done => a.done += 1,
                JobState::Failed => a.failed += 1,
                JobState::Paused => a.paused += 1,
                JobState::Cancelled => a.cancelled += 1,
                // `Pending` = submitted but waiting for a host slot (NOT yet
                // transferring). The transferring/working states are separate so
                // the row only reads "Downloading" once bytes can actually move.
                JobState::Pending => {
                    a.queued += 1;
                    a.active += 1;
                }
                // Resolving/Downloading/Verifying = a slot is held and work is
                // actually happening.
                _ => {
                    a.downloading += 1;
                    a.active += 1;
                }
            }
        }
        Some(a)
    }
}

/// A roll-up of a request's per-variation download state, for the row summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acquisition {
    /// Variations requested for download.
    pub requested: usize,
    pub done: usize,
    /// In-flight total (`queued + downloading`); kept for back-compat.
    pub active: usize,
    /// Submitted but waiting for a host slot (`Pending`) — not yet transferring.
    pub queued: usize,
    /// A slot is held and work is happening (resolving/downloading/verifying).
    pub downloading: usize,
    pub failed: usize,
    /// Paused (kept `.part` + resume_offset, resumable).
    pub paused: usize,
    /// Cancelled (will not resume on its own).
    pub cancelled: usize,
}

impl Acquisition {
    /// Every requested variation finished successfully.
    pub fn all_done(&self) -> bool {
        self.requested > 0 && self.done == self.requested
    }
}

/// Lifecycle state of a request (see DESIGN.md §4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    #[default]
    Queued,
    Querying,
    /// Confident auto-match; ready to download without user input.
    Matched,
    /// Ambiguous; user must pick from `candidates`.
    NeedsSelection,
    NotFound,
    Ready,
    Downloading,
    Verifying,
    Done,
    Failed {
        error: String,
    },
    Paused,
    Cancelled,
}

/// A search result from a mirror, the unit the matcher scores and the
/// downloader fetches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Candidate {
    pub md5: String,
    pub title: String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Page count, when the mirror exposes one. `None` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<Format>,
    /// File size in bytes, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Host that produced this candidate (search mirror).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_host: Option<String>,
    /// Absolute URL of the cover image parsed from the search-result row's cover
    /// cell (`/comicscovers/<bucket>/<md5>.jpg` for comics, `/covers/…` otherwise),
    /// when the mirror exposes one. `#[serde(default)]` + `skip_serializing_if` so
    /// old persisted rows decode unchanged and rows without a cover add no key
    /// (NO schema bump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    /// Confidence 0.0..=1.0 assigned by the matcher.
    #[serde(default)]
    pub score: f32,
    /// Per-variation download state. `None` = this variation has not been
    /// requested for download. This is what lets several variations of one book
    /// be in different states at once (e.g. the epub `Done` while the pdf is
    /// still `Downloading`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<DownloadJob>,
}

impl Candidate {
    /// Whether this variation has been requested for download (has a job).
    pub fn is_requested(&self) -> bool {
        self.job.is_some()
    }
}

/// Tracks an in-flight or completed download for a request.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DownloadJob {
    pub state: JobState,
    /// Download host the job was routed to (per-host queue key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub attempts: u32,
    pub bytes_done: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// Smoothed download speed in bytes/sec, observed while downloading. `None`
    /// before any throughput is measured. Not persisted across restarts (a fresh
    /// `start_downloads` re-measures), but kept on the in-memory job so a
    /// re-snapshot reflects the latest live speed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_bps: Option<u64>,
    /// Estimated seconds remaining at the current smoothed speed. `None` when the
    /// total is unknown or the speed is zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u64>,
    /// Byte offset to resume from on restart (HTTP Range).
    pub resume_offset: u64,
    pub md5_verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Final on-disk path, once written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    /// Transient per-leg state for an in-flight **speculative (hedged) download**:
    /// when the primary transport stalls, the scheduler races a second transport
    /// for the same book from a different mirror/host into its own temp file. Each
    /// element is one racing leg. Empty for a normal (un-hedged) download, and
    /// cleared again the moment the race resolves (the winning leg's fields are
    /// written back onto this job), so a completed download looks identical to a
    /// non-hedged one. Not persisted across restarts — a relaunch clears it (see
    /// `reset_inflight_for_resume`) and resumes as a single normal attempt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hedges: Vec<HedgeLeg>,
}

/// One racing transport of a speculative (hedged) download. The book *variation*
/// owns the race (via [`DownloadJob::hedges`]); each leg fetches the WHOLE file
/// independently into its own `temp_path`, and the first leg to return a
/// verified, md5-complete file wins (its temp is promoted to the final dest and
/// the siblings are cancelled + cleaned). See `docs/SPECULATIVE_DOWNLOAD.md`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HedgeLeg {
    /// md5 this leg is fetching (may differ from the job's selected md5 when the
    /// hedge draws an alternate sibling candidate).
    pub md5: String,
    /// Download host this leg was routed to (per-host queue key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Unique sibling temp path this leg streams to (so two legs never clobber one
    /// `.part`). Promoted to the final dest on a win, removed on a loss.
    pub temp_path: String,
    /// Bytes streamed so far on this leg.
    #[serde(default)]
    pub bytes_done: u64,
    /// Smoothed throughput of this leg in bytes/sec, `None` until measurable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_bps: Option<u64>,
    /// Lifecycle of this leg.
    #[serde(default)]
    pub state: JobState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    #[default]
    Pending,
    Resolving,
    Downloading,
    Verifying,
    Done,
    Failed,
    /// Download was paused mid-flight; the `.part` and `resume_offset` are kept
    /// so it can resume where it left off.
    Paused,
    /// Download was cancelled; it will not resume on its own.
    Cancelled,
}

/// Common ebook formats. `Other` keeps unknown extensions round-trippable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Epub,
    Pdf,
    Mobi,
    Azw3,
    Djvu,
    Cbz,
    Cbr,
    Txt,
    Other(String),
}

impl Format {
    pub fn parse(s: &str) -> Format {
        match s.trim().to_ascii_lowercase().as_str() {
            "epub" => Format::Epub,
            "pdf" => Format::Pdf,
            "mobi" => Format::Mobi,
            "azw3" => Format::Azw3,
            "djvu" => Format::Djvu,
            "cbz" => Format::Cbz,
            "cbr" => Format::Cbr,
            "txt" => Format::Txt,
            other => Format::Other(other.to_string()),
        }
    }

    pub fn ext(&self) -> String {
        match self {
            Format::Epub => "epub".into(),
            Format::Pdf => "pdf".into(),
            Format::Mobi => "mobi".into(),
            Format::Azw3 => "azw3".into(),
            Format::Djvu => "djvu".into(),
            Format::Cbz => "cbz".into(),
            Format::Cbr => "cbr".into(),
            Format::Txt => "txt".into(),
            Format::Other(s) => s.clone(),
        }
    }
}

/// Per-list configuration affecting matching and file output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListSettings {
    /// Preferred formats, most-preferred first.
    #[serde(default = "default_format_pref")]
    pub format_pref: Vec<Format>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Filename template, e.g. "{seq:02} - {authors} - {title}.{ext}".
    #[serde(default = "default_naming_template")]
    pub naming_template: String,
    /// Confidence at/above which a single best candidate auto-downloads.
    #[serde(default = "default_auto_threshold")]
    pub auto_threshold: f32,
    /// Confidence below which nothing is offered (treated as not-found).
    #[serde(default = "default_near_threshold")]
    pub near_threshold: f32,
    /// "Right book" confidence (title + author) at/above which the top candidate
    /// auto-matches and its best variation is downloaded without asking — even when
    /// several formats/sizes exist (those are auto-ranked). Below this (but above
    /// `near_threshold`) the request goes to `NeedsSelection`. Keyed on how sure we
    /// are it's the correct book, not on variation ambiguity.
    #[serde(default = "default_title_match_threshold")]
    pub title_match_threshold: f32,
    /// Sequence numbering scope: true = per-group, false = per-list.
    #[serde(default = "default_true")]
    pub seq_per_group: bool,
    /// How many top-ranked candidate variations (distinct md5s) to keep per
    /// request, so the user can later swap to a different copy.
    #[serde(default = "default_keep_top")]
    pub keep_top: usize,
}

fn default_format_pref() -> Vec<Format> {
    // Formats friendly to BOTH Kindle (incl. Send-to-Kindle) and iPad (Apple
    // Books). EPUB is ideal (reflowable on both); PDF is the universal fallback.
    // MOBI/AZW3 are Kindle-only, so they're excluded from the default.
    vec![Format::Epub, Format::Pdf]
}
fn default_naming_template() -> String {
    "{seq:02} - {authors} - {title}.{ext}".to_string()
}
fn default_auto_threshold() -> f32 {
    0.85
}
fn default_near_threshold() -> f32 {
    0.45
}
fn default_title_match_threshold() -> f32 {
    // ~0.9: a strong title match (or full request-title containment) plus a
    // reasonable author. Tuned so "right title, only format/size differs"
    // auto-matches, while a different-but-similar book still asks.
    0.9
}
fn default_true() -> bool {
    true
}
fn default_keep_top() -> usize {
    5
}

impl Default for ListSettings {
    fn default() -> Self {
        ListSettings {
            format_pref: default_format_pref(),
            language: None,
            naming_template: default_naming_template(),
            auto_threshold: default_auto_threshold(),
            near_threshold: default_near_threshold(),
            title_match_threshold: default_title_match_threshold(),
            seq_per_group: true,
            keep_top: default_keep_top(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand_with(state: Option<JobState>) -> Candidate {
        Candidate {
            md5: "x".into(),
            title: "t".into(),
            authors: vec![],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: Some(Format::Epub),
            size_bytes: None,
            source_host: None,
            cover_url: None,
            score: 1.0,
            job: state.map(|s| DownloadJob {
                state: s,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn acquisition_rolls_up_per_variation_states() {
        let mut req = BookRequest::new(BookInput::default());
        // No requested variations -> still in discovery.
        req.candidates = vec![cand_with(None), cand_with(None)];
        assert!(req.acquisition().is_none());

        // epub done, pdf downloading, a third failed.
        req.candidates = vec![
            cand_with(Some(JobState::Done)),
            cand_with(Some(JobState::Downloading)),
            cand_with(Some(JobState::Failed)),
            cand_with(None),
        ];
        let a = req.acquisition().unwrap();
        assert_eq!(a.requested, 3);
        assert_eq!(a.done, 1);
        assert_eq!(a.active, 1);
        assert_eq!(a.failed, 1);
        assert!(!a.all_done());

        // All done.
        req.candidates = vec![
            cand_with(Some(JobState::Done)),
            cand_with(Some(JobState::Done)),
        ];
        assert!(req.acquisition().unwrap().all_done());
    }

    #[test]
    fn job_without_hedges_field_decodes_to_empty() {
        // An old persisted job (no `hedges` key) must decode with an empty vec —
        // the serde(default) keeps existing rows readable with NO schema bump.
        let json = r#"{
            "state": "downloading",
            "attempts": 1,
            "bytes_done": 100,
            "resume_offset": 0,
            "md5_verified": false
        }"#;
        let job: DownloadJob = serde_json::from_str(json).expect("decodes without hedges");
        assert!(job.hedges.is_empty());
    }

    #[test]
    fn empty_hedges_is_skipped_in_serialization() {
        // A normal (un-hedged) job serializes WITHOUT a `hedges` key, so on-disk
        // blobs are identical to before the feature.
        let job = DownloadJob {
            state: JobState::Done,
            ..Default::default()
        };
        let s = serde_json::to_string(&job).unwrap();
        assert!(!s.contains("hedges"), "empty hedges must be skipped: {s}");
    }

    #[test]
    fn hedge_legs_round_trip() {
        let job = DownloadJob {
            state: JobState::Downloading,
            hedges: vec![
                HedgeLeg {
                    md5: "aaaa".into(),
                    host: Some("hostA".into()),
                    temp_path: "/tmp/out.bin".into(),
                    bytes_done: 10,
                    speed_bps: Some(1000),
                    state: JobState::Downloading,
                },
                HedgeLeg {
                    md5: "bbbb".into(),
                    host: Some("hostB".into()),
                    temp_path: "/tmp/out.bin.hedge.bbbb.0".into(),
                    bytes_done: 20,
                    speed_bps: None,
                    state: JobState::Pending,
                },
            ],
            ..Default::default()
        };
        let s = serde_json::to_string(&job).unwrap();
        let back: DownloadJob = serde_json::from_str(&s).unwrap();
        assert_eq!(job, back);
        assert_eq!(back.hedges.len(), 2);
        assert_eq!(back.hedges[1].temp_path, "/tmp/out.bin.hedge.bbbb.0");
    }
}
