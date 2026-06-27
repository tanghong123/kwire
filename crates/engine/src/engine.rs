//! The goal-driven execution engine (driver task) — `docs/EXECUTION_MODEL.md` §6.
//!
//! A single long-lived task continuously reconciles each book's CURRENT state
//! (its `status`) toward its `goal`, doing ALL network I/O **off** the library
//! lock. The flow of one tick is:
//!
//!   1. PLAN — under a BRIEF library lock, clone every list's per-orchestrator
//!      `Arc<Mutex<Orchestrator>>`; drop the library lock. Then, under each
//!      orchestrator's OWN brief lock, snapshot it and collect ACTIONABLE books
//!      (`status < goal`, not blocked, within budgets).
//!   2. EXECUTE — for each actionable book, spawn a bounded worker that locks
//!      ONLY that orchestrator, runs the single-book transition (search OR
//!      download — network, NO library lock), persists the result, emits events.
//!   3. WAIT — on the wake `Notify` (a command changed a goal, or a worker
//!      finished) or a short idle timer, then repeat.
//!
//! The library lock is therefore never held across network I/O, so no command is
//! ever starved → the app never freezes (the whole point). Transitions are
//! idempotent + monotonic (they re-read the current state before committing), so
//! a command that changes the goal concurrently is safe.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex, Semaphore};

use libgen_core::model::{DownloadList, Goal, JobState, RequestStatus};
use libgen_core::orchestrator::{Event, Orchestrator};
use libgen_core::queue::{Progress, Scheduler};

use crate::state::{Config, EngineHandles};
use crate::{bridge, viewmodel};

/// How long the driver sleeps between ticks when nothing wakes it. A safety net:
/// goal/state changes wake it immediately via the `Notify`.
const IDLE_TICK: Duration = Duration::from_millis(750);

/// Minimum spacing between in-session "completed-but-stuck download" reconciliation
/// sweeps (see [`reconcile_inflight_sweep`]). The tick loop wakes often (≤750 ms,
/// plus on every goal/state change), so this throttles the sweep to a modest
/// cadence instead of re-walking every list on each wake.
const RECONCILE_CADENCE: Duration = Duration::from_secs(30);

/// One unit of reconciliation work the engine will perform: a single book and the
/// transition it needs next.
#[derive(Clone)]
struct WorkItem {
    list_id: String,
    orch: Arc<Mutex<Orchestrator>>,
    group_path: Vec<usize>,
    book_index: usize,
    kind: WorkKind,
    /// For a Download item, the specific variation to fetch — so each variation is
    /// its own dispatch unit (parallel). `None` for Query/Reverify (per book).
    md5: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkKind {
    /// New → Querying → resolved (search + match).
    Query,
    /// Matched/Ready → Downloading → Done (drive the scheduler).
    Download,
    /// A `Done` book whose goal was (re-)raised to Complete after a Re-query:
    /// re-verify against the fresh top candidate (file kept).
    Reverify,
}

/// A per-book state-change event for the UI (extends `query://book` +
/// `download://progress`). Emitted as `engine://book` whenever the engine
/// advances a book or a command changes its goal. JSON shape (see report):
/// `{ list_id, book_id, status, goal }` where `status` and `goal` are the
/// snake_case strings of the engine's `RequestStatus` / `Goal`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BookStatePayload {
    pub list_id: String,
    pub book_id: String,
    pub status: String,
    pub goal: String,
}

/// Spawn an engine driver for a test / headless harness against explicit shared
/// [`EngineHandles`] and an [`EngineEmitter`]. Returns nothing; the task runs
/// until the runtime is dropped.
pub fn spawn_with<E: EngineEmitter>(handles: EngineHandles, emitter: E) {
    tokio::spawn(async move {
        run_engine(handles, emitter).await;
    });
}

/// A no-op [`EngineEmitter`] for headless tests (drops every event).
#[doc(hidden)]
#[derive(Clone, Copy, Default)]
pub struct NoopEmitter;

impl EngineEmitter for NoopEmitter {
    fn emit_event(&self, _list_id: &str, _shape: &DownloadList, _ev: &Event) {}
    fn emit_book_state(&self, _payload: BookStatePayload) {}
}

/// Sink for the engine's live events. The Tauri front end consumes them; tests
/// substitute a collector / no-op so the driver can run headlessly.
pub trait EngineEmitter: Send + Sync + 'static {
    /// Forward an orchestrator [`Event`] (query stage / planned / download
    /// progress) for `list_id`, given a `shape` snapshot to translate tree
    /// positions to flat `bkN` ids.
    fn emit_event(&self, list_id: &str, shape: &DownloadList, ev: &Event);
    /// Emit a per-book state/goal change (`engine://book`).
    fn emit_book_state(&self, payload: BookStatePayload);
    /// Ask the front end to refresh the whole library view (`library://refresh`).
    /// Used after a background job-state change the per-tick events don't cover —
    /// e.g. the in-session completed-but-stuck reconciliation flips a job to Done.
    fn emit_refresh(&self) {}
}

/// Identity of one in-flight transition, so the driver never spawns a second
/// worker for the same unit. Includes the variation md5 for Download items so two
/// variations of ONE book dispatch as SEPARATE units and download in PARALLEL
/// (`None` for per-book Query/Reverify, which stay one-per-book).
type BookKey = (String, Vec<usize>, usize, Option<String>);

