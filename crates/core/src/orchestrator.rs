//! Pipeline orchestration (DESIGN.md §3): tie parse → query → match → persist →
//! naming → download together behind a **UI-agnostic command + event surface**.
//!
//! A front end never touches the network or DB directly. It hands the
//! orchestrator a parsed [`DownloadList`] plus a [`SearchClient`] and (optionally)
//! a download [`Scheduler`], then drives it with [`Command`]s and observes
//! [`Event`]s. The orchestrator persists every state transition through
//! [`store::Store`] so a quit/crash resumes cleanly, and computes destination
//! paths through [`naming`].
//!
//! Everything is driveable headlessly: tests use a replay [`SearchClient`] and a
//! mock resolver, with no live network.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use crate::matching;
use crate::model::{
    BookRequest, Candidate, DownloadJob, DownloadList, Format, Group, JobState, RequestStatus,
};
use crate::naming;
use crate::queue::{DownloadRequest, Progress, Scheduler};
use crate::search::SearchClient;
use crate::store::Store;

/// Commands a front end issues to drive the pipeline. UI-agnostic.
#[derive(Debug, Clone)]
pub enum Command {
    /// Run query + match for every queued request in the list, persisting the
    /// resulting statuses and candidates.
    QueryAll,
    /// Download every `ready` (matched / explicitly-ready) request, computing
    /// destination paths via `naming` and persisting job state.
    StartDownloads,
    /// Pick a specific candidate (by md5) for the request at a tree position,
    /// transitioning it to `Ready`.
    SelectCandidate {
        group_path: Vec<usize>,
        book_index: usize,
        md5: String,
    },
    /// Re-queue a failed/not-found request so the next `QueryAll` retries it.
    Retry {
        group_path: Vec<usize>,
        book_index: usize,
    },
}

/// Events the orchestrator emits as the pipeline progresses. A superset of the
/// scheduler's [`Progress`] plus higher-level status transitions, so any front
/// end can render a live queue without reaching into the engine.
#[derive(Debug, Clone)]
pub enum Event {
    /// A request changed status (after query/match, selection, or retry).
    StatusChanged {
        group_path: Vec<usize>,
        book_index: usize,
        title: String,
        status: RequestStatus,
    },
    /// Per-book query-stage transition during a `query_all` pass, so a front end
    /// can show "being queried" vs. its resolved outcome live. `stage` is one of
    /// `"querying"` (search in flight), `"matched"`, `"needs_selection"`, or
    /// `"not_found"` (the resolved status). Emitted alongside `StatusChanged`
    /// (which carries the full typed status); this is the compact per-book signal
    /// the UI's query progress reflects.
    QueryStage {
        group_path: Vec<usize>,
        book_index: usize,
        title: String,
        stage: String,
    },
    /// The planned destination path for a ready request (before download).
    Planned {
        group_path: Vec<usize>,
        book_index: usize,
        title: String,
        md5: String,
        destination: PathBuf,
    },
    /// A download-engine progress/lifecycle event, forwarded verbatim.
    Download(Progress),
    /// All requested work for a command finished.
    Done,
}

/// The planned download for one ready request: where it goes on disk and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDownload {
    pub group_path: Vec<usize>,
    pub book_index: usize,
    pub title: String,
    pub md5: String,
    pub destination: PathBuf,
}

/// One finished variation whose file is NOT at its current correct destination,
/// plus where to take it from. Computed by `compute_relocations` and shared by the
/// "Reorganize needed?" check and the relocate action so the two never disagree.
struct Relocation {
    group_path: Vec<usize>,
    book_index: usize,
    md5: String,
    /// The file's real current location (recorded `output_path`, or a disk match).
    src: PathBuf,
    /// Where it should be under the current scheme.
    dest: PathBuf,
    /// `true` when `src` belongs to ANOTHER list (duplicate via copy, don't move it).
    is_copy: bool,
}

/// A live download started by [`Orchestrator::begin_download`]: everything the
/// engine needs to drain the transfer **off** the per-list orchestrator lock.
///
/// The begin-phase (under the lock) plans + marks the book `Downloading` + spawns
/// `scheduler.run`; the engine then drains `rx` with NO lock held (applying each
/// `Progress` under a *brief* lock per tick via [`Orchestrator::apply_progress`]),
/// awaits `run`, and calls [`Orchestrator::finish_download`] (brief lock) with the
/// md5s that reached `Done`. So multiple books in a list download concurrently —
/// the per-list lock is never held across the transfer (`docs/SYNCHRONIZATION.md`
/// §4).
pub struct DownloadSession {
    /// Progress stream from the spawned `scheduler.run`. Drained off-lock.
    pub rx: mpsc::Receiver<Progress>,
    /// This book's still-`Pending` planned variations (md5 → destination), used by
    /// `apply_progress` to persist per-variation job state as bytes move.
    pub pending: Vec<PlannedDownload>,
    /// The spawned scheduler run; awaited (off-lock) after the stream closes.
    pub run: tokio::task::JoinHandle<Vec<crate::queue::JobOutcome>>,
    /// The distinct md5s submitted to the scheduler for this book.
    pub md5s: Vec<String>,
}

/// What [`Orchestrator::begin_reverify`] captures (under the lock) so the engine
/// can run a re-verify search OFF the per-list lock, then apply via
/// [`Orchestrator::finish_reverify`] (mirrors the Query begin/finish dance).
pub struct ReverifyPrep {
    pub input: crate::model::BookInput,
    pub settings: crate::model::ListSettings,
    /// The md5 of the book's on-disk (`Done`) copy being verified.
    pub downloaded_md5: String,
    /// Clone of the shared search client, so the search runs with NO lock held.
    pub search: Arc<SearchClient>,
}

/// Drives the parse → query → match → persist → naming → download pipeline for a
/// single persisted list.
pub struct Orchestrator {
    store: Store,
    list_id: i64,
    search: Arc<SearchClient>,
    out_dir: PathBuf,
    /// Bound on concurrent in-flight searches in [`Orchestrator::query_all`].
    query_concurrency: usize,
}

/// Default bound on concurrent searches issued by `query_all`.
const DEFAULT_QUERY_CONCURRENCY: usize = 6;

/// Re-verify flags a downloaded book for review ("Check download") only when the
/// on-disk copy's OWN title is a poor symmetric match to the request title — i.e.
/// it's probably a different/more-specific book (e.g. a #11 volume when the
/// base title was wanted). At or above this, the copy is plausibly the right book
/// (e.g. "The Jungle Book #1" for "The Jungle Book: Mowgli's Story") and we do NOT nag — even
/// if a marginally better-ranked edition exists, or the exact md5 wasn't re-found
/// in the fresh search. This is robust to the search not re-returning the copy.
const REVIEW_TITLE_MATCH_MIN: f32 = 0.3;

/// Whether a downloaded `copy` should be flagged for review against `request_title`.
/// Whether a cover URL is a remote http(s) link (vs a local cached file path).
fn is_http_url(u: &str) -> bool {
    u.starts_with("http://") || u.starts_with("https://")
}

/// One book that still needs its cover localized — produced by
/// [`Orchestrator::cover_targets`] for the off-lock backfill loop.
#[derive(Debug, Clone)]
pub struct CoverTarget {
    pub group_path: Vec<usize>,
    pub book_index: usize,
    pub title: String,
    pub author: String,
    pub isbn: Option<String>,
    /// Stable thumbnail key (the first candidate's md5).
    pub key: String,
    /// An existing remote cover URL (from search), if any — localize this rather
    /// than doing a fresh Open Library lookup.
    pub existing_remote: Option<String>,
    /// The downloaded file on disk (a `Done` variation's `output_path`), if the
    /// book has a copy. Lets the backfill GENERATE a cover locally (epub embedded
    /// image / pdf first page / synthetic) when no online cover is found.
    pub local_file: Option<String>,
}

/// The recommended REPLACEMENT for a book under review: the alternative copy the
/// UI would offer instead of the downloaded one. Mirrors the viewmodel — prefer
/// the best-ranked candidate of the SAME format as the downloaded copy, else the
/// top-ranked candidate; in all cases a copy OTHER than the downloaded one.
/// Returns `None` if there's no alternative. Used to compare against a prior
/// "Accept current copy" decision (`BookRequest::review_dismissed`).
fn recommended_replacement(req: &BookRequest) -> Option<String> {
    let downloaded = req.candidates.iter().find(|c| {
        matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Done))
            && c.job
                .as_ref()
                .and_then(|j| j.output_path.as_ref())
                .is_some()
    });
    let dl_md5 = downloaded.map(|c| c.md5.as_str());
    if let Some(fmt) = downloaded.and_then(|c| c.extension.as_ref()) {
        if let Some(c) = req
            .candidates
            .iter()
            .find(|c| c.extension.as_ref() == Some(fmt) && Some(c.md5.as_str()) != dl_md5)
        {
            return Some(c.md5.clone());
        }
    }
    req.candidates
        .iter()
        .find(|c| Some(c.md5.as_str()) != dl_md5)
        .map(|c| c.md5.clone())
}

/// A short chronicle detail for a just-completed discovery (query + match).
fn discovery_detail(req: &BookRequest) -> String {
    let n = req.candidates.len();
    match &req.status {
        RequestStatus::Matched => {
            let ext = req
                .candidates
                .first()
                .and_then(|c| c.extension.as_ref())
                .map(|e| e.ext())
                .unwrap_or_default();
            format!("{n} candidate(s) → matched (auto-selected {ext})")
        }
        RequestStatus::NeedsSelection => format!("{n} candidate(s) → needs selection"),
        RequestStatus::NotFound => "no candidates found".to_string(),
        _ => format!("{n} candidate(s)"),
    }
}

/// Apply a standing "Accept current copy" decision to a freshly-computed review
/// `flag`. `req.review_dismissed.is_some()` means the user accepted the downloaded
/// copy. Once accepted, keep it accepted (suppress the flag) UNLESS a genuinely
/// better copy exists — a candidate scoring STRICTLY higher than the downloaded
/// one. Equal-or-lower-scoring alternatives (the same book with a different title,
/// a re-ordered duplicate) must not re-ask — matching the recommendation's md5
/// alone was fragile because that md5 churns between identical-score candidates,
/// so repeated accepts never stuck. A strictly-better copy still surfaces.
fn honor_review_after_accept(flag: bool, req: &BookRequest, downloaded: &Candidate) -> bool {
    if !flag || req.review_dismissed.is_none() {
        return flag; // not flagged, or never accepted → unchanged
    }
    let better_exists = req
        .candidates
        .iter()
        .any(|c| c.md5 != downloaded.md5 && c.score > downloaded.score + f32::EPSILON);
    better_exists // suppress (false) unless a strictly higher-scoring copy appeared
}

fn should_flag_review(request_title: &str, copy: &Candidate) -> bool {
    crate::matching::request_title_match(request_title, &copy.title) < REVIEW_TITLE_MATCH_MIN
}

/// Drop any candidate the user has DISMISSED (removed) for this book, so a
/// re-query / re-verify never re-surfaces it.
fn drop_dismissed(candidates: Vec<Candidate>, dismissed: &[String]) -> Vec<Candidate> {
    if dismissed.is_empty() {
        return candidates;
    }
    candidates
        .into_iter()
        .filter(|c| !dismissed.iter().any(|d| d == &c.md5))
        .collect()
}

/// Re-merge a fresh ranked candidate list (from a re-verify search) with the
/// book's `prior` candidates, **carrying every prior job forward**. A re-verify
/// rebuilds `req.candidates` from the fresh search; without this, only the
/// downloaded copy's job was preserved, so a SECOND requested variation
/// (e.g. a queued `epub` sibling of a `Done` `pdf`) lost its `job` and reverted
/// to "available" — the state-loss bug.
///
/// Rules: fresh order wins (fresh scores/titles); a fresh candidate that matches
/// a prior md5 inherits that prior candidate's `job` (Done/Pending/Failed/…);
/// any prior candidate carrying a `job` that the fresh list dropped is appended
/// so it stays tracked/downloadable.
fn merge_preserving_jobs(fresh: Vec<Candidate>, prior: &[Candidate]) -> Vec<Candidate> {
    let mut merged: Vec<Candidate> = Vec::with_capacity(fresh.len() + prior.len());
    let mut seen: HashSet<String> = HashSet::new();
    for mut c in fresh {
        if !seen.insert(c.md5.clone()) {
            continue; // dedupe fresh duplicates
        }
        if let Some(p) = prior.iter().find(|p| p.md5 == c.md5) {
            // Take the fresh score/title, keep the prior job (the requested/Done
            // state — never present on a fresh search candidate).
            c.job = p.job.clone();
        }
        merged.push(c);
    }
    // Any prior REQUESTED variation the fresh list didn't surface must survive,
    // or its queued/in-flight/done job is lost.
    for p in prior {
        if p.job.is_some() && !seen.contains(&p.md5) {
            seen.insert(p.md5.clone());
            merged.push(p.clone());
        }
    }
    merged
}

impl Orchestrator {
    /// Create an orchestrator over an already-open [`Store`], a parsed list, a
    /// search client, and an output directory. The list is inserted into the
    /// store and its assigned id retained.
    pub fn new(
        mut store: Store,
        list: &DownloadList,
        search: SearchClient,
        out_dir: impl Into<PathBuf>,
    ) -> Result<Self> {
        let list_id = store.insert_list(list).context("persisting list")?;
        Ok(Orchestrator {
            store,
            list_id,
            search: Arc::new(search),
            out_dir: out_dir.into(),
            query_concurrency: DEFAULT_QUERY_CONCURRENCY,
        })
    }

    /// Attach to an existing persisted list (resume path) without re-inserting.
    pub fn attach(
        store: Store,
        list_id: i64,
        search: SearchClient,
        out_dir: impl Into<PathBuf>,
    ) -> Self {
        Orchestrator {
            store,
            list_id,
            search: Arc::new(search),
            out_dir: out_dir.into(),
            query_concurrency: DEFAULT_QUERY_CONCURRENCY,
        }
    }

    /// Override the bound on concurrent searches in [`Orchestrator::query_all`].
    pub fn with_query_concurrency(mut self, n: usize) -> Self {
        self.query_concurrency = n.max(1);
        self
    }

    pub fn list_id(&self) -> i64 {
        self.list_id
    }

    /// Reload the current persisted list from the store.
    pub fn snapshot(&self) -> Result<DownloadList> {
        self.store
            .load_list(self.list_id)?
            .context("list vanished from store")
    }

    // -----------------------------------------------------------------------
    // Single-book transition functions (the execution engine's primitives).
    //
    // Each advances ONE book one step toward its goal and persists the result.
    // They are idempotent + monotonic: every step re-reads the current state
    // before committing, so a concurrent command (which may have changed the
    // goal or the book) is safe. Network I/O happens inside (the caller holds
    // only the per-orchestrator lock, never the library lock).
    // -----------------------------------------------------------------------

    /// Advance ONE book through discovery: `New`(`Queued`) → `Querying` →
    /// `Matched`/`NeedsSelection`/`NotFound`. Re-runs search + match for the book
    /// at `(group_path, book_index)`, persisting the new status + candidates and
    /// (on `Matched`) auto-selecting + auto-requesting the best variation, exactly
    /// as the whole-list `query_all` pass does per book. Emits a
    /// `QueryStage{querying}` before the search and a `QueryStage{resolved}` +
    /// `StatusChanged` after.
    ///
    /// Idempotent: a no-op (returns `Ok(false)`) if the book is no longer
    /// `Queued` when the step starts (another pass already advanced it). Returns
    /// `Ok(true)` when it performed the transition.
    pub async fn query_one(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        events: &mpsc::Sender<Event>,
    ) -> Result<bool> {
        // Re-read: only act if still queued (monotonic — don't re-query a book a
        // command already moved past).
        let input = match self.request_at(group_path, book_index)? {
            Some(req) if req.status == RequestStatus::Queued => req.input.clone(),
            _ => return Ok(false),
        };
        let settings = self.snapshot()?.settings;

        // Mark Querying (persist + emit) so a mid-flight snapshot reflects it.
        self.mark_querying(group_path, book_index, events).await?;

        // Network: search off the library lock (caller holds only this orch).
        let candidates = self.search.search(&input).await.unwrap_or_default();

        // Re-read before committing (the book/goal may have changed underneath).
        let mut req = match self.request_at(group_path, book_index)? {
            Some(r) => r,
            None => return Ok(false),
        };
        // If a command rewound/advanced it out of Querying, don't clobber.
        if req.status != RequestStatus::Querying {
            return Ok(false);
        }
        let candidates = drop_dismissed(candidates, &req.dismissed);
        let outcome = matching::evaluate(&req.input, candidates, &settings);
        req.candidates = outcome.ranked;
        req.status = outcome.status.clone();
        if req.status == RequestStatus::Matched {
            req.selected = req.candidates.first().map(|c| c.md5.clone());
            if let Some(best) = req.candidates.first_mut() {
                request_job(best);
            }
        }
        req.log_event(None, None, "discovered", discovery_detail(&req));
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;

        let _ = events
            .send(Event::QueryStage {
                group_path: group_path.to_vec(),
                book_index,
                title: req.input.title.clone(),
                stage: query_stage_str(&req.status).to_string(),
            })
            .await;
        let _ = events
            .send(Event::StatusChanged {
                group_path: group_path.to_vec(),
                book_index,
                title: req.input.title.clone(),
                status: req.status,
            })
            .await;
        Ok(true)
    }

    /// Clone of the shared search client. Lets the engine run a book's network
    /// search WITHOUT holding this orchestrator's lock — the lock guards the
    /// store/state only, never network I/O (see `docs/EXECUTION_MODEL.md` §sync).
    pub fn search_client(&self) -> Arc<SearchClient> {
        Arc::clone(&self.search)
    }

    /// Lock-free query, phase 1: if the book is still `Queued`, mark it
    /// `Querying` (persist + emit) and return `(input, settings)` for the caller
    /// to run the search OFF-lock. `None` if a command already moved it. The
    /// caller then runs `search_client().search(&input)` +
    /// `matching::evaluate(&input, cands, &settings)` with NO lock held and
    /// applies the result via [`finish_query`]. This is what lets many books in
    /// one list query concurrently (the per-list lock is held only for these two
    /// brief state writes, not across the network).
    pub async fn begin_query(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        events: &mpsc::Sender<Event>,
    ) -> Result<Option<(crate::model::BookInput, crate::model::ListSettings)>> {
        let input = match self.request_at(group_path, book_index)? {
            Some(req) if req.status == RequestStatus::Queued => req.input.clone(),
            _ => return Ok(None),
        };
        let settings = self.snapshot()?.settings;
        self.mark_querying(group_path, book_index, events).await?;
        Ok(Some((input, settings)))
    }

    /// Lock-free query, phase 2: apply the off-lock match `outcome`. Re-reads
    /// first (monotonic — skips if a command moved the book out of `Querying`).
    pub async fn finish_query(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        outcome: crate::matching::MatchOutcome,
        events: &mpsc::Sender<Event>,
    ) -> Result<bool> {
        let mut req = match self.request_at(group_path, book_index)? {
            Some(r) if r.status == RequestStatus::Querying => r,
            _ => return Ok(false),
        };
        req.candidates = drop_dismissed(outcome.ranked, &req.dismissed);
        // Filtering dismissed copies can leave nothing to act on → not found.
        req.status = if req.candidates.is_empty()
            && matches!(
                outcome.status,
                RequestStatus::Matched | RequestStatus::NeedsSelection
            ) {
            RequestStatus::NotFound
        } else {
            outcome.status.clone()
        };
        if req.status == RequestStatus::Matched {
            req.selected = req.candidates.first().map(|c| c.md5.clone());
            if let Some(best) = req.candidates.first_mut() {
                request_job(best);
            }
        }
        req.log_event(None, None, "discovered", discovery_detail(&req));
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        let _ = events
            .send(Event::QueryStage {
                group_path: group_path.to_vec(),
                book_index,
                title: req.input.title.clone(),
                stage: query_stage_str(&req.status).to_string(),
            })
            .await;
        let _ = events
            .send(Event::StatusChanged {
                group_path: group_path.to_vec(),
                book_index,
                title: req.input.title.clone(),
                status: req.status,
            })
            .await;
        Ok(true)
    }

    /// Drive ONE book's chosen variation(s) through the scheduler:
    /// `Matched`/`Ready` → `Downloading` → `Done`. Plans the whole list (to assign
    /// stable sequence numbers + destination paths), then downloads only THIS
    /// book's still-`Pending` variations, persisting per-variation job progress and
    /// emitting `Download`/`Planned` events as bytes move. After the run it fires
    /// the trash-on-replace hook for any md5 that finished (a no-op unless the book
    /// has a pending replacement).
    ///
    /// Returns `Ok(true)` if it submitted any download, `Ok(false)` if the book had
    /// nothing pending (already downloaded / not requested / blocked).
    ///
    /// Thin wrapper around the decomposed [`begin_download`](Self::begin_download) →
    /// drain → [`finish_download`](Self::finish_download) phases, holding this
    /// orchestrator's lock across the WHOLE transfer (it has `&mut self`). Kept for
    /// existing callers/tests; the **engine** uses the decomposed off-lock path so
    /// books in one list download concurrently (`docs/SYNCHRONIZATION.md` §4).
    pub async fn download_one(
        &mut self,
        scheduler: &Arc<Scheduler>,
        group_path: &[usize],
        book_index: usize,
        events: &mpsc::Sender<Event>,
    ) -> Result<bool> {
        let session = match self
            .begin_download(scheduler, group_path, book_index, None, events)
            .await?
        {
            Some(s) => s,
            None => return Ok(false),
        };
        let DownloadSession {
            mut rx,
            pending,
            run,
            ..
        } = session;
        let mut completed: Vec<String> = Vec::new();
        while let Some(prog) = rx.recv().await {
            if let Progress::Done { md5, .. } = &prog {
                completed.push(md5.clone());
            }
            self.apply_progress(&pending, &prog)?;
            let _ = events.send(Event::Download(prog)).await;
        }
        let _ = run.await;
        self.finish_download(group_path, book_index, &completed)
            .await?;
        Ok(true)
    }

