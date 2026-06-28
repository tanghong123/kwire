//! Scheduler with one queue per download host (DESIGN.md §8).
//!
//! Each `HostQueue` has its own concurrency limit + token-bucket rate limiter
//! (with jitter). Jobs are routed by *resolved* host; failover re-resolves to an
//! alternate mirror and re-enqueues onto that host's queue. Resolution happens
//! in a separate, lighter pool before a job lands on a host queue.
//!
//! UI-agnostic: progress is surfaced through an mpsc channel of [`Progress`]
//! events that any front end can subscribe to.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::{mpsc, Mutex, OnceCell, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::download::{
    download_with_client_cancellable, part_path, DownloadError, DownloadTarget, ResolverChain,
};

/// Configuration for the speculative (hedged) download controller. OFF by
/// default — the defaults mirror `docs/SPECULATIVE_DOWNLOAD.md` §10. When
/// `enabled` is false the scheduler behaves exactly as before: a stalled
/// download is never raced.
#[derive(Debug, Clone)]
pub struct HedgeConfig {
    /// Master switch. `false` (default) ⇒ no hedging ever.
    pub enabled: bool,
    /// How long a leg's smoothed throughput must stay at/below `stall_min_bps`
    /// before it counts as stalled.
    pub stall_window: Duration,
    /// Throughput threshold (bytes/sec); at/below this for `stall_window` ⇒
    /// stalled. 0-byte hangs are the degenerate case.
    pub stall_min_bps: u64,
    /// Don't hedge until the leg has fetched at least this many bytes — tiny files
    /// finish inside the window anyway.
    pub min_hedge_file_bytes: u64,
    /// Max concurrent transports per book variation (1 primary + hedges).
    pub max_legs_per_book: usize,
    /// Global cap on extra (hedge) legs in flight across the whole scheduler.
    pub max_concurrent_hedges: usize,
}

impl Default for HedgeConfig {
    fn default() -> Self {
        HedgeConfig {
            enabled: false,
            stall_window: Duration::from_secs(15),
            stall_min_bps: 8 * 1024,
            min_hedge_file_bytes: 256 * 1024,
            max_legs_per_book: 2,
            max_concurrent_hedges: 2,
        }
    }
}

/// Politeness limits for a single host.
#[derive(Debug, Clone)]
pub struct HostLimits {
    pub max_concurrency: usize,
    pub min_interval: Duration,
    pub max_attempts: u32,
}

impl Default for HostLimits {
    fn default() -> Self {
        HostLimits {
            max_concurrency: 2,
            min_interval: Duration::from_millis(500),
            max_attempts: 4,
        }
    }
}

/// A unit of work submitted to the scheduler: an md5 plus where to write it.
#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub md5: String,
    pub dest: std::path::PathBuf,
    /// Byte offset to resume from (persisted across restarts). 0 = fresh.
    pub resume_offset: u64,
    /// Known size from the search candidate, used as the total when the host
    /// omits `Content-Length` so progress %/ETA still render. `None` if unknown.
    pub expected_size: Option<u64>,
}

impl DownloadRequest {
    pub fn new(md5: impl Into<String>, dest: impl Into<std::path::PathBuf>) -> Self {
        DownloadRequest {
            md5: md5.into(),
            dest: dest.into(),
            resume_offset: 0,
            expected_size: None,
        }
    }
}

/// The shared backend CDN a download host fronts. Hosts in the SAME group serve
/// from the same origin, so once one fails (the CDN is down/slow) its siblings
/// fail identically — failover skips same-group hosts to reach an INDEPENDENT
/// lane. An unknown host is its own group (so it's never skipped spuriously).
fn cdn_group(host: &str) -> Option<&'static str> {
    let h = host.to_ascii_lowercase();
    let li_family = [
        "libgen.li",
        "libgen.vg",
        "libgen.la",
        "libgen.bz",
        "libgen.gl",
    ];
    if li_family.iter().any(|f| h.starts_with(f)) {
        Some("booksdl") // cdn*.booksdl.lc
    } else if h.starts_with("libgen.pw") || h.starts_with("randombook") {
        Some("libgen-download")
    } else if h.contains("ipfs")
        || h.contains("dweb")
        || h.contains("pinata")
        || h.contains("gateway")
    {
        Some("ipfs")
    } else if h.contains("annas") {
        Some("annas")
    } else {
        // Unknown host: NOT part of a known shared-CDN family, so never grouped
        // (each is treated independently — normal failover applies).
        None
    }
}

/// Progress / lifecycle event for a request, emitted on the scheduler's
/// subscription channel. UI-agnostic.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Resolution succeeded; routed onto `host`'s queue.
    Resolved {
        md5: String,
        /// Stable per-leg id within this md5's race (primary = 0, hedges = 1,2,…).
        leg_id: u64,
        /// Whether the backend launched this leg as a hedge (authoritative label).
        is_hedge: bool,
        host: String,
        total_bytes: Option<u64>,
    },
    /// A transfer is continuing from an existing on-disk `.part` rather than
    /// starting fresh. `offset` is the byte count already on disk. Informational
    /// (chronicle); the `Resolved`/`Bytes` events drive state.
    Resuming {
        md5: String,
        leg_id: u64,
        is_hedge: bool,
        host: String,
        offset: u64,
    },
    /// Bytes streamed so far for a request (cumulative).
    Bytes {
        md5: String,
        leg_id: u64,
        is_hedge: bool,
        host: String,
        bytes_done: u64,
        total_bytes: Option<u64>,
        /// Smoothed throughput in bytes/sec, `None` until measurable (≥2 ticks).
        speed_bps: Option<u64>,
        /// Estimated seconds remaining, `None` when total/speed unknown or zero.
        eta_secs: Option<u64>,
    },
    /// A leg's smoothed throughput stayed at/below the stall threshold for the
    /// configured window (after `min_hedge_file_bytes`) without erroring. Purely
    /// informational for the UI/engine; the *decision* to launch a hedge is made
    /// by the scheduler's hedge controller so it can enforce the global cap. The
    /// slow leg keeps running as insurance.
    Stalled {
        md5: String,
        leg_id: u64,
        is_hedge: bool,
        host: String,
        bytes_done: u64,
        speed_bps: Option<u64>,
    },
    /// A retry is scheduled after a transient failure.
    Retrying {
        md5: String,
        leg_id: u64,
        is_hedge: bool,
        host: String,
        attempt: u32,
        backoff: Duration,
        error: String,
    },
    /// Failing over to an alternate mirror after exhausting a host.
    FailingOver {
        md5: String,
        leg_id: u64,
        is_hedge: bool,
        from_host: String,
        error: String,
    },
    /// Completed successfully; file written to `path`.
    Done {
        md5: String,
        host: String,
        path: std::path::PathBuf,
        bytes_written: u64,
    },
    /// Permanently failed after retries/failover.
    Failed { md5: String, error: String },
    /// A generic download-path DIAGNOSTIC note, surfaced into the persisted history
    /// so design elements that are otherwise invisible (cdn edge rotation; the
    /// 200-ignored-Range → restart-from-scratch path) leave a verifiable trace.
    /// Best-effort and behavior-neutral: emitted alongside the real control flow,
    /// never gating it.
    Note { md5: String, detail: String },
    /// A specific leg ended (won, exhausted, cancelled, panicked, or abandoned when
    /// a sibling won). Emitted by the leg task's Drop guard so it fires on EVERY exit.
    LegEnded { md5: String, leg_id: u64 },
    /// The request was deliberately stopped via cancellation. `paused` true means
    /// the `.part` (+ `resume_offset`) was kept so it can resume; false means it
    /// was a hard cancel (the `.part` is removed).
    Cancelled {
        md5: String,
        paused: bool,
        /// Byte offset preserved on disk for a paused job to resume from.
        resume_offset: u64,
    },
}