fn item_key(item: &WorkItem) -> BookKey {
    (
        item.list_id.clone(),
        item.group_path.clone(),
        item.book_index,
        item.md5.clone(),
    )
}

/// The driver loop. Runs until the process exits.
///
/// Workers are **detached**: a tick spawns the actionable work and returns
/// immediately to WAIT, so a long download never blocks planning/executing work
/// for OTHER books or lists. Re-planning is safe because (a) every transition is
/// idempotent + monotonic and (b) an `inflight` set keyed by book stops the same
/// book being spawned twice before its status flips.
async fn run_engine<E: EngineEmitter>(handles: EngineHandles, emitter: E) {
    let emitter = Arc::new(emitter);
    let handles = Arc::new(handles);
    // Global query/reverify concurrency budget (downloads are bounded separately
    // by the per-host scheduler). Persistent across ticks.
    let query_budget = handles
        .config
        .lock()
        .map(|c| c.app.query_concurrency.max(1))
        .unwrap_or(8);
    let gate = Arc::new(Semaphore::new(query_budget));
    let inflight: Arc<Mutex<std::collections::HashSet<BookKey>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));

    // Download worker pool (pull model — docs/DOWNLOAD_SCHEDULING.md). A pool of
    // `G = max_concurrent_downloads` workers each pull ONE queued book and download
    // it, so a book is never "spawned then parked on a slot": it waits in this
    // queue (state: queued) until a free worker pulls it (state: downloading). The
    // number of downloading books therefore equals the number of busy workers (≤ G).
    //
    // The pool is RESIZED LIVE to track the setting (see the reconcile step in the
    // loop), so changing "Max concurrent downloads" takes effect without a restart.
    let (dl_tx, dl_rx) = mpsc::unbounded_channel::<WorkItem>();
    let dl_rx = Arc::new(Mutex::new(dl_rx));
    let dl_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // In-session reconciliation cadence: a completed-but-stuck download (its
    // `Progress::Done` lost to an edit/re-query race) is otherwise frozen in the
    // downloading view until relaunch. Sweep all lists every `RECONCILE_CADENCE`
    // and promote any whose file is complete + md5-verifies. First sweep runs on
    // the first tick (`Instant::now() - cadence`).
    let mut last_reconcile = std::time::Instant::now() - RECONCILE_CADENCE;

    loop {
        // Reconcile the pool size up to the current setting: GROW by spawning
        // workers here (a worker retires itself when the target drops — see
        // `download_worker`). On the first iteration this spawns the initial `G`.
        let target = handles
            .config
            .lock()
            .map(|c| c.app.max_concurrent_downloads.max(1))
            .unwrap_or(5);
        while dl_count.load(Ordering::SeqCst) < target {
            dl_count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(download_worker(
                Arc::clone(&handles),
                Arc::clone(&emitter),
                Arc::clone(&dl_rx),
                Arc::clone(&inflight),
                Arc::clone(&dl_count),
            ));
        }
        tick(&handles, &emitter, &gate, &inflight, &dl_tx).await;
        // In-session "completed-but-stuck download" reconciliation, throttled to
        // `RECONCILE_CADENCE` (the loop wakes far more often). Cheap when idle: for
        // a settled list the worklist is empty and no file is hashed.
        if last_reconcile.elapsed() >= RECONCILE_CADENCE {
            last_reconcile = std::time::Instant::now();
            reconcile_inflight_sweep(&handles, &emitter, &inflight).await;
        }
        // WAIT: woken by a goal/state change or a finished worker, else idle tick.
        tokio::select! {
            _ = handles.engine_wake.notified() => {}
            _ = tokio::time::sleep(IDLE_TICK) => {}
        }
    }
}