    /// Lock-free download, phase 1 (`docs/SYNCHRONIZATION.md` §4): under the
    /// per-list lock ONLY, plan the list, restrict to THIS book's still-`Pending`
    /// variations, emit `Planned`, mark the book `Downloading`, build one
    /// [`DownloadRequest`] per distinct md5 (carrying `resume_offset` +
    /// `expected_size`), and spawn `scheduler.run`. Returns a [`DownloadSession`]
    /// the engine drains OFF-lock (`rx` + `apply_progress` per tick + `run`), or
    /// `None` if nothing is pending. **No network happens here** — the spawned run
    /// streams off the lock once this returns.
    pub async fn begin_download(
        &mut self,
        scheduler: &Arc<Scheduler>,
        group_path: &[usize],
        book_index: usize,
        only_md5: Option<&str>,
        events: &mpsc::Sender<Event>,
    ) -> Result<Option<DownloadSession>> {
        // Plan the whole list so sequence numbers stay stable + every requested
        // variation gets a destination (cheap; no network).
        let planned = self.plan_downloads()?;
        // Restrict to THIS book's still-`Pending` variations — and, when the engine
        // dispatches per VARIATION (so a book's copies download in parallel), to the
        // ONE requested md5, so two concurrent workers never submit the same md5.
        let pending: Vec<PlannedDownload> = {
            let list = self.snapshot()?;
            planned
                .into_iter()
                .filter(|p| p.group_path == group_path && p.book_index == book_index)
                .filter(|p| only_md5.is_none_or(|m| p.md5 == m))
                .filter(|p| {
                    request_at_in(&list, &p.group_path, p.book_index)
                        .and_then(|req| req.candidates.iter().find(|c| c.md5 == p.md5))
                        .and_then(|c| c.job.as_ref())
                        .map(|j| j.state == JobState::Pending)
                        .unwrap_or(false)
                })
                .collect()
        };
        if pending.is_empty() {
            return Ok(None);
        }

        for p in &pending {
            // If a prior naming scheme left a partial at a different path, adopt it
            // so we resume rather than restart (numbered folders / source-order seq
            // changed the dest out from under an in-progress download).
            adopt_orphaned_part(&p.destination, &self.out_dir);
            let _ = events
                .send(Event::Planned {
                    group_path: p.group_path.clone(),
                    book_index: p.book_index,
                    title: p.title.clone(),
                    md5: p.md5.clone(),
                    destination: p.destination.clone(),
                })
                .await;
        }

        // Build one DownloadRequest per distinct md5 (dedupe), carrying any
        // persisted resume_offset. The book is NOT marked `Downloading` here:
        // the variations stay `Pending` (queued) and only flip to `Downloading`
        // when the scheduler actually acquires a host slot for them (honest
        // queued→downloading transition — see docs/DOWNLOAD_SCHEDULING.md §10).
        let mut seen_md5: HashSet<String> = HashSet::new();
        let mut requests = Vec::new();
        let mut md5s = Vec::new();
        {
            let list = self.snapshot()?;
            for p in &pending {
                if !seen_md5.insert(p.md5.clone()) {
                    continue;
                }
                let cand = request_at_in(&list, &p.group_path, p.book_index)
                    .and_then(|req| req.candidates.iter().find(|c| c.md5 == p.md5));
                let resume_offset = cand
                    .and_then(|c| c.job.as_ref())
                    .map(|j| j.resume_offset)
                    .unwrap_or(0);
                let mut dr = DownloadRequest::new(p.md5.clone(), p.destination.clone());
                dr.resume_offset = resume_offset;
                dr.expected_size = cand.and_then(|c| c.size_bytes);
                requests.push(dr);
                md5s.push(p.md5.clone());
            }
        }

        let (tx, rx) = mpsc::channel::<Progress>(1024);
        let sched = Arc::clone(scheduler);
        let run = tokio::spawn(async move { sched.run(requests, tx).await });

        Ok(Some(DownloadSession {
            rx,
            pending,
            run,
            md5s,
        }))
    }

    /// Lock-free download, phase 3 (`docs/SYNCHRONIZATION.md` §4): under the
    /// per-list lock ONLY, settle a finished download. Fires the trash-on-replace
    /// hook for each md5 that reached `Done` (a no-op unless the book has a pending
    /// replacement). The per-variation terminal job state was already persisted by
    /// the engine's per-tick [`apply_progress`](Self::apply_progress), so this only
    /// performs the post-transfer settle.
    pub async fn finish_download(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        completed: &[String],
    ) -> Result<()> {
        // Trash-on-replace: if a finished md5 is the recommended copy of this book,
        // move the old file to Trash + clear review/pending. No-op otherwise.
        for md5 in completed {
            let _ = self.trash_after_replace_done(group_path, book_index, md5);
        }
        Ok(())
    }

    /// Re-verify ONE downloaded (`Done`) book against the CURRENT search + match
    /// algorithm: re-search, compare the fresh top candidate to the on-disk copy,
    /// set `review`/recommended on mismatch (file kept). Single-book form of
    /// [`Orchestrator::reverify_downloads`]. Returns `Ok(true)` if it flagged the
    /// book for review, `Ok(false)` otherwise (incl. when the book has no
    /// downloaded copy to verify).
    ///
    /// Thin wrapper around the decomposed [`begin_reverify`](Self::begin_reverify) →
    /// search OFF-lock → [`finish_reverify`](Self::finish_reverify) phases, holding
    /// this orchestrator's lock across the WHOLE search (it has `&mut self`). Kept
    /// for existing callers/tests; the **engine** uses the decomposed off-lock path
    /// so re-verifies in one list run concurrently (`docs/SYNCHRONIZATION.md` §4).
    pub async fn reverify_one(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        events: &mpsc::Sender<Event>,
    ) -> Result<bool> {
        let prep = match self.begin_reverify(group_path, book_index)? {
            Some(p) => p,
            None => return Ok(false),
        };
        let fresh = prep.search.search(&prep.input).await.unwrap_or_default();
        let outcome = matching::evaluate(&prep.input, fresh, &prep.settings);
        self.finish_reverify(
            group_path,
            book_index,
            outcome,
            &prep.downloaded_md5,
            events,
        )
        .await
    }

    /// Lock-free reverify, phase 1 (`docs/SYNCHRONIZATION.md` §4, mirrors
    /// [`begin_query`](Self::begin_query)): if the book has a downloaded copy,
    /// capture `(input, settings, downloaded_md5, search_arc)` so the caller can run
    /// `search.search(&input)` + `matching::evaluate(&input, fresh, &settings)` with
    /// NO lock held, then apply via [`finish_reverify`](Self::finish_reverify).
    /// `None` if the book has no on-disk copy to verify.
    pub fn begin_reverify(
        &self,
        group_path: &[usize],
        book_index: usize,
    ) -> Result<Option<ReverifyPrep>> {
        let req = match self.request_at(group_path, book_index)? {
            Some(req) => req,
            None => return Ok(None),
        };
        let downloaded_md5 = match downloaded_md5(&req) {
            Some(m) => m,
            None => return Ok(None),
        };
        let settings = self.snapshot()?.settings;
        Ok(Some(ReverifyPrep {
            input: req.input.clone(),
            settings,
            downloaded_md5,
            search: Arc::clone(&self.search),
        }))
    }