/// Outcome of a single submitted request, returned by [`Scheduler::run`].
#[derive(Debug, Clone)]
pub struct JobOutcome {
    pub md5: String,
    pub result: Result<std::path::PathBuf, String>,
}

/// Token-bucket-ish rate limiter: enforces a minimum interval between grants,
/// plus a small jitter, per host. Combined with the concurrency semaphore this
/// gives "polite" pacing.
struct RateLimiter {
    min_interval: Duration,
    /// Earliest [`Instant`] the next request may start.
    next_allowed: Mutex<Instant>,
}

impl RateLimiter {
    fn new(min_interval: Duration) -> Self {
        RateLimiter {
            min_interval,
            next_allowed: Mutex::new(Instant::now()),
        }
    }

    /// Wait until the host is allowed to issue another request, then reserve the
    /// next slot.
    async fn acquire(&self) {
        if self.min_interval.is_zero() {
            return;
        }
        let sleep_until = {
            let mut next = self.next_allowed.lock().await;
            let now = Instant::now();
            let start = if *next > now { *next } else { now };
            // Jitter: up to 25% of the interval, to avoid thundering herds.
            let jitter_ms = (self.min_interval.as_millis() as u64) / 4;
            let jitter = if jitter_ms > 0 {
                Duration::from_millis(fastrand::u64(0..=jitter_ms))
            } else {
                Duration::ZERO
            };
            let reserved = start + self.min_interval + jitter;
            *next = reserved;
            start
        };
        let now = Instant::now();
        if sleep_until > now {
            tokio::time::sleep(sleep_until - now).await;
        }
    }
}

/// Per-host queue: a concurrency gate + rate limiter + its limits.
struct HostQueue {
    limits: HostLimits,
    semaphore: Arc<Semaphore>,
    rate: Arc<RateLimiter>,
}

impl HostQueue {
    fn new(limits: HostLimits) -> Self {
        let semaphore = Arc::new(Semaphore::new(limits.max_concurrency.max(1)));
        let rate = Arc::new(RateLimiter::new(limits.min_interval));
        HostQueue {
            limits,
            semaphore,
            rate,
        }
    }
}

/// Builds a scheduler. Hosts are discovered lazily at resolve time; per-host
/// overrides may be registered ahead of time.
pub struct SchedulerBuilder {
    resolvers: ResolverChain,
    client: Client,
    default_limits: HostLimits,
    host_overrides: HashMap<String, HostLimits>,
    resolve_concurrency: usize,
    global_concurrency: usize,
    base_backoff: Duration,
    max_backoff: Duration,
    hedge: HedgeConfig,
}

/// Default global download cap: effectively unlimited (so `run`-based tests and
/// the CLI are unconstrained). The app sets a real value (`G`, default 5) via
/// [`SchedulerBuilder::global_concurrency`]. Kept below tokio's
/// `Semaphore::MAX_PERMITS` so `Semaphore::new` never panics.
pub const UNLIMITED_GLOBAL_CONCURRENCY: usize = usize::MAX >> 4;

impl SchedulerBuilder {
    pub fn new(resolvers: ResolverChain, client: Client) -> Self {
        SchedulerBuilder {
            resolvers,
            client,
            default_limits: HostLimits::default(),
            host_overrides: HashMap::new(),
            resolve_concurrency: 4,
            global_concurrency: UNLIMITED_GLOBAL_CONCURRENCY,
            // Retry backoff: start at 1s, double each step (1, 2, 4, 8, …), capped at
            // 200s. Full jitter (see `backoff_for`) randomizes within [0, ceiling] to
            // avoid synchronized retries against the shared CDN.
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(200),
            hedge: HedgeConfig::default(),
        }
    }

    /// Cap the TOTAL number of concurrent download legs (primary + hedge) across
    /// every host and list — the global `G`. Composes with the per-host caps:
    /// effective concurrency is `min(G, Σ per-host caps)`. Defaults to unlimited.
    pub fn global_concurrency(mut self, n: usize) -> Self {
        self.global_concurrency = n.max(1);
        self
    }

    /// Configure the speculative (hedged) download controller. Defaults to
    /// disabled.
    pub fn hedge(mut self, hedge: HedgeConfig) -> Self {
        self.hedge = hedge;
        self
    }

    pub fn default_limits(mut self, limits: HostLimits) -> Self {
        self.default_limits = limits;
        self
    }

    pub fn host_override(mut self, host: impl Into<String>, limits: HostLimits) -> Self {
        self.host_overrides.insert(host.into(), limits);
        self
    }

    pub fn resolve_concurrency(mut self, n: usize) -> Self {
        self.resolve_concurrency = n.max(1);
        self
    }

    pub fn base_backoff(mut self, d: Duration) -> Self {
        self.base_backoff = d;
        self
    }

    pub fn max_backoff(mut self, d: Duration) -> Self {
        self.max_backoff = d;
        self
    }

    pub fn build(self) -> Scheduler {
        Scheduler {
            resolvers: self.resolvers,
            client: self.client,
            default_limits: self.default_limits,
            host_overrides: self.host_overrides,
            host_queues: Mutex::new(HashMap::new()),
            resolve_gate: Arc::new(Semaphore::new(self.resolve_concurrency)),
            global_gate: Arc::new(Semaphore::new(self.global_concurrency)),
            base_backoff: self.base_backoff,
            max_backoff: self.max_backoff,
            cancels: Mutex::new(HashMap::new()),
            hedge_gate: Arc::new(Semaphore::new(self.hedge.max_concurrent_hedges.max(1))),
            hedge: self.hedge,
        }
    }
}

/// A handle to an in-flight request's cancellation, registered for the duration
/// of its download so a front end can pause or cancel it by md5.
#[derive(Clone)]
struct CancelHandle {
    token: CancellationToken,
    /// `true` once a *pause* was requested (keep the `.part`), `false` for a hard
    /// cancel (remove the `.part`). Shared so the download task reads the intent
    /// the canceller set when it triggered the token.
    paused: Arc<std::sync::atomic::AtomicBool>,
}

/// The download scheduler: routes resolved jobs to per-host queues and drives
/// retry/backoff + mirror failover. Clone-free; share via `Arc`.
pub struct Scheduler {
    resolvers: ResolverChain,
    client: Client,
    default_limits: HostLimits,
    host_overrides: HashMap<String, HostLimits>,
    host_queues: Mutex<HashMap<String, Arc<HostQueue>>>,
    resolve_gate: Arc<Semaphore>,
    /// Global cap on concurrent download legs (primary + hedge) across all hosts
    /// and lists — the `G` from docs/DOWNLOAD_SCHEDULING.md. Every leg holds one
    /// permit for its transferring lifetime; a job waiting for a permit stays
    /// `Pending` (queued), so the queued→downloading transition is honest.
    global_gate: Arc<Semaphore>,
    base_backoff: Duration,
    max_backoff: Duration,
    /// Cancellation handles for in-flight requests, keyed by md5. Registered when
    /// a request starts and removed when it finishes, so `pause`/`cancel` can
    /// reach a download that is actively streaming.
    cancels: Mutex<HashMap<String, CancelHandle>>,
    /// Global cap on extra (hedge) legs in flight at once
    /// (`HedgeConfig::max_concurrent_hedges`). A hedge leg must hold one of these
    /// permits for its lifetime; when exhausted, a newly-stalled leg simply keeps
    /// running un-hedged until a slot frees.
    hedge_gate: Arc<Semaphore>,
    /// Speculative-download configuration (off by default).
    hedge: HedgeConfig,
}