/// One reconciliation tick: PLAN actionable work, then EXECUTE it by spawning a
/// detached worker per book (skipping books already in flight). Returns promptly —
/// it does NOT await the workers.
async fn tick<E: EngineEmitter>(
    handles: &Arc<EngineHandles>,
    emitter: &Arc<E>,
    gate: &Arc<Semaphore>,
    inflight: &Arc<Mutex<std::collections::HashSet<BookKey>>>,
    dl_tx: &mpsc::UnboundedSender<WorkItem>,
) {
    // --- PLAN ---
    // BRIEF library lock: clone the per-orch Arcs, then drop the guard.
    let arcs = {
        let lib = handles.library.lock().await;
        lib.all_arcs()
    };

    // For each orchestrator, take its OWN brief lock, snapshot, collect work.
    let mut work: Vec<WorkItem> = Vec::new();
    for (list_id, orch) in &arcs {
        let snap = {
            let guard = orch.lock().await;
            match guard.snapshot() {
                Ok(s) => s,
                Err(_) => continue,
            }
        };
        collect_actionable(list_id, orch, &snap, &mut work);
    }
    if work.is_empty() {
        return;
    }

    // --- EXECUTE ---
    // Build the scheduler lazily iff any download work exists (avoids spinning one
    // up just to discover/reverify). If it can't be built, drop download items.
    let needs_download = work.iter().any(|w| w.kind == WorkKind::Download);
    let scheduler: Option<Arc<Scheduler>> = if needs_download {
        ensure_scheduler_from(&handles.scheduler, &handles.config, None)
            .await
            .ok()
    } else {
        None
    };

    // Reserve in-flight keys up front (one short lock) so a book is handled once.
    // Download items are ENQUEUED to the worker pool (a free worker pulls them);
    // query/reverify items are spawned directly under the query budget.
    let mut to_run: Vec<WorkItem> = Vec::new();
    {
        let mut guard = inflight.lock().await;
        for item in work {
            if item.kind == WorkKind::Download && scheduler.is_none() {
                continue;
            }
            let key = item_key(&item);
            if guard.insert(key) {
                if item.kind == WorkKind::Download {
                    // Queue for the download worker pool; a worker transitions the
                    // book to `downloading` only when it actually pulls it.
                    let _ = dl_tx.send(item);
                } else {
                    to_run.push(item);
                }
            }
        }
    }

    for item in to_run {
        let key = item_key(&item);
        let gate = Arc::clone(gate);
        let emitter = Arc::clone(emitter);
        let wake = Arc::clone(&handles.engine_wake);
        let inflight = Arc::clone(inflight);
        tokio::spawn(async move {
            // Query/reverify share the query budget.
            let _permit = gate.acquire_owned().await.expect("engine gate");
            run_item(&emitter, None, item).await;
            inflight.lock().await.remove(&key);
            // A finished worker may unblock the next transition (e.g. a Matched
            // book is now downloadable): wake the driver to re-plan promptly.
            wake.notify_one();
        });
    }
}

/// In-session sweep for **stuck (not-Done) downloads**: across every loaded list,
/// reuse [`reconcile_completed_inflight`] (the SAME judgement the startup
/// integrity scan uses) to fix any in-flight variation that has NO live transport:
/// a complete file → `Done`, a partial/absent file → re-queued to resume, and one
/// over the attempt cap → `Failed`.
///
/// LIVENESS AUTHORITY: the engine's `inflight` set is the source of truth for what
/// is genuinely mid-download. We snapshot the md5s it holds (a download `BookKey`
/// carries `Some(md5)`) and pass them as the live set, so a slow-but-alive transfer
/// is never touched. If any list changed, ask the front end to refresh so the
/// reconciled book updates live (no relaunch). Cheap when nothing is stuck: a
/// settled list yields an empty worklist and hashes no files.
async fn reconcile_inflight_sweep<E: EngineEmitter>(
    handles: &Arc<EngineHandles>,
    emitter: &Arc<E>,
    inflight: &Arc<Mutex<std::collections::HashSet<BookKey>>>,
) {
    let arcs = {
        let lib = handles.library.lock().await;
        lib.all_arcs()
    };
    // Snapshot the live download md5s (a download key is `(.., Some(md5))`).
    let live_md5s: std::collections::HashSet<String> = {
        let guard = inflight.lock().await;
        guard.iter().filter_map(|k| k.3.clone()).collect()
    };
    let mut fixed = 0usize;
    for (_, orch) in &arcs {
        fixed += reconcile_completed_inflight(orch, &live_md5s, "engine in-session sweep").await;
    }
    if fixed > 0 {
        emitter.emit_refresh();
    }
}

/// One download worker (there are `G` of them — `max_concurrent_downloads`). Each
/// loops forever: pull the next queued book, ensure the shared scheduler, run the
/// single-book download transition, then free its in-flight key and wake the
/// driver. Holding the receiver lock across `recv()` serializes the PULL so each
/// queued book goes to exactly one free worker; the (slow) download runs with the
/// lock released, so all `G` workers download concurrently.
async fn download_worker<E: EngineEmitter>(
    handles: Arc<EngineHandles>,
    emitter: Arc<E>,
    rx: Arc<Mutex<mpsc::UnboundedReceiver<WorkItem>>>,
    inflight: Arc<Mutex<std::collections::HashSet<BookKey>>>,
    dl_count: Arc<std::sync::atomic::AtomicUsize>,
) {
    loop {
        // SHRINK: if the live concurrency setting dropped below the current pool
        // size, retire this worker (after any job it held has finished).
        let target = handles
            .config
            .lock()
            .map(|c| c.app.max_concurrent_downloads.max(1))
            .unwrap_or(5);
        if dl_count.load(Ordering::SeqCst) > target {
            dl_count.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        // Pull the next queued book, but wake periodically so an IDLE worker can
        // still notice a shrink and retire. (When items are flowing, `recv`
        // returns immediately, so this poll adds no latency to active downloads.)
        let item = {
            let mut guard = rx.lock().await;
            match tokio::time::timeout(Duration::from_secs(2), guard.recv()).await {
                Ok(Some(i)) => i,
                Ok(None) => {
                    dl_count.fetch_sub(1, Ordering::SeqCst);
                    return; // channel closed → shutting down
                }
                Err(_) => continue, // recv timeout → re-check the target
            }
        };
        let key = item_key(&item);
        // Reuse the shared scheduler (built lazily once).
        let scheduler = ensure_scheduler_from(&handles.scheduler, &handles.config, None)
            .await
            .ok();
        if let Some(s) = scheduler {
            run_item(&emitter, Some(&s), item).await;
        }
        inflight.lock().await.remove(&key);
        handles.engine_wake.notify_one();
    }
}

/// Scan one list's snapshot and append the actionable work for it. A book is
/// actionable when its CURRENT `status` is behind its `goal` AND it is not blocked
/// (NeedsSelection awaiting a pick, NotFound, Failed, or already in-flight).
fn collect_actionable(
    list_id: &str,
    orch: &Arc<Mutex<Orchestrator>>,
    snap: &DownloadList,
    out: &mut Vec<WorkItem>,
) {
    let positions = bridge::positions(snap);
    for p in positions {
        let req = match group_book(snap, &p.group_path, p.book_index) {
            Some(r) => r,
            None => continue,
        };
        let kind = match actionable_kind(req) {
            Some(k) => k,
            None => continue,
        };
        if kind == WorkKind::Download {
            // One item PER pending variation, so a book's variations dispatch to
            // separate workers and download in parallel (not one-book-at-a-time).
            for c in &req.candidates {
                if matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Pending)) {
                    out.push(WorkItem {
                        list_id: list_id.to_string(),
                        orch: Arc::clone(orch),
                        group_path: p.group_path.clone(),
                        book_index: p.book_index,
                        kind,
                        md5: Some(c.md5.clone()),
                    });
                }
            }
        } else {
            out.push(WorkItem {
                list_id: list_id.to_string(),
                orch: Arc::clone(orch),
                group_path: p.group_path,
                book_index: p.book_index,
                kind,
                md5: None,
            });
        }
    }
}