    /// Lock-free reverify, phase 2 (`docs/SYNCHRONIZATION.md` §4): apply the
    /// off-lock match `outcome` against the retained downloaded copy. Re-reads the
    /// book first (monotonic — the downloaded variation may have changed), sets
    /// `review` IFF the fresh top is a different md5, and merges the fresh ranked
    /// list while preserving the downloaded variation's `Done` job + `output_path`.
    /// Returns `Ok(true)` if it flagged the book for review.
    pub async fn finish_reverify(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        outcome: crate::matching::MatchOutcome,
        downloaded_md5: &str,
        events: &mpsc::Sender<Event>,
    ) -> Result<bool> {
        let mut req = match self.request_at(group_path, book_index)? {
            Some(r) => r,
            None => return Ok(false),
        };
        let downloaded = match req
            .candidates
            .iter()
            .find(|c| {
                c.md5 == downloaded_md5
                    && matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Done))
                    && c.job
                        .as_ref()
                        .and_then(|j| j.output_path.as_ref())
                        .is_some()
            })
            .cloned()
        {
            Some(c) => c,
            // The downloaded copy changed/vanished underneath (a command moved the
            // book) — drop the stale result, the engine re-plans.
            None => return Ok(false),
        };

        // The downloaded copy being the TOP-ranked result means it's the best
        // available match — accept it as done, never flag "check download" (even
        // if the loose title heuristic would). Otherwise fall back to the heuristic.
        let is_top_pick = outcome
            .ranked
            .first()
            .map(|c| c.md5 == downloaded.md5)
            .unwrap_or(false);
        let mut review = !is_top_pick && should_flag_review(&req.input.title, &downloaded);

        // Merge fresh ranked list (fresh scores/titles win) while carrying EVERY
        // prior job forward — the downloaded copy's Done job AND any other
        // requested variation (e.g. a queued epub sibling). Preserving only the
        // downloaded job here dropped second variations (state-loss bug).
        let merged = merge_preserving_jobs(outcome.ranked, &req.candidates);
        req.candidates = drop_dismissed(merged, &req.dismissed);
        // Honor a prior "Accept current copy". `review_dismissed` set = the user
        // accepted the downloaded copy. Keep it accepted UNLESS a genuinely better
        // copy now exists — one scoring strictly higher than the downloaded one.
        // Equal-score alternatives (the SAME book with a cleaner/messier title, or
        // a re-ordered duplicate) must NOT re-ask: matching on the recommendation's
        // md5 alone was fragile (the "recommended" md5 churns between identical-
        // score candidates), which is why repeated accepts never stuck.
        let flagged = review;
        review = honor_review_after_accept(review, &req, &downloaded);
        if !flagged {
            req.review_dismissed = None;
        }
        req.review = review;
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;

        let _ = events
            .send(Event::StatusChanged {
                group_path: group_path.to_vec(),
                book_index,
                title: req.input.title.clone(),
                status: req.status.clone(),
            })
            .await;
        Ok(review)
    }

    /// Run query + match for every queued request, persisting candidates and the
    /// resulting status. Emits a `StatusChanged` per request and a final `Done`.
    pub async fn query_all(&mut self, events: &mpsc::Sender<Event>) -> Result<()> {
        let list = self.snapshot()?;
        let settings = list.settings.clone();

        // Walk the tree, collecting positions to query (those still `Queued`),
        // together with the input each needs to search.
        let mut work: Vec<(Vec<usize>, usize, crate::model::BookInput)> = Vec::new();
        walk(&list.groups, &mut Vec::new(), &mut |path, bi, req| {
            if req.status == RequestStatus::Queued {
                work.push((path.to_vec(), bi, req.input.clone()));
            }
        });

        // Run the searches concurrently against the shared (read-only) client,
        // bounded by `query_concurrency`. Each task is independent; results come
        // back in completion order (which may differ from declaration order —
        // fine, persistence is keyed by position). Persisting + emitting happens
        // sequentially on this task as results arrive (the store needs `&mut`).
        //
        // Before a book's search is actually dispatched it is marked `Querying`
        // ("being queried") and persisted, so a mid-pass snapshot — and the
        // `QueryStage` event — reflect in-flight progress; books still in the pool
        // beyond the concurrency window stay `Queued` ("waiting to be queried").
        use futures::stream::{FuturesUnordered, StreamExt};
        let mut futs = FuturesUnordered::new();
        let mut iter = work.into_iter();
        // Prime the pool up to the concurrency bound.
        for _ in 0..self.query_concurrency {
            match iter.next() {
                Some((gp, bi, input)) => {
                    self.mark_querying(&gp, bi, events).await?;
                    futs.push(search_one(Arc::clone(&self.search), gp, bi, input));
                }
                None => break,
            }
        }

        while let Some((group_path, book_index, candidates)) = futs.next().await {
            // Refill the pool to keep `query_concurrency` searches in flight,
            // marking the next book `Querying` as it is dispatched.
            if let Some((gp, bi, input)) = iter.next() {
                self.mark_querying(&gp, bi, events).await?;
                futs.push(search_one(Arc::clone(&self.search), gp, bi, input));
            }

            let mut req = match self.request_at(&group_path, book_index)? {
                Some(r) => r,
                None => continue,
            };
            let candidates = drop_dismissed(candidates, &req.dismissed);
            let outcome = matching::evaluate(&req.input, candidates, &settings);

            req.candidates = outcome.ranked;
            req.status = outcome.status.clone();
            // Auto-matched requests pre-select their top candidate (back-compat)
            // AND auto-request the best variation for download, so the default is
            // "download one best copy". NeedsSelection / NotFound request nothing.
            if req.status == RequestStatus::Matched {
                req.selected = req.candidates.first().map(|c| c.md5.clone());
                if let Some(best) = req.candidates.first_mut() {
                    request_job(best);
                }
            }

            req.log_event(None, None, "discovered", discovery_detail(&req));
            self.store
                .update_request(self.list_id, &group_path, book_index, &req)?;

            let _ = events
                .send(Event::QueryStage {
                    group_path: group_path.clone(),
                    book_index,
                    title: req.input.title.clone(),
                    stage: query_stage_str(&req.status).to_string(),
                })
                .await;
            let _ = events
                .send(Event::StatusChanged {
                    group_path,
                    book_index,
                    title: req.input.title.clone(),
                    status: req.status,
                })
                .await;
        }
        let _ = events.send(Event::Done).await;
        Ok(())
    }

    /// Mark the request at a position `Querying` ("search in flight"), persist it,
    /// and emit a `QueryStage{stage:"querying"}` event so a front end can show the
    /// book being queried before its result lands.
    async fn mark_querying(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        events: &mpsc::Sender<Event>,
    ) -> Result<()> {
        if let Some(mut req) = self.request_at(group_path, book_index)? {
            req.status = RequestStatus::Querying;
            self.store
                .update_request(self.list_id, group_path, book_index, &req)?;
            let _ = events
                .send(Event::QueryStage {
                    group_path: group_path.to_vec(),
                    book_index,
                    title: req.input.title.clone(),
                    stage: query_stage_str(&RequestStatus::Querying).to_string(),
                })
                .await;
        }
        Ok(())
    }

    /// Replace the list's preferred-format order (most-preferred first) and
    /// persist it. Affects future matching/keep decisions; existing requests are
    /// unchanged until re-queried.
    pub fn set_format_pref(&mut self, formats: Vec<Format>) -> Result<()> {
        let mut list = self.snapshot()?;
        list.settings.format_pref = formats;
        self.store.update_settings(self.list_id, &list.settings)?;
        Ok(())
    }

    /// Replace the whole per-list [`ListSettings`] (format pref, match
    /// thresholds, naming template, `keep_top`, sequence scope, language) and
    /// persist it. Affects future matching/keep/naming; existing requests are
    /// unchanged until re-queried/re-planned.
    pub fn update_settings(&mut self, settings: crate::model::ListSettings) -> Result<()> {
        self.store.update_settings(self.list_id, &settings)?;
        Ok(())
    }

    /// Select a candidate by md5 for a request, transitioning it to `Ready` so
    /// the next download pass fetches it. Works from ANY status — including
    /// re-selecting a different variation after a book is already `Done` (the
    /// user found the copy unsatisfactory). When the md5 changes, the previous
    /// download job is cleared so the new variation downloads fresh rather than
    /// resuming the wrong file.
    pub fn select_candidate(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
    ) -> Result<()> {
        let mut req = self
            .request_at(group_path, book_index)?
            .context("request not found")?;
        anyhow::ensure!(
            req.candidates.iter().any(|c| c.md5 == md5),
            "md5 {md5} is not among this request's candidates"
        );
        let changed = req.selected.as_deref() != Some(md5);
        req.selected = Some(md5.to_string());
        req.status = RequestStatus::Ready;
        if changed {
            // Different file -> don't resume the old one; start clean.
            req.job = None;
        }
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        Ok(())
    }

    /// Re-queue a request (clears candidates/selection, status back to `Queued`).
    pub fn retry(&mut self, group_path: &[usize], book_index: usize) -> Result<()> {
        let mut req = self
            .request_at(group_path, book_index)?
            .context("request not found")?;
        req.status = RequestStatus::Queued;
        req.selected = None;
        req.candidates.clear();
        req.job = None;
        // Re-arm the goal so the engine actually re-discovers + downloads it. Retry
        // is most often clicked AFTER a relaunch (which parks every goal at Idle),
        // so without this the reset book would just sit Queued with nothing acting
        // on it — the "Retry does nothing" symptom.
        req.goal = crate::model::Goal::Complete;
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        Ok(())
    }

    /// Correct a book's search metadata (title/authors) and re-query it from
    /// scratch. For a book the search couldn't match under its imported title
    /// (e.g. "D'Aulaires' Book of Greek Myths" that only resolves as "Greek Myths
    /// D'Aulaires'"): the user edits the title/author, and this re-runs discovery
    /// with the corrected input. Resets like [`retry`] (clears candidates/selection/
    /// job, status → Queued, goal → Complete) so the engine re-discovers it.
    /// Empty `title` is rejected (a book must keep a searchable title).
    pub fn edit_book_input(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        title: &str,
        authors: Vec<String>,
    ) -> Result<()> {
        let title = title.trim();
        anyhow::ensure!(!title.is_empty(), "a book title cannot be empty");
        let mut req = self
            .request_at(group_path, book_index)?
            .context("request not found")?;
        req.input.title = title.to_string();
        req.input.authors = authors
            .into_iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
        // Re-query from scratch with the corrected metadata.
        req.status = RequestStatus::Queued;
        req.selected = None;
        req.candidates.clear();
        req.job = None;
        req.goal = crate::model::Goal::Complete;
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        tracing::info!(
            title = %req.input.title,
            authors = ?req.input.authors,
            "book metadata edited — re-querying"
        );
        Ok(())
    }

    /// Set the execution `goal` for EVERY book in the list and persist it. The
    /// engine reads each book's goal when planning actionable work; this is how
    /// the per-list Start (`Complete`) / Stop (`Idle`) commands express intent.
    /// Returns how many books were updated. A brief, network-free operation.
    pub fn set_goal_all(&mut self, goal: crate::model::Goal) -> Result<usize> {
        let list = self.snapshot()?;
        let mut positions: Vec<(Vec<usize>, usize)> = Vec::new();
        walk(&list.groups, &mut Vec::new(), &mut |path, bi, _req| {
            positions.push((path.to_vec(), bi));
        });
        let n = positions.len();
        for (gp, bi) in positions {
            if let Some(mut req) = self.request_at(&gp, bi)? {
                if req.goal != goal {
                    req.goal = goal;
                    self.store.update_request(self.list_id, &gp, bi, &req)?;
                }
            }
        }
        Ok(n)
    }

    /// Set the execution `goal` for ONE book and persist it. Used by the engine to
    /// settle a book that has reached its goal (e.g. lowering a re-verified `Done`
    /// book to `Match` so it isn't re-verified every tick). Idempotent.
    pub fn set_goal_one(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        goal: crate::model::Goal,
    ) -> Result<()> {
        if let Some(mut req) = self.request_at(group_path, book_index)? {
            if req.goal != goal {
                req.goal = goal;
                self.store
                    .update_request(self.list_id, group_path, book_index, &req)?;
            }
        }
        Ok(())
    }

    /// Mark a book as `NotFound` by user choice (the "this isn't available, give
    /// up" action on a `NeedsSelection` book). Clears candidates/selection and
    /// parks the goal at `Idle` so the engine won't keep retrying. The book then
    /// shows under "Cannot download". A later Re-query can still resurrect it.
    pub fn mark_not_found(&mut self, group_path: &[usize], book_index: usize) -> Result<()> {
        if let Some(mut req) = self.request_at(group_path, book_index)? {
            req.status = RequestStatus::NotFound;
            req.goal = crate::model::Goal::Idle;
            req.selected = None;
            req.candidates.clear();
            req.job = None;
            self.store
                .update_request(self.list_id, group_path, book_index, &req)?;
        }
        Ok(())
    }

    /// Re-query reset (the discovery half of the UI's Re-query, sans the search):
    /// for every NON-`Done` book that isn't actively downloading / on disk, reset
    /// its status to `Queued` and clear stale candidates/selection/job so the
    /// engine re-discovers it with the current algorithm. `Done` books are left
    /// intact (the engine re-verifies them separately). This is exactly
    /// [`Orchestrator::requery_unsettled`] expressed for the goal-driven engine;
    /// the command then sets `goal = Complete` for the whole list and notifies the
    /// engine, which performs the actual network passes. Returns how many were
    /// reset.
    pub fn requery_reset(&mut self) -> Result<usize> {
        self.requery_unsettled()
    }

    /// Reset every UNSETTLED book back to `Queued` (clearing its stale
    /// candidates/selection) so a following [`query_all`] re-discovers it with
    /// the current search + matching algorithm. A book is "settled" — and so left
    /// untouched — only if a variation is actually IN FLIGHT or already on disk
    /// (`Downloading`/`Verifying`/`Done`/`Paused`). A merely `Pending` job (the
    /// auto-matched best copy that is queued but not yet started) has used no
    /// bandwidth, so it IS re-queried — that's how re-query refreshes books that
    /// were matched by an older/worse algorithm. This is the engine half of the
    /// UI's "Re-query" action. Returns how many books were reset.
    pub fn requery_unsettled(&mut self) -> Result<usize> {
        let list = self.snapshot()?;
        let mut positions: Vec<(Vec<usize>, usize)> = Vec::new();
        walk(&list.groups, &mut Vec::new(), &mut |path, bi, req| {
            let in_flight_or_done = req.candidates.iter().any(|c| {
                matches!(
                    &c.job,
                    Some(j) if matches!(
                        j.state,
                        JobState::Downloading
                            | JobState::Verifying
                            | JobState::Done
                            | JobState::Paused
                    )
                )
            });
            // Re-query everything that isn't actively downloading / downloaded
            // (and isn't already queued waiting for its first query).
            if !in_flight_or_done && req.status != RequestStatus::Queued {
                positions.push((path.to_vec(), bi));
            }
        });
        let n = positions.len();
        for (gp, bi) in positions {
            let mut req = self.request_at(&gp, bi)?.context("request vanished")?;
            req.status = RequestStatus::Queued;
            req.selected = None;
            req.candidates.clear();
            req.job = None;
            self.store.update_request(self.list_id, &gp, bi, &req)?;
        }
        Ok(n)
    }

    /// Re-verify every DOWNLOADED book against the CURRENT search + matching
    /// algorithm, flagging any whose on-disk copy is no longer the best match so
    /// the user can replace it (the "verify downloaded books" half of Re-query).
    ///
    /// For each book that has a `Done` variation (a candidate whose job state is
    /// `Done`, with its `output_path`):
    ///   1. Re-run the SAME search + [`matching::evaluate`] the query pass uses to
    ///      get fresh ranked candidates. The downloaded file is NEVER touched.
    ///   2. Merge: KEEP the downloaded candidate (preserving its `Done` job +
    ///      `output_path`) in the candidate set even if it is absent from / not at
    ///      the top of the fresh results; refresh the OTHER variations from the
    ///      fresh ranked list (dedupe by md5; fresh scores/titles win).
    ///   3. Set [`BookRequest::review`] = true IFF the downloaded md5 is NOT the
    ///      top-ranked fresh candidate (a better match exists); else `false`. The
    ///      book stays `Done`.
    ///
    /// Returns how many books were flagged for review.
    pub async fn reverify_downloads(&mut self, events: &mpsc::Sender<Event>) -> Result<usize> {
        let list = self.snapshot()?;
        let settings = list.settings.clone();

        // Collect the downloaded books to re-verify: those with a Done variation.
        let mut work: Vec<(Vec<usize>, usize, crate::model::BookInput)> = Vec::new();
        walk(&list.groups, &mut Vec::new(), &mut |path, bi, req| {
            if downloaded_md5(req).is_some() {
                work.push((path.to_vec(), bi, req.input.clone()));
            }
        });

        let mut flagged = 0usize;
        for (group_path, book_index, input) in work {
            // Fresh search + match (reusing the same client + evaluate as queries).
            let fresh = self.search.search(&input).await.unwrap_or_default();
            let outcome = matching::evaluate(&input, fresh, &settings);

            let mut req = match self.request_at(&group_path, book_index)? {
                Some(r) => r,
                None => continue,
            };
            // The downloaded variation (its Done job + output_path are preserved).
            let downloaded = match req
                .candidates
                .iter()
                .find(|c| {
                    matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Done))
                        && c.job
                            .as_ref()
                            .and_then(|j| j.output_path.as_ref())
                            .is_some()
                })
                .cloned()
            {
                Some(c) => c,
                None => continue,
            };

            // Top-ranked downloaded copy = best available match → accept as done,
            // never flag for review.
            let is_top_pick = outcome
                .ranked
                .first()
                .map(|c| c.md5 == downloaded.md5)
                .unwrap_or(false);
            let mut review = !is_top_pick && should_flag_review(&req.input.title, &downloaded);

            // Merge fresh ranked list (fresh scores/titles win) while carrying
            // EVERY prior job forward — the downloaded copy's Done job + any OTHER
            // requested variation (e.g. a queued epub sibling). Preserving only the
            // downloaded job here dropped second variations (state-loss bug).
            let merged = merge_preserving_jobs(outcome.ranked, &req.candidates);
            req.candidates = drop_dismissed(merged, &req.dismissed);
            // Honor a prior "Accept current copy" (see finish_reverify): once
            // accepted, stay accepted unless a strictly higher-scoring copy exists.
            let was_flagged = review;
            review = honor_review_after_accept(review, &req, &downloaded);
            if !was_flagged {
                req.review_dismissed = None;
            }
            req.review = review;
            // Leave status Done (roll-up already reflects the Done variation).
            self.store
                .update_request(self.list_id, &group_path, book_index, &req)?;

            if review {
                flagged += 1;
            }
            let _ = events
                .send(Event::StatusChanged {
                    group_path,
                    book_index,
                    title: req.input.title.clone(),
                    status: req.status.clone(),
                })
                .await;
        }
        Ok(flagged)
    }

    /// Replace a downloaded book's copy with the recommended one: request the
    /// `recommended_md5` variation for download (Pending, so the next
    /// `start_downloads` fetches it) AND record [`BookRequest::trash_on_replace`]
    /// = `{ old_md5, old_path }` from the CURRENT `Done` variation, so that once
    /// the new copy finishes its old file is moved to Trash (see
    /// [`Orchestrator::trash_after_replace_done`]). Persisted immediately.
    ///
    /// Errors if `recommended_md5` is not among this book's candidates, or the
    /// book has no downloaded (`Done`) variation with an `output_path` to replace.
    pub fn replace_download(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        recommended_md5: &str,
    ) -> Result<()> {
        let mut req = self
            .request_at(group_path, book_index)?
            .context("request not found")?;
        anyhow::ensure!(
            req.candidates.iter().any(|c| c.md5 == recommended_md5),
            "md5 {recommended_md5} is not among this request's candidates"
        );
        // The currently-downloaded variation (the one we will trash on success).
        let (old_md5, old_path) = req
            .candidates
            .iter()
            .find_map(|c| {
                let j = c.job.as_ref()?;
                if matches!(j.state, JobState::Done) {
                    j.output_path.as_ref().map(|p| (c.md5.clone(), p.clone()))
                } else {
                    None
                }
            })
            .context("book has no downloaded copy to replace")?;
        anyhow::ensure!(
            old_md5 != recommended_md5,
            "the recommended copy is already the downloaded one"
        );

        // Enrol the recommended variation for download (fresh Pending job).
        if let Some(cand) = req.candidates.iter_mut().find(|c| c.md5 == recommended_md5) {
            request_job(cand);
        }
        req.trash_on_replace = Some(crate::model::TrashPending { old_md5, old_path });
        // The user has acted on the recommendation, so the book leaves the
        // "Check download" (review) state immediately — it's now re-acquiring the
        // recommended copy (the old file is queued for Trash once that succeeds).
        req.review = false;
        req.review_dismissed = None; // a new copy supersedes any prior accept decision
                                     // The new Pending job rolls the status up to a queued/downloading state.
        req.status = roll_up_status(&req);
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        Ok(())
    }

    /// **Accept the currently-downloaded copy**: clear the `review` ("check
    /// download") flag so the book reads as a settled `Done` without replacing its
    /// file. The user's way of saying "this copy is fine" when the heuristic
    /// flagged a possible better match. No-op if the book wasn't under review.
    pub fn accept_review(&mut self, group_path: &[usize], book_index: usize) -> Result<()> {
        let mut req = self
            .request_at(group_path, book_index)?
            .context("request not found")?;
        req.review = false;
        // Remember WHICH recommended replacement the user declined, so a later
        // re-verify suppresses the flag only while it keeps recommending this same
        // copy — a genuinely different (newly-better) recommendation will surface
        // again. `None` if there was no alternative to decline.
        req.review_dismissed = recommended_replacement(&req);
        req.log_event(
            req.selected.clone(),
            None,
            "accepted",
            "user accepted the current downloaded copy",
        );
        // Settle the book so the engine doesn't re-verify it every tick and
        // re-raise the flag: a `Done` book at goal `Complete` is actionable as a
        // Reverify (engine.rs `actionable_kind`), which would re-run the heuristic
        // and undo this accept. Lowering the goal to `Match` (the same thing the
        // post-reverify path does) stops that — the copy is accepted for good.
        req.goal = crate::model::Goal::Match;
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        Ok(())
    }

    /// Manually remove a DOWNLOADED variation: move its file to the macOS Trash
    /// (never a hard delete; a missing file is tolerated), clear that candidate's
    /// download job so it no longer reads as on-disk, and RE-EVALUATE the book's
    /// status from what remains: another variation still downloaded → `Done`; a
    /// variation queued/in-flight → `Downloading`; candidates remain but none
    /// acquired → `NeedsSelection` (user chooses); no candidates left → `Queued`
    /// (re-discover). Also clears `review`/`trash_on_replace` if they referenced
    /// this copy.
    /// Errors if `md5` isn't among the book's candidates.
    pub fn remove_variation(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
    ) -> Result<()> {
        let mut req = self
            .request_at(group_path, book_index)?
            .context("request not found")?;
        let cand = req
            .candidates
            .iter_mut()
            .find(|c| c.md5 == md5)
            .with_context(|| format!("md5 {md5} is not among this request's candidates"))?;
        // Move the on-disk file to Trash if this variation has one.
        if let Some(path) = cand
            .job
            .as_ref()
            .filter(|j| matches!(j.state, JobState::Done))
            .and_then(|j| j.output_path.clone())
        {
            let _ = trash::delete(&path); // tolerate a missing/already-removed file
        }
        // Removing a copy means "don't offer this one again": DROP the candidate
        // entirely (it was often retained only because it had a download) and
        // remember the md5 so a re-query won't re-surface it.
        if !req.dismissed.iter().any(|m| m == md5) {
            req.dismissed.push(md5.to_string());
        }
        req.candidates.retain(|c| c.md5 != md5);

        if req.selected.as_deref() == Some(md5) {
            req.selected = None;
        }
        if req.trash_on_replace.as_ref().map(|t| t.old_md5.as_str()) == Some(md5) {
            req.trash_on_replace = None;
        }
        req.review = false; // re-evaluating; any stale review no longer applies
        req.review_dismissed = None;

        // Re-evaluate status from the remaining variations' jobs.
        let any_done = req
            .candidates
            .iter()
            .any(|c| matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Done)));
        let any_active = req.candidates.iter().any(|c| {
            matches!(
                c.job.as_ref().map(|j| &j.state),
                Some(JobState::Resolving | JobState::Downloading | JobState::Verifying)
            )
        });
        let any_pending = req
            .candidates
            .iter()
            .any(|c| matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Pending)));
        req.status = if any_done {
            RequestStatus::Done
        } else if any_active {
            RequestStatus::Downloading
        } else if any_pending {
            // A queued-but-not-started copy: matched and waiting to download (e.g.
            // the user removed a wrong copy while the right one is queued). Reads
            // as "queued", not "downloading".
            RequestStatus::Matched
        } else if !req.candidates.is_empty() {
            RequestStatus::NeedsSelection
        } else {
            RequestStatus::Queued
        };

        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        Ok(())
    }

    /// Trash-on-complete hook: call when a variation `completed_md5` of the book
    /// at `(group_path, book_index)` has just reached `Done`. If that book has a
    /// pending `trash_on_replace` AND `completed_md5` is the recommended copy
    /// (i.e. != the old md5), move the OLD file (`old_path`) to the macOS Trash,
    /// then clear `review` + `trash_on_replace` and drop the old variation's
    /// `Done` job / `output_path` so it no longer reads as downloaded.
    ///
    /// A no-op (returns `false`) when the book has no pending replacement, or the
    /// completed md5 isn't the recommended one. Returns `true` when it trashed the
    /// old file and cleared the pending state. The Trash move uses the `trash`
    /// crate — never a hard delete; a missing old file is tolerated.
    pub fn trash_after_replace_done(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        completed_md5: &str,
    ) -> Result<bool> {
        let mut req = match self.request_at(group_path, book_index)? {
            Some(r) => r,
            None => return Ok(false),
        };
        let pending = match &req.trash_on_replace {
            Some(p) => p.clone(),
            None => return Ok(false),
        };
        // Only act once the RECOMMENDED (new) copy finished, not the old one.
        if completed_md5 == pending.old_md5 {
            return Ok(false);
        }
        // The completed variation must actually be Done before we trash the old.
        let completed_done = req.candidates.iter().any(|c| {
            c.md5 == completed_md5
                && matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Done))
        });
        if !completed_done {
            return Ok(false);
        }

        // Move the old file to Trash (safe-delete). A missing path is fine —
        // the goal (old file gone) is already satisfied.
        let old = std::path::Path::new(&pending.old_path);
        if old.exists() {
            trash::delete(old).with_context(|| format!("moving {} to Trash", pending.old_path))?;
        }

        // Drop the old variation's Done job/output_path so it no longer reads as
        // downloaded, and clear the review + pending-trash bookkeeping.
        if let Some(old_cand) = req.candidates.iter_mut().find(|c| c.md5 == pending.old_md5) {
            old_cand.job = None;
        }
        req.trash_on_replace = None;
        req.review = false;
        req.review_dismissed = None;
        req.status = roll_up_status(&req);
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        Ok(true)
    }

    /// Request a specific variation (by md5) for download: set that candidate's
    /// `job` to a fresh `Pending` job. Idempotent — if the variation is already
    /// requested (has a job), its state is left untouched. Errors if `md5` is not
    /// among this request's candidates. Persisted immediately.
    pub fn request_variation(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
    ) -> Result<()> {
        let mut req = self
            .request_at(group_path, book_index)?
            .context("request not found")?;
        let cand = req
            .candidates
            .iter_mut()
            .find(|c| c.md5 == md5)
            .with_context(|| format!("md5 {md5} is not among this request's candidates"))?;
        // (Re)arm this variation for download. `request_job` only CREATES a job
        // when none exists, so on its own it's a no-op for a Failed/Cancelled/Done
        // variation — which is exactly when the UI's "Retry"/"Re-download" runs.
        // Reset a NON-active job to a fresh Pending so the engine fetches it again
        // (clearing the prior error / partial / verified state). An in-flight job
        // (Pending/Resolving/Downloading/Verifying/Paused) is left untouched so a
        // repeated request never interrupts a live transfer.
        let prior = cand.job.as_ref().map(|j| j.state.clone());
        let rearm = matches!(
            prior,
            None | Some(JobState::Failed | JobState::Cancelled | JobState::Done)
        );
        let ext = cand.extension.as_ref().map(|e| e.ext());
        if rearm {
            cand.job = Some(DownloadJob {
                state: JobState::Pending,
                ..Default::default()
            });
        }
        if rearm {
            let what = match prior {
                Some(JobState::Failed) => "retry requested",
                Some(JobState::Cancelled) => "re-requested (was cancelled)",
                Some(JobState::Done) => "re-download requested",
                _ => "selected for download",
            };
            req.log_event(Some(md5.to_string()), ext, "selected", what);
        }
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        Ok(())
    }

    /// Add a user-supplied **manual md5** as a candidate for a book and arm it for
    /// download. For "cannot download" books where search found nothing: the user
    /// pastes a known md5 (the command reduces a libgen URL to its md5 first). If
    /// the md5 is already a candidate it is just (re)armed. Clears the not-found /
    /// review state and drives the book to `Complete` so the engine fetches it.
    pub fn add_manual_candidate(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
    ) -> Result<()> {
        let md5 = md5.trim().to_ascii_lowercase();
        let mut req = self
            .request_at(group_path, book_index)?
            .context("request not found")?;
        if !req.candidates.iter().any(|c| c.md5 == md5) {
            req.candidates.push(Candidate {
                md5: md5.clone(),
                title: req.input.title.clone(),
                authors: req.input.authors.clone(),
                year: None,
                publisher: None,
                language: None,
                pages: None,
                extension: None,
                size_bytes: None,
                source_host: Some("manual".into()),
                cover_url: None,
                score: 1.0, // user-supplied → trusted
                job: None,
            });
        }
        // Arm the (new or existing) candidate unless it's already in flight.
        if let Some(cand) = req.candidates.iter_mut().find(|c| c.md5 == md5) {
            let active = matches!(
                cand.job.as_ref().map(|j| &j.state),
                Some(
                    JobState::Pending
                        | JobState::Resolving
                        | JobState::Downloading
                        | JobState::Verifying
                        | JobState::Paused
                )
            );
            if !active {
                cand.job = Some(DownloadJob {
                    state: JobState::Pending,
                    ..Default::default()
                });
            }
        }
        req.status = RequestStatus::Matched;
        req.review = false;
        req.goal = crate::model::Goal::Complete;
        req.log_event(Some(md5.clone()), None, "manual", "md5 entered manually");
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        Ok(())
    }

    /// Cancel a requested variation (by md5): clear its `job` so it is no longer
    /// requested for download. A `Done` variation is left as-is (a completed
    /// download stays recorded). Persisted immediately. Errors if `md5` is not
    /// among this request's candidates.
    pub fn cancel_variation(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
    ) -> Result<()> {
        let mut req = self
            .request_at(group_path, book_index)?
            .context("request not found")?;
        let cand = req
            .candidates
            .iter_mut()
            .find(|c| c.md5 == md5)
            .with_context(|| format!("md5 {md5} is not among this request's candidates"))?;
        if !matches!(cand.job.as_ref().map(|j| &j.state), Some(JobState::Done)) {
            cand.job = None;
        }
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        Ok(())
    }

    // -- Pause / cancel of in-flight or queued downloads (item 2) -----------

    /// Pause a requested variation (by md5). Signals the scheduler to stop the
    /// download if it is actively streaming (the `.part` + `resume_offset` are
    /// kept so it can resume), and marks the persisted job `Paused`. If the
    /// variation is merely queued (`Pending`, not yet started) it is marked
    /// `Paused` directly. Errors if `md5` is not among this request's candidates.
    pub async fn pause_variation(
        &mut self,
        scheduler: &Arc<Scheduler>,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
    ) -> Result<()> {
        let signalled = scheduler.pause(md5).await;
        // Whether or not an in-flight download was signalled, reflect Paused in
        // the store. For an in-flight job the eventual `Progress::Cancelled` will
        // also persist the exact resume_offset; setting it here keeps a queued or
        // already-finished-streaming job consistent immediately.
        self.set_variation_state(group_path, book_index, md5, |job| {
            if !matches!(job.state, JobState::Done) {
                job.state = JobState::Paused;
            }
        })?;
        let _ = signalled;
        Ok(())
    }

    /// Resume a paused variation (by md5): its job goes back to `Pending`
    /// (keeping `resume_offset` so the ranged downloader continues from the
    /// partial file). The next `start_downloads` pass picks it up. Errors if
    /// `md5` is not among this request's candidates.
    pub fn resume_variation(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
    ) -> Result<()> {
        self.set_variation_state(group_path, book_index, md5, |job| {
            if matches!(job.state, JobState::Paused | JobState::Cancelled) {
                job.state = JobState::Pending;
                job.last_error = None;
            }
        })
    }

    /// Cancel a variation's download (by md5) — whether in-flight or queued.
    /// Signals the scheduler to abort an active stream (its `.part` is removed),
    /// and marks the persisted job `Cancelled` (resume_offset reset). A `Done`
    /// variation is left as-is. Errors if `md5` is not among this request's
    /// candidates.
    pub async fn cancel_download(
        &mut self,
        scheduler: &Arc<Scheduler>,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
    ) -> Result<()> {
        let _ = scheduler.cancel(md5).await;
        self.set_variation_state(group_path, book_index, md5, |job| {
            if !matches!(job.state, JobState::Done) {
                job.state = JobState::Cancelled;
                job.resume_offset = 0;
            }
        })?;
        Ok(())
    }

    /// Pause every requested variation across the whole list. Signals the
    /// scheduler to stop all in-flight downloads (keeping their `.part`s) and
    /// marks every non-`Done` job `Paused`.
    pub async fn pause_all(&mut self, scheduler: &Arc<Scheduler>) -> Result<()> {
        scheduler.pause_all().await;
        self.map_all_jobs(|job| {
            if !matches!(job.state, JobState::Done | JobState::Cancelled) {
                job.state = JobState::Paused;
            }
        })
    }

    /// Resume every paused (or cancelled) variation across the list: each such
    /// job returns to `Pending`, keeping its `resume_offset`. The next
    /// `start_downloads` continues them.
    pub fn resume_all(&mut self) -> Result<()> {
        self.map_all_jobs(|job| {
            if matches!(job.state, JobState::Paused | JobState::Cancelled) {
                job.state = JobState::Pending;
                job.last_error = None;
            }
        })
    }

    /// Apply `f` to the job of the variation `md5` under a request, persisting
    /// the change and rolling the request's status up from its variations.
    fn set_variation_state(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
        f: impl FnOnce(&mut DownloadJob),
    ) -> Result<()> {
        let mut req = self
            .request_at(group_path, book_index)?
            .context("request not found")?;
        let cand = req
            .candidates
            .iter_mut()
            .find(|c| c.md5 == md5)
            .with_context(|| format!("md5 {md5} is not among this request's candidates"))?;
        let mut job = cand.job.clone().unwrap_or_default();
        f(&mut job);
        cand.job = Some(job);
        req.status = roll_up_status(&req);
        self.store
            .update_request(self.list_id, group_path, book_index, &req)?;
        Ok(())
    }

    /// Apply `f` to every requested variation's job across the whole list,
    /// persisting per-book and rolling each request's status up.
    fn map_all_jobs(&mut self, mut f: impl FnMut(&mut DownloadJob)) -> Result<()> {
        let list = self.snapshot()?;
        let mut positions: Vec<(Vec<usize>, usize)> = Vec::new();
        walk(&list.groups, &mut Vec::new(), &mut |path, bi, _req| {
            positions.push((path.to_vec(), bi));
        });
        for (group_path, book_index) in positions {
            let mut req = match self.request_at(&group_path, book_index)? {
                Some(r) => r,
                None => continue,
            };
            let mut changed = false;
            for cand in req.candidates.iter_mut() {
                if let Some(job) = cand.job.as_mut() {
                    f(job);
                    changed = true;
                }
            }
            if changed {
                req.status = roll_up_status(&req);
                self.store
                    .update_request(self.list_id, &group_path, book_index, &req)?;
            }
        }
        Ok(())
    }

    /// Integrity check (run in the background on launch): a variation marked
    /// `Done` whose file is GONE from disk — moved/deleted out from under us, or a
    /// same-name collision that lost it — is demoted to `Failed` with a "data lost"
    /// reason. This stops a dead "Reveal" (the file isn't there) and makes the copy
    /// re-downloadable, instead of silently presenting a Done book with no file.
    /// A `Done` job with no recorded `output_path` is also treated as lost.
    /// Returns the number of variations demoted.
    pub fn flag_missing_downloads(&mut self) -> Result<usize> {
        let list = self.snapshot()?;
        let mut positions: Vec<(Vec<usize>, usize)> = Vec::new();
        walk(&list.groups, &mut Vec::new(), &mut |path, bi, _req| {
            positions.push((path.to_vec(), bi));
        });
        let mut count = 0usize;
        for (gp, bi) in positions {
            let mut req = match self.request_at(&gp, bi)? {
                Some(r) => r,
                None => continue,
            };
            let mut changed = false;
            for cand in req.candidates.iter_mut() {
                let missing = cand
                    .job
                    .as_ref()
                    .filter(|j| j.state == JobState::Done)
                    .map(|j| {
                        j.output_path
                            .as_deref()
                            .map(|p| !Path::new(p).exists())
                            .unwrap_or(true) // Done but no recorded path → treat as lost
                    })
                    .unwrap_or(false);
                if missing {
                    if let Some(job) = cand.job.as_mut() {
                        job.state = JobState::Failed;
                        job.last_error =
                            Some("downloaded file is missing (data lost) — re-download".into());
                    }
                    changed = true;
                    count += 1;
                    tracing::warn!(
                        title = %req.input.title,
                        md5 = %cand.md5,
                        "integrity: Done file missing on disk — demoted to Failed (data lost)"
                    );
                }
            }
            if changed {
                req.status = roll_up_status(&req);
                self.store.update_request(self.list_id, &gp, bi, &req)?;
            }
        }
        Ok(count)
    }

    /// `(group_path, book_index, md5, output_path)` for every `Done` variation —
    /// the read-only worklist the background integrity scan hashes OFF-lock (so the
    /// expensive md5 verification never holds the per-list lock).
    pub fn done_variations(&self) -> Result<Vec<(Vec<usize>, usize, String, Option<String>)>> {
        let list = self.snapshot()?;
        let mut out = Vec::new();
        walk(&list.groups, &mut Vec::new(), &mut |path, bi, req| {
            for c in &req.candidates {
                if c.job
                    .as_ref()
                    .map(|j| j.state == JobState::Done)
                    .unwrap_or(false)
                {
                    out.push((
                        path.to_vec(),
                        bi,
                        c.md5.clone(),
                        c.job.as_ref().and_then(|j| j.output_path.clone()),
                    ));
                }
            }
        });
        Ok(out)
    }

    /// Demote ONE `Done` variation (by md5) to `Failed` with `reason`. Used by the
    /// integrity scan when a downloaded file's content md5 doesn't match what was
    /// requested (overwritten/corrupt). Returns whether it changed.
    pub fn demote_variation(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
        reason: &str,
    ) -> Result<bool> {
        let mut req = match self.request_at(group_path, book_index)? {
            Some(r) => r,
            None => return Ok(false),
        };
        let mut changed = false;
        if let Some(cand) = req.candidates.iter_mut().find(|c| c.md5 == md5) {
            if let Some(job) = cand.job.as_mut() {
                if job.state == JobState::Done {
                    job.state = JobState::Failed;
                    job.last_error = Some(reason.to_string());
                    changed = true;
                }
            }
        }
        if changed {
            req.status = roll_up_status(&req);
            self.store
                .update_request(self.list_id, group_path, book_index, &req)?;
            tracing::warn!(
                md5,
                reason,
                "integrity: md5 mismatch — demoted Done → Failed"
            );
        }
        Ok(changed)
    }

    /// Reset any in-flight (`Downloading`/`Resolving`/`Verifying`) variation jobs
    /// to `Pending` so a fresh `start_downloads` continues them after a restart
    /// (item 3, resume-on-launch). `resume_offset` is preserved so the ranged
    /// downloader continues from the partial file rather than refetching it.
    /// Paused/Cancelled/Done/Failed jobs are left untouched. Returns the number
    /// of jobs reset.
    pub fn reset_inflight_for_resume(&mut self) -> Result<usize> {
        let mut count = 0usize;
        self.map_all_jobs(|job| {
            if matches!(
                job.state,
                JobState::Downloading | JobState::Resolving | JobState::Verifying
            ) {
                job.state = JobState::Pending;
                count += 1;
            }
            // Hedge legs are transient: a relaunch resumes as a SINGLE normal
            // attempt from the main `.part`. But a hedge leg may have downloaded
            // MORE than the main leg (same file, different host) — so before
            // dropping the legs, PROMOTE the largest partial (main or any hedge)
            // into the main dest's `.part`, then delete the rest. This preserves
            // the furthest progress instead of discarding it (the resume-loss bug:
            // a 19 MB hedge leg was thrown away while a 5 MB main leg resumed).
            let legs = std::mem::take(&mut job.hedges);
            if let Some(main) = job.output_path.as_deref().filter(|s| !s.is_empty()) {
                let main_part = crate::download::part_path(std::path::Path::new(main));
                let part_len =
                    |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                // The biggest hedge-leg .part, if any beats the main .part.
                let mut best: Option<(u64, std::path::PathBuf)> = None;
                for leg in &legs {
                    if leg.temp_path.is_empty() {
                        continue;
                    }
                    let lp = crate::download::part_path(std::path::Path::new(&leg.temp_path));
                    let len = part_len(&lp);
                    if best.as_ref().map(|(b, _)| len > *b).unwrap_or(true) {
                        best = Some((len, lp));
                    }
                }
                if let Some((len, lp)) = &best {
                    if *len > part_len(&main_part) {
                        if let Some(parent) = main_part.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::rename(lp, &main_part); // promote furthest progress
                    }
                }
            }
            // Clean up every hedge leg's temp file + its `.part` (the promoted one
            // was renamed away, so this removes only the now-superseded leftovers).
            for leg in &legs {
                if leg.temp_path.is_empty() {
                    continue;
                }
                let temp = std::path::PathBuf::from(&leg.temp_path);
                let _ = std::fs::remove_file(&temp);
                let _ = std::fs::remove_file(crate::download::part_path(&temp));
            }
        })?;
        Ok(count)
    }

    /// Rewind every book whose CURRENT `status` is a transient in-flight stage
    /// (`Querying`/`Downloading`/`Verifying`) back to its pre-flight discovery
    /// state so a paused launch shows a settled list. `Querying` (a search was
    /// cut off) → `Queued` (re-queryable); `Downloading`/`Verifying` (a download
    /// was cut off) → `Matched` if the book has a chosen variation, else `Queued`.
    /// The per-variation jobs are rewound separately by
    /// [`Orchestrator::reset_inflight_for_resume`]; this fixes the book-level
    /// roll-up status. Returns how many books were rewound.
    pub fn rewind_inflight_status(&mut self) -> Result<usize> {
        let list = self.snapshot()?;
        let mut positions: Vec<(Vec<usize>, usize)> = Vec::new();
        walk(&list.groups, &mut Vec::new(), &mut |path, bi, req| {
            if matches!(
                req.status,
                RequestStatus::Querying | RequestStatus::Downloading | RequestStatus::Verifying
            ) {
                positions.push((path.to_vec(), bi));
            }
        });
        let n = positions.len();
        for (gp, bi) in positions {
            if let Some(mut req) = self.request_at(&gp, bi)? {
                req.status = match req.status {
                    RequestStatus::Querying => RequestStatus::Queued,
                    _ => {
                        if req.selected.is_some() || req.candidates.iter().any(|c| c.is_requested())
                        {
                            RequestStatus::Matched
                        } else {
                            RequestStatus::Queued
                        }
                    }
                };
                self.store.update_request(self.list_id, &gp, bi, &req)?;
            }
        }
        Ok(n)
    }

    /// Assign and persist a stable sequence number to every book that has at
    /// least one requested variation but no number yet (item 7b). Numbers are
    /// allocated per the list's `seq_per_group` scope.
    ///
    /// Numbers follow the **source-list reading order** (the depth-first order in
    /// which books are parsed): within a scope, the requested books are numbered
    /// 1, 2, 3… in source order. This makes the number a function of source
    /// position, independent of the order in which books happen to be matched /
    /// requested / downloaded — which can differ from declaration order because
    /// searches run concurrently and planning happens incrementally per book
    /// (`begin_download`).
    ///
    /// The number of a book whose download is already `Done` is **frozen** (the
    /// "stable per-book" guarantee — a file on disk is never renumbered). Books
    /// not yet downloaded are assigned/updated to their source rank, stepping past
    /// any frozen number that would collide, so an earlier-source book that gets
    /// requested late still sorts ahead of later ones without disturbing files
    /// already written.
    fn assign_sequence_numbers(&mut self) -> Result<()> {
        let list = self.snapshot()?;
        let per_group = list.settings.seq_per_group;

        // EVERY book, in source (depth-first) order, grouped by scope (per-list → one
        // bucket keyed by the empty path; per-group → keyed by the group's path).
        // Number them 1, 2, 3… by fixed IMPORT POSITION — NOT by position among the
        // currently-matched/requested subset. Numbering only the requested books made
        // the sequence DRIFT as more books matched over time (a book downloaded early,
        // when few were matched, got a low number that later shifted up), so files on
        // disk no longer matched the plan. A book's number is now its permanent slot
        // in the imported list and never changes. (Files keep their old number until
        // the user runs "Reorganize", which renames them to match.)
        let mut by_scope: std::collections::HashMap<Vec<usize>, Vec<(Vec<usize>, usize)>> =
            std::collections::HashMap::new();

        walk(&list.groups, &mut Vec::new(), &mut |path, bi, _req| {
            let scope = if per_group { path.to_vec() } else { Vec::new() };
            by_scope.entry(scope).or_default().push((path.to_vec(), bi));
        });

        for requested in by_scope.into_values() {
            let mut n: u32 = 1;
            for (group_path, book_index) in requested {
                if let Some(mut req) = self.request_at(&group_path, book_index)? {
                    if req.seq != Some(n) {
                        req.seq = Some(n);
                        self.store
                            .update_request(self.list_id, &group_path, book_index, &req)?;
                    }
                }
                n += 1;
            }
        }
        Ok(())
    }

    /// The per-list output folder (`<out_dir>/<sanitized list title>`), where
    /// thumbnails and downloaded files live. Falls back to `out_dir` for an
    /// empty/blank list title. Mirrors `plan_downloads`' E5 list-root logic.
    fn list_dir(&self, list: &DownloadList) -> PathBuf {
        if list.title.trim().is_empty() {
            self.out_dir.clone()
        } else {
            self.out_dir.join(naming::sanitize_component(&list.title))
        }
    }

    /// Best-effort cover backfill (E3): for every matched book that has NO cover
    /// from search (libgen only emits covers for comic rows, and Anna's Archive
    /// covers only when that mirror matched), look one up on **Open Library** and
    /// stamp it onto the book's candidates so the UI renders it. The chosen image
    /// is also downloaded to `<list folder>/thumbnails/<md5>.jpg` so it is durable
    /// and offline-loadable.
    ///
    /// Networked, so it is NOT called from the hot query/plan path — a front end
    /// invokes it explicitly (e.g. after `query_all`). One mirror/lookup failure
    /// never aborts the pass: per-book errors are swallowed (logged) so a missing
    /// cover is simply absent. Returns the number of books a cover was set on.
    /// Brief READ for the off-lock cover backfill: the list's directory (for the
    /// thumbnail cache) plus `(group_path, book_index, title, author, isbn, key)`
    /// for every matched book that still lacks a cover. Returning this lets a
    /// caller do the (network) lookup with NO orchestrator lock held, then persist
    /// via [`apply_cover`](Self::apply_cover).
    #[allow(clippy::type_complexity)]
    pub fn cover_targets(&self) -> Result<(PathBuf, Vec<CoverTarget>)> {
        let list = self.snapshot()?;
        let list_dir = self.list_dir(&list);
        let mut targets = Vec::new();
        walk(&list.groups, &mut Vec::new(), &mut |path, bi, req| {
            let matched = req.candidates.iter().any(|c| c.is_requested())
                || req.status == RequestStatus::Matched;
            if !matched || req.candidates.is_empty() {
                return;
            }
            // A cover is "localized" once its url is a local file path (not http).
            // Skip those; (re)process books with no cover OR a remote one (so a
            // broken/remote url gets cached to a local .jpg the UI can serve).
            let covers: Vec<&str> = req
                .candidates
                .iter()
                .filter_map(|c| c.cover_url.as_deref())
                .collect();
            // A local cover only counts if it's a USABLE image — a cached
            // placeholder (1×1 GIF) or corrupt .jpg must be re-processed so it's
            // regenerated, not left rendering blank forever.
            let has_local = covers
                .iter()
                .any(|u| !is_http_url(u) && crate::cover_gen::cover_file_usable(Path::new(u)));
            if has_local {
                return;
            }
            let existing_remote = covers
                .iter()
                .find(|u| is_http_url(u))
                .map(|u| u.to_string());
            let key = req
                .candidates
                .first()
                .map(|c| c.md5.clone())
                .unwrap_or_else(|| req.input.title.clone());
            // The downloaded copy's path, if any (a `Done` variation with an
            // output_path that still exists on disk) — the source for local cover
            // generation when no online cover is found.
            let local_file = req.candidates.iter().find_map(|c| {
                let j = c.job.as_ref()?;
                if matches!(j.state, JobState::Done) {
                    j.output_path
                        .as_ref()
                        .filter(|p| std::path::Path::new(p).exists())
                        .cloned()
                } else {
                    None
                }
            });
            targets.push(CoverTarget {
                group_path: path.to_vec(),
                book_index: bi,
                title: req.input.title.clone(),
                author: req.input.authors.join(" "),
                isbn: req.input.isbn.clone(),
                key,
                existing_remote,
                local_file,
            });
        });
        Ok((list_dir, targets))
    }

    /// Stamp a resolved cover (a local thumbnail path) onto every candidate of a
    /// book (brief WRITE). Overwrites any prior (e.g. remote) cover so the book
    /// converges to the locally-cached image.
    pub fn apply_cover(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        cover: &str,
    ) -> Result<()> {
        if let Some(mut req) = self.request_at(group_path, book_index)? {
            for c in &mut req.candidates {
                c.cover_url = Some(cover.to_string());
            }
            self.store
                .update_request(self.list_id, group_path, book_index, &req)?;
        }
        Ok(())
    }

    pub async fn backfill_covers(
        &mut self,
        covers: &crate::covers::CoverClient,
        client: &reqwest::Client,
    ) -> Result<usize> {
        let list = self.snapshot()?;
        let list_dir = self.list_dir(&list);

        // Collect the matched books that still lack any cover, with the metadata
        // the lookup needs. Done up front so we hold no borrow across awaits.
        // (group_path, book_index, title, author, isbn).
        type CoverTarget = (Vec<usize>, usize, String, String, Option<String>);
        let mut targets: Vec<CoverTarget> = Vec::new();
        walk(&list.groups, &mut Vec::new(), &mut |path, bi, req| {
            let has_cover = req.candidates.iter().any(|c| c.cover_url.is_some());
            let matched = req.candidates.iter().any(|c| c.is_requested())
                || req.status == RequestStatus::Matched;
            if has_cover || !matched || req.candidates.is_empty() {
                return;
            }
            targets.push((
                path.to_vec(),
                bi,
                req.input.title.clone(),
                req.input.authors.join(" "),
                req.input.isbn.clone(),
            ));
        });

        let mut set = 0usize;
        for (group_path, book_index, title, author, isbn) in targets {
            let url = match covers.cover_url(&title, &author, isbn.as_deref()).await {
                Ok(Some(u)) => u,
                Ok(None) => continue,
                Err(e) => {
                    tracing::debug!(title = %title, error = %e, "cover lookup failed");
                    continue;
                }
            };
            // Stamp the cover onto every candidate so any chosen variation shows
            // it (the viewmodel surfaces `Candidate.cover_url`).
            let mut req = match self.request_at(&group_path, book_index)? {
                Some(r) => r,
                None => continue,
            };
            if req.candidates.iter().any(|c| c.cover_url.is_some()) {
                continue; // a concurrent pass already set one
            }
            let key = req
                .candidates
                .first()
                .map(|c| c.md5.clone())
                .unwrap_or_else(|| title.clone());
            for c in &mut req.candidates {
                c.cover_url = Some(url.clone());
            }
            self.store
                .update_request(self.list_id, &group_path, book_index, &req)?;
            set += 1;

            // Durably cache the image locally (best effort — a failure just means
            // the UI falls back to the remote URL).
            if let Err(e) = crate::covers::store_thumbnail(client, &list_dir, &key, &url).await {
                tracing::debug!(title = %title, error = %e, "thumbnail store failed");
            }
        }
        Ok(set)
    }

    /// Compute the planned destination path for every requested **variation**
    /// (each candidate that carries a `job`), persisting the path onto that
    /// candidate's job. Pure planning step — no download. A book with several
    /// requested variations (e.g. an epub + a pdf) yields one plan entry per
    /// variation, each at its own filename. This is what `run-list`'s dry run
    /// prints, and what `start_downloads` feeds the scheduler. Sequence numbering
    /// honors `settings.seq_per_group` (variations of one book share its number,
    /// disambiguated by extension / a `(2)` suffix).
    pub fn plan_downloads(&mut self) -> Result<Vec<PlannedDownload>> {
        // First, assign-and-persist a stable sequence number to every book that
        // has requested variations but no number yet (item 7b). Existing numbers
        // are reused, so inserting a book later never renumbers existing files.
        self.assign_sequence_numbers()?;

        let list = self.snapshot()?;
        let settings = list.settings.clone();
        let mut taken: HashSet<PathBuf> = HashSet::new();
        let mut planned = Vec::new();

        // E5: two-level foldering `<out_dir>/<list title>/<sub-group>/file`. The
        // list title is the first folder level; nested group/sub-group names add
        // further levels under it. A book with no sub-group lands directly in the
        // list folder. An empty/blank list title is skipped so we never emit a
        // `_` folder (falls back to `<out_dir>/<sub-group>/file`).
        let list_root = if list.title.trim().is_empty() {
            self.out_dir.clone()
        } else {
            self.out_dir.join(naming::sanitize_component(&list.title))
        };

        plan_groups(
            &list.groups,
            &list_root,
            &mut Vec::new(),
            &mut Vec::new(),
            &settings,
            &mut taken,
            &mut planned,
        );

        // Persist the planned path onto each requested variation's job. Group the
        // plan entries by book position so each book is written once.
        let mut by_book: std::collections::HashMap<(Vec<usize>, usize), Vec<&PlannedDownload>> =
            std::collections::HashMap::new();
        for p in &planned {
            by_book
                .entry((p.group_path.clone(), p.book_index))
                .or_default()
                .push(p);
        }
        for ((group_path, book_index), entries) in by_book {
            let mut req = self
                .request_at(&group_path, book_index)?
                .context("request vanished while planning")?;
            for p in entries {
                if let Some(cand) = req.candidates.iter_mut().find(|c| c.md5 == p.md5) {
                    let mut job = cand.job.clone().unwrap_or_default();
                    // Do NOT move a finished file's recorded location: a `Done`
                    // variation keeps the `output_path` it was actually saved to, so
                    // "Reveal" still finds it. Only (re)assign the planned path for
                    // variations not yet downloaded. (Reorganize moves files + paths
                    // explicitly.)
                    if job.state != JobState::Done {
                        job.output_path = Some(p.destination.to_string_lossy().into_owned());
                        cand.job = Some(job);
                    }
                }
            }
            self.store
                .update_request(self.list_id, &group_path, book_index, &req)?;
        }
        Ok(planned)
    }

    /// This list's output folder (`<out_dir>/<sanitized title>`, or `out_dir` for a
    /// blank title) — the root under which its downloads live. The Reorganize command
    /// passes every OTHER list's folder to [`relocate_downloads_to_current_scheme`] so
    /// a book shared across lists is duplicated into each rather than moved between.
    pub fn list_folder(&self) -> Result<PathBuf> {
        let list = self.snapshot()?;
        Ok(if list.title.trim().is_empty() {
            self.out_dir.clone()
        } else {
            self.out_dir.join(naming::sanitize_component(&list.title))
        })
    }

    /// **Reorganize already-downloaded files** to the CURRENT naming/foldering
    /// scheme (two-level `<list>/<sub-group>/…`, sanitized names, current seq).
    /// Books finished under an older layout (flat or one-level) are moved to where
    /// `plan_downloads` would place them now, and their persisted `output_path` is
    /// updated. Safe + idempotent: it MOVES (atomic rename, with a copy+remove
    /// fallback across devices), SKIPS rather than overwrites if the destination
    /// already exists, never deletes a file, and leaves `.part`/in-flight jobs
    /// alone. Returns `(moved, skipped, errors)`. Triggered explicitly by the user.
    /// The current correct destination for every requested variation under the
    /// list's folder (pure — `plan_groups` does NOT persist output_path).
    fn plan_current(&self, list: &DownloadList) -> Vec<PlannedDownload> {
        let settings = list.settings.clone();
        let list_root = if list.title.trim().is_empty() {
            self.out_dir.clone()
        } else {
            self.out_dir.join(naming::sanitize_component(&list.title))
        };
        let mut taken: HashSet<PathBuf> = HashSet::new();
        let mut planned: Vec<PlannedDownload> = Vec::new();
        plan_groups(
            &list.groups,
            &list_root,
            &mut Vec::new(),
            &mut Vec::new(),
            &settings,
            &mut taken,
            &mut planned,
        );
        planned
    }

    /// The single source of truth for "which finished files are out of place, and
    /// where do we take them from" — shared by [`reorganize_needed`] and
    /// [`relocate_downloads_to_current_scheme`] so the gating CHECK and the ACTION
    /// can never disagree (a past bug: the check used a stale name-match the action
    /// didn't, so the button grayed while files actually needed moving).
    ///
    /// For each FINISHED variation whose correct destination is empty, the source is
    /// the variation's recorded `output_path` (authoritative — `plan_downloads`
    /// preserves it for Done variations, so it points at the real file), falling
    /// back to a disk scan by the destination's sequence-stripped "Author - Title"
    /// name for legacy rows with no/stale recorded path. A source under ANOTHER
    /// list's root is a cross-list duplicate (copy); anything else is moved.
    /// Returns the moves plus a count of finished variations whose file was found
    /// nowhere (skipped).
    fn compute_relocations(
        &self,
        list: &DownloadList,
        planned: &[PlannedDownload],
        sibling_roots: &[PathBuf],
    ) -> (Vec<Relocation>, Vec<PathBuf>) {
        let mut by_stable: std::collections::HashMap<String, Vec<PathBuf>> =
            std::collections::HashMap::new();
        collect_files_recursive(&self.out_dir, &mut |f| {
            if let Some(name) = f.file_name().and_then(|n| n.to_str()) {
                if !name.ends_with(".part") {
                    by_stable
                        .entry(strip_seq_prefix(name).to_string())
                        .or_default()
                        .push(f.to_path_buf());
                }
            }
        });
        let is_sibling = |f: &PathBuf| sibling_roots.iter().any(|r| f.starts_with(r));
        let mut moves = Vec::new();
        let mut skipped: Vec<PathBuf> = Vec::new();
        for p in planned {
            let job = request_at_in(list, &p.group_path, p.book_index)
                .and_then(|r| r.candidates.iter().find(|c| c.md5 == p.md5))
                .and_then(|c| c.job.clone());
            if job.as_ref().map(|j| j.state == JobState::Done) != Some(true) {
                continue;
            }
            let dest = p.destination.clone();
            if dest.exists() {
                continue; // already in place (output_path refresh is the caller's job)
            }
            // (a) Authoritative: the recorded real location of this finished file.
            let recorded = job
                .as_ref()
                .and_then(|j| j.output_path.as_deref())
                .map(PathBuf::from)
                .filter(|f| f.as_path() != dest.as_path() && f.exists());
            // (b) Fallback: a file on disk under the destination's stable name.
            let stable = dest
                .file_name()
                .and_then(|n| n.to_str())
                .map(strip_seq_prefix)
                .unwrap_or("")
                .to_string();
            let from_disk = by_stable.get(&stable).and_then(|v| {
                v.iter()
                    .find(|f| f.as_path() != dest.as_path() && f.exists() && !is_sibling(f))
                    .or_else(|| {
                        v.iter()
                            .find(|f| f.as_path() != dest.as_path() && f.exists())
                    })
                    .cloned()
            });
            let src = match recorded.or(from_disk) {
                Some(s) => s,
                None => {
                    skipped.push(dest.clone());
                    continue;
                }
            };
            let is_copy = is_sibling(&src);
            moves.push(Relocation {
                group_path: p.group_path.clone(),
                book_index: p.book_index,
                md5: p.md5.clone(),
                src,
                dest,
                is_copy,
            });
        }
        (moves, skipped)
    }

    pub fn relocate_downloads_to_current_scheme(
        &mut self,
        sibling_roots: &[PathBuf],
    ) -> Result<(usize, usize, usize)> {
        self.assign_sequence_numbers()?;
        let list = self.snapshot()?;
        let planned = self.plan_current(&list);

        // Files already AT their correct destination: just make sure output_path
        // reflects it (no move).
        for p in &planned {
            let is_done = request_at_in(&list, &p.group_path, p.book_index)
                .and_then(|r| r.candidates.iter().find(|c| c.md5 == p.md5))
                .and_then(|c| c.job.as_ref())
                .map(|j| j.state == JobState::Done)
                .unwrap_or(false);
            if is_done && p.destination.exists() {
                self.set_variation_output_path(
                    &p.group_path,
                    p.book_index,
                    &p.md5,
                    &p.destination,
                )?;
            }
        }

        let (moves, skipped) = self.compute_relocations(&list, &planned, sibling_roots);
        // Observability: a finished file recorded as Done but found nowhere is a
        // real anomaly — log each so "why didn't X reorganize?" is answerable.
        for dest in &skipped {
            tracing::warn!(dest = %dest.display(), "reorganize: finished file not found at any source — skipped");
        }
        let (mut moved, mut errors) = (0usize, 0usize);
        for r in &moves {
            if let Some(parent) = r.dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let did = if r.is_copy {
                std::fs::copy(&r.src, &r.dest).map(|_| ()) // duplicate across lists; keep the source
            } else {
                std::fs::rename(&r.src, &r.dest).or_else(|_| {
                    std::fs::copy(&r.src, &r.dest)
                        .and_then(|_| std::fs::remove_file(&r.src))
                        .map(|_| ())
                })
            };
            match did {
                Ok(_) => {
                    moved += 1;
                    tracing::info!(
                        src = %r.src.display(), dest = %r.dest.display(), copy = r.is_copy,
                        "reorganize: relocated"
                    );
                    self.set_variation_output_path(&r.group_path, r.book_index, &r.md5, &r.dest)?;
                }
                Err(e) => {
                    tracing::error!(src = %r.src.display(), dest = %r.dest.display(), error = %e, "reorganize: relocate failed");
                    let _ = std::fs::remove_file(&r.dest); // clean a half-written copy
                    errors += 1;
                }
            }
        }
        // Clear old, now-empty layout folders left behind by the moves. Never
        // removes out_dir itself, and keeps any folder still holding a file.
        prune_empty_dirs(&self.out_dir);
        tracing::info!(
            moved,
            skipped = skipped.len(),
            errors,
            "reorganize complete"
        );
        Ok((moved, skipped.len(), errors))
    }

    /// Dry-run of [`relocate_downloads_to_current_scheme`]: returns `true` iff at
    /// least one finished file WOULD be moved (its canonical destination is empty
    /// but a matching file exists elsewhere). Moves nothing and does not persist
    /// `output_path` — used to gray out the "Reorganize now" button when the
    /// on-disk layout is already canonical.
    pub fn reorganize_needed(&mut self) -> Result<bool> {
        self.assign_sequence_numbers()?;
        let list = self.snapshot()?;
        let planned = self.plan_current(&list);
        // Same computation the action uses (empty sibling_roots: a sibling source
        // is still a relocation — copy vs move doesn't change "is one needed"), so
        // the button never grays while files actually need moving.
        let (moves, _skipped) = self.compute_relocations(&list, &planned, &[]);
        Ok(!moves.is_empty())
    }

    /// Diagnostic sibling of [`reorganize_needed`]: the (found-on-disk → planned)
    /// path pairs that WOULD move. Lets a false "needs reorganize" be pinpointed
    /// (e.g. a seq number that drifted between the file and the current plan).
    #[doc(hidden)]
    pub fn reorganize_plan_diff(&mut self) -> Result<Vec<(String, String)>> {
        self.assign_sequence_numbers()?;
        let list = self.snapshot()?;
        let planned = self.plan_current(&list);
        let (moves, _skipped) = self.compute_relocations(&list, &planned, &[]);
        Ok(moves
            .into_iter()
            .map(|r| {
                (
                    r.src.to_string_lossy().into_owned(),
                    r.dest.to_string_lossy().into_owned(),
                )
            })
            .collect())
    }

    /// Persist `output_path` for one variation (md5) of a book.
    fn set_variation_output_path(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        md5: &str,
        path: &Path,
    ) -> Result<()> {
        if let Some(mut req) = self.request_at(group_path, book_index)? {
            if let Some(cand) = req.candidates.iter_mut().find(|c| c.md5 == md5) {
                if let Some(job) = cand.job.as_mut() {
                    job.output_path = Some(path.to_string_lossy().into_owned());
                }
            }
            self.store
                .update_request(self.list_id, group_path, book_index, &req)?;
        }
        Ok(())
    }

    /// Plan, then download every requested variation that is still `Pending`,
    /// routing one [`DownloadRequest`] per variation (across all books) through
    /// the scheduler. Per-variation job state (state, host, bytes, output path,
    /// errors) is persisted onto the matching candidate as events arrive — so an
    /// epub can reach `Done` while a pdf of the same book is still downloading.
    pub async fn start_downloads(
        &mut self,
        scheduler: &Arc<Scheduler>,
        events: &mpsc::Sender<Event>,
    ) -> Result<()> {
        let planned = self.plan_downloads()?;

        // Restrict to variations whose job is still `Pending` (skip ones already
        // Done/Failed/in-flight). `plan_downloads` set output paths for all, so
        // every entry here has a destination.
        let pending: Vec<PlannedDownload> = {
            let list = self.snapshot()?;
            planned
                .iter()
                .filter(|p| {
                    request_at_in(&list, &p.group_path, p.book_index)
                        .and_then(|req| req.candidates.iter().find(|c| c.md5 == p.md5))
                        .and_then(|c| c.job.as_ref())
                        .map(|j| j.state == JobState::Pending)
                        .unwrap_or(false)
                })
                .cloned()
                .collect()
        };

        for p in &pending {
            // If a prior naming scheme left a partial at a different path, adopt it
            // so we resume rather than restart (numbered folders / source-order seq
            // changed the dest out from under an in-progress download).
            adopt_orphaned_part(&p.destination, &self.out_dir);
            let _ = events
                .send(Event::Planned {
                    group_path: p.group_path.clone(),
                    book_index: p.book_index,
                    title: p.title.clone(),
                    md5: p.md5.clone(),
                    destination: p.destination.clone(),
                })
                .await;
        }
        if pending.is_empty() {
            let _ = events.send(Event::Done).await;
            return Ok(());
        }

        // Dedupe by md5 (item 7a): if several books request the SAME md5, the
        // file is downloaded ONCE (to the first variation's destination) and
        // verified once. The other destinations are filled by copying the
        // verified file after the single download completes — see
        // `apply_progress`'s `Done` handling. We submit one `DownloadRequest`
        // per distinct md5, carrying any persisted `resume_offset` so a paused
        // job continues from its `.part`.
        let mut seen_md5: HashSet<String> = HashSet::new();
        let mut requests = Vec::new();
        {
            let list = self.snapshot()?;
            for p in &pending {
                // Mark the variation's owning request downloading (row roll-up).
                self.set_status(&p.group_path, p.book_index, RequestStatus::Downloading)?;
                if !seen_md5.insert(p.md5.clone()) {
                    // A duplicate md5 — it'll be satisfied by the copy step.
                    continue;
                }
                let cand = request_at_in(&list, &p.group_path, p.book_index)
                    .and_then(|req| req.candidates.iter().find(|c| c.md5 == p.md5));
                let resume_offset = cand
                    .and_then(|c| c.job.as_ref())
                    .map(|j| j.resume_offset)
                    .unwrap_or(0);
                let mut dr = DownloadRequest::new(p.md5.clone(), p.destination.clone());
                dr.resume_offset = resume_offset;
                dr.expected_size = cand.and_then(|c| c.size_bytes);
                requests.push(dr);
            }
        }

        let (tx, mut rx) = mpsc::channel::<Progress>(1024);
        let sched = Arc::clone(scheduler);
        let run = tokio::spawn(async move { sched.run(requests, tx).await });

        // Forward progress + reflect terminal outcomes into persisted state.
        while let Some(prog) = rx.recv().await {
            self.apply_progress(&pending, &prog)?;
            let _ = events.send(Event::Download(prog)).await;
        }
        let _ = run.await;
        let _ = events.send(Event::Done).await;
        Ok(())
    }

    /// Reflect a scheduler [`Progress`] event onto the persisted **candidate**
    /// (variation) that owns its md5, then roll the request's status up from its
    /// variations' job states. Keyed by md5 so several variations of one book
    /// update independently (epub `Done` while pdf still `Downloading`).
    ///
    /// Public so the engine can call it under a BRIEF per-list lock per progress
    /// tick while draining a [`DownloadSession`] OFF-lock (`docs/SYNCHRONIZATION.md`
    /// §4) — the lock is held only for this single-row update, never across the
    /// transfer.
    pub fn apply_progress(&mut self, planned: &[PlannedDownload], prog: &Progress) -> Result<()> {
        let md5 = match prog {
            Progress::Resolved { md5, .. }
            | Progress::Bytes { md5, .. }
            | Progress::Stalled { md5, .. }
            | Progress::Retrying { md5, .. }
            | Progress::Done { md5, .. }
            | Progress::Failed { md5, .. }
            | Progress::Cancelled { md5, .. } => md5.clone(),
            Progress::FailingOver { md5, .. } => md5.clone(),
            Progress::Resuming { md5, .. } => md5.clone(),
            Progress::Note { md5, .. } => md5.clone(),
        };
        for p in planned.iter().filter(|p| p.md5 == md5) {
            let mut req = match self.request_at(&p.group_path, p.book_index)? {
                Some(r) => r,
                None => continue,
            };
            let cand = match req.candidates.iter_mut().find(|c| c.md5 == md5) {
                Some(c) => c,
                None => continue,
            };
            let ext = cand.extension.as_ref().map(|e| e.ext());
            let mut job = cand.job.clone().unwrap_or_default();
            // A chronicle entry for the meaningful lifecycle transitions (NOT the
            // high-frequency Bytes/Stalled ticks). Pushed to `req.history` below.
            let mut event: Option<(&'static str, String)> = None;
            match prog {
                Progress::Resolved {
                    host, total_bytes, ..
                } => {
                    job.host = Some(host.clone());
                    job.total_bytes = *total_bytes;
                    job.state = JobState::Downloading;
                    event = Some(("downloading", format!("started on {host}")));
                }
                Progress::Resuming { host, offset, .. } => {
                    // Informational: continuing from an on-disk partial. Don't touch
                    // job state (the Resolved/Bytes events drive it) — just chronicle.
                    event = Some((
                        "resuming",
                        format!("resuming from {} MB on {host}", offset / (1024 * 1024)),
                    ));
                }
                Progress::Bytes {
                    host,
                    bytes_done,
                    total_bytes,
                    speed_bps,
                    eta_secs,
                    ..
                } => {
                    // With hedging, two legs (different hosts) report progress for
                    // the SAME md5. Show the LEADING leg only: ignore a lagging
                    // leg's smaller byte count so the row doesn't flicker between
                    // hosts, and adopt the leading leg's host so host + bytes stay
                    // consistent. (max-based, like the Done/Cancelled arms.)
                    if *bytes_done >= job.bytes_done {
                        // Chronicle the serving CDN edge the FIRST time we see it (and
                        // on any rotation), so the history shows which cdnN.booksdl.lc
                        // actually delivered the bytes — not just the mirror front-door.
                        let new_edge = host.ends_with(".booksdl.lc")
                            && job.host.as_deref() != Some(host.as_str());
                        job.bytes_done = *bytes_done;
                        job.host = Some(host.clone());
                        job.total_bytes = *total_bytes;
                        job.speed_bps = *speed_bps;
                        job.eta_secs = *eta_secs;
                        if new_edge {
                            event = Some(("downloading", format!("serving from {host}")));
                        }
                    }
                }
                Progress::Stalled { speed_bps, .. } => {
                    // Informational only: the slow leg keeps running while the
                    // scheduler races a hedge. Reflect the (low) live speed; the
                    // race resolves into a normal Done/Failed.
                    job.speed_bps = *speed_bps;
                }
                Progress::Retrying {
                    host,
                    attempt,
                    backoff,
                    error,
                    ..
                } => {
                    job.attempts = *attempt;
                    job.last_error = Some(error.clone());
                    event = Some((
                        "retry",
                        format!(
                            "retry attempt {attempt} on {host} after {}s backoff — {error}",
                            backoff.as_secs().max(1)
                        ),
                    ));
                }
                Progress::FailingOver {
                    from_host, error, ..
                } => {
                    job.last_error = Some(error.clone());
                    event = Some((
                        "failover",
                        format!("failing over from {from_host} — {error}"),
                    ));
                }
                Progress::Done {
                    host,
                    path,
                    bytes_written,
                    ..
                } => {
                    // Dedupe (item 7a): the md5 was downloaded once to `path`. If
                    // this variation's own destination differs (a second book
                    // wanting the same md5), copy the verified file there instead
                    // of downloading it again.
                    if &p.destination != path && !p.destination.as_os_str().is_empty() {
                        if let Some(parent) = p.destination.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if let Err(e) = std::fs::copy(path, &p.destination) {
                            job.state = JobState::Failed;
                            job.last_error = Some(format!(
                                "copying deduped md5 from {} to {}: {e}",
                                path.display(),
                                p.destination.display()
                            ));
                            cand.job = Some(job);
                            req.status = roll_up_status(&req);
                            self.store.update_request(
                                self.list_id,
                                &p.group_path,
                                p.book_index,
                                &req,
                            )?;
                            continue;
                        }
                    }
                    job.state = JobState::Done;
                    job.md5_verified = true;
                    job.bytes_done = job.bytes_done.max(*bytes_written);
                    job.output_path = Some(p.destination.to_string_lossy().into_owned());
                    // Transfer finished: clear the live speed/ETA readout.
                    job.speed_bps = None;
                    job.eta_secs = None;
                    event = Some((
                        "done",
                        format!("completed on {host} ({} MB)", bytes_written / (1024 * 1024)),
                    ));
                }
                Progress::Failed { error, .. } => {
                    job.state = JobState::Failed;
                    job.last_error = Some(error.clone());
                    job.speed_bps = None;
                    job.eta_secs = None;
                    event = Some(("failed", format!("failed — {error}")));
                }
                Progress::Note { detail, .. } => {
                    // Diagnostic-only: chronicle a download-path note (edge rotation
                    // outcome, Range-ignored restart). Does NOT touch job state — the
                    // real lifecycle events (Bytes/Failed/Done) still drive it.
                    event = Some(("note", detail.clone()));
                }
                Progress::Cancelled {
                    paused,
                    resume_offset,
                    ..
                } => {
                    // No longer actively moving bytes: drop the live readout.
                    job.speed_bps = None;
                    job.eta_secs = None;
                    if *paused {
                        job.state = JobState::Paused;
                        job.resume_offset = *resume_offset;
                        job.bytes_done = job.bytes_done.max(*resume_offset);
                        event = Some(("paused", "paused".into()));
                    } else {
                        job.state = JobState::Cancelled;
                        job.resume_offset = 0;
                        event = Some(("cancelled", "cancelled".into()));
                    }
                }
            }
            // Phase B: record this download's terminal outcome against its host so
            // the failover chain auto-orders toward sites that actually deliver
            // (host known from the earlier `Resolved`). Best-effort — a stats
            // write must never fail a download.
            let outcome = match job.state {
                JobState::Done => Some(true),
                JobState::Failed => Some(false),
                _ => None,
            };
            let host = job.host.clone();
            cand.job = Some(job);
            // Chronicle the transition (cand borrow released above).
            if let Some((kind, detail)) = event {
                req.log_event(Some(md5.clone()), ext.clone(), kind, detail);
            }
            // Roll the request status up from the per-variation states.
            req.status = roll_up_status(&req);
            self.store
                .update_request(self.list_id, &p.group_path, p.book_index, &req)?;
            if let (Some(ok), Some(host)) = (outcome, host) {
                let _ = self.store.record_site_outcome(
                    &host,
                    crate::store::SiteRole::Download,
                    ok,
                    None,
                );
            }
        }
        Ok(())
    }

    fn set_status(
        &mut self,
        group_path: &[usize],
        book_index: usize,
        status: RequestStatus,
    ) -> Result<()> {
        if let Some(mut req) = self.request_at(group_path, book_index)? {
            req.status = status;
            self.store
                .update_request(self.list_id, group_path, book_index, &req)?;
        }
        Ok(())
    }

    /// Fetch the request at a tree position from the freshly-loaded list.
    fn request_at(&self, group_path: &[usize], book_index: usize) -> Result<Option<BookRequest>> {
        let list = self.snapshot()?;
        Ok(request_at_in(&list, group_path, book_index).cloned())
    }

    /// Raw titles of books in THIS list that "claim" `md5` — i.e. have selected it
    /// or hold a download job for the candidate with that md5. Used by the
    /// cross-list selection guard (which a single per-list orchestrator can't do
    /// alone) to refuse committing one file to two differently-titled books.
    pub fn titles_claiming_md5(&self, md5: &str) -> Result<Vec<String>> {
        let list = self.snapshot()?;
        let mut titles = Vec::new();
        walk(&list.groups, &mut Vec::new(), &mut |_path, _bi, req| {
            let claims = req.selected.as_deref() == Some(md5)
                || req
                    .candidates
                    .iter()
                    .any(|c| c.md5 == md5 && c.job.is_some());
            if claims {
                titles.push(req.input.title.clone());
            }
        });
        Ok(titles)
    }
}

