# Design: pull-based download scheduling (honest state + balanced hosts)

Status: **draft for review** (2026-06-22). No code changed yet.

## 1. Problem (observed in the running app)

- Clicking "start" submits *every* ready book at once. The book flips to
  **Downloading** immediately, even though only a few transfer. The active panel
  showed `libgen.li 94/2`.
- Two eager transitions cause it:
  1. `orchestrator::begin_download` calls `set_status(Downloading)` at **submit**
     time, per book — before any slot exists.
  2. `queue::process_one_inner` emits `Progress::Resolved` (which the orchestrator
     maps to `JobState::Downloading`) **before** the per-host semaphore is acquired
     in `download_on_host`. `Scheduler::run` also spawns one task per request up
     front, so all ~94 tasks park on per-host semaphores while already reported as
     "downloading."
- Host binding is **greedy-first**: `resolve_and_pick_host` does a non-blocking
  `try_acquire` down the chain and, if no host is free, **falls back to the first
  (preferred) host**. Under saturation everything piles onto `libgen.li`
  (→ 47 waiting) while independent lanes (`libgen.download`, `ipfs`) are
  under-used (→ 4, or 0 for ipfs).
- There is **no global concurrency notion** — total concurrency is just the sum of
  per-host caps, and it's invisible/uncontrollable.

## 2. Goals

1. A variation transitions `queued → downloading` **only when a transfer is
   actually starting** (a host slot is held).
2. An explicit, bounded **global** number of concurrent downloads, with per-host
   politeness preserved (per-host cap + token-bucket rate limit).
3. **Balanced** host use: spread across healthy hosts, don't starve later-chain
   lanes, don't pile on the preferred one. (Reuses Phase B's SLUM+quality order.)
4. **Controlled ordering**: user-prioritized ("move to top") first, else FIFO.
5. Preserve every existing feature: resume (`.part` + Range), md5 verify,
   retry/backoff, mirror failover, hedging, pause/cancel/resume, dedupe by md5,
   per-list concurrency.

Non-goals: schema changes (job states already persist), search/match changes.

## 3. Proposed architecture — a single shared, pull-based driver

Replace *"each book pushes its requests; N tasks park on per-host semaphores"*
with *"one shared scheduler owns a global pending queue and a fixed pool of `G`
download workers that PULL work when they can actually run it."*

### Components
- **PendingQueue** (in the shared `Scheduler`): priority queue of
  `QueuedJob { md5, dest, expected_size, resume_offset, resolver_chain, priority,
  route (list/group/book/variation), seq }`. Order = `priority DESC`, then `seq
  ASC` (FIFO). Dedup by md5 at enqueue (existing behavior).
- **Global cap `G`** = `max_concurrent_downloads` — realized as the **size of the
  worker pool** (`G` long-lived worker tasks), not a bare semaphore, so count +
  ordering + host-binding are all decided in one place.
- **Per-host `HostQueue`**: `Semaphore(H_i)` + rate limiter — **unchanged**;
  politeness only.
- **`Notify`** ("work available / slot freed") to park idle workers and wake them
  when a job is enqueued, a host slot frees, or a priority changes.

### Worker loop (each of the `G` workers)
```
loop {
    (job, host, host_permit) = acquire_runnable().await;  // blocks until a job
                                                          // whose host has a free slot
    transition job: Pending -> Downloading;               // HONEST: a slot is held now
    result = download(job, host, host_permit);            // = today's download_on_host body
    on all-hosts-exhausted: requeue-or-fail per policy;
    drop host_permit; notify();                           // a slot freed
    persist outcome; emit Done/Failed;
}
```

### `acquire_runnable()` — the heart
```
loop {
    { lock pending+hosts;                          // brief critical section
      for job in pending (priority order) {
        for host in job.resolver_chain (Phase-B order, SLUM-down last) {
          if host_queue(host).try_acquire() {      // non-blocking per-host slot
            remove job from pending; return (job, host, permit);
          }
        }
      }
    }
    notify.notified().await;                       // nothing runnable; wait for a
                                                   // freed slot or new job, then re-scan
}
```
Guarantees: ≤ `G` concurrent transfers (pool size), ≤ `H_i` per host, **no worker
idles while a runnable (job,host) pair exists**, and load **spreads** because
saturated hosts are skipped (so ipfs/independent lanes get used). The job's host
is known from its resolver (`resolver.host`) *before* the network resolve, so the
scan needs no network.

## 4. Answer to "semaphore vs condition variable?"

Your instinct — *"acquire a slot, then pick a queued book, then transition it"* —
is exactly the worker loop. On the primitive:

- A **bare global `Semaphore(G)`** bounds the *count* but doesn't control **which**
  queued job proceeds or **which host** it binds to — the parked tasks are
  pre-committed, which is the bug today. Necessary but not sufficient.
- The fix is the **pull model**: an explicit **priority PendingQueue** + a fixed
  **worker pool** (size `G` = the global cap) + a **`Notify`** (tokio's
  condition-variable equivalent) to wake idle workers. Per-host **`Semaphore`s**
  stay for politeness.