/// Decide what transition (if any) a book needs next, honoring its goal and the
/// blocked/in-flight rules.
fn actionable_kind(req: &libgen_core::model::BookRequest) -> Option<WorkKind> {
    // Idle goal → never act.
    if req.goal == Goal::Idle {
        return None;
    }
    // New: discover it (both Match and Complete goals want this).
    if req.status == RequestStatus::Queued {
        return Some(WorkKind::Query);
    }
    // A requested variation still in `Pending` should be DOWNLOADED whenever the
    // goal wants completion — REGARDLESS of the coarse book status. That status is
    // a roll-up and can lag the per-variation state: a book stuck reading
    // `Downloading` while its only variation is actually `Pending` (no worker
    // running), or a user picking a copy on a `NeedsSelection`/`Failed` book. Keying
    // dispatch off the pending variation (not the status) un-sticks those. The
    // engine's `inflight` set dedups books that are GENUINELY mid-download — a real
    // in-flight transfer has its variation in `Downloading`/`Resolving`/`Verifying`
    // (not `Pending`), so `has_pending_variation` is false for it and this never
    // double-dispatches an active download.
    if req.goal >= Goal::Complete && has_pending_variation(req) {
        return Some(WorkKind::Download);
    }
    // A `Done` book a Re-query just raised to `Complete` (with no pending variation)
    // → re-verify it ONCE against the fresh top candidate (the file is kept).
    // `run_item` then lowers the goal to `Match` so it settles and isn't re-verified
    // every tick. A Done book at goal `Match`/`Idle` has reached its goal.
    if req.status == RequestStatus::Done && req.goal >= Goal::Complete {
        return Some(WorkKind::Reverify);
    }
    None
}

/// Whether the book has a requested variation still in `Pending` (queued for
/// download but not yet started / done).
fn has_pending_variation(req: &libgen_core::model::BookRequest) -> bool {
    req.candidates
        .iter()
        .any(|c| matches!(c.job.as_ref().map(|j| &j.state), Some(JobState::Pending)))
}