/// Shared state for one book variation's speculative race. The first (primary)
/// leg and any hedge legs share this: the `winner` `OnceCell` is the
/// linearization point (exactly one leg observes `Ok` first and promotes its
/// temp to the final dest); `group_cancel` stops every losing leg; `legs` tracks
/// per-leg temp paths so the winner can clean siblings.
struct RaceGroup {
    /// Final destination every leg races toward.
    dest: std::path::PathBuf,
    /// Set exactly once, by the first leg to finish `Ok`. The held value is the
    /// temp path that was promoted (for logging/debug only).
    winner: OnceCell<()>,
    /// Cancels all *losing* legs when the winner is decided.
    group_cancel: CancellationToken,
    /// Temp paths of every leg in the race (primary + hedges), so on a win the
    /// winner removes the losers' temps/`.part`s. Keyed by md5#host so a leg can
    /// exclude its own temp from cleanup.
    legs: Mutex<Vec<LegTemp>>,
    /// How many legs (primary + hedges) are currently in the race — gates
    /// `max_legs_per_book`.
    leg_count: std::sync::atomic::AtomicUsize,
    /// Hosts already racing, so a hedge target avoids them.
    hosts: Mutex<Vec<String>>,
    /// Monotonic source of per-leg ids within this race. The primary takes 0, each
    /// hedge `fetch_add(1)` (start order). Stable identity for the UI.
    next_leg_id: std::sync::atomic::AtomicU64,
}

/// A leg's temp target inside a [`RaceGroup`].
#[derive(Clone)]
struct LegTemp {
    /// The dest-shaped temp the leg streams to (its own `.part` is `temp.part`).
    /// For the primary leg this is the real `dest` (so resume is unchanged).
    leg_dest: std::path::PathBuf,
}