- So: **per-host semaphores (keep) + worker-pool/`Notify` for the global cap and
  ordering.** The "explicit conditional variable guarding # of concurrent
  downloads" you intuited = the worker-pool size + `Notify`. (Equivalently, `G`
  could be a `Semaphore(G)` acquired at the top of each worker iteration — same
  effect; a fixed pool is simpler and avoids spawning unbounded tasks.)
- Key point: the primitive matters less than the **pull-from-an-explicit-queue**
  shape. Neither a bare semaphore nor a bare condvar fixes the eager transition
  unless jobs are pulled lazily instead of pushed.

## 5. State machine

- `Pending` (queued): matched/selected, awaiting a slot. **No host assigned** → UI
  shows "queued", no host badge.
- `Downloading`: a worker holds a host slot and is transferring. Set **at pull
  time**.
- `Verifying → Done / Failed / Paused / Cancelled`: unchanged.
- Fix (a): `begin_download`'s replacement **enqueues** and leaves variations
  `Pending`; the book's coarse status becomes `Downloading` only when ≥1 variation
  is actually downloading (derive it, or set on the first `Started` event).
- Fix (b): emit `Progress::Started` (or move `Resolved`) **after** the host slot is
  acquired.

## 6. Host selection & balancing (ties into Phase B)

- Each job's resolver chain is ordered by Phase B: SLUM-up + high measured success
  + low latency first; SLUM-down sinks (never dropped).
- `acquire_runnable` tries hosts in that order with non-blocking `try_acquire`, so
  a job runs on the best **currently-free** host. Saturated hosts are skipped →
  load spreads; independent lanes get used. Per-host caps + rate limits stay polite.

## 7. Ordering / fairness

- "Move to top" raises `priority`; queue is priority-then-FIFO; re-prioritizing
  calls `notify()`.
- Per-list fairness (optional, later): round-robin across lists or a per-list
  in-flight cap so one big list can't starve others. Default: global priority-FIFO.

## 8. Progress routing, resume, dedupe, hedging, pause/cancel

- **Progress routing**: each `QueuedJob` carries its route; the shared scheduler
  emits one progress stream keyed by route, and the engine applies it to the right
  orchestrator. (This is the biggest plumbing change — today progress is a
  per-book session.)
- **Resume**: `QueuedJob.resume_offset` + `.part` reuse — unchanged
  `download_on_host` body.
- **Dedupe by md5**: skip at enqueue if already queued/in-flight; on `Done`,
  satisfy all books sharing that md5 (existing).
- **Hedging**: a stalled transfer still spawns a sibling leg on a *different* host.
  Open question: count hedges against `G` or give them a small separate budget
  (today they use non-blocking `try_acquire` so they never block).
- **Pause/cancel**: per-md5 cancel handle unchanged; a paused job leaves the
  in-flight set (a `Paused` state, not auto-pulled); resume re-enqueues `Pending`.

## 9. Settings

- **New**: `max_concurrent_downloads` (`G`) — global cap, surfaced in Settings.
  Default ~5. Composes with per-host caps: effective ≤ `min(G, Σ H_i)`.
- Keep per-host `max_concurrency` (`H`), rate, attempts.

## 10. Incremental rollout (keeps tests green)

- **Step 1 — quick, low-risk (no redesign):** fix only the two eager transitions:
  (a) don't `set_status(Downloading)` at submit — enqueue as `Pending`; (b) move
  the `Resolved`/`Started` emission to *after* the host semaphore acquire in
  `download_on_host`. Effect: only the truly-transferring jobs (≤ `Σ H_i`) show
  "downloading"; the rest correctly show "queued." Honest state, no balancing
  change yet.
- **Step 2 — the redesign:** shared `PendingQueue` + worker pool (`G`) +
  `acquire_runnable`; replace per-book `scheduler.run` with `enqueue`; centralize
  progress routing; add the `G` setting.
- **Step 3 — balancing:** wire Phase B host order into `acquire_runnable`; add
  per-list fairness if needed.

## 11. Risks & testing

- Risk: centralizing progress routing touches the engine↔orchestrator boundary
  (per-list locks, pause/cancel/resume). Hedging vs `G`. Livelock-free
  `acquire_runnable` (must `notify()` on *every* slot release and enqueue).
- Tests: `acquire_runnable` picks highest-priority job whose host is free; respects
  `G` and `H_i`; skips saturated hosts; wakes on slot release. Timing test (mock
  hosts): `G=5, H=2`, 3 hosts, 20 jobs → ≤5 concurrent, ≤2/host, balanced, all
  finish. State test: a job stays `Pending` until a slot is held; only `G` show
  `Downloading`. Regression: resume, dedupe, failover, hedge, pause/cancel.

## 12. Open questions for review

1. **`G` default** and does it *replace* or *compose with* per-host caps?
   (Recommend compose: effective ≤ `min(G, Σ H_i)`, default `G = 5`.)
2. **Hedging budget**: count hedge legs against `G`, or a small separate budget?
3. **Fairness**: global priority-FIFO now, per-list round-robin later — OK?
4. **Primitive**: confirm worker-pool + `Notify` + per-host semaphores (vs a bare
   global semaphore).
5. **Land Step 1 now** (honest state) while Step 2/3 are reviewed?