/// Run one work item: lock ONLY its orchestrator, run the single-book transition
/// (network off the library lock), and forward events. After the transition,
/// emit an `engine://book` state event from the freshly-persisted book.
async fn run_item<E: EngineEmitter>(
    emitter: &Arc<E>,
    scheduler: Option<&Arc<Scheduler>>,
    item: WorkItem,
) {
    // A `shape` snapshot (brief lock) lets the emitter translate tree positions to
    // flat ids; transitions mutate statuses, not tree shape, so it stays valid.
    let (tx, mut rx) = mpsc::channel::<Event>(1024);
    let list_id = item.list_id.clone();
    let emitter_pump = Arc::clone(emitter);
    let shape = {
        let guard = item.orch.lock().await;
        match guard.snapshot() {
            Ok(s) => s,
            Err(_) => return,
        }
    };
    let pump = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            emitter_pump.emit_event(&list_id, &shape, &ev);
        }
    });

    // CRITICAL (see docs/EXECUTION_MODEL.md §sync): the per-list orchestrator lock
    // guards the store/state ONLY. The network (search/download) must run with the
    // lock RELEASED, or every book in a list serializes. The `inflight` set already
    // gives per-book mutual exclusion, so it's safe to release between phases.
    match item.kind {
        WorkKind::Query => {
            // begin (brief lock) → search + match OFF-lock → finish (brief lock).
            let prep = {
                let mut g = item.orch.lock().await;
                match g.begin_query(&item.group_path, item.book_index, &tx).await {
                    Ok(Some((input, settings))) => Some((input, settings, g.search_client())),
                    _ => None,
                }
            };
            if let Some((input, settings, search)) = prep {
                // No lock held here — concurrent with every other book's search.
                let candidates = search.search(&input).await.unwrap_or_default();
                // `evaluate` dispatches on input.freeform: a free-form add ("title
                // author" as one string) is scored against each candidate's
                // title+author combined, not structured field-vs-field.
                let outcome = libgen_core::matching::evaluate(&input, candidates, &settings);
                let mut g = item.orch.lock().await;
                let _ = g
                    .finish_query(&item.group_path, item.book_index, outcome, &tx)
                    .await;
            }
        }
        WorkKind::Reverify => {
            // begin (brief lock) → search + match OFF-lock → finish (brief lock).
            // Mirrors the Query dance so re-verifies in one list run concurrently.
            let prep = {
                let g = item.orch.lock().await;
                g.begin_reverify(&item.group_path, item.book_index)
                    .ok()
                    .flatten()
            };
            if let Some(prep) = prep {
                // No lock held here — concurrent with every other book's search.
                let fresh = prep.search.search(&prep.input).await.unwrap_or_default();
                // Re-verify a free-form add the same way it was first matched
                // (`evaluate` dispatches on prep.input.freeform).
                let outcome = libgen_core::matching::evaluate(&prep.input, fresh, &prep.settings);
                let mut g = item.orch.lock().await;
                let _ = g
                    .finish_reverify(
                        &item.group_path,
                        item.book_index,
                        outcome,
                        &prep.downloaded_md5,
                        &tx,
                    )
                    .await;
                // Settle the verified book so it isn't re-verified every tick.
                let _ = g.set_goal_one(&item.group_path, item.book_index, Goal::Match);
            }
        }
        WorkKind::Download => {
            if let Some(s) = scheduler {
                // begin (brief lock → plan + mark Downloading + spawn scheduler.run)
                // → drain rx OFF-lock (apply_progress under a BRIEF lock per tick)
                // → finish (brief lock → settle). The per-list lock is NEVER held
                // across the transfer, so books in a list download concurrently
                // (`docs/SYNCHRONIZATION.md` §4).
                let session = {
                    let mut g = item.orch.lock().await;
                    g.begin_download(
                        s,
                        &item.group_path,
                        item.book_index,
                        item.md5.as_deref(),
                        &tx,
                    )
                    .await
                    .ok()
                    .flatten()
                };
                if let Some(mut session) = session {
                    let mut completed: Vec<String> = Vec::new();
                    // Drain progress with NO orchestrator lock held across rx.recv().
                    while let Some(prog) = session.rx.recv().await {
                        if let Progress::Done { md5, .. } = &prog {
                            completed.push(md5.clone());
                        }
                        {
                            let mut g = item.orch.lock().await;
                            if let Err(e) = g.apply_progress(&session.pending, &prog) {
                                tracing::warn!(error = %e, "apply_progress failed");
                            }
                        }
                        let _ = tx.send(Event::Download(prog)).await;
                    }
                    let _ = session.run.await;
                    let mut g = item.orch.lock().await;
                    let _ = g
                        .finish_download(&item.group_path, item.book_index, &completed)
                        .await;
                    // Settle a normally-completed Done book's goal to `Match` so the
                    // engine doesn't then re-verify it (re-verify needs an explicit
                    // Re-query to raise the goal back to Complete).
                    if let Ok(snap) = g.snapshot() {
                        if let Some(req) = group_book(&snap, &item.group_path, item.book_index) {
                            let settled = req.status == RequestStatus::Done
                                && !req.candidates.iter().any(|c| {
                                    matches!(
                                        c.job.as_ref().map(|j| &j.state),
                                        Some(JobState::Pending)
                                    )
                                });
                            if settled {
                                let _ =
                                    g.set_goal_one(&item.group_path, item.book_index, Goal::Match);
                            }
                        }
                    }
                }
            }
        }
    }

    // Emit the freshly-persisted per-book state for the UI (brief lock).
    {
        let guard = item.orch.lock().await;
        if let Ok(snap) = guard.snapshot() {
            if let Some(req) = group_book(&snap, &item.group_path, item.book_index) {
                let book_id = bridge::flat_id_in(&snap, &item.group_path, item.book_index)
                    .unwrap_or_else(|| format!("bk{}", item.book_index));
                emitter.emit_book_state(BookStatePayload {
                    list_id: item.list_id.clone(),
                    book_id,
                    status: viewmodel::discovery_str(&req.status).to_string(),
                    goal: goal_str(req.goal).to_string(),
                });
            }
        }
    }
    drop(tx);
    let _ = pump.await;
}

/// The snake_case string of a [`Goal`] for the `engine://book` event.
#[allow(dead_code)]
fn goal_str(goal: Goal) -> &'static str {
    match goal {
        Goal::Idle => "idle",
        Goal::Match => "match",
        Goal::Complete => "complete",
    }
}

/// Resolve a book by tree position within a snapshot (engine-local helper).
fn group_book<'a>(
    list: &'a DownloadList,
    group_path: &[usize],
    book_index: usize,
) -> Option<&'a libgen_core::model::BookRequest> {
    let mut groups = &list.groups;
    let (last, parents) = group_path.split_last()?;
    for &gi in parents {
        groups = &groups.get(gi)?.subgroups;
    }
    groups.get(*last)?.books.get(book_index)
}