/// Per-leg context threaded through `process_one_inner`/`download_on_host`.
struct LegCtx {
    /// Stable per-leg id within the race (primary = 0, hedges = 1,2,…). The UI keys
    /// legs by this, so a host change within a leg stays one line.
    leg_id: u64,
    /// Where THIS leg streams to (the primary uses the real dest; a hedge uses a
    /// unique sibling temp). On a win this is promoted to `race.dest`.
    leg_dest: std::path::PathBuf,
    /// True for a hedge leg (so it never spawns a further hedge off its own
    /// stall, and so it carries a hedge-gate permit for its lifetime).
    is_hedge: bool,
    /// Resolver index this leg starts resolving from.
    resolver_start: usize,
    /// Hosts this leg must avoid (the hosts already racing) — for hedge legs.
    exclude_hosts: Vec<String>,
    /// The global hedge-gate permit a hedge leg holds for its whole lifetime;
    /// dropped (freeing a global slot) when the leg ends. `None` for the primary.
    _hedge_permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// RAII guard whose `Drop` emits [`Progress::LegEnded`] for a leg. Installed at the
/// top of `process_one_leg`, it fires the leg's death signal on EVERY exit path
/// (return, `?`, cancel/abort, unwind, race-group cancel) from one place.
struct LegEndGuard {
    events: mpsc::Sender<Progress>,
    md5: String,
    leg_id: u64,
}

impl Drop for LegEndGuard {
    fn drop(&mut self) {
        // Drop can't await; best-effort. A lost LegEnded is caught by the UI's TTL backstop.
        let _ = self.events.try_send(Progress::LegEnded {
            md5: self.md5.clone(),
            leg_id: self.leg_id,
        });
    }
}

impl RaceGroup {
    fn new(dest: std::path::PathBuf) -> Self {
        RaceGroup {
            dest,
            winner: OnceCell::new(),
            group_cancel: CancellationToken::new(),
            legs: Mutex::new(Vec::new()),
            leg_count: std::sync::atomic::AtomicUsize::new(0),
            hosts: Mutex::new(Vec::new()),
            next_leg_id: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl Scheduler {
    /// Get (or lazily create) the queue for `host`.
    async fn host_queue(&self, host: &str) -> Arc<HostQueue> {
        let mut map = self.host_queues.lock().await;
        if let Some(q) = map.get(host) {
            return q.clone();
        }
        let limits = self
            .host_overrides
            .get(host)
            .cloned()
            .unwrap_or_else(|| self.default_limits.clone());
        let q = Arc::new(HostQueue::new(limits));
        map.insert(host.to_string(), q.clone());
        q
    }

    /// Request that the in-flight download for `md5` stop and **keep** its
    /// `.part` so it can resume later (pause). Returns `true` if a matching
    /// in-flight download was found and signalled. A no-op (`false`) if the md5
    /// isn't currently downloading.
    pub async fn pause(&self, md5: &str) -> bool {
        self.signal_cancel(md5, true).await
    }

    /// Request that the in-flight download for `md5` stop and **remove** its
    /// `.part` (hard cancel). Returns `true` if signalled.
    pub async fn cancel(&self, md5: &str) -> bool {
        self.signal_cancel(md5, false).await
    }

    /// Pause every in-flight download (keep `.part`s). Returns how many were
    /// signalled.
    pub async fn pause_all(&self) -> usize {
        self.signal_cancel_all(true).await
    }

    /// Cancel every in-flight download (remove `.part`s). Returns how many were
    /// signalled.
    pub async fn cancel_all(&self) -> usize {
        self.signal_cancel_all(false).await
    }

    async fn signal_cancel(&self, md5: &str, paused: bool) -> bool {
        let map = self.cancels.lock().await;
        if let Some(h) = map.get(md5) {
            h.paused.store(paused, std::sync::atomic::Ordering::SeqCst);
            h.token.cancel();
            true
        } else {
            false
        }
    }

    async fn signal_cancel_all(&self, paused: bool) -> usize {
        let map = self.cancels.lock().await;
        for h in map.values() {
            h.paused.store(paused, std::sync::atomic::Ordering::SeqCst);
            h.token.cancel();
        }
        map.len()
    }

    /// Register a cancellation handle for an md5 about to start downloading.
    async fn register_cancel(&self, md5: &str) -> CancelHandle {
        let handle = CancelHandle {
            token: CancellationToken::new(),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        self.cancels
            .lock()
            .await
            .insert(md5.to_string(), handle.clone());
        handle
    }

    async fn unregister_cancel(&self, md5: &str) {
        self.cancels.lock().await.remove(md5);
    }

    /// Drive a batch of requests to completion concurrently, emitting progress
    /// on `events`. Returns one [`JobOutcome`] per request (order not
    /// guaranteed). Per-host concurrency/rate limits are enforced throughout.
    pub async fn run(
        self: &Arc<Self>,
        requests: Vec<DownloadRequest>,
        events: mpsc::Sender<Progress>,
    ) -> Vec<JobOutcome> {
        let mut handles = Vec::with_capacity(requests.len());
        for req in requests {
            let this = Arc::clone(self);
            let events = events.clone();
            handles.push(tokio::spawn(
                async move { this.process_one(req, events).await },
            ));
        }
        let mut outcomes = Vec::with_capacity(handles.len());
        for h in handles {
            match h.await {
                Ok(o) => outcomes.push(o),
                Err(join_err) => outcomes.push(JobOutcome {
                    md5: "<unknown>".to_string(),
                    result: Err(format!("task panicked: {join_err}")),
                }),
            }
        }
        outcomes
    }

    /// Resolve a single request, then download it on its host queue with
    /// retry/backoff and mirror failover.
    async fn process_one(
        self: Arc<Self>,
        req: DownloadRequest,
        events: mpsc::Sender<Progress>,
    ) -> JobOutcome {
        let md5 = req.md5.clone();
        // Register a cancellation handle so pause/cancel can reach this request
        // while it downloads. Removed on the way out (any return path).
        let cancel = self.register_cancel(&md5).await;
        // The primary leg roots a (possibly trivial) race group at the dest. Its
        // own leg target IS the dest (so resume semantics are untouched). When
        // hedging is enabled and the primary stalls, a sibling leg is launched
        // into its own temp path; the first verified finisher promotes + wins.
        let race = Arc::new(RaceGroup::new(req.dest.clone()));
        race.legs.lock().await.push(LegTemp {
            leg_dest: req.dest.clone(),
        });
        race.leg_count.store(1, Ordering::SeqCst);
        let leg = LegCtx {
            // The primary leg takes id 0 (first `fetch_add`).
            leg_id: race.next_leg_id.fetch_add(1, Ordering::SeqCst),
            leg_dest: req.dest.clone(),
            is_hedge: false,
            resolver_start: 0,
            exclude_hosts: Vec::new(),
            // The primary leg holds no hedge permit (it's the un-hedged base).
            _hedge_permit: None,
        };
        // Collect hedge-leg handles spawned during the primary's run so we await
        // them (and their cleanup) before returning the variation's outcome.
        let hedges: Arc<Mutex<Vec<tokio::task::JoinHandle<JobOutcome>>>> =
            Arc::new(Mutex::new(Vec::new()));
        // Global concurrency gate (G): hold one permit for this primary leg's whole
        // transferring lifetime, so total in-flight legs (primary + hedges) never
        // exceed G. A job blocked here stays `Pending`/queued — `Progress::Resolved`
        // (→ Downloading) is emitted only after BOTH this permit and a per-host slot
        // are held, so the queued→downloading transition is honest.
        let _global_permit = self
            .global_gate
            .clone()
            .acquire_owned()
            .await
            .expect("global gate");
        let outcome = self
            .process_one_inner(&req, &events, &cancel, race.clone(), leg, &hedges)
            .await;
        // Drain spawned hedge legs (they are cancelled via group_cancel once a
        // winner exists, so this completes promptly).
        let handles: Vec<_> = std::mem::take(&mut *hedges.lock().await);
        let mut results = vec![outcome];
        for h in handles {
            if let Ok(o) = h.await {
                results.push(o);
            }
        }
        self.unregister_cancel(&md5).await;
        // The variation's outcome is the winner if any leg succeeded; else the
        // primary's terminal error (or the first error).
        if let Some(ok) = results.iter().find(|o| o.result.is_ok()) {
            return JobOutcome {
                md5,
                result: ok.result.clone(),
            };
        }
        // No winner: prefer the primary's outcome (first in the vec).
        results.into_iter().next().unwrap_or(JobOutcome {
            md5,
            result: Err("no legs ran".to_string()),
        })
    }

    /// Drive ONE leg of a (possibly trivial) race to completion: resolve →
    /// download-on-host (with stall→hedge) → promote-or-discard. Boxed-`dyn`
    /// return so the recursive hedge spawn (`download_on_host` → `try_launch_hedge`
    /// → here) has a finite, `Send` future type rather than an infinite opaque one.
    #[allow(clippy::too_many_arguments)]
    fn process_one_inner<'a>(
        self: &'a Arc<Self>,
        req: &'a DownloadRequest,
        events: &'a mpsc::Sender<Progress>,
        cancel: &'a CancelHandle,
        race: Arc<RaceGroup>,
        leg: LegCtx,
        hedges: &'a Arc<Mutex<Vec<tokio::task::JoinHandle<JobOutcome>>>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = JobOutcome> + Send + 'a>> {
        Box::pin(self.process_one_leg(req, events, cancel, race, leg, hedges))
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_one_leg(
        self: &Arc<Self>,
        req: &DownloadRequest,
        events: &mpsc::Sender<Progress>,
        cancel: &CancelHandle,
        race: Arc<RaceGroup>,
        leg: LegCtx,
        hedges: &Arc<Mutex<Vec<tokio::task::JoinHandle<JobOutcome>>>>,
    ) -> JobOutcome {
        let md5 = req.md5.clone();
        // RAII death signal: emit `LegEnded` for THIS leg on EVERY exit path
        // (normal return, `?`, cancel/abort, unwind, race-group cancel). One piece
        // of code covers them all — `Drop` runs whenever this function's frame
        // unwinds or returns. Capture `leg.leg_id` (Copy) now, before `leg` is later
        // borrowed/consumed.
        let _leg_end_guard = LegEndGuard {
            events: events.clone(),
            md5: md5.clone(),
            leg_id: leg.leg_id,
        };
        // Mirror index to start resolving from; advanced on failover.
        let mut resolver_start = leg.resolver_start;
        // CDNs whose hosts have already failed this leg — their siblings are
        // skipped on failover so we don't burn attempts on the same dead backend.
        let mut failed_cdns: HashSet<String> = HashSet::new();

        // Failover loop across mirrors.
        loop {
            // If a pause/cancel arrived before (or between) resolves, stop now.
            // A hedge leg cancelled because a sibling won is a silent loss.
            if cancel.token.is_cancelled() || race.group_cancel.is_cancelled() {
                if leg.is_hedge {
                    return self.silent_hedge_loss(&md5, &leg, &race).await;
                }
                return self.emit_cancelled(&md5, req, cancel, events).await;
            }
            let last_error: String;
            // ---- resolve + host-spill pick (lighter, separate pool) ----
            // Instead of routing blindly to the first-resolved host, try to pick a
            // resolvable host that has a FREE concurrency slot right now, so a job
            // doesn't queue behind a saturated host while another is idle. Returns
            // the chosen target, its resolver index (for failover), and — when a
            // slot was grabbed without blocking — a pre-acquired permit so the
            // first download attempt reuses it (no double-acquire, no race). A
            // hedge leg additionally EXCLUDES hosts already racing this book.
            let resolved = {
                let _permit = self.resolve_gate.acquire().await.expect("resolve gate");
                self.resolve_and_pick_host(
                    resolver_start,
                    &req.md5,
                    &leg.exclude_hosts,
                    &failed_cdns,
                )
                .await
            };
            let (resolver_idx, target, queue, spill_permit): (
                usize,
                DownloadTarget,
                Arc<HostQueue>,
                Option<tokio::sync::OwnedSemaphorePermit>,
            ) = match resolved {
                Ok(v) => v,
                Err(e) => {
                    last_error = e.to_string();
                    if leg.is_hedge {
                        // A hedge that can't even resolve an alternate host just
                        // ends silently; the primary leg owns the book's verdict.
                        return self.silent_hedge_loss(&md5, &leg, &race).await;
                    }
                    let _ = events
                        .send(Progress::Failed {
                            md5: md5.clone(),
                            error: last_error.clone(),
                        })
                        .await;
                    return JobOutcome {
                        md5,
                        result: Err(last_error),
                    };
                }
            };

            let host = target.host.clone();
            // Record this leg's host on the race so a later hedge avoids it.
            {
                let mut hs = race.hosts.lock().await;
                if !hs.contains(&host) {
                    hs.push(host.clone());
                }
            }
            // NOTE: `Progress::Resolved` (which flips the variation to
            // `Downloading`) is emitted by `download_on_host` AFTER the host
            // concurrency slot is acquired — so a job waiting for a slot stays
            // `Pending`/queued instead of falsely reading as downloading.

            // ---- download with retry/backoff on this host ----
            match self
                .download_on_host(
                    &queue,
                    &target,
                    resolver_idx,
                    req,
                    events,
                    cancel,
                    spill_permit,
                    &race,
                    &leg,
                    leg.leg_id,
                    leg.is_hedge,
                    hedges,
                )
                .await
            {
                Ok((_path, bytes_written)) => {
                    // First-finisher-wins: try to claim the race. Exactly one leg
                    // sets `winner`; it promotes its temp to the dest + cleans the
                    // losers. A leg that loses the set discards its own temp.
                    match self.promote_or_discard(&race, &leg).await {
                        Ok(final_path) => {
                            let _ = events
                                .send(Progress::Done {
                                    md5: md5.clone(),
                                    host: host.clone(),
                                    path: final_path.clone(),
                                    bytes_written,
                                })
                                .await;
                            return JobOutcome {
                                md5,
                                result: Ok(final_path),
                            };
                        }
                        Err(()) => {
                            // Lost the race-to-set: a sibling already won.
                            return self.silent_hedge_loss(&md5, &leg, &race).await;
                        }
                    }
                }
                // A deliberate pause/cancel: stop here, don't fail over. If we were
                // cancelled because a sibling won, it's a silent loss.
                Err(DownloadError::Cancelled { .. }) => {
                    if race.group_cancel.is_cancelled() && leg.is_hedge {
                        return self.silent_hedge_loss(&md5, &leg, &race).await;
                    }
                    if race.group_cancel.is_cancelled() {
                        // Primary leg cancelled by a winning hedge: silent loss too
                        // (the hedge already promoted + cleaned).
                        return self.silent_hedge_loss(&md5, &leg, &race).await;
                    }
                    return self.emit_cancelled(&md5, req, cancel, events).await;
                }
                Err(e) => {
                    last_error = e.to_string();
                    // Permanent errors (bad md5) shouldn't trigger blind failover
                    // to the same logical file unless another mirror exists; we
                    // still try the next mirror — a different mirror may serve a
                    // correct copy. 404 likewise may succeed elsewhere.
                    let next = resolver_idx + 1;
                    if next < self.resolvers.len() {
                        // This host's CDN just failed — skip its siblings (same CDN)
                        // on failover so we spill to an INDEPENDENT lane, not another
                        // host backed by the same dead origin. (Unknown hosts aren't
                        // grouped, so they never suppress a legit alternate mirror.)
                        if let Some(g) = cdn_group(&host) {
                            failed_cdns.insert(g.to_string());
                        }
                        let _ = events
                            .send(Progress::FailingOver {
                                md5: md5.clone(),
                                leg_id: leg.leg_id,
                                is_hedge: leg.is_hedge,
                                from_host: host.clone(),
                                error: last_error.clone(),
                            })
                            .await;
                        resolver_start = next;
                        continue;
                    }
                    // No more mirrors. A hedge leg that exhausts its mirrors just
                    // ends silently (frees its slot); the primary owns the verdict.
                    if leg.is_hedge {
                        return self.silent_hedge_loss(&md5, &leg, &race).await;
                    }
                    let _ = events
                        .send(Progress::Failed {
                            md5: md5.clone(),
                            error: last_error.clone(),
                        })
                        .await;
                    return JobOutcome {
                        md5,
                        result: Err(last_error),
                    };
                }
            }
        }
    }

    /// Resolve a cancellation into a `Cancelled` outcome: for a hard cancel the
    /// `.part` is removed; for a pause it is kept and its length becomes the
    /// resume offset. Emits a [`Progress::Cancelled`].
    async fn emit_cancelled(
        &self,
        md5: &str,
        req: &DownloadRequest,
        cancel: &CancelHandle,
        events: &mpsc::Sender<Progress>,
    ) -> JobOutcome {
        let paused = cancel.paused.load(std::sync::atomic::Ordering::SeqCst);
        let resume_offset = if paused {
            // Keep the .part; its on-disk length is where we resume.
            current_part_len(&req.dest).await
        } else {
            // Hard cancel: discard the partial file.
            let _ = tokio::fs::remove_file(part_path(&req.dest)).await;
            0
        };
        let _ = events
            .send(Progress::Cancelled {
                md5: md5.to_string(),
                paused,
                resume_offset,
            })
            .await;
        JobOutcome {
            md5: md5.to_string(),
            result: Err(if paused {
                "paused".to_string()
            } else {
                "cancelled".to_string()
            }),
        }
    }

    /// A leg that lost the race: remove its own temp + `.part` (hard-cancel
    /// semantics for a loser) and report a silent loss. Never emits
    /// `Failed`/`Done` — the winning leg owns the book's verdict.
    ///
    /// CRITICAL: never delete `race.dest` itself. The primary leg's `leg_dest`
    /// IS the dest, and a winning hedge may have already promoted its file there
    /// — so a losing primary removes only its `.part`, leaving the promoted final
    /// file intact. Hedge legs (distinct temp) clean both their temp and `.part`.
    async fn silent_hedge_loss(
        &self,
        md5: &str,
        leg: &LegCtx,
        race: &Arc<RaceGroup>,
    ) -> JobOutcome {
        if leg.leg_dest != race.dest {
            let _ = tokio::fs::remove_file(&leg.leg_dest).await;
        }
        let _ = tokio::fs::remove_file(part_path(&leg.leg_dest)).await;
        JobOutcome {
            md5: md5.to_string(),
            result: Err("hedge-lost".to_string()),
        }
    }

    /// First-finisher-wins linearization point. The first leg to call this sets
    /// the race's `winner` `OnceCell`: it cancels the siblings, promotes its own
    /// `leg_dest` to the final `dest` (atomic same-fs rename), and removes every
    /// other leg's temp + `.part`. A leg that loses the set returns `Err(())` and
    /// the caller discards its own just-finished temp.
    async fn promote_or_discard(
        &self,
        race: &Arc<RaceGroup>,
        leg: &LegCtx,
    ) -> Result<std::path::PathBuf, ()> {
        // Atomic claim: exactly one leg wins the set.
        if race.winner.set(()).is_err() {
            // Someone already won.
            return Err(());
        }
        // We won. Stop the other legs.
        race.group_cancel.cancel();
        // Promote our temp to the final dest. If our leg_dest already IS the dest
        // (the primary leg), the file is already in place.
        if leg.leg_dest != race.dest {
            // Best-effort: ensure the destination doesn't already exist from a
            // racing rename (impossible by construction, but be defensive).
            let _ = tokio::fs::rename(&leg.leg_dest, &race.dest).await;
        }
        // Clean every OTHER leg's temp + .part.
        let legs = race.legs.lock().await.clone();
        for lt in legs {
            if lt.leg_dest == leg.leg_dest || lt.leg_dest == race.dest {
                continue;
            }
            let _ = tokio::fs::remove_file(&lt.leg_dest).await;
            let _ = tokio::fs::remove_file(part_path(&lt.leg_dest)).await;
        }
        Ok(race.dest.clone())
    }

    /// Resolve `md5` (starting at resolver `start`) and pick which host to download
    /// from, preferring a resolvable host that has a FREE concurrency slot right
    /// now (host-spill) so jobs don't pile up behind a saturated host while another
    /// is idle. Strategy:
    ///   1. Walk the resolver chain from `start`. The FIRST resolver that succeeds
    ///      defines the "preferred" target (kept as a fallback) and its index (so
    ///      the caller's failover advances correctly).
    ///   2. For each successfully-resolved target (preferred first), `try_acquire`
    ///      its host queue's concurrency permit WITHOUT blocking. The first host
    ///      with a free slot wins — its permit is handed back so the first download
    ///      attempt reuses it (no double-acquire). Per-host concurrency and rate
    ///      limits are still fully honored: we only ever take a real permit, and the
    ///      rate limiter is applied in `download_on_host` regardless.
    ///   3. If NO resolvable host has a free slot, fall back to the preferred host
    ///      with no pre-acquired permit; `download_on_host` then blocks politely on
    ///      that host's semaphore as before.
    ///
    /// Errors only if the chain produced no resolvable host at all.
    #[allow(clippy::type_complexity)]
    #[allow(clippy::type_complexity)]
    async fn resolve_and_pick_host(
        &self,
        start: usize,
        md5: &str,
        exclude_hosts: &[String],
        exclude_cdns: &HashSet<String>,
    ) -> Result<
        (
            usize,
            DownloadTarget,
            Arc<HostQueue>,
            Option<tokio::sync::OwnedSemaphorePermit>,
        ),
        DownloadError,
    > {
        let mut preferred: Option<(usize, DownloadTarget, Arc<HostQueue>)> = None;
        let mut last_err = DownloadError::Permanent("no resolvers configured".to_string());

        for idx in start..self.resolvers.len() {
            let target = match self.resolvers.resolve_with(idx, md5).await {
                Ok(t) => t,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };
            // A hedge leg must avoid hosts already racing this book.
            if exclude_hosts.iter().any(|h| h == &target.host) {
                last_err = DownloadError::Permanent(format!("host {} already racing", target.host));
                continue;
            }
            // Skip a host that shares its backend CDN with one that already failed:
            // libgen.li/vg/la all front cdn*.booksdl.lc, so once that CDN is down
            // they fail identically — jump straight to an INDEPENDENT lane instead.
            if let Some(g) = cdn_group(&target.host) {
                if exclude_cdns.contains(g) {
                    last_err = DownloadError::Transient(format!(
                        "skipping {} (same CDN '{g}' as an already-failed host)",
                        target.host
                    ));
                    continue;
                }
            }
            let queue = self.host_queue(&target.host).await;

            // Try to grab a slot on this host without waiting.
            if let Ok(permit) = queue.semaphore.clone().try_acquire_owned() {
                // This host is idle enough — spill the job here.
                return Ok((idx, target, queue, Some(permit)));
            }

            // No free slot on this host; remember the first one as the fallback and
            // keep scanning the chain for an idle alternative.
            if preferred.is_none() {
                preferred = Some((idx, target, queue));
            }
        }

        match preferred {
            // No host had a free slot — wait politely on the preferred host.
            Some((idx, target, queue)) => Ok((idx, target, queue, None)),
            // Nothing resolved at all.
            None => Err(last_err),
        }
    }

    /// Run the download against one host queue, honoring concurrency + rate
    /// limits, retrying transient errors up to `max_attempts` with exponential
    /// backoff + jitter. Returns the final path + bytes written on success.
    ///
    /// `first_permit`, when `Some`, is a concurrency permit already acquired for
    /// this host by the spill picker; the first attempt reuses it instead of
    /// acquiring afresh (avoids double-counting against the host cap). Subsequent
    /// attempts acquire normally.
    #[allow(clippy::too_many_arguments)]
    async fn download_on_host(
        self: &Arc<Self>,
        queue: &Arc<HostQueue>,
        target: &DownloadTarget,
        resolver_idx: usize,
        req: &DownloadRequest,
        events: &mpsc::Sender<Progress>,
        cancel: &CancelHandle,
        mut first_permit: Option<tokio::sync::OwnedSemaphorePermit>,
        race: &Arc<RaceGroup>,
        leg: &LegCtx,
        leg_id: u64,
        is_hedge: bool,
        hedges: &Arc<Mutex<Vec<tokio::task::JoinHandle<JobOutcome>>>>,
    ) -> Result<(std::path::PathBuf, u64), DownloadError> {
        let max_attempts = queue.limits.max_attempts.max(1);
        let mut attempt = 0u32;
        let mut total_written = 0u64;
        let mut resume_offset = req.resume_offset;
        // A locally-owned target so retries can refresh the resolved URL (libgen's
        // `get.php?key=…` is short-lived; reusing it across a retry/backoff hits an
        // EXPIRED key). The first attempt uses the URL `resolve_and_pick_host`
        // already produced; later attempts re-resolve via the same resolver.
        let mut target = target.clone();
        // This leg streams to its OWN dest-shaped target (the primary uses the
        // real dest; a hedge uses a unique sibling temp).
        let leg_dest = leg.leg_dest.clone();
        // The total for %/ETA: the host's Content-Length if it sent one, else the
        // size we already know from the search candidate. Without this, hosts that
        // omit Content-Length (libgen.pw/ipfs) leave the bar/ETA blank.
        let total_bytes = target.total_bytes.or(req.expected_size);
        // Smooths throughput across the progress ticks observed below so the UI
        // shows a steady speed/ETA rather than a per-chunk sawtooth.
        let mut speed = crate::speed::SpeedTracker::new();
        let start = Instant::now();
        // Has this leg already launched a hedge off a stall? (At most one.)
        let mut hedge_launched = false;

        loop {
            attempt += 1;

            // RETRY → refresh the resolved URL: the previous attempt's link may have
            // a now-expired short-lived key (libgen `get.php?key=…`), so a plain
            // retry would keep failing on a stale URL. Re-resolve via the same
            // resolver (same host/queue). If re-resolution fails, keep the old
            // target — the download will surface its own error / fail over.
            if attempt > 1 {
                if let Ok(fresh) = self.resolvers.resolve_with(resolver_idx, &req.md5).await {
                    target = fresh;
                }
            }

            // Concurrency gate for this host. Reuse the spill-acquired permit on the
            // first attempt if we have one; otherwise acquire (blocking) now.
            let _permit = match first_permit.take() {
                Some(p) => p,
                None => queue
                    .semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("host semaphore"),
            };
            // A host slot is now HELD — only now is this leg genuinely downloading.
            // Announce it on the first attempt so the variation flips
            // queued→downloading exactly when a transfer can begin (not at submit
            // time, when many jobs are still waiting behind the per-host cap).
            if attempt == 1 {
                let _ = events
                    .send(Progress::Resolved {
                        md5: req.md5.clone(),
                        leg_id,
                        is_hedge,
                        host: target.host.clone(),
                        total_bytes: target.total_bytes,
                    })
                    .await;
                // Continuing from a partial already on disk → chronicle it.
                let existing = current_part_len(&leg_dest).await;
                if existing > 0 {
                    let _ = events
                        .send(Progress::Resuming {
                            md5: req.md5.clone(),
                            leg_id,
                            is_hedge,
                            host: target.host.clone(),
                            offset: existing,
                        })
                        .await;
                }
            }
            // Rate limit (token bucket + jitter) for this host.
            queue.rate.acquire().await;

            // Base progress on the ACTUAL bytes already on disk, not the caller's
            // resume hint. `download_with_client` resumes from whatever the `.part`
            // holds, so a stale hint (e.g. 0 — the job's resume_offset is only set
            // on pause) would seed the SpeedTracker at 0 and then jump to the
            // partial size on the next reading, reporting a bogus multi-MB/s burst.
            // `total_written` is 0 at the top of every attempt, so this is the true
            // starting offset for both reporting AND the download Range request.
            resume_offset = current_part_len(&leg_dest).await;
            let bytes_done = resume_offset + total_written;
            let speed_bps = speed.observe(bytes_done, start.elapsed().as_secs_f64());
            let _ = events
                .send(Progress::Bytes {
                    md5: req.md5.clone(),
                    leg_id,
                    is_hedge,
                    host: crate::download::current_edge(&req.md5)
                        .unwrap_or_else(|| target.host.clone()),
                    bytes_done,
                    total_bytes,
                    speed_bps,
                    eta_secs: crate::speed::eta_secs(bytes_done, total_bytes, speed_bps),
                })
                .await;

            // Run the actual transfer concurrently with a stall monitor (which polls
            // this leg's `.part` size, feeds a SpeedTracker, and fires a hedge when
            // the windowed rate stays at/below the threshold). The download observes
            // BOTH this request's cancel token and the race's group_cancel (so a
            // winning sibling stops this leg).
            let dl = download_with_client_cancellable(
                &self.client,
                &target,
                &leg_dest,
                resume_offset,
                &cancel.token,
                Some(events),
                Some(&req.md5),
            );

            // Always run the poll loop while the transfer streams: it drives the
            // per-leg PROGRESS emit every tick, so %/ETA/bar move in real time even
            // with hedging OFF (the default). The stall→hedge decision INSIDE the
            // loop is still gated on `hedge.enabled`, so with hedging off this only
            // adds a lightweight ~200ms `.part`-size poll — no extra network, and
            // pause/cancel still observe the same cancel tokens.
            let result = {
                tokio::pin!(dl);
                loop {
                    // Poll on a tick while the download runs. This drives BOTH the
                    // stall monitor AND the per-leg progress emit, so cap it at
                    // 200ms: a fraction of the stall window would be multiple
                    // seconds (stall_window/8 ≈ 1.9s by default), which is far too
                    // coarse to show progress — a small/fast file finishes between
                    // polls, so the UI only ever sees 0% → done. Stall detection
                    // keys off elapsed time, not tick count, so a finer tick is
                    // harmless there.
                    let tick = (self.hedge.stall_window / 8)
                        .min(Duration::from_millis(200))
                        .max(Duration::from_millis(25));
                    tokio::select! {
                        biased;
                        // If a sibling won the race, abort this leg promptly.
                        _ = race.group_cancel.cancelled() => {
                            cancel.token.cancel();
                            break (&mut dl).await;
                        }
                        out = &mut dl => break out,
                        _ = tokio::time::sleep(tick) => {
                            // Sample on-disk progress for this leg, feed the tracker.
                            // `current_part_len` is the `.part`'s TOTAL length, which
                            // already includes any resumed bytes — do NOT add
                            // `resume_offset` again (that double-counts it and makes
                            // the reported size exceed the real total on a resume).
                            let on_disk = current_part_len(&leg_dest).await;
                            let sp = speed.observe(on_disk, start.elapsed().as_secs_f64());
                            let _ = events
                                .send(Progress::Bytes {
                                    md5: req.md5.clone(),
                                    leg_id,
                                    is_hedge,
                                    host: crate::download::current_edge(&req.md5)
                                        .unwrap_or_else(|| target.host.clone()),
                                    bytes_done: on_disk,
                                    total_bytes,
                                    speed_bps: sp,
                                    eta_secs: crate::speed::eta_secs(on_disk, total_bytes, sp),
                                })
                                .await;
                            // Stall → maybe hedge (only the primary leg hedges, once).
                            if self.hedge.enabled
                                && !leg.is_hedge
                                && !hedge_launched
                                && self.is_stalled(
                                    &mut speed,
                                    on_disk - resume_offset,
                                    start.elapsed(),
                                )
                            {
                                let _ = events
                                    .send(Progress::Stalled {
                                        md5: req.md5.clone(),
                                        leg_id,
                                        is_hedge,
                                        host: target.host.clone(),
                                        bytes_done: on_disk,
                                        speed_bps: sp,
                                    })
                                    .await;
                                if self.try_launch_hedge(req, events, race, hedges).await {
                                    hedge_launched = true;
                                }
                            }
                        }
                    }
                }
            };

            match result {
                Ok(written) => {
                    total_written += written;
                    let bytes_done = resume_offset + total_written;
                    let speed_bps = speed.observe(bytes_done, start.elapsed().as_secs_f64());
                    let _ = events
                        .send(Progress::Bytes {
                            md5: req.md5.clone(),
                            leg_id,
                            is_hedge,
                            host: crate::download::current_edge(&req.md5)
                                .unwrap_or_else(|| target.host.clone()),
                            bytes_done,
                            total_bytes,
                            speed_bps,
                            eta_secs: crate::speed::eta_secs(bytes_done, total_bytes, speed_bps),
                        })
                        .await;
                    return Ok((leg_dest.clone(), total_written));
                }
                // Deliberate pause/cancel: stop immediately, no retry.
                Err(e @ DownloadError::Cancelled { .. }) => {
                    return Err(e);
                }
                Err(e) => {
                    // Permanent: stop retrying this host immediately.
                    if !e.is_transient() {
                        return Err(e);
                    }
                    // On a transient mid-transfer failure, resume from whatever the
                    // .part has on disk.
                    let new_offset = current_part_len(&leg_dest).await;
                    // Reset the attempt budget when THIS attempt downloaded a
                    // meaningful chunk: a transfer that keeps making progress
                    // (despite transient drops) shouldn't exhaust its retries and
                    // give up. Progress is bounded by the file size, so this can't
                    // loop forever — an attempt that stops making progress still
                    // counts and eventually trips `max_attempts`.
                    const PROGRESS_RESET_BYTES: u64 = 500 * 1024;
                    if new_offset.saturating_sub(resume_offset) >= PROGRESS_RESET_BYTES {
                        attempt = 0;
                    }
                    if attempt >= max_attempts {
                        return Err(e);
                    }
                    resume_offset = new_offset;
                    total_written = 0;

                    // `attempt` may have just been reset to 0 (progress made);
                    // `backoff_for` underflows at 0, and a progressing retry wants
                    // minimal delay anyway, so floor at 1.
                    let backoff = self.backoff_for(attempt.max(1));
                    let _ = events
                        .send(Progress::Retrying {
                            md5: req.md5.clone(),
                            leg_id,
                            is_hedge,
                            host: target.host.clone(),
                            attempt,
                            backoff,
                            error: e.to_string(),
                        })
                        .await;
                    // Drop the permit before sleeping so we don't hold a host
                    // concurrency slot during backoff.
                    drop(_permit);
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    /// True when this leg's smoothed throughput has stayed at/below
    /// `stall_min_bps` for at least `stall_window`, after fetching at least
    /// `min_hedge_file_bytes`. `leg_bytes` is bytes fetched on THIS leg (excludes
    /// any resume offset); `elapsed` is how long the leg has been streaming.
    fn is_stalled(
        &self,
        speed: &mut crate::speed::SpeedTracker,
        leg_bytes: u64,
        elapsed: Duration,
    ) -> bool {
        // Arm only after the window has elapsed (ignore slow starts) and enough
        // bytes have arrived that hedging is worthwhile.
        if elapsed < self.hedge.stall_window || leg_bytes < self.hedge.min_hedge_file_bytes {
            return false;
        }
        match speed.speed_bps() {
            Some(v) => v <= self.hedge.stall_min_bps,
            // No measurable speed yet but past the window with bytes flowing — a
            // hung socket; treat as stalled.
            None => true,
        }
    }

    /// Attempt to launch a hedge leg for a stalled primary. Respects
    /// `max_legs_per_book` and the global `max_concurrent_hedges` semaphore; both
    /// are non-blocking `try_acquire`s, so a denied hedge just leaves the slow leg
    /// running (it may un-stall or a slot frees later). Returns true if a hedge
    /// leg was spawned.
    async fn try_launch_hedge(
        self: &Arc<Self>,
        req: &DownloadRequest,
        events: &mpsc::Sender<Progress>,
        race: &Arc<RaceGroup>,
        hedges: &Arc<Mutex<Vec<tokio::task::JoinHandle<JobOutcome>>>>,
    ) -> bool {
        // Per-book leg cap (1 primary + hedges ≤ max_legs_per_book).
        let cap = self.hedge.max_legs_per_book.max(1);
        // Reserve a leg slot atomically.
        let prev = race.leg_count.fetch_add(1, Ordering::SeqCst);
        if prev + 1 > cap {
            race.leg_count.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        // Global hedge cap (held for the hedge leg's lifetime).
        let hedge_permit = match self.hedge_gate.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                race.leg_count.fetch_sub(1, Ordering::SeqCst);
                return false;
            }
        };
        // Global concurrency cap (G): hedge legs count against it too. If no global
        // slot is free, skip the hedge — the slow primary keeps running un-hedged
        // (a denied hedge is never fatal). Held for the hedge leg's lifetime.
        let global_permit = match self.global_gate.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                race.leg_count.fetch_sub(1, Ordering::SeqCst);
                return false; // `hedge_permit` drops here, freeing the hedge slot
            }
        };

        // A unique sibling temp for the hedge leg, so it never clobbers the
        // primary's `.part`. Same parent dir as the dest → atomic promote rename.
        static HEDGE_SEQ: AtomicU64 = AtomicU64::new(0);
        let n = HEDGE_SEQ.fetch_add(1, Ordering::Relaxed);
        let short = &req.md5[..req.md5.len().min(8)];
        let mut name = race.dest.as_os_str().to_owned();
        name.push(format!(".hedge.{short}.{n}"));
        let leg_dest = std::path::PathBuf::from(name);

        // Hosts already racing — the hedge must pick a different one.
        let exclude_hosts = race.hosts.lock().await.clone();
        // OBSERVABILITY: a hedge launch was previously invisible, so "did this md5
        // race on two hosts?" could only be inferred. Log every launch (md5 + the
        // host(s) the hedge will avoid, i.e. the stalled primary's) so a flicker /
        // two-leg report is answerable from the log, not guessed.
        tracing::info!(
            md5 = %req.md5,
            avoiding_hosts = ?exclude_hosts,
            leg = n,
            "hedge leg launching: primary stalled past the hedge window — racing this md5 on an alternate host"
        );

        race.legs.lock().await.push(LegTemp {
            leg_dest: leg_dest.clone(),
        });

        let hedge_leg = LegCtx {
            // Next id in start order (primary was 0).
            leg_id: race.next_leg_id.fetch_add(1, Ordering::SeqCst),
            leg_dest,
            is_hedge: true,
            // Start resolving from the top of the chain; the exclude list keeps it
            // off the in-race hosts. (A resolver index past the stalled one would
            // also work, but excluding by host is more robust across md5/host axes.)
            resolver_start: 0,
            exclude_hosts,
            _hedge_permit: Some(hedge_permit),
        };

        // Spawn the hedge leg sharing the same race group. It runs the full
        // resolve+download path on an alternate host into its own temp.
        let this = Arc::clone(self);
        let req2 = req.clone();
        let events2 = events.clone();
        let cancel2 = CancelHandle {
            // The hedge leg shares the book's cancel token (pause/cancel reach it)
            // but its own download is additionally bounded by group_cancel.
            token: race.group_cancel.child_token(),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let race2 = Arc::clone(race);
        let hedges2 = Arc::clone(hedges);
        let handle = tokio::spawn(async move {
            // Hold the global permit for the hedge leg's whole lifetime so it
            // counts against G (released when this task ends).
            let _global_permit = global_permit;
            this.process_one_inner(&req2, &events2, &cancel2, race2, hedge_leg, &hedges2)
                .await
        });
        hedges.lock().await.push(handle);
        true
    }

    /// Exponential backoff with full jitter, capped at `max_backoff`.
    fn backoff_for(&self, attempt: u32) -> Duration {
        let exp = self
            .base_backoff
            .saturating_mul(1u32 << (attempt - 1).min(16));
        let capped = exp.min(self.max_backoff);
        // Full jitter in [0, capped].
        let millis = capped.as_millis() as u64;
        if millis == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(fastrand::u64(0..=millis))
        }
    }
}

/// Length of the on-disk `.part` for `dest`, or 0 if absent.
async fn current_part_len(dest: &std::path::Path) -> u64 {
    let part = crate::download::part_path(dest);
    match tokio::fs::metadata(&part).await {
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod cdn_tests {
    use super::cdn_group;

    #[test]
    fn cdn_group_buckets_libgen_family_and_isolates_independent_lanes() {
        // The libgen.li family all front the same booksdl CDN → grouped.
        assert_eq!(cdn_group("libgen.li"), Some("booksdl"));
        assert_eq!(cdn_group("libgen.vg"), Some("booksdl"));
        assert_eq!(cdn_group("libgen.la"), Some("booksdl"));
        // Independent lanes are distinct groups (so failover reaches them).
        assert_eq!(cdn_group("libgen.pw"), Some("libgen-download"));
        assert_eq!(cdn_group("randombook.org"), Some("libgen-download"));
        assert_eq!(cdn_group("gateway.ipfs.io"), Some("ipfs"));
        assert_eq!(cdn_group("annas-archive.gl"), Some("annas"));
        assert_ne!(cdn_group("libgen.li"), cdn_group("gateway.ipfs.io"));
        // Unknown hosts are NOT grouped → never spuriously skipped (so a genuine
        // alternate mirror, or the test mock on 127.0.0.1, still fails over).
        assert_eq!(cdn_group("example.com"), None);
        assert_eq!(cdn_group("127.0.0.1"), None);
    }
}

#[cfg(test)]
mod leg_guard_tests {
    use super::{LegEndGuard, Progress};
    use tokio::sync::mpsc;

    /// The Drop guard is the death signal for a leg: dropping it (any exit path
    /// of `process_one_leg`) emits exactly one `LegEnded` carrying the leg's id.
    /// This is the mechanism §3-A relies on; the end-to-end behavior on a real
    /// download is covered in `tests/download_queue.rs`
    /// (`leg_emits_exactly_one_leg_ended_with_monotonic_primary_id`).
    #[tokio::test]
    async fn drop_guard_emits_exactly_one_leg_ended() {
        let (tx, mut rx) = mpsc::channel::<Progress>(8);
        {
            let _guard = LegEndGuard {
                events: tx,
                md5: "abc123".into(),
                leg_id: 7,
            };
            // Guard alive: nothing emitted yet.
            assert!(rx.try_recv().is_err());
        } // guard dropped here → emits LegEnded

        match rx.try_recv() {
            Ok(Progress::LegEnded { md5, leg_id }) => {
                assert_eq!(md5, "abc123");
                assert_eq!(leg_id, 7);
            }
            other => panic!("expected one LegEnded, got {other:?}"),
        }
        // Exactly one event; the sender is dropped with the guard, so the channel
        // is now closed/empty.
        assert!(rx.try_recv().is_err(), "exactly one LegEnded");
    }
}