// ---------------------------------------------------------------------------
// Tree helpers (pure)
// ---------------------------------------------------------------------------

/// Resolve a request by its `(group_path, book_index)` position within a list.
fn request_at_in<'a>(
    list: &'a DownloadList,
    group_path: &[usize],
    book_index: usize,
) -> Option<&'a BookRequest> {
    let group = group_at(&list.groups, group_path)?;
    group.books.get(book_index)
}

fn group_at<'a>(groups: &'a [Group], path: &[usize]) -> Option<&'a Group> {
    let (&first, rest) = path.split_first()?;
    let g = groups.get(first)?;
    if rest.is_empty() {
        Some(g)
    } else {
        group_at(&g.subgroups, rest)
    }
}

/// Depth-first walk over every book, invoking `f(group_path, book_index, req)`.
fn walk<'a, F: FnMut(&[usize], usize, &'a BookRequest)>(
    groups: &'a [Group],
    path: &mut Vec<usize>,
    f: &mut F,
) {
    for (gi, g) in groups.iter().enumerate() {
        path.push(gi);
        for (bi, b) in g.books.iter().enumerate() {
            f(path, bi, b);
        }
        walk(&g.subgroups, path, f);
        path.pop();
    }
}

/// Recursive planner: compute destination paths for every requested *variation*
/// (candidate with a `job`) of each book, using the book's **persisted** stable
/// sequence number (`req.seq`, assigned by `assign_sequence_numbers`). Variations
/// of one book share its number and are disambiguated by extension / a `(2)`
/// suffix through the shared `taken` set.
fn plan_groups(
    groups: &[Group],
    root: &Path,
    group_path: &mut Vec<usize>,
    group_names: &mut Vec<String>,
    settings: &crate::model::ListSettings,
    taken: &mut HashSet<PathBuf>,
    out: &mut Vec<PlannedDownload>,
) {
    for (gi, g) in groups.iter().enumerate() {
        group_path.push(gi);
        // Number each (sub)group folder by its order among its siblings — the
        // group order is meaningful, so `<list>/01 - Lift-Off/`, `02 - …`. Mirrors
        // the per-file `NN - ` prefix; sanitized downstream.
        group_names.push(format!("{:02} - {}", gi + 1, g.name));

        for (bi, b) in g.books.iter().enumerate() {
            // The variations requested for download: candidates carrying a job,
            // in candidate (rank) order.
            let requested: Vec<&Candidate> =
                b.candidates.iter().filter(|c| c.is_requested()).collect();
            if requested.is_empty() {
                // Nothing requested → no sequence number, no file. (Discovery /
                // not-found / needs-selection books fall here.)
                continue;
            }
            // The book's persisted, stable number (assigned before planning).
            // Defensive fallback to 1 if somehow unset.
            let seq = b.seq.unwrap_or(1);
            let names: Vec<&str> = group_names.iter().map(|s| s.as_str()).collect();
            let placed = naming::destinations_for_variations(
                root,
                &names,
                settings,
                seq,
                &b.input.title,
                &b.input.authors,
                &requested,
                taken,
            );
            for (md5, dest) in placed {
                out.push(PlannedDownload {
                    group_path: group_path.clone(),
                    book_index: bi,
                    title: b.input.title.clone(),
                    md5,
                    destination: dest,
                });
            }
        }
        plan_groups(
            &g.subgroups,
            root,
            group_path,
            group_names,
            settings,
            taken,
            out,
        );
        group_names.pop();
        group_path.pop();
    }
}