// ---------------------------------------------------------------------------
// Scheduler building (shared by the engine and the commands layer)
// ---------------------------------------------------------------------------

/// Max download attempts a sessionless stuck variation may be re-queued before the
/// reconciliation gives up on it and marks it `Failed`. Stops a dead source from
/// thrashing (re-queue → fail-fast → re-queue …) forever; the failed copy becomes a
/// visible, user-retryable state instead.
pub const RECONCILE_MAX_ATTEMPTS: u32 = 3;

/// Session-gated reconciliation of every **in-flight-but-not-Done** variation in
/// ONE list. The `live_md5s` set is the LIVENESS AUTHORITY: a variation whose md5
/// has an in-flight transport in the engine's `inflight` set is NEVER touched (no
/// false kill of a slow-but-alive transfer). On startup nothing is live, so the
/// set is empty and every stuck variation qualifies.
///
/// For each SESSIONLESS in-flight variation (its persisted job says
/// `Downloading`/`Resolving`/`Verifying`/`Pending` but no transport is running):
///   * final file present + size matches (== `total_bytes` when known) + content
///     md5 verifies → **Done** (the lost `Progress::Done` reconciled in);
///   * file partial or absent → **re-queue** (`state → Pending`, `attempts += 1`,
///     KEEPING `resume_offset`/`.part`) so the engine's drive loop resumes it. We
///     never call `begin_download` here — we only fix persisted state;
///   * once a variation's `attempts` has already reached
///     [`RECONCILE_MAX_ATTEMPTS`] without completing → **Failed** (stop thrashing).
///
/// Returns the number of variations whose persisted state changed. Shared by the
/// startup integrity scan and the running engine's tick loop. CHEAP when idle: a
/// settled list yields an empty worklist and hashes nothing; a file is hashed only
/// when a sessionless in-flight job actually has a final file on disk. Hashing runs
/// OFF the per-list lock. `context` distinguishes the call site in the log line.
pub async fn reconcile_completed_inflight(
    orch: &Arc<Mutex<Orchestrator>>,
    live_md5s: &std::collections::HashSet<String>,
    context: &str,
) -> usize {
    let candidates = {
        let g = orch.lock().await;
        g.inflight_variations().unwrap_or_default()
    };
    let mut fixed = 0usize;
    for v in candidates {
        // LIVENESS GATE: a variation with a live in-flight transport is never
        // touched, even if quiet — the engine owns it.
        if live_md5s.contains(&v.md5) {
            continue;
        }
        let md5 = v.md5.as_str();

        // Is the final file complete (present, full size, md5-verified)? Hash only
        // when there's a recorded path AND its size matches — off the lock.
        let mut complete = false;
        if let Some(output_path) = v.output_path.as_deref() {
            let path = std::path::Path::new(output_path);
            if let Ok(meta) = std::fs::metadata(path) {
                let size_ok = v.total_bytes.map(|t| meta.len() == t).unwrap_or(true);
                if size_ok {
                    let actual = libgen_core::download::md5_of_file(path).await;
                    complete = matches!(actual, Ok(h) if h.eq_ignore_ascii_case(md5));
                }
            }
        }

        if complete {
            let promoted = {
                let mut g = orch.lock().await;
                g.promote_variation(&v.group_path, v.book_index, md5)
                    .unwrap_or(false)
            };
            if promoted {
                fixed += 1;
                tracing::info!(
                    md5 = %md5,
                    path = v.output_path.as_deref().unwrap_or(""),
                    context,
                    "reconciled completed-but-stuck download — promoted sessionless in-flight job to Done"
                );
            }
            continue;
        }

        // NOT complete and NOT live: the file is partial/absent and no transport is
        // running. Re-queue it (resuming from the `.part`) unless it has already
        // burned through the attempt cap, in which case fail it so a dead source
        // stops thrashing and becomes user-retryable.
        if v.attempts >= RECONCILE_MAX_ATTEMPTS {
            let failed = {
                let mut g = orch.lock().await;
                g.fail_inflight_variation(
                    &v.group_path,
                    v.book_index,
                    md5,
                    "download did not complete after repeated attempts — source may be unavailable; retry manually",
                )
                .unwrap_or(false)
            };
            if failed {
                fixed += 1;
                tracing::warn!(
                    md5 = %md5,
                    attempts = v.attempts,
                    context,
                    "reconciled stuck download — attempt cap reached, marked Failed (re-queue thrash guard)"
                );
            }
        } else {
            let requeued = {
                let mut g = orch.lock().await;
                g.requeue_variation(&v.group_path, v.book_index, md5)
                    .unwrap_or(false)
            };
            if requeued {
                fixed += 1;
                tracing::info!(
                    md5 = %md5,
                    attempts = v.attempts + 1,
                    context,
                    "reconciled stuck download — no live session, re-queued to resume from partial"
                );
            }
        }
    }
    fixed
}