/// Strip a leading `"<digits> - "` sequence-number prefix from a filename, so the
/// remaining part (`"Author - Title.epub"`) is stable across seq renumbering. Used
/// to locate an already-downloaded file regardless of which number it was given.
/// When the naming scheme changes mid-download (numbered folders / source-order
/// seq), a partial `.part` is left at the OLD dest path while planning now points
/// to a NEW one — so the download would restart from scratch. Find such an orphan
/// by its sequence-stripped name anywhere under `out_dir` and adopt the LARGEST
/// one into the current dest's `.part` (only if bigger than what's already there),
/// so the transfer RESUMES. Skips hedge-leg temp parts. Best-effort.
fn adopt_orphaned_part(dest: &Path, out_dir: &Path) {
    let target = crate::download::part_path(dest);
    let want = match dest.file_name().and_then(|n| n.to_str()) {
        Some(n) => strip_seq_prefix(n).to_string(), // stable "author - title.ext"
        None => return,
    };
    if want.is_empty() {
        return;
    }
    let cur_len = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    let mut best: Option<(u64, PathBuf)> = None;
    collect_files_recursive(out_dir, &mut |f| {
        let name = match f.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => return,
        };
        if !name.ends_with(".part") || name.contains(".hedge.") || f == target.as_path() {
            return;
        }
        let base = &name[..name.len() - ".part".len()];
        if strip_seq_prefix(base) != want {
            return;
        }
        let len = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        if best.as_ref().map(|(b, _)| len > *b).unwrap_or(true) {
            best = Some((len, f.to_path_buf()));
        }
    });
    if let Some((len, src)) = best {
        if len > cur_len {
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(&src, &target);
        }
    }
}

pub fn strip_seq_prefix(name: &str) -> &str {
    let after_digits = name.trim_start_matches(|c: char| c.is_ascii_digit());
    if after_digits.len() < name.len() && after_digits.starts_with(" - ") {
        &after_digits[3..]
    } else {
        name
    }
}

/// Remove empty directories UNDER `root` (bottom-up), leaving `root` itself in
/// place. Best-effort: used after relocating files to clear old, now-empty layout
/// folders. A directory holding any file (including a `.part`) is kept. Returns
/// whether `dir` is empty after pruning (so the caller can prune its parent).
fn prune_empty_dirs(dir: &Path) -> bool {
    let mut empty = true;
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if prune_empty_dirs(&path) {
                        let _ = std::fs::remove_dir(&path);
                    } else {
                        empty = false;
                    }
                } else {
                    empty = false;
                }
            }
        }
        Err(_) => empty = false,
    }
    empty
}

/// Visit every regular file under `dir` (recursively), calling `f` for each.
/// Best-effort: unreadable directories are skipped.
pub fn collect_files_recursive(dir: &Path, f: &mut impl FnMut(&Path)) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files_recursive(&path, f);
            } else {
                f(&path);
            }
        }
    }
}

/// Every `.part` file under `dir` (incomplete/abandoned downloads, including
/// hedge-leg temps like `…​.hedge.<md5>.N.part`). Pure — no side effects — so the
/// selection is unit-testable independent of the (Trash-moving) cleanup.
pub fn find_part_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_files_recursive(dir, &mut |f| {
        if f.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(".part"))
            .unwrap_or(false)
        {
            out.push(f.to_path_buf());
        }
    });
    out
}

/// Move every `.part` file under `dir` to the Trash (recoverable — never a hard
/// delete). Returns `(count moved, total bytes freed)`. Best-effort: a file that
/// can't be trashed is skipped.
pub fn trash_part_files(dir: &Path) -> (usize, u64) {
    let mut count = 0usize;
    let mut bytes = 0u64;
    for p in find_part_files(dir) {
        let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        if trash::delete(&p).is_ok() {
            count += 1;
            bytes += sz;
        }
    }
    if count > 0 {
        tracing::info!(count, bytes, dir = %dir.display(), "trashed .part files");
    }
    (count, bytes)
}

/// Search one request's input against the shared client, returning its position
/// alongside the (possibly empty) candidate list. Errors are swallowed into an
/// empty result so one mirror failure never aborts the whole concurrent pass.
async fn search_one(
    search: Arc<SearchClient>,
    group_path: Vec<usize>,
    book_index: usize,
    input: crate::model::BookInput,
) -> (Vec<usize>, usize, Vec<Candidate>) {
    let candidates = search.search(&input).await.unwrap_or_default();
    (group_path, book_index, candidates)
}

/// Map a request status to its compact query-stage string for the `QueryStage`
/// event: `"querying"` while a search is in flight, then the resolved
/// `"matched"` / `"needs_selection"` / `"not_found"`. Other (download-phase)
/// statuses fall back to `"matched"` since by then discovery is settled.
fn query_stage_str(status: &RequestStatus) -> &'static str {
    match status {
        RequestStatus::Querying => "querying",
        RequestStatus::NeedsSelection => "needs_selection",
        RequestStatus::NotFound => "not_found",
        RequestStatus::Queued => "queued",
        _ => "matched",
    }
}

/// The md5 of a book's DOWNLOADED variation: a candidate whose job is `Done`
/// with an `output_path`. `None` when the book has no copy on disk yet. Used by
/// [`Orchestrator::reverify_downloads`] to find which books to re-verify.
fn downloaded_md5(req: &BookRequest) -> Option<String> {
    req.candidates.iter().find_map(|c| {
        let j = c.job.as_ref()?;
        if matches!(j.state, JobState::Done) && j.output_path.is_some() {
            Some(c.md5.clone())
        } else {
            None
        }
    })
}

/// Mark a candidate as requested for download. Idempotent: an existing job
/// (whatever its state) is left untouched, so re-requesting never resets
/// progress; only a not-yet-requested candidate gets a fresh `Pending` job.
fn request_job(cand: &mut Candidate) {
    if cand.job.is_none() {
        cand.job = Some(DownloadJob {
            state: JobState::Pending,
            ..Default::default()
        });
    }
}