/// Build (or reuse) the shared scheduler from the bare shared handles — what the
/// engine task holds. The cached scheduler lives behind `scheduler`; the configured
/// sites/limits come from `config` (or an explicit `site`).
pub async fn ensure_scheduler_from(
    scheduler: &tokio::sync::Mutex<Option<Arc<libgen_core::queue::Scheduler>>>,
    config: &std::sync::Mutex<Config>,
    site: Option<&str>,
) -> Result<Arc<libgen_core::queue::Scheduler>, String> {
    let mut guard = scheduler.lock().await;
    if let Some(s) = guard.as_ref() {
        return Ok(Arc::clone(s));
    }
    let cfg = config.lock().expect("config mutex poisoned").clone();
    let sched = Arc::new(build_scheduler(site, &cfg).map_err(|e| e.to_string())?);
    *guard = Some(Arc::clone(&sched));
    Ok(sched)
}

/// Order `hosts` best-first by live SLUM availability (cached snapshot) + measured
/// quality (`site_quality` for `role`). Degrades to the given order when there's
/// no data (no network, fresh DB) — see [`libgen_core::ranking::order_hosts`].
fn order_by_quality(
    cfg: &Config,
    role: libgen_core::store::SiteRole,
    hosts: &[String],
) -> Vec<String> {
    let slum = libgen_core::slum::SlumReport::load(cfg.slum_cache_path());
    let quality = open_store(cfg)
        .ok()
        .and_then(|s| s.site_quality(role).ok())
        .unwrap_or_default();
    libgen_core::ranking::order_hosts(hosts, slum.as_ref(), &quality)
}

/// Open a [`libgen_core::store::Store`] against the configured on-disk DB,
/// creating its parent dir.
pub fn open_store(cfg: &Config) -> Result<libgen_core::store::Store, String> {
    if let Some(parent) = cfg.db_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating db dir {}: {e}", parent.display()))?;
    }
    libgen_core::store::Store::open(&cfg.db_path).map_err(|e| e.to_string())
}

/// Build a download scheduler. The resolver chain comes from an explicit
/// non-empty `site` (comma-separated mirrors, or a `{md5}` direct-URL template
/// for tests) when given, else from the app-config failover order — auto-ordered
/// by live SLUM health + measured success (Phase B). Per-host politeness
/// (concurrency, rate, attempts) and the global cap come from the app settings.
pub fn build_scheduler(
    site: Option<&str>,
    cfg: &Config,
) -> Result<libgen_core::queue::Scheduler, anyhow::Error> {
    use libgen_core::download::resolver_for_site;
    use libgen_core::download::{host_of, DirectUrlResolver, Resolver, ResolverChain};
    use libgen_core::queue::SchedulerBuilder;

    let app = &cfg.app;
    // Browser-like UA + redirect following: real mirrors gate on UA and
    // 307-redirect to a CDN (see cmd_run.rs).
    let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Kwire/1.0";
    // DOWNLOAD client: bounds connection setup only (the streaming body must NOT
    // have an overall timeout, or large downloads would be killed). The headers
    // phase + body idle-stall are bounded inside `download_with_client_cancellable`.
    let client = reqwest::Client::builder()
        .user_agent(ua)
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()?;
    // RESOLVE client: resolver fetches (ads.php→get.php, by-id JSON, md5→CID) are
    // SMALL responses, so a full overall timeout is safe and stops a hung mirror
    // from stalling resolution forever (which would funnel everything onto one
    // host and starve the spill). Resolvers use this; the scheduler streams with
    // the download client above.
    let resolve_client = reqwest::Client::builder()
        .user_agent(ua)
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut resolvers: Vec<Arc<dyn Resolver>> = Vec::new();
    // Resolve a chain. An explicit `site` (non-empty) wins so tests can pin a
    // mock direct-URL template; otherwise use the configured failover order.
    let explicit = site.map(str::trim).filter(|s| !s.is_empty());
    match explicit {
        Some(spec) => {
            for entry in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if entry.contains("{md5}") {
                    // Direct-URL template (used by mock servers in tests).
                    resolvers.push(Arc::new(DirectUrlResolver::new(
                        host_of(entry),
                        entry.to_string(),
                        resolve_client.clone(),
                    )) as Arc<dyn Resolver>);
                } else {
                    resolvers.push(resolver_for_site(entry, &resolve_client)?);
                }
            }
        }
        None => {
            // The download chain is the fixed libgen+ family (all front the same
            // booksdl CDN — multiple mirrors give resolve-resilience only, not
            // throughput; see DESIGN §13b). Auto-ordered by live health so a down
            // mirror sinks in the failover chain.
            let chain: Vec<String> = libgen_core::download::LIBGEN_FAMILY_SITES
                .iter()
                .map(|s| s.to_string())
                .collect();
            let ordered = order_by_quality(cfg, libgen_core::store::SiteRole::Download, &chain);
            for entry in ordered {
                resolvers.push(resolver_for_site(&entry, &resolve_client)?);
            }
        }
    }

    let chain = ResolverChain::new(resolvers);
    // NOTE: total download concurrency is now bounded by the engine's download
    // WORKER POOL (`max_concurrent_downloads` workers, each pulling one book), so
    // the scheduler's own global gate is left unlimited; per-host caps still apply.
    Ok(SchedulerBuilder::new(chain, client)
        .default_limits(app.host_limits())
        .hedge(app.hedge_config())
        .build())
}