/// Roll a request's status up from its per-variation job states (the row
/// summary). With variations in mixed states the request reads as `Downloading`
/// while any is in flight, `Done` once all requested variations finished,
/// `Failed` only if every requested variation failed. Requests with no requested
/// variations keep their discovery status.
fn roll_up_status(req: &BookRequest) -> RequestStatus {
    let a = match req.acquisition() {
        Some(a) => a,
        None => return req.status.clone(),
    };
    if a.downloading > 0 {
        // A slot is held and bytes can move → genuinely Downloading.
        RequestStatus::Downloading
    } else if a.queued > 0 {
        // Submitted but waiting for a host slot — honest "queued/ready", NOT
        // Downloading (the row shouldn't claim a transfer that isn't happening).
        RequestStatus::Ready
    } else if a.paused > 0 {
        // Anything paused (and nothing actively downloading) reads as Paused so
        // the row reflects that work is suspended and resumable.
        RequestStatus::Paused
    } else if a.done == a.requested {
        RequestStatus::Done
    } else if a.done > 0 {
        // Some done, the rest failed/cancelled (none active/paused) → surface as
        // Done for the copies we got, so the row isn't stuck in an error state.
        RequestStatus::Done
    } else if a.cancelled == a.requested {
        RequestStatus::Cancelled
    } else if a.failed > 0 {
        RequestStatus::Failed {
            error: "all requested variations failed".to_string(),
        }
    } else {
        // Only cancelled variations remain.
        RequestStatus::Cancelled
    }
}

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BookInput, ListSettings};
    use crate::search::MirrorConfig;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("fixtures")
            .join("search")
    }

    fn config() -> MirrorConfig {
        let toml = r#"
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
        "#;
        MirrorConfig::from_toml(toml).unwrap()
    }

    fn small_list() -> DownloadList {
        let mut g = Group::new("Batch 1");
        for (t, a) in [
            ("Treasure Island", "Robert Louis Stevenson"),
            ("The Adventures of Tom Sawyer", "Mark Twain"),
            ("Anne of Green Gables", "L. M. Montgomery"),
            ("A Book That Has No Recorded Fixture Anywhere", ""),
        ] {
            g.books.push(BookRequest::new(BookInput {
                title: t.into(),
                authors: if a.is_empty() { vec![] } else { vec![a.into()] },
                ..Default::default()
            }));
        }
        DownloadList {
            title: "Mini".into(),
            settings: ListSettings::default(),
            groups: vec![g],
        }
    }

    async fn drain(mut rx: mpsc::Receiver<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some(e) = rx.recv().await {
            out.push(e);
        }
        out
    }

    #[tokio::test]
    async fn query_all_transitions_and_persists() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/tmp/out").unwrap();

        let (tx, rx) = mpsc::channel(64);
        let ev_task = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = ev_task.await.unwrap();

        let list = orch.snapshot().unwrap();
        let books = &list.groups[0].books;
        // Treasure Island, Tom Sawyer, Anne of Green Gables auto-match.
        assert_eq!(books[0].status, RequestStatus::Matched, "Treasure Island");
        assert_eq!(books[1].status, RequestStatus::Matched, "Tom Sawyer");
        assert_eq!(books[2].status, RequestStatus::Matched, "Anne");
        // No fixture → not found.
        assert_eq!(books[3].status, RequestStatus::NotFound, "no-fixture");
        // Matched requests pre-selected a candidate.
        assert!(books[0].selected.is_some());
        assert!(!books[0].candidates.is_empty());
    }

    #[tokio::test]
    async fn query_all_emits_querying_then_resolved_stage_per_book() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        // Concurrency 1 so the per-book Querying transition is observable before
        // its result lands (with a higher bound several would be in flight at once).
        let mut orch = Orchestrator::new(store, &small_list(), search, "/tmp/out")
            .unwrap()
            .with_query_concurrency(1);

        let (tx, rx) = mpsc::channel(256);
        let ev_task = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let events = ev_task.await.unwrap();

        // Every queued book emitted a `QueryStage{stage:"querying"}` first.
        let querying: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::QueryStage { stage, title, .. } if stage == "querying" => {
                    Some(title.as_str())
                }
                _ => None,
            })
            .collect();
        for t in [
            "Treasure Island",
            "The Adventures of Tom Sawyer",
            "Anne of Green Gables",
            "A Book That Has No Recorded Fixture Anywhere",
        ] {
            assert!(querying.contains(&t), "{t} emitted a querying stage");
        }

        // Resolved stages: the three fixture-backed books matched, the last is
        // not_found — and each was preceded by its querying stage.
        let resolved: std::collections::HashMap<String, String> = events
            .iter()
            .filter_map(|e| match e {
                Event::QueryStage { stage, title, .. } if stage != "querying" => {
                    Some((title.clone(), stage.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            resolved.get("Treasure Island").map(String::as_str),
            Some("matched")
        );
        assert_eq!(
            resolved
                .get("A Book That Has No Recorded Fixture Anywhere")
                .map(String::as_str),
            Some("not_found")
        );

        // Final persisted state: no book is left in the transient Querying state.
        let list = orch.snapshot().unwrap();
        for b in &list.groups[0].books {
            assert_ne!(b.status, RequestStatus::Querying, "{}", b.input.title);
        }
    }

    #[tokio::test]
    async fn plan_assigns_destinations_per_group() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();

        let (tx, rx) = mpsc::channel(64);
        let ev_task = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = ev_task.await.unwrap();

        let planned = orch.plan_downloads().unwrap();
        // The 3 matched books get planned destinations; the not-found one doesn't.
        assert_eq!(planned.len(), 3);
        // Sequence numbers are per-group, 1..=3 in declaration order.
        let seqs: Vec<&str> = planned
            .iter()
            .map(|p| {
                p.destination
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .split(' ')
                    .next()
                    .unwrap()
            })
            .collect();
        assert_eq!(seqs, vec!["01", "02", "03"]);
        // All under /books/<list title>/Batch 1/ (E5: list title is the first
        // folder level, the group is the second).
        for p in &planned {
            assert!(
                p.destination.starts_with("/books/Mini/01 - Batch 1"),
                "{:?}",
                p.destination
            );
            assert!(p.destination.extension().is_some());
        }
        // Plan persisted output_path onto the requested variation's job (the
        // auto-requested best candidate of the first matched book).
        let list = orch.snapshot().unwrap();
        let best = &list.groups[0].books[0].candidates[0];
        assert!(best
            .job
            .as_ref()
            .and_then(|j| j.output_path.as_ref())
            .is_some());
    }

    #[tokio::test]
    async fn per_list_sequence_scope() {
        let mut list = small_list();
        list.settings.seq_per_group = false;
        // Add a second group with one fixture-backed book.
        let mut g2 = Group::new("Batch 2");
        g2.books.push(BookRequest::new(BookInput {
            title: "Anne of Green Gables".into(),
            authors: vec!["L. M. Montgomery".into()],
            ..Default::default()
        }));
        list.groups.push(g2);

        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &list, search, "/books").unwrap();

        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        let planned = orch.plan_downloads().unwrap();
        // Numbering is by fixed IMPORT POSITION across the whole list (per-list scope),
        // counting EVERY book — so the un-matched "No Recorded Fixture" book at
        // position 4 still consumes seq 04. The 4 matched/downloaded books (TI=1,
        // Tom Sawyer=2, Anne-b1=3, Anne-b2=5) therefore plan to 01,02,03,05 — a
        // gap at 04, and a number that never shifts as books match over time.
        let seqs: Vec<String> = planned
            .iter()
            .map(|p| {
                p.destination
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .split(' ')
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(seqs, vec!["01", "02", "03", "05"]);
    }

    /// E3: `backfill_covers` stamps an Open Library cover onto a matched book
    /// that has none from search. Uses the ISBN short-circuit so the lookup is
    /// fully offline (no transport call), and a fast-failing client so the
    /// best-effort local thumbnail fetch can't hang the test.
    #[tokio::test]
    async fn backfill_covers_stamps_open_library_cover() {
        let mut g = Group::new("Batch 1");
        let mut b = BookRequest::new(BookInput {
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            isbn: Some("9781402714672".into()),
            ..Default::default()
        });
        // A matched book with one candidate that carries NO cover.
        b.status = RequestStatus::Matched;
        let mut cand = Candidate {
            md5: "a".repeat(32),
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: Some(Format::Epub),
            size_bytes: None,
            source_host: None,
            cover_url: None,
            score: 0.9,
            job: None,
        };
        request_job(&mut cand);
        b.candidates = vec![cand];
        g.books.push(b);
        let list = DownloadList {
            title: "Covers".into(),
            settings: ListSettings::default(),
            groups: vec![g],
        };

        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let out = std::env::temp_dir().join(format!("cov-{}", std::process::id()));
        let mut orch = Orchestrator::new(store, &list, search, &out).unwrap();

        // Replay client with no fixtures: the ISBN path must not touch it.
        let covers = crate::covers::CoverClient::replay("/nonexistent");
        // Fast-failing HTTP client so the thumbnail fetch errors immediately
        // (swallowed) rather than reaching out to the network.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(1))
            .build()
            .unwrap();

        let n = orch.backfill_covers(&covers, &client).await.unwrap();
        assert_eq!(n, 1, "one book got a cover");
        let snap = orch.snapshot().unwrap();
        assert_eq!(
            snap.groups[0].books[0].candidates[0].cover_url.as_deref(),
            Some("https://covers.openlibrary.org/b/isbn/9781402714672-M.jpg"),
            "OL ISBN cover stamped onto candidate"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// E5: the composed destination is `<out_dir>/<list>/<sub-group>/file` — the
    /// list title is the first folder level and nested sub-groups add further
    /// levels under it. A book in a nested subgroup lands two levels deep.
    #[tokio::test]
    async fn plan_nests_under_list_then_subgroup() {
        // List "My List" → group "Parent" → subgroup "Child" → one fixture book.
        let mut child = Group::new("Child");
        child.books.push(BookRequest::new(BookInput {
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            ..Default::default()
        }));
        let mut parent = Group::new("Parent");
        parent.subgroups.push(child);
        let list = DownloadList {
            title: "My List".into(),
            settings: ListSettings::default(),
            groups: vec![parent],
        };

        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &list, search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        let planned = orch.plan_downloads().unwrap();
        assert_eq!(planned.len(), 1);
        let dest = &planned[0].destination;
        // <out_dir>/<list>/<NN - group>/<NN - subgroup>/<seq - name>.
        assert!(
            dest.starts_with("/books/My List/01 - Parent/01 - Child"),
            "{dest:?}"
        );
        assert!(dest
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("01 "));
    }

    /// `relocate_downloads_to_current_scheme` moves a file finished under the OLD
    #[test]
    fn find_part_files_finds_part_and_hedge_parts_recursively_only() {
        let out = std::env::temp_dir().join(format!("lgdl-parts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let sub = out.join("List/Group");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(out.join("a.epub"), b"x").unwrap(); // not a .part
        std::fs::write(sub.join("b.epub.part"), b"x").unwrap();
        std::fs::write(sub.join("c.epub.hedge.deadbeef.0.part"), b"x").unwrap();
        std::fs::write(sub.join("d.pdf"), b"x").unwrap(); // not a .part
        let mut got: Vec<String> = find_part_files(&out)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, vec!["b.epub.part", "c.epub.hedge.deadbeef.0.part"]);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[tokio::test]
    async fn done_variations_lists_done_and_demote_variation_marks_failed() {
        let mut g = Group::new("Batch");
        let mut b = BookRequest::new(BookInput {
            title: "Treasure Island".into(),
            ..Default::default()
        });
        b.status = RequestStatus::Done;
        b.selected = Some("a".repeat(32));
        b.candidates = vec![Candidate {
            md5: "a".repeat(32),
            title: "Treasure Island".into(),
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
            job: Some(DownloadJob {
                state: JobState::Done,
                output_path: Some("/x/Treasure Island - aaaaaa.epub".into()),
                ..Default::default()
            }),
        }];
        g.books.push(b);
        let list = DownloadList {
            title: "L".into(),
            settings: ListSettings::default(),
            groups: vec![g],
        };
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &list, search, "/x").unwrap();

        let dones = orch.done_variations().unwrap();
        assert_eq!(dones.len(), 1);
        assert_eq!(dones[0].2, "a".repeat(32));
        assert_eq!(
            dones[0].3.as_deref(),
            Some("/x/Treasure Island - aaaaaa.epub")
        );

        let changed = orch
            .demote_variation(
                &dones[0].0,
                dones[0].1,
                &"a".repeat(32),
                "wrong md5 (data lost)",
            )
            .unwrap();
        assert!(changed);
        let snap = orch.snapshot().unwrap();
        let job = snap.groups[0].books[0].candidates[0].job.clone().unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert!(job
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("wrong md5"));
        // Idempotent: a Failed variation is not re-demoted, and no longer "Done".
        assert!(!orch
            .demote_variation(&dones[0].0, dones[0].1, &"a".repeat(32), "x")
            .unwrap());
        assert!(orch.done_variations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn flag_missing_downloads_demotes_only_the_ones_whose_file_is_gone() {
        let out = std::env::temp_dir().join(format!("lgdl-integ-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        let present = out.join("present.epub");
        std::fs::write(&present, b"bytes").unwrap();
        let missing = out.join("gone.epub"); // never created on disk

        let done_cand = |md5: &str, path: &std::path::Path| Candidate {
            md5: md5.to_string(),
            title: "T".into(),
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
            job: Some(DownloadJob {
                state: JobState::Done,
                output_path: Some(path.to_string_lossy().into_owned()),
                ..Default::default()
            }),
        };
        let mut g = Group::new("Batch");
        let mut a = BookRequest::new(BookInput {
            title: "Present".into(),
            ..Default::default()
        });
        a.status = RequestStatus::Done;
        a.selected = Some("a".repeat(32));
        a.candidates = vec![done_cand(&"a".repeat(32), &present)];
        let mut b = BookRequest::new(BookInput {
            title: "Gone".into(),
            ..Default::default()
        });
        b.status = RequestStatus::Done;
        b.selected = Some("b".repeat(32));
        b.candidates = vec![done_cand(&"b".repeat(32), &missing)];
        g.books.extend([a, b]);
        let list = DownloadList {
            title: "My List".into(),
            settings: ListSettings::default(),
            groups: vec![g],
        };
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &list, search, &out).unwrap();

        let n = orch.flag_missing_downloads().unwrap();
        assert_eq!(n, 1, "only the missing-file variation is demoted");
        let snap = orch.snapshot().unwrap();
        let job_of = |title: &str| {
            snap.groups[0]
                .books
                .iter()
                .find(|bk| bk.input.title == title)
                .unwrap()
                .candidates[0]
                .job
                .clone()
                .unwrap()
        };
        assert_eq!(
            job_of("Present").state,
            JobState::Done,
            "present file stays Done"
        );
        let gone = job_of("Gone");
        assert_eq!(
            gone.state,
            JobState::Failed,
            "missing file demoted to Failed"
        );
        assert!(
            gone.last_error
                .as_deref()
                .unwrap_or("")
                .contains("data lost"),
            "reason set: {:?}",
            gone.last_error
        );
        assert_eq!(
            orch.flag_missing_downloads().unwrap(),
            0,
            "a Failed job is not re-demoted"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// reorganize locates a finished file via its recorded `output_path` even when
    /// the on-disk NAME doesn't match the current scheme's stable name — and the
    /// gating check (`reorganize_needed`) agrees with the action (the bug that left
    /// the button grayed while files actually needed moving).
    #[tokio::test]
    async fn reorganize_uses_output_path_when_the_filename_does_not_match() {
        let out = std::env::temp_dir().join(format!("lgdl-reloc-op-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let list_root = out.join("My List");
        std::fs::create_dir_all(&list_root).unwrap();
        // The real file's name does NOT match the dest's "Author - Title" stable
        // name, so a name scan can't find it — only output_path can.
        let real_file = list_root.join("99 - weird old name.epub");
        std::fs::write(&real_file, b"book bytes").unwrap();

        let mut g = Group::new("Batch");
        let mut b = BookRequest::new(BookInput {
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            ..Default::default()
        });
        b.status = RequestStatus::Done;
        b.selected = Some("a".repeat(32));
        b.candidates = vec![Candidate {
            md5: "a".repeat(32),
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: Some(Format::Epub),
            size_bytes: None,
            source_host: None,
            cover_url: None,
            score: 1.0,
            job: Some(DownloadJob {
                state: JobState::Done,
                output_path: Some(real_file.to_string_lossy().into_owned()),
                ..Default::default()
            }),
        }];
        g.books.push(b);
        let list = DownloadList {
            title: "My List".into(),
            settings: ListSettings::default(),
            groups: vec![g],
        };
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &list, search, &out).unwrap();

        let dest = orch.plan_downloads().unwrap()[0].destination.clone();
        assert!(dest != real_file);
        assert!(
            dest.to_string_lossy()
                .contains("Robert Louis Stevenson - Treasure Island"),
            "dest uses the current naming scheme: {}",
            dest.display()
        );

        // CHECK agrees with ACTION: needed=true (via output_path), then it moves.
        assert!(
            orch.reorganize_needed().unwrap(),
            "check must see the misplaced file via output_path"
        );
        let (moved, _skipped, errors) = orch.relocate_downloads_to_current_scheme(&[]).unwrap();
        assert_eq!((moved, errors), (1, 0), "moved via output_path");
        assert!(!real_file.exists(), "old file gone");
        assert!(dest.exists(), "file now at the correct dest");
        assert_eq!(std::fs::read(&dest).unwrap(), b"book bytes");
        assert!(
            !orch.reorganize_needed().unwrap(),
            "idempotent — now canonical"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// flat layout into the current `<list>/…` structure, updates `output_path`,
    /// and is idempotent + safe (a collision is skipped, never overwritten).
    #[tokio::test]
    async fn relocate_moves_old_layout_file_into_list_folder() {
        let out = std::env::temp_dir().join(format!("lgdl-reloc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();

        // One Matched book with a selected, DONE variation.
        let mut g = Group::new("Batch");
        let mut b = BookRequest::new(BookInput {
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            ..Default::default()
        });
        b.status = RequestStatus::Done;
        b.selected = Some("a".repeat(32));
        b.candidates = vec![Candidate {
            md5: "a".repeat(32),
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: Some(Format::Epub),
            size_bytes: None,
            source_host: None,
            cover_url: None,
            score: 1.0,
            job: Some(DownloadJob {
                state: JobState::Done,
                ..Default::default()
            }),
        }];
        g.books.push(b);
        let list = DownloadList {
            title: "My List".into(),
            settings: ListSettings::default(),
            groups: vec![g],
        };

        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &list, search, &out).unwrap();

        // Work out the correct destination, then place the file under the OLD flat
        // layout using the SAME filename (a different folder, what the user has).
        let dest = orch.plan_downloads().unwrap()[0].destination.clone();
        let old_file = out.join(dest.file_name().unwrap());
        std::fs::write(&old_file, b"the book bytes").unwrap();
        assert!(
            old_file != dest,
            "old flat path differs from the list-folder dest"
        );

        // Dry-run says reorganize IS needed while the file sits at the old path.
        assert!(
            orch.reorganize_needed().unwrap(),
            "reorganize_needed() true while a finished file is misplaced"
        );

        let (moved, _skipped, errors) = orch.relocate_downloads_to_current_scheme(&[]).unwrap();
        assert_eq!((moved, errors), (1, 0), "the old-layout file is moved");
        assert!(!old_file.exists(), "old file no longer there");

        // The new path is under the list folder, and output_path now points to it.
        let snap = orch.snapshot().unwrap();
        let new_path = snap.groups[0].books[0].candidates[0]
            .job
            .as_ref()
            .unwrap()
            .output_path
            .clone()
            .unwrap();
        assert!(
            new_path.contains("/My List/"),
            "moved under the list folder: {new_path}"
        );
        assert!(std::path::Path::new(&new_path).exists(), "file at new path");
        assert_eq!(
            std::fs::read(&new_path).unwrap(),
            b"the book bytes",
            "contents preserved"
        );

        // Idempotent: a second run moves nothing, and the dry-run now says so.
        let (moved2, _s2, e2) = orch.relocate_downloads_to_current_scheme(&[]).unwrap();
        assert_eq!((moved2, e2), (0, 0), "second run is a no-op");
        assert!(
            !orch.reorganize_needed().unwrap(),
            "reorganize_needed() false once the layout is canonical"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A book that appears in TWO lists must be DUPLICATED (copied) into each list's
    /// folder by Reorganize — NOT moved out of the other list. Moving it made the two
    /// lists fight over one file every Reorganize, so the button never settled.
    #[tokio::test]
    async fn relocate_duplicates_a_shared_book_across_lists() {
        let out = std::env::temp_dir().join(format!("lgdl-reloc-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();

        let make_list = |title: &str| {
            let mut g = Group::new("Batch");
            let mut b = BookRequest::new(BookInput {
                title: "Treasure Island".into(),
                authors: vec!["Robert Louis Stevenson".into()],
                ..Default::default()
            });
            b.status = RequestStatus::Done;
            b.selected = Some("a".repeat(32));
            b.candidates = vec![Candidate {
                md5: "a".repeat(32),
                title: "Treasure Island".into(),
                authors: vec!["Robert Louis Stevenson".into()],
                year: None,
                publisher: None,
                language: None,
                pages: None,
                extension: Some(Format::Epub),
                size_bytes: None,
                source_host: None,
                cover_url: None,
                score: 1.0,
                job: Some(DownloadJob {
                    state: JobState::Done,
                    ..Default::default()
                }),
            }];
            g.books.push(b);
            DownloadList {
                title: title.into(),
                settings: ListSettings::default(),
                groups: vec![g],
            }
        };

        let list_a = make_list("List A");
        let list_b = make_list("List B");
        let mut orch_a = Orchestrator::new(
            Store::open_in_memory().unwrap(),
            &list_a,
            SearchClient::replay(config(), fixtures_dir()),
            &out,
        )
        .unwrap();
        let mut orch_b = Orchestrator::new(
            Store::open_in_memory().unwrap(),
            &list_b,
            SearchClient::replay(config(), fixtures_dir()),
            &out,
        )
        .unwrap();

        // List A already has the file at its canonical destination.
        let dest_a = orch_a.plan_downloads().unwrap()[0].destination.clone();
        std::fs::create_dir_all(dest_a.parent().unwrap()).unwrap();
        std::fs::write(&dest_a, b"shared book bytes").unwrap();

        let folder_a = orch_a.list_folder().unwrap();
        let dest_b = orch_b.plan_downloads().unwrap()[0].destination.clone();
        assert!(!dest_b.exists(), "list B has no file yet");

        // List B reorganizes with A as a sibling: it must COPY, not move.
        let (n, _s, e) = orch_b
            .relocate_downloads_to_current_scheme(&[folder_a])
            .unwrap();
        assert_eq!((n, e), (1, 0), "B places one file with no errors");
        assert!(dest_b.exists(), "B now has its OWN copy");
        assert!(
            dest_a.exists(),
            "A's copy is untouched — duplicated, not moved (the ping-pong bug)"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Regression for E6: sequence numbers must follow the **source-list order**
    /// of books, NOT the order in which books happen to be requested/downloaded.
    /// Here books are requested in REVERSE source order (mimicking concurrent
    /// search completion / incremental per-book planning), and we assign numbers
    /// incrementally after each request — yet each book ends up with its
    /// source-position number (TI=1, Tom Sawyer=2, Anne=3).
    #[tokio::test]
    async fn seq_follows_source_order_not_request_order() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // Clear the auto-assigned jobs so we control request order precisely.
        for bi in 0..3 {
            let mut req = orch.request_at(&[0], bi).unwrap().unwrap();
            for c in &mut req.candidates {
                c.job = None;
            }
            req.seq = None;
            orch.store
                .update_request(orch.list_id, &[0], bi, &req)
                .unwrap();
        }

        // Request + number books in REVERSE source order: Anne(2), Tom Sawyer(1),
        // TI(0). assign_sequence_numbers runs after each (as begin_download
        // would), so a naive "append next number" scheme would give Anne=1.
        for bi in [2usize, 1, 0] {
            let mut req = orch.request_at(&[0], bi).unwrap().unwrap();
            request_job(req.candidates.first_mut().unwrap());
            orch.store
                .update_request(orch.list_id, &[0], bi, &req)
                .unwrap();
            orch.assign_sequence_numbers().unwrap();
        }

        let list = orch.snapshot().unwrap();
        assert_eq!(list.groups[0].books[0].seq, Some(1), "TI (source #1)");
        assert_eq!(
            list.groups[0].books[1].seq,
            Some(2),
            "Tom Sawyer (source #2)"
        );
        assert_eq!(list.groups[0].books[2].seq, Some(3), "Anne (source #3)");
    }

    #[tokio::test]
    async fn select_then_retry() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // Select an explicit candidate for Anne (index 2).
        let list = orch.snapshot().unwrap();
        let md5 = list.groups[0].books[2].candidates[1].md5.clone();
        orch.select_candidate(&[0], 2, &md5).unwrap();
        let list = orch.snapshot().unwrap();
        assert_eq!(list.groups[0].books[2].status, RequestStatus::Ready);
        assert_eq!(
            list.groups[0].books[2].selected.as_deref(),
            Some(md5.as_str())
        );

        // Retry the not-found book → back to queued, candidates cleared.
        orch.retry(&[0], 3).unwrap();
        let list = orch.snapshot().unwrap();
        assert_eq!(list.groups[0].books[3].status, RequestStatus::Queued);
        assert!(list.groups[0].books[3].candidates.is_empty());
    }

    #[tokio::test]
    async fn query_all_auto_requests_one_best_on_matched() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        assert_eq!(book.status, RequestStatus::Matched);
        // The best candidate (index 0) is auto-requested for download (Pending).
        assert_eq!(
            book.candidates[0].job.as_ref().map(|j| &j.state),
            Some(&JobState::Pending),
            "best variation auto-requested on Matched"
        );
        // No other variation is requested by default (one best copy).
        assert!(
            book.candidates[1..].iter().all(|c| c.job.is_none()),
            "only the best variation is requested by default"
        );
        // Acquisition rolls up exactly one requested variation.
        assert_eq!(book.acquisition().unwrap().requested, 1);

        // NotFound book requests nothing.
        let nf = &list.groups[0].books[3];
        assert_eq!(nf.status, RequestStatus::NotFound);
        assert!(nf.candidates.iter().all(|c| c.job.is_none()));
    }

    #[tokio::test]
    async fn request_and_cancel_a_variation_updates_candidate_and_persists() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // Treasure Island has >= 2 variations: request a second one (a pdf-or-other md5).
        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        assert!(book.candidates.len() >= 2);
        let second = book.candidates[1].md5.clone();
        assert!(book.candidates[1].job.is_none());

        orch.request_variation(&[0], 0, &second).unwrap();
        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        assert_eq!(
            book.candidates[1].job.as_ref().map(|j| &j.state),
            Some(&JobState::Pending),
            "second variation now requested"
        );
        assert_eq!(book.acquisition().unwrap().requested, 2);

        // Idempotent: requesting again doesn't reset an in-progress job.
        {
            let mut req = book.clone();
            req.candidates[1].job.as_mut().unwrap().state = JobState::Downloading;
            orch.store
                .update_request(orch.list_id, &[0], 0, &req)
                .unwrap();
        }
        orch.request_variation(&[0], 0, &second).unwrap();
        let list = orch.snapshot().unwrap();
        assert_eq!(
            list.groups[0].books[0].candidates[1]
                .job
                .as_ref()
                .map(|j| &j.state),
            Some(&JobState::Downloading),
            "re-requesting must not reset progress"
        );

        // Cancel clears the job (it isn't Done).
        // First reset it to Pending so cancel applies.
        {
            let mut req = list.groups[0].books[0].clone();
            req.candidates[1].job.as_mut().unwrap().state = JobState::Pending;
            orch.store
                .update_request(orch.list_id, &[0], 0, &req)
                .unwrap();
        }
        orch.cancel_variation(&[0], 0, &second).unwrap();
        let list = orch.snapshot().unwrap();
        assert!(list.groups[0].books[0].candidates[1].job.is_none());

        // A Done variation survives cancel.
        let first = list.groups[0].books[0].candidates[0].md5.clone();
        {
            let mut req = list.groups[0].books[0].clone();
            req.candidates[0].job.as_mut().unwrap().state = JobState::Done;
            orch.store
                .update_request(orch.list_id, &[0], 0, &req)
                .unwrap();
        }
        orch.cancel_variation(&[0], 0, &first).unwrap();
        let list = orch.snapshot().unwrap();
        assert_eq!(
            list.groups[0].books[0].candidates[0]
                .job
                .as_ref()
                .map(|j| &j.state),
            Some(&JobState::Done),
            "a Done variation is kept on cancel"
        );

        // Unknown md5 errors.
        assert!(orch.request_variation(&[0], 0, &"z".repeat(32)).is_err());
        assert!(orch.cancel_variation(&[0], 0, &"z".repeat(32)).is_err());
    }

    /// Mark candidate `idx` of book (group [0], index 0 = "Treasure Island") `Done` with a
    /// fake output_path, persist, and return its md5.
    fn mark_done(orch: &mut Orchestrator, book: usize, idx: usize, path: &str) -> String {
        let list = orch.snapshot().unwrap();
        let mut req = list.groups[0].books[book].clone();
        let md5 = req.candidates[idx].md5.clone();
        req.candidates[idx].job = Some(DownloadJob {
            state: JobState::Done,
            md5_verified: true,
            output_path: Some(path.into()),
            ..Default::default()
        });
        req.status = RequestStatus::Done;
        orch.store
            .update_request(orch.list_id, &[0], book, &req)
            .unwrap();
        md5
    }

    /// Retrying a FAILED variation must actually re-arm it to `Pending` (the
    /// engine only downloads pending variations). Regression: `request_job` is a
    /// no-op when a job already exists, so a Failed variation's "Retry" did
    /// nothing ("Tom Sawyer" epub: ads.php HTTP 500 → Failed → Retry had no effect).
    #[tokio::test]
    async fn retry_rearms_a_failed_variation_to_pending() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // Mark book 0's first candidate as a FAILED download (mirrors the report).
        let md5 = {
            let list = orch.snapshot().unwrap();
            let mut req = list.groups[0].books[0].clone();
            let md5 = req.candidates[0].md5.clone();
            req.candidates[0].job = Some(DownloadJob {
                state: JobState::Failed,
                last_error: Some("transient: HTTP 500: ads.php failed".into()),
                attempts: 3,
                ..Default::default()
            });
            req.status = RequestStatus::Failed {
                error: "transient: HTTP 500: ads.php failed".into(),
            };
            orch.store
                .update_request(orch.list_id, &[0], 0, &req)
                .unwrap();
            md5
        };

        orch.request_variation(&[0], 0, &md5).unwrap();

        let after = orch.snapshot().unwrap();
        let job = after.groups[0].books[0].candidates[0]
            .job
            .as_ref()
            .expect("variation still has a job");
        assert_eq!(
            job.state,
            JobState::Pending,
            "Failed variation re-armed to Pending"
        );
        assert!(job.last_error.is_none(), "prior error cleared on retry");
        assert_eq!(job.attempts, 0, "attempt budget reset");
        // The engine treats a pending variation as actionable for download.
        assert!(after.groups[0].books[0]
            .candidates
            .iter()
            .any(|c| matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Pending))));
    }

    /// State-loss repro: a SECOND variation (epub) requested on a book that
    /// already has a Done pdf must KEEP its requested job across a re-verify and a
    /// restart. The bug: `reverify_downloads`/`finish_reverify` rebuild
    /// `req.candidates` from the fresh search and preserve only the *downloaded*
    /// variation's job, so a Pending/Failed epub sibling's job is dropped (the
    /// fresh candidate carries no job) → it reverts to "available" (job == None).
    #[tokio::test]
    async fn second_variation_survives_reverify_and_restart() {
        for armed in [JobState::Pending, JobState::Downloading, JobState::Failed] {
            let store = Store::open_in_memory().unwrap();
            let search = SearchClient::replay(config(), fixtures_dir());
            let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
            let (tx, rx) = mpsc::channel(64);
            let t = tokio::spawn(drain(rx));
            orch.query_all(&tx).await.unwrap();
            drop(tx);
            let _ = t.await.unwrap();

            // Book 0 ("Treasure Island") has >= 2 candidates from the fixture. Make the FIRST
            // a Done pdf (a downloaded copy), then request the SECOND as the user's
            // second variation, armed in the state under test.
            let epub_md5 = {
                let list = orch.snapshot().unwrap();
                let book = &list.groups[0].books[0];
                assert!(book.candidates.len() >= 2, "need a pdf + epub sibling");
                book.candidates[1].md5.clone()
            };
            let pdf_md5 = mark_done(&mut orch, 0, 0, "/books/Batch 1/01 - Treasure Island.pdf");

            // Request the epub variation, then force it into the state under test
            // (request_variation always arms Pending; emulate the engine having
            // advanced it to Downloading, or a transfer having Failed).
            orch.request_variation(&[0], 0, &epub_md5).unwrap();
            orch.set_goal_one(&[0], 0, crate::model::Goal::Complete)
                .unwrap();
            if armed != JobState::Pending {
                let mut req = orch.snapshot().unwrap().groups[0].books[0].clone();
                let c = req
                    .candidates
                    .iter_mut()
                    .find(|c| c.md5 == epub_md5)
                    .unwrap();
                c.job.as_mut().unwrap().state = armed.clone();
                req.status = roll_up_status(&req);
                orch.store
                    .update_request(orch.list_id, &[0], 0, &req)
                    .unwrap();
            }

            // Sanity: the epub really is armed before any re-verify.
            let armed_state = orch.snapshot().unwrap().groups[0].books[0]
                .candidates
                .iter()
                .find(|c| c.md5 == epub_md5)
                .and_then(|c| c.job.as_ref())
                .map(|j| j.state.clone());
            assert_eq!(
                armed_state.as_ref(),
                Some(&armed),
                "[{armed:?}] epub armed before reverify"
            );

            // A user "Re-query"/re-verify runs over every Done book — INCLUDING this
            // one, which also has a live epub sibling. This is where the sibling job
            // is at risk.
            let (tx, rx) = mpsc::channel(64);
            let t = tokio::spawn(drain(rx));
            orch.reverify_downloads(&tx).await.unwrap();
            drop(tx);
            let _ = t.await.unwrap();

            // The restart sequence (resume_on_launch): in-flight → Pending, rewind
            // book status, park goals Idle.
            orch.reset_inflight_for_resume().unwrap();
            orch.rewind_inflight_status().unwrap();
            orch.set_goal_all(crate::model::Goal::Idle).unwrap();

            // ASSERT: across re-verify + restart the second variation is STILL
            // requested. Downloading/Failed both resume as Pending (reset_inflight
            // turns Downloading→Pending; Failed stays Failed). Either way: NOT None.
            let after = orch.snapshot().unwrap();
            let book = &after.groups[0].books[0];
            let epub = book
                .candidates
                .iter()
                .find(|c| c.md5 == epub_md5)
                .unwrap_or_else(|| panic!("[{armed:?}] epub candidate vanished entirely"));
            assert!(
                epub.job.is_some(),
                "[{armed:?}] second variation lost its job → shows as 'available' (state-loss bug)"
            );
            // And the Done pdf is untouched.
            let pdf = book
                .candidates
                .iter()
                .find(|c| c.md5 == pdf_md5)
                .expect("done pdf kept");
            assert_eq!(
                pdf.job.as_ref().map(|j| &j.state),
                Some(&JobState::Done),
                "[{armed:?}] done pdf preserved"
            );
        }
    }

    #[tokio::test]
    async fn reverify_flags_when_downloaded_is_not_fresh_top() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // Make the downloaded copy a WRONG book by retitling a candidate to
        // something unrelated to the request ("Treasure Island"), then mark it downloaded.
        // Re-verify must flag it (its own title is a poor match for the request).
        let top_md5 = orch.snapshot().unwrap().groups[0].books[0].candidates[0]
            .md5
            .clone();
        let done_md5 = {
            let list = orch.snapshot().unwrap();
            let mut req = list.groups[0].books[0].clone();
            let outside = req.candidates.len() - 1;
            req.candidates[outside].title = "An Entirely Different Book".into();
            let md5 = req.candidates[outside].md5.clone();
            orch.store
                .update_request(orch.list_id, &[0], 0, &req)
                .unwrap();
            mark_done(
                &mut orch,
                0,
                outside,
                "/books/Batch 1/01 - Treasure Island.pdf",
            );
            md5
        };
        assert_ne!(done_md5, top_md5);

        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        let flagged = orch.reverify_downloads(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();
        assert_eq!(flagged, 1, "the mismatched book is flagged");

        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        assert!(book.review, "review flag set");
        // Fresh top is recommended (candidate index 0 after merge).
        assert_eq!(book.candidates[0].md5, top_md5);
        // The downloaded copy is preserved with its Done job + output_path.
        let dl = book
            .candidates
            .iter()
            .find(|c| c.md5 == done_md5)
            .expect("downloaded copy kept");
        assert_eq!(
            dl.job.as_ref().map(|j| &j.state),
            Some(&JobState::Done),
            "downloaded job preserved"
        );
        assert!(dl
            .job
            .as_ref()
            .and_then(|j| j.output_path.as_ref())
            .is_some());
        // Status stays Done.
        assert_eq!(book.status, RequestStatus::Done);

        // Accepting the current copy clears the review flag (book settles as Done
        // without replacing the file) AND lowers the goal so the engine won't
        // re-verify and re-flag it.
        orch.set_goal_one(&[0], 0, crate::model::Goal::Complete)
            .unwrap();
        orch.accept_review(&[0], 0).unwrap();
        let after = orch.snapshot().unwrap();
        let bk = &after.groups[0].books[0];
        assert!(!bk.review, "review cleared on accept");
        assert_eq!(bk.status, RequestStatus::Done);
        assert_eq!(
            bk.goal,
            crate::model::Goal::Match,
            "goal settled so reverify won't re-flag"
        );
        // Accept recorded WHICH recommendation was declined (the fresh top md5),
        // so a later re-verify can suppress only that same one. The suppress/
        // surface decision itself is unit-tested in `honor_review_after_accept_*`.
        assert_eq!(
            bk.review_dismissed.as_deref(),
            Some(top_md5.as_str()),
            "accept remembered which recommendation was declined"
        );
    }

    /// The chronicle records the download journey: a Resolved→Failed sequence for
    /// a variation lands as "downloading" then "failed" events tagged with its md5.
    #[tokio::test]
    async fn apply_progress_chronicles_the_download_journey() {
        use crate::queue::Progress;
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // Discovery already logged a "discovered" event during query_all.
        let snap = orch.snapshot().unwrap();
        let md5 = snap.groups[0].books[0].candidates[0].md5.clone();
        assert!(
            snap.groups[0].books[0]
                .history
                .iter()
                .any(|e| e.kind == "discovered"),
            "query logged a discovered event"
        );

        let planned = orch.plan_downloads().unwrap();

        // Simulate the scheduler: routed onto a host, resuming from a partial,
        // then failed.
        orch.apply_progress(
            &planned,
            &Progress::Resolved {
                md5: md5.clone(),
                host: "libgen.li".into(),
                total_bytes: Some(1000),
            },
        )
        .unwrap();
        orch.apply_progress(
            &planned,
            &Progress::Resuming {
                md5: md5.clone(),
                host: "libgen.li".into(),
                offset: 5 * 1024 * 1024,
            },
        )
        .unwrap();
        orch.apply_progress(
            &planned,
            &Progress::Failed {
                md5: md5.clone(),
                error: "transient: HTTP 500: ads.php failed".into(),
            },
        )
        .unwrap();

        let h = &orch.snapshot().unwrap().groups[0].books[0].history;
        let dl = h
            .iter()
            .find(|e| e.kind == "downloading")
            .expect("downloading event");
        assert_eq!(dl.md5.as_deref(), Some(md5.as_str()));
        assert!(
            dl.detail.contains("libgen.li"),
            "records the host: {}",
            dl.detail
        );
        let resuming = h
            .iter()
            .find(|e| e.kind == "resuming")
            .expect("resuming event");
        assert!(
            resuming.detail.contains("5 MB"),
            "records the resume offset: {}",
            resuming.detail
        );
        let failed = h.iter().find(|e| e.kind == "failed").expect("failed event");
        assert!(
            failed.detail.contains("500"),
            "records the error: {}",
            failed.detail
        );
        // Chronological: downloading precedes failed.
        let idx = |k: &str| h.iter().position(|e| e.kind == k).unwrap();
        assert!(idx("downloading") < idx("failed"));
    }

    /// A partial left at an OLD-scheme path (different folder + seq) is adopted
    /// into the current dest's `.part` so the download resumes, not restarts.
    /// On resume-reset, the FURTHEST progress (a hedge leg's bigger `.part`) is
    /// promoted into the main dest `.part` instead of being discarded — the
    /// resume-loss regression where a 19 MB hedge leg was deleted and a 5 MB main
    /// leg resumed.
    #[tokio::test]
    async fn reset_inflight_promotes_largest_hedge_partial() {
        use crate::model::HedgeLeg;
        let out = std::env::temp_dir().join(format!("lgdl-hedge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        let dest = out.join("01 - Group/04 - Author - Title.epub");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let main_part = crate::download::part_path(&dest);
        std::fs::write(&main_part, vec![0u8; 5_000]).unwrap(); // small main leg (5 KB)
                                                               // A hedge leg that got FURTHER (19 KB) in its own temp .part.
        let hedge_temp = out.join("01 - Group/04 - Author - Title.epub.hedge.abcd.1");
        std::fs::write(crate::download::part_path(&hedge_temp), vec![0u8; 19_000]).unwrap();

        let mut g = Group::new("Group");
        let mut b = BookRequest::new(BookInput {
            title: "Title".into(),
            ..Default::default()
        });
        b.status = RequestStatus::Downloading;
        b.candidates = vec![Candidate {
            md5: "a".repeat(32),
            title: "Title".into(),
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
            job: Some(DownloadJob {
                state: JobState::Downloading,
                output_path: Some(dest.to_string_lossy().into_owned()),
                hedges: vec![HedgeLeg {
                    md5: "a".repeat(32),
                    temp_path: hedge_temp.to_string_lossy().into_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
        }];
        g.books.push(b);
        let list = DownloadList {
            title: "L".into(),
            settings: ListSettings::default(),
            groups: vec![g],
        };
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &list, search, &out).unwrap();

        orch.reset_inflight_for_resume().unwrap();

        assert_eq!(
            std::fs::metadata(&main_part).unwrap().len(),
            19_000,
            "main .part promoted to the largest leg's progress"
        );
        assert!(
            !crate::download::part_path(&hedge_temp).exists(),
            "hedge leg temp cleaned up after promotion"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn adopt_orphaned_part_migrates_a_renamed_partial() {
        let out = std::env::temp_dir().join(format!("lgdl-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        // Old layout: <out>/Lift-Off/12 - Mark Twain - Tom Sawyer.epub.part (12 bytes)
        let old_dir = out.join("Lift-Off");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(
            old_dir.join("12 - Mark Twain - Tom Sawyer.epub.part"),
            b"123456789012",
        )
        .unwrap();
        // A small fresh partial at the NEW dest (should be replaced by the bigger).
        let new_dir = out.join("01 - Lift-Off");
        std::fs::create_dir_all(&new_dir).unwrap();
        let dest = new_dir.join("04 - Mark Twain - Tom Sawyer.epub");
        std::fs::write(crate::download::part_path(&dest), b"12").unwrap();

        adopt_orphaned_part(&dest, &out);

        let target = crate::download::part_path(&dest);
        assert_eq!(
            std::fs::metadata(&target).unwrap().len(),
            12,
            "the larger orphaned partial was adopted into the current dest .part"
        );
        assert!(
            !old_dir
                .join("12 - Mark Twain - Tom Sawyer.epub.part")
                .exists(),
            "orphan moved (not copied)"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn titles_claiming_md5_reports_books_that_selected_or_hold_a_job_for_it() {
        // Book A selected the md5; book B merely has it as an un-jobbed candidate;
        // book C is unrelated. Only A "claims" it. (The cross-list guard compares
        // these titles to refuse one file under two differently-titled books.)
        let shared = "a".repeat(32);
        let mk_cand = |md5: &str, with_job: bool| Candidate {
            md5: md5.to_string(),
            title: "x".into(),
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
            job: with_job.then(|| DownloadJob {
                state: JobState::Pending,
                ..Default::default()
            }),
        };
        let mut g = Group::new("G");
        let mut a = BookRequest::new(BookInput {
            title: "The Secret Garden".into(),
            ..Default::default()
        });
        a.selected = Some(shared.clone());
        a.candidates = vec![mk_cand(&shared, false)];
        let mut b = BookRequest::new(BookInput {
            title: "Browsing".into(),
            ..Default::default()
        });
        b.candidates = vec![mk_cand(&shared, false)]; // present but not selected, no job
        let mut c = BookRequest::new(BookInput {
            title: "Other".into(),
            ..Default::default()
        });
        c.candidates = vec![mk_cand(&"b".repeat(32), true)];
        g.books.extend([a, b, c]);
        let list = DownloadList {
            title: "L".into(),
            settings: ListSettings::default(),
            groups: vec![g],
        };
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let orch = Orchestrator::new(store, &list, search, "/books").unwrap();

        let claimers = orch.titles_claiming_md5(&shared).unwrap();
        assert_eq!(
            claimers,
            vec!["The Secret Garden".to_string()],
            "only the selecting book claims it"
        );
        let other = orch.titles_claiming_md5(&"b".repeat(32)).unwrap();
        assert_eq!(other, vec!["Other".to_string()]);
    }

    #[test]
    fn honor_review_after_accept_sticks_unless_strictly_better() {
        let mk = |md5: &str, score: f32, done: bool| Candidate {
            md5: md5.to_string(),
            title: "A Little Princess".into(),
            authors: vec![],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: Some(Format::Epub),
            size_bytes: None,
            source_host: None,
            cover_url: None,
            score,
            job: done.then(|| DownloadJob {
                state: JobState::Done,
                ..Default::default()
            }),
        };
        let downloaded = mk("d", 1.0, true);
        let mut req = BookRequest::new(BookInput::default());

        // Accepted (review_dismissed set) + an EQUAL-score alternative → STAY
        // accepted (this is the churn the user hit).
        req.review_dismissed = Some("x".repeat(32));
        req.candidates = vec![downloaded.clone(), mk("e", 1.0, false)];
        assert!(
            !honor_review_after_accept(true, &req, &downloaded),
            "equal-score alt must not re-ask"
        );

        // A strictly HIGHER-scoring copy → re-surface for review.
        req.candidates = vec![downloaded.clone(), mk("e", 1.0 + 0.2, false)];
        assert!(
            honor_review_after_accept(true, &req, &downloaded),
            "a genuinely better copy surfaces"
        );

        // Never accepted (no review_dismissed) → behaves as the raw flag.
        req.review_dismissed = None;
        req.candidates = vec![downloaded.clone(), mk("e", 1.0, false)];
        assert!(
            honor_review_after_accept(true, &req, &downloaded),
            "un-accepted book keeps its flag"
        );

        // Not flagged → never invents a flag.
        req.review_dismissed = Some("x".repeat(32));
        assert!(!honor_review_after_accept(false, &req, &downloaded));
    }

    #[test]
    fn recommended_replacement_prefers_same_format_then_top_excluding_downloaded() {
        let cand = |md5: &str, fmt: Format, done: bool| Candidate {
            md5: md5.to_string(),
            title: "t".into(),
            authors: vec![],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: Some(fmt),
            size_bytes: None,
            source_host: None,
            cover_url: None,
            score: 0.0,
            job: done.then(|| DownloadJob {
                state: JobState::Done,
                output_path: Some("/x".into()),
                ..Default::default()
            }),
        };
        // Downloaded epub + a higher pdf + a sibling epub: prefer the SAME-format
        // (epub) alternative over the top-ranked pdf, and never the downloaded one.
        let mut req = BookRequest::new(BookInput::default());
        req.candidates = vec![
            cand(&"d".repeat(32), Format::Epub, true),  // downloaded
            cand(&"p".repeat(32), Format::Pdf, false),  // top-ranked, different fmt
            cand(&"e".repeat(32), Format::Epub, false), // same-fmt alternative
        ];
        assert_eq!(
            recommended_replacement(&req).as_deref(),
            Some("e".repeat(32).as_str()),
            "same-format alternative preferred"
        );
        // No same-format alternative → fall back to the top-ranked non-downloaded.
        req.candidates.remove(2);
        assert_eq!(
            recommended_replacement(&req).as_deref(),
            Some("p".repeat(32).as_str()),
            "top-ranked non-downloaded fallback"
        );
    }

    #[test]
    fn should_flag_review_distinguishes_right_book_from_wrong() {
        let cand = |title: &str| Candidate {
            md5: "0".repeat(32),
            title: title.into(),
            authors: vec![],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: Some(Format::Epub),
            size_bytes: Some(1),
            source_host: None,
            cover_url: None,
            score: 0.0,
            job: None,
        };
        // Right book, minor title variation → NOT flagged (the Jungle Book case).
        assert!(!should_flag_review(
            "The Jungle Book: Mowgli's Story",
            &cand("The Jungle Book #1")
        ));
        // Subtitle expansion of the same book → NOT flagged.
        assert!(!should_flag_review(
            "Treasure Island",
            &cand("Treasure Island: A Novel")
        ));
        assert!(!should_flag_review(
            "Treasure Island",
            &cand("Treasure Island")
        ));
        // A different / more-specific volume → flagged (the long-sequel case).
        assert!(should_flag_review(
            "Heidi",
            &cand(
                "Heidi: Twenty Thousand Alpine Goats Under the Snowy Mountain, \
                 Being the Eleventh Marvelous Chronicle (Heidi #11)"
            )
        ));
        // A totally unrelated title → flagged.
        assert!(should_flag_review(
            "Treasure Island",
            &cand("Quantum Field Theory")
        ));
    }

    #[tokio::test]
    async fn reverify_does_not_flag_a_good_top_few_copy() {
        // The "right book, not #1" case: the on-disk copy is the RIGHT book but not literally
        // #1 (e.g. a #2/#3 of the matching format). It's still in the top few, so
        // re-verify must NOT push a (cross-format) replacement.
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // Mark candidate index 1 (within the top few) as downloaded.
        let _ = mark_done(&mut orch, 0, 1, "/books/Batch 1/01 - Treasure Island.epub");
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        let flagged = orch.reverify_downloads(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();
        assert_eq!(flagged, 0, "a top-few copy is not flagged for review");
        assert!(!orch.snapshot().unwrap().groups[0].books[0].review);
    }

    #[tokio::test]
    async fn reverify_does_not_flag_when_downloaded_is_fresh_top() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // Mark the TOP candidate (index 0) as the downloaded copy: re-verify must
        // NOT flag it (the on-disk copy is still the best match).
        let top_md5 = mark_done(&mut orch, 0, 0, "/books/Batch 1/01 - Treasure Island.epub");

        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        let flagged = orch.reverify_downloads(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();
        assert_eq!(flagged, 0, "best-copy book is not flagged");

        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        assert!(!book.review);
        assert_eq!(book.candidates[0].md5, top_md5, "top still top");
        assert_eq!(book.status, RequestStatus::Done);
    }

    #[tokio::test]
    async fn replace_download_sets_recommended_pending_and_records_trash() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // Download a non-top copy, then replace with the recommended (top) one.
        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        let top_md5 = book.candidates[0].md5.clone();
        let old_path = "/books/Batch 1/01 - Treasure Island.pdf";
        let old_md5 = mark_done(&mut orch, 0, 1, old_path);

        orch.replace_download(&[0], 0, &top_md5).unwrap();

        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        // Recommended variation is now Pending (enrolled for download).
        let rec = book
            .candidates
            .iter()
            .find(|c| c.md5 == top_md5)
            .expect("recommended kept");
        assert_eq!(
            rec.job.as_ref().map(|j| &j.state),
            Some(&JobState::Pending),
            "recommended set Pending"
        );
        // trash_on_replace records the OLD downloaded file.
        let pending = book.trash_on_replace.as_ref().expect("trash recorded");
        assert_eq!(pending.old_md5, old_md5);
        assert_eq!(pending.old_path, old_path);
    }

    #[tokio::test]
    async fn trash_on_complete_moves_old_file_and_clears_review() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // A REAL temp file standing in for the old downloaded copy.
        let dir = tempfile::tempdir().unwrap();
        let old_file = dir.path().join("old-copy.pdf");
        std::fs::write(&old_file, b"old wrong copy").unwrap();
        assert!(old_file.exists());

        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        let top_md5 = book.candidates[0].md5.clone();
        // Mark the non-top copy as the (real) downloaded file, flag review.
        let old_md5 = mark_done(&mut orch, 0, 1, old_file.to_str().unwrap());
        {
            let list = orch.snapshot().unwrap();
            let mut req = list.groups[0].books[0].clone();
            req.review = true;
            orch.store
                .update_request(orch.list_id, &[0], 0, &req)
                .unwrap();
        }

        // Replace with recommended, then simulate the recommended copy completing.
        orch.replace_download(&[0], 0, &top_md5).unwrap();
        {
            let list = orch.snapshot().unwrap();
            let mut req = list.groups[0].books[0].clone();
            let rec = req
                .candidates
                .iter_mut()
                .find(|c| c.md5 == top_md5)
                .unwrap();
            rec.job = Some(DownloadJob {
                state: JobState::Done,
                md5_verified: true,
                output_path: Some("/books/Batch 1/01 - Treasure Island.epub".into()),
                ..Default::default()
            });
            req.status = RequestStatus::Done;
            orch.store
                .update_request(orch.list_id, &[0], 0, &req)
                .unwrap();
        }

        let trashed = orch.trash_after_replace_done(&[0], 0, &top_md5).unwrap();
        assert!(trashed, "old file trashed on recommended completion");
        // The old file is gone from its path (moved to Trash, not at old_file).
        assert!(!old_file.exists(), "old file moved to Trash");

        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        assert!(!book.review, "review cleared");
        assert!(book.trash_on_replace.is_none(), "pending trash cleared");
        // Old variation no longer reads as downloaded.
        let old = book.candidates.iter().find(|c| c.md5 == old_md5);
        assert!(
            old.map(|c| c.job.is_none()).unwrap_or(true),
            "old variation's Done job dropped"
        );
        // Completing the OLD md5 (not recommended) is a no-op.
        assert!(!orch.trash_after_replace_done(&[0], 0, &old_md5).unwrap());
    }

    #[tokio::test]
    async fn remove_variation_trashes_file_and_re_evaluates_status() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // A REAL file for the downloaded copy, then remove it.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("copy.epub");
        std::fs::write(&file, b"a downloaded copy").unwrap();
        let done_md5 = mark_done(&mut orch, 0, 0, file.to_str().unwrap());

        orch.remove_variation(&[0], 0, &done_md5).unwrap();

        assert!(!file.exists(), "the file was moved to Trash");
        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        // The candidate is removed entirely AND recorded as dismissed.
        assert!(
            book.candidates.iter().all(|c| c.md5 != done_md5),
            "removed candidate is gone from the list"
        );
        assert!(
            book.dismissed.iter().any(|m| m == &done_md5),
            "removed md5 is recorded as dismissed"
        );
        // No variation acquired now, but candidates remain → user must choose.
        assert_eq!(book.status, RequestStatus::NeedsSelection);
        assert!(!book.review);
    }

    #[tokio::test]
    async fn dismissed_md5_is_not_resurfaced_by_requery() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // Remove (dismiss) the top Treasure Island candidate, then re-query: it must NOT
        // come back even though the fresh search still returns it.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.epub");
        std::fs::write(&file, b"x").unwrap();
        let gone = mark_done(&mut orch, 0, 0, file.to_str().unwrap());
        orch.remove_variation(&[0], 0, &gone).unwrap();

        orch.requery_unsettled().unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        assert!(
            book.candidates.iter().all(|c| c.md5 != gone),
            "a dismissed md5 must not be re-surfaced by re-query"
        );
        assert!(book.dismissed.iter().any(|m| m == &gone));
    }

    #[tokio::test]
    async fn query_one_advances_single_book_and_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();

        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        // Advance only Treasure Island (group [0], book 0).
        let acted = orch.query_one(&[0], 0, &tx).await.unwrap();
        assert!(acted, "first query_one advances the book");
        drop(tx);
        let _ = t.await.unwrap();

        let list = orch.snapshot().unwrap();
        assert_eq!(list.groups[0].books[0].status, RequestStatus::Matched);
        // The other books were untouched (still queued).
        assert_eq!(list.groups[0].books[1].status, RequestStatus::Queued);

        // Idempotent: a second call is a no-op (already matched, not queued).
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        let acted = orch.query_one(&[0], 0, &tx).await.unwrap();
        assert!(!acted, "second query_one is a no-op (book already matched)");
        drop(tx);
        let _ = t.await.unwrap();
    }

    #[tokio::test]
    async fn set_goal_all_persists_goal_for_every_book() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let n = orch.set_goal_all(crate::model::Goal::Complete).unwrap();
        assert_eq!(n, 4);
        let list = orch.snapshot().unwrap();
        for b in &list.groups[0].books {
            assert_eq!(b.goal, crate::model::Goal::Complete);
        }
        orch.set_goal_all(crate::model::Goal::Idle).unwrap();
        let list = orch.snapshot().unwrap();
        for b in &list.groups[0].books {
            assert_eq!(b.goal, crate::model::Goal::Idle);
        }
    }

    #[tokio::test]
    async fn reselect_a_different_md5_after_done_requeues_fresh() {
        let store = Store::open_in_memory().unwrap();
        let search = SearchClient::replay(config(), fixtures_dir());
        let mut orch = Orchestrator::new(store, &small_list(), search, "/books").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let t = tokio::spawn(drain(rx));
        orch.query_all(&tx).await.unwrap();
        drop(tx);
        let _ = t.await.unwrap();

        // Treasure Island (index 0) has several kept variations. Seed a book-level job to
        // exercise the legacy swap-after-done path (`select_candidate` clears the
        // old book job when the chosen md5 changes).
        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        assert!(book.candidates.len() >= 2, "need alternate md5s to swap to");
        {
            let mut req = book.clone();
            req.job = Some(crate::model::DownloadJob {
                state: JobState::Done,
                output_path: Some("/books/Batch 1/01 - x.epub".into()),
                ..Default::default()
            });
            orch.store
                .update_request(orch.list_id, &[0], 0, &req)
                .unwrap();
        }
        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        assert!(book.job.is_some(), "seeded a book job");
        let current = book.selected.clone().unwrap();
        let other = book
            .candidates
            .iter()
            .map(|c| c.md5.clone())
            .find(|m| *m != current)
            .expect("a non-selected variation");

        // Mark it downloaded, then swap to a different md5 because the copy was
        // unsatisfactory.
        orch.set_status(&[0], 0, RequestStatus::Done).unwrap();
        orch.select_candidate(&[0], 0, &other).unwrap();

        let list = orch.snapshot().unwrap();
        let book = &list.groups[0].books[0];
        assert_eq!(book.status, RequestStatus::Ready, "re-queued for download");
        assert_eq!(book.selected.as_deref(), Some(other.as_str()));
        assert!(
            book.job.is_none(),
            "swapping md5 must clear the old job so the new copy downloads fresh"
        );
    }
}