/// Build the search client from config: replay (offline) when a replay dir is
/// configured, else live mirrors.
pub fn build_search(cfg: &Config) -> Result<libgen_core::search::SearchClient, String> {
    use libgen_core::search::{LiveTransport, MirrorConfig};
    let mut mirror_cfg = MirrorConfig::load(&cfg.mirrors)
        .map_err(|e| format!("loading mirrors from {}: {e}", cfg.mirrors.display()))?;
    // Auto-order search mirrors by live SLUM health + measured success (Phase B):
    // a down/flaky mirror sinks so it's tried last (the search client fails over in
    // list order). No-op when there's no data (e.g. replay/tests → identity order).
    let hosts: Vec<String> = mirror_cfg
        .search_mirrors
        .iter()
        .map(|m| m.host.clone())
        .collect();
    let ranked = order_by_quality(cfg, libgen_core::store::SiteRole::Search, &hosts);
    let rank_of = |host: &str| ranked.iter().position(|h| h == host).unwrap_or(usize::MAX);
    mirror_cfg.search_mirrors.sort_by_key(|m| rank_of(&m.host));
    Ok(match &cfg.replay_dir {
        Some(dir) => libgen_core::search::SearchClient::replay(mirror_cfg, dir.clone()),
        None => libgen_core::search::SearchClient::new(mirror_cfg, Box::new(LiveTransport::new())),
    })
}

// ---------------------------------------------------------------------------
// Unit tests (generic engine logic only — no tauri)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use libgen_core::model::{BookInput, BookRequest, Candidate, DownloadJob};

    fn cand(state: Option<JobState>) -> Candidate {
        Candidate {
            md5: "0".repeat(32),
            title: "T".into(),
            authors: vec![],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: None,
            size_bytes: None,
            source_host: None,
            cover_url: None,
            score: 0.0,
            job: state.map(|s| DownloadJob {
                state: s,
                ..Default::default()
            }),
        }
    }

    fn req(status: RequestStatus, goal: Goal, var: Option<JobState>) -> BookRequest {
        let mut r = BookRequest::new(BookInput {
            title: "T".into(),
            ..Default::default()
        });
        r.status = status;
        r.goal = goal;
        r.candidates = vec![cand(var)];
        r
    }

    #[test]
    fn pending_variation_dispatches_download_regardless_of_stale_status() {
        // The bug: a book stuck reading `Downloading` (or `NeedsSelection`) while
        // its variation is actually `Pending` must still be dispatched.
        for status in [
            RequestStatus::Matched,
            RequestStatus::Downloading, // stale roll-up, nothing actually running
            RequestStatus::NeedsSelection, // user picked a copy on an ambiguous book
            RequestStatus::Failed { error: "x".into() },
        ] {
            assert_eq!(
                actionable_kind(&req(
                    status.clone(),
                    Goal::Complete,
                    Some(JobState::Pending)
                )),
                Some(WorkKind::Download),
                "status {status:?} with a pending variation should download"
            );
        }
    }

    #[test]
    fn genuinely_downloading_is_not_re_dispatched() {
        // A real in-flight transfer has its variation in Downloading (not Pending),
        // so it is NOT re-dispatched here (the inflight set + this both protect it).
        assert_eq!(
            actionable_kind(&req(
                RequestStatus::Downloading,
                Goal::Complete,
                Some(JobState::Downloading)
            )),
            None
        );
    }

    #[test]
    fn idle_and_user_blocked_without_pending_do_nothing() {
        // Idle goal never acts.
        assert_eq!(
            actionable_kind(&req(
                RequestStatus::Matched,
                Goal::Idle,
                Some(JobState::Pending)
            )),
            None
        );
        // NeedsSelection with NO pending variation waits for the user.
        assert_eq!(
            actionable_kind(&req(RequestStatus::NeedsSelection, Goal::Complete, None)),
            None
        );
    }

    #[test]
    fn selected_ready_book_dispatches_only_once_armed_with_a_pending_job() {
        // A user-selected variation lands the book in `Ready` under a `Complete`
        // goal. BEFORE the orchestrator fix, selection left NO pending candidate
        // job, so this state was dead — the engine never downloaded (the `d` /
        // Picker no-op bug). The fix arms the chosen variation `Pending`.
        assert_eq!(
            actionable_kind(&req(RequestStatus::Ready, Goal::Complete, None)),
            None,
            "selection with no pending variation is invisible to the engine (the bug)"
        );
        assert_eq!(
            actionable_kind(&req(
                RequestStatus::Ready,
                Goal::Complete,
                Some(JobState::Pending)
            )),
            Some(WorkKind::Download),
            "once the selected variation is armed Pending, the engine downloads it"
        );
    }

    #[test]
    fn done_complete_reverifies_when_nothing_pending() {
        assert_eq!(
            actionable_kind(&req(RequestStatus::Done, Goal::Complete, None)),
            Some(WorkKind::Reverify)
        );
        // …but a pending (recommended) variation on a Done book downloads first.
        assert_eq!(
            actionable_kind(&req(
                RequestStatus::Done,
                Goal::Complete,
                Some(JobState::Pending)
            )),
            Some(WorkKind::Download)
        );
    }
}
