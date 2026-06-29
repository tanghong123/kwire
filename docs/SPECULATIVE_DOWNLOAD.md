# Speculative / Hedged Download: race a stalled mirror against a fresh one

Status: **partially implemented; hedging default-OFF** — extends
`docs/EXECUTION_MODEL.md` (the goal-driven engine + per-host `Scheduler`).

What has LANDED in `crates/core/src/queue.rs`: the per-leg model — `Progress`
carries `leg_id` (start-order: primary = 0, hedges 1,2,…) + `is_hedge`, the
`LegEnded` variant + its RAII Drop guard, `RaceGroup.next_leg_id`, the stall
monitor, and the hedge controller. The UI consumes legs through the shared
`LegTracker` (alt-copy rows; see `docs/LEG_LIFECYCLE.md`).

What is GATED OFF: actually *launching* a hedge. `HedgeConfig::enabled` defaults
`false` (`queue.rs`), so no speculative leg is spawned in production yet — pending
field validation that racing a second mirror is a net win on real (slow) libgen
downloads. Flip `enabled` to exercise the full path; the plumbing + UI are ready.

## 0. TL;DR

When a download makes slow or no progress for a configured window, the scheduler
launches a **competing** download of the *same book* from a different
mirror/host into a *distinct* temp file. The first transport to produce a
**verified, md5-complete** file wins: it is promoted to the final path and every
sibling transport for that book is cancelled and its `.part` cleaned up. We cache
**all** discovered mirrors per book at search time so a hedge has somewhere to
go. The feature is **off by default** (opt-in / adaptive), capped to a small
number of concurrent hedges, and fully respects the existing per-host
concurrency, rate limits, and host-spill.

The core seam: today the unit of work is **one `DownloadRequest` (one md5) → one
`process_one` → one host queue**. A hedge makes the unit of work **one *book
variation* → up to *K* concurrent transports across distinct hosts/md5s, racing
into distinct temp files, joined on first-verified-wins.**

---

## 1. Goal & non-goals

### Goal

Resilience for **slow or dead mirrors**. The libgen mirror population is flaky:
`libgen.li`/`vg`/`la` all front one CDN (`booksdl.lc`), `libgen.pw`/
`randombook.org` front another (`libgen.download`, *no Range → not resumable*),
and IPFS gateways (`ipfs.io`/`dweb.link`/`pinata`) are independently rate-limited
and frequently 403/429/504 (see the resolver docs in `download.rs`). A single
chosen mirror can crawl at a few KB/s or hang mid-stream while a *different* lane
would deliver the same bytes in seconds. Today the only escape is a transient
**error** (reset/timeout) that trips retry/failover in `download_on_host`; a
mirror that is merely *slow* (never errors) is not handled at all — the user
watches a 2 KB/s bar. Hedging closes that gap.

### When hedging helps vs. wastes

| Situation | Hedge? | Why |
|-----------|--------|-----|
| Mirror streaming at a healthy rate | **No** | A second transport only burns bandwidth + politeness budget for no latency win. |
| Mirror stalled (0 bytes for N s) or crawling (< threshold bytes/s for N s) | **Yes** | A fresh lane likely beats waiting; the slow one keeps running as insurance until someone wins. |
| Only one mirror exists for the book | **No** | Nothing to race; fall back to today's retry/failover on the single host. |
| Tiny file (e.g. < ~256 KB) | **No** | It finishes inside the stall window anyway; hedging adds load with no benefit. |
| Many books still queued, host budgets tight | **Throttled** | Hedges compete for the same per-host slots as first-attempts; cap them so they never starve un-started books (see §4). |

### Non-goals

- **Not** a parallel/multi-connection accelerator (segmented download of one
  file across mirrors). Each transport fetches the **whole** file independently;
  we keep the winner and discard the rest. (libgen mirrors don't share a byte
  layout we could stitch, and Range support is inconsistent.)
- **Not** a replacement for retry/failover. Transient *errors* are still handled
  by `download_on_host` (retry) and `process_one_inner` (failover) exactly as
  today. Hedging is the answer to **slowness without error**.
- **Not** on by default. Default bandwidth/politeness posture is unchanged unless
  the user opts in (or adaptive mode is enabled — see §10).

---

## 2. Caching all mirrors for a book

### What "all mirrors" means here

Two distinct axes, both needed:

1. **Same md5, multiple download lanes.** One md5 is fetchable from every
   download *site* in the resolver chain (`libgen.li`, `libgen.vg`, `libgen.la`,
   `libgen.pw`, `randombook.org`, `ipfs`). This is already what `ResolverChain`
   walks for failover — but only *sequentially, on error*. For hedging we want to
   know the *full set of independent backend hosts* an md5 resolves to (the three
   independent CDN lanes: `booksdl.lc` family, `libgen.download`, IPFS), so a
   hedge picks a host **different from the stalled one**. No model change is
   needed for this axis — the chain already encodes it — but the *scheduler* must
   expose "resolve this md5 on a host other than `H`".

2. **Distinct md5s of the same book.** Search returns several `Candidate`s for
   one `BookRequest` — e.g. the same EPUB uploaded twice (different md5), or the
   same edition on two search mirrors. These are *different files* that satisfy
   the *same request variation* (same format, "right book"). A hedge may race
   `candidate_A.md5` on host X against `candidate_B.md5` on host Y. This axis
   **is** already in the model: `BookRequest.candidates: Vec<Candidate>`, each
   with its own `md5` and `source_host`. Today only `selected` (one md5) is
   downloaded; hedging draws *alternates* from the sibling candidates of the same
   format.

So "all mirrors for a book" = **{ for each candidate of the chosen format: its
md5 } × { each independent download host the chain resolves that md5 to }**. The
search-result candidates give the md5 axis; the resolver chain gives the host
axis. We already store both — the gap is (a) keeping *enough* candidates (not
just the winner) and (b) recording which host actually served bytes so a hedge
avoids the stalled one.

### Where it is stored (model change)

The minimal, JSON-compatible model change:

| Type | Field | Purpose |
|------|-------|---------|
| `Candidate` (model.rs) | already has `md5`, `source_host`, `extension`, `size_bytes` | The per-md5 mirror axis. **No change** — but see `keep_top` below. |
| `DownloadJob` (model.rs) | **add** `hedges: Vec<HedgeLeg>` `#[serde(default)]` | Per-leg transport state for an in-flight hedge: `{ md5, host, temp_path, bytes_done, speed_bps, state }`. Empty for a normal (un-hedged) download. |
| `DownloadJob` (model.rs) | already has `host: Option<String>` | The host that *won* (or is leading). |
| `HedgeLeg` (new, model.rs) | `{ md5: String, host: Option<String>, temp_path: String, bytes_done: u64, speed_bps: Option<u64>, state: JobState }` | One racing transport. `temp_path` is a unique sibling of the dest (see §4). |

Why a per-job `Vec<HedgeLeg>` rather than a per-book mirror set:

- The book *variation* (one format → one `selected` md5 today) is the natural
  owner of a race. The job that drives that variation already lives on the chosen
  `Candidate.job`. Hanging the legs off the job keeps the race's lifetime tied to
  the variation's lifetime and to the existing `acquisition()` roll-up.
- The md5 *axis* (alternate candidates) is read from the sibling `candidates`
  on demand when a hedge is launched; it does not need duplicating into the job.
- The host *axis* is resolved lazily by the scheduler (it already does this).

**`keep_top`** (`ListSettings.keep_top`, default 5) already controls how many
top-ranked variations (distinct md5s) are retained per request. Hedging *relies*
on this: it is the persisted pool of alternate md5s. No new persistence is needed
to keep alternates — they're already kept. (A future tweak could bias retention
toward *distinct hosts* so the pool is more lane-diverse, but that is optional.)

### Persistence / migration

Candidates and jobs are stored as **JSON blobs** (`candidate.json` column in
`store.rs`, `insert_candidate_tx`), not as typed SQL columns. Therefore:

- Adding `hedges: Vec<HedgeLeg>` to `DownloadJob` and the new `HedgeLeg` struct is
  a **serde-only** change — `#[serde(default, skip_serializing_if = "Vec::is_empty")]`
  makes old rows decode with an empty vec. **No SQL migration, no schema bump**
  (current `SCHEMA_VERSION = 4`). This is the same zero-SQL pattern the `goal`
  work avoided for candidates (it only bumped to v4 for the *book*-level column).
- Hedge legs are **transient by nature**: on a clean relaunch the
  `reset_inflight_for_resume` normalizer (EXECUTION_MODEL §9) already rewinds
  in-flight `Downloading` to `Matched`. Hedge legs should be cleared there too
  (drop the vec, delete stray temp files) so a resumed download starts as a
  single normal attempt — the surviving `.part` of the *leading* leg can seed the
  resume offset if it belongs to the winning host (see §7).

---

## 3. Stall detection

### Definition

A download leg is **stalled** when, over a sliding window of `stall_window`
seconds, its observed throughput stays at or below `stall_min_bps`:

```
stalled(leg) ⇔ for the last `stall_window` seconds,
               (bytes_done(now) - bytes_done(now - stall_window)) / stall_window  ≤  stall_min_bps
```

Two regimes fold into one rule:

- **No progress** (`stall_min_bps` effectively includes 0): 0 bytes for
  `stall_window` seconds → stalled. Covers a hung socket that hasn't errored.
- **Slow progress**: e.g. < 8 KB/s for `stall_window` seconds → stalled even
  though bytes trickle.

### Known vs. unknown total

`DownloadTarget.total_bytes` is `None` for `libgen.pw`/IPFS (the resolvers set it
so) and may be seeded from `Candidate.size_bytes` (EXECUTION_MODEL §14 backlog).
Stall detection is **independent of the total** — it is computed purely from the
*byte delta over time*, which the scheduler already measures: `download_on_host`
feeds `bytes_done` + `start.elapsed()` into `crate::speed::SpeedTracker` and
emits `Progress::Bytes { speed_bps, .. }`. So:

- **`speed_bps` already exists** as the smoothed throughput signal. Stall
  detection reuses it: a leg is a stall candidate once `speed_bps` has been
  `Some(v)` with `v ≤ stall_min_bps` continuously for `stall_window`. (Using the
  smoothed value avoids tripping on a single slow chunk.)
- The **window** guards against a slow *start* (TLS handshake, resolver dance,
  rate-limiter wait): we only arm the detector after the first bytes arrive *and*
  the leg has been streaming for at least `stall_window`, so a job that hasn't
  started yet is never "stalled".
- `total_bytes` is used only as a *secondary* guard: if it is known and the
  remaining bytes would finish within the window at the current rate, don't hedge
  (it's basically done). When unknown, fall back to the raw rate rule.

### Where it lives

In the **`Scheduler`**, alongside the existing speed tracking, **not** in the
orchestrator. Rationale: the scheduler is where `bytes_done`/`speed_bps` are
computed (`download_on_host`), where the per-host budgets live, and where
cancellation is already wired (`cancels: Mutex<HashMap<String, CancelHandle>>`).
The orchestrator stays a thin driver that submits requests and applies
`Progress` events. Concretely:

- `download_on_host` already loops emitting `Progress::Bytes`. Add a stall check
  on each tick: when armed and below threshold for the window, emit a new
  `Progress::Stalled { md5, host, bytes_done, speed_bps }` event **and** signal
  the scheduler's hedge controller (see §4) — *without* cancelling the slow leg.
  The slow leg keeps running as insurance.

`Progress::Stalled` is purely informational for the UI/engine; the *decision* to
spawn a hedge is made by the hedge controller so it can enforce the global cap.

---

## 4. Hedging mechanics

### The unit of work becomes a race

Today `Scheduler::run` spawns one `process_one` per `DownloadRequest`; that
future resolves+downloads one md5 on one host. The hedge introduces a **race
group** per book variation:

```
RaceGroup {
  book key (list_id, group_path, book_index, format),
  dest: PathBuf,                 // the final destination
  legs: Vec<Leg>,                // 1..=max_legs concurrent transports
  winner: OnceCell<Leg>,         // set atomically by the first verified finisher
  group_cancel: CancellationToken, // cancels all *losing* legs
}
Leg { md5, resolver_index/host, temp_path, cancel: CancellationToken }
```

- The **first** leg is the normal attempt (today's behavior), writing to the
  existing `dest.part` (so a resumed download keeps working unchanged).
- A **hedge leg** is added when the stall controller fires. It picks:
  - a **different host** than the stalled leg (resolve the *same* md5 starting at
    a resolver index whose `target.host` differs — reusing
    `resolve_and_pick_host`'s host-spill logic, but constrained to *exclude* hosts
    already in the group), OR
  - a **different md5** (an alternate sibling `Candidate` of the same format),
    resolved on any free host.
  - Preference order: an *independent CDN lane* (IPFS / `libgen.download` /
    `booksdl.lc` family — pick a lane not already racing) beats a same-family
    host, because same-family hosts share a CDN and rarely add bandwidth (the
    resolver docs call this out explicitly).

### Distinct temp files

`download.rs` already streams to `dest.part` then atomically renames to `dest`.
For hedges we need **per-leg** temp files so two transports don't clobber one
`.part`. Introduce a unique temp name per leg, e.g.
`dest.part.hedge.<short-md5>.<host-tag>` (still a sibling of `dest`, so the final
`fs::rename` is same-filesystem and atomic). The first (normal) leg keeps using
plain `dest.part` so its resume semantics are untouched. `download.rs` already
takes the destination path as a parameter, so this is just choosing the path the
leg writes to — `part_path` stays the single source of truth for "the `.part` of
*this* dest", and a hedge leg simply has its own dest-shaped temp target whose
`.part` is unique. (Concretely: a leg downloads to a unique `leg_dest`, then on
win we rename `leg_dest` → the real `dest`.)

### Respecting per-host concurrency, rate limits, host-spill

Each leg goes through the **same** `download_on_host` path as a normal download:
it acquires the destination host's semaphore permit and waits on that host's rate
limiter. So:

- A hedge leg **cannot** exceed a host's `max_concurrency` — it competes for the
  same semaphore as everything else on that host. (This is exactly what the
  existing `host_wide_concurrency_cap` test guarantees; a hedge is just another
  acquirer.)
- A hedge leg is subject to the host's `min_interval` rate limit.
- By construction a hedge targets a **different host** than the stalled leg, so
  it lands on a *different* semaphore — it does not pile onto the saturated host.
  This is the same intuition as host-spill (`resolve_and_pick_host`); we reuse
  that "find a resolvable host with a free slot" routine, additionally excluding
  hosts already in the race group.

### Caps so hedges don't starve other books

Two limits, both in the hedge controller:

| Cap | Default | Meaning |
|-----|---------|---------|
| `max_legs_per_book` | 2 | At most one hedge per stalled variation (2 transports total). A third is only allowed if a leg *errors out* (frees its slot). |
| `max_concurrent_hedges` (global) | 2 | Across the whole app, at most this many *extra* (hedge) legs in flight at once. A semaphore in the controller. When exhausted, a newly-stalled leg simply waits (it may un-stall, or a hedge slot frees up). |

The global cap ensures hedges are a *small* tax on the host budgets. First-attempt
downloads of not-yet-started books are **never** preempted: a hedge only ever
*adds* a leg using a host-spill free slot; if no free slot exists on any alternate
host, the hedge is deferred (the slow leg keeps running). This guarantees a hedge
never delays an un-started book past what the host caps already impose.

---

## 5. First-finisher-wins

### Atomic promotion

A leg "finishes" only when `download_with_client_cancellable` returns `Ok` —
which means the bytes are fully streamed **and** md5-verified (the function hashes
the assembled `.part` and rejects a mismatch *before* returning). So every
finisher is already a verified-complete file in its own temp path. The race group
resolves the winner with a single `OnceCell`/atomic-swap:

```
on leg L finishing Ok:
  if group.winner.set(L).is_ok() {          // L is the FIRST → it wins
     group.group_cancel.cancel();           // stop every other leg
     fs::rename(L.leg_dest, group.dest);     // atomic promote (same fs)
     emit Progress::Done { md5: L.md5, host: L.host, path: dest, .. }
     cleanup: remove every OTHER leg's temp + .part
  } else {                                   // someone already won
     // L lost the race-to-set: discard L's just-finished temp file.
     fs::remove_file(L.leg_dest); fs::remove_file(part_path(L.leg_dest));
  }
```

`OnceCell::set` (or an `AtomicBool` compare-exchange) is the linearization point:
exactly one leg observes `Ok`. The promotion `rename` and the losers' cleanup
happen only on that path.

### Races

- **Two legs finish near-simultaneously.** Both call `set`; the `OnceCell`
  serializes them. The loser hits the `else` branch and deletes its own temp.
  Because each leg has a **distinct** temp path, the loser's delete cannot touch
  the winner's file. The final `dest` is written by exactly one `rename`.
- **Winner finishes while a loser is mid-stream.** `group_cancel.cancel()` trips
  each loser's `CancellationToken`; `download_with_client_cancellable` already
  observes the token (`tokio::select!` on `cancel.cancelled()`) and returns
  `Cancelled`. The controller treats a `Cancelled`-because-we-won as a *silent*
  loss (not surfaced as an error), and removes the loser's temp + `.part` (hard
  cancel semantics).
- **md5 of the winner is wrong.** Impossible by construction: a leg can only win
  on `Ok`, and `Ok` already implies md5 match. A leg whose bytes mismatch returns
  `Permanent` and **does not** win (see §7).

### Partial files & resume

- The **first/normal** leg keeps writing `dest.part` and remains
  resume-from-`resume_offset` capable exactly as today.
- Hedge legs write their own temp `.part`s. If the user **pauses** the whole book
  mid-race, the leading leg (highest `bytes_done`, ideally a Range-capable host)
  is kept as the resumable one and its `.part`/offset are persisted onto the
  job's `resume_offset`/`host`; the other legs are hard-cancelled (temps removed).
  This keeps pause/resume single-stream and simple — we never try to resume two
  legs at once.

---

## 6. Integration with the goal engine

### Representation: one variation, N transient transports

A hedge is modeled as **one variation with up to *K* in-flight transports**, not
as N separate book variations. Reasons:

- The book's `status` roll-up (`acquisition()`) counts *variations*, not
  transports. A hedged EPUB is still "1 variation downloading", so
  "Downloading 1/2" stays correct.
- The chosen variation already owns a `DownloadJob`; the racing legs hang off it
  as `job.hedges: Vec<HedgeLeg>` (§2). The engine/`download_one` continues to see
  exactly one job per variation.
- When the race resolves, the winning leg's `{host, md5, output_path,
  md5_verified}` are written back onto the `DownloadJob` and `job.hedges` is
  cleared — leaving the persisted state identical to a normal completed download
  (so re-verify/replace/trash logic is unchanged).

`download_one` (orchestrator) changes minimally: it still builds `DownloadRequest`s
and consumes `Progress`, but it now also consumes `Progress::Stalled` /
new hedge-lifecycle events and updates `job.hedges` for the UI. The decision to
hedge lives in the scheduler; the orchestrator just *reflects* it.

### What the UI shows

- A hedged variation row shows e.g. **"trying 2 mirrors"** with the leading leg's
  speed/ETA (the fastest leg's `speed_bps`). The existing `download://progress`
  event (`ProgressPayload`) gains the leading-leg fields; a new lightweight
  `Progress::Stalled` / hedge event drives the "trying N mirrors" badge.
- On win, the row collapses back to a normal "Done" with the winning host —
  indistinguishable from a non-hedged completion.

### Pause / stop / cancel / replace

| Action | Effect on a race |
|--------|------------------|
| **Pause** (per-book / `pause_all`) | Cancel-keep the leading leg (persist its `.part`+offset+host as the resume seed if it's Range-capable); hard-cancel the rest. Resume later as a single normal attempt. |
| **Stop** (goal → `Idle`, per-list) | Same as pause for in-flight legs (the scheduler's existing pause path), driven by the engine lowering the goal. |
| **Cancel** (hard) | `group_cancel.cancel()` + remove every leg's temp/`.part`. No file promoted. |
| **Replace** (verify flow) | Unchanged: replace sets a new recommended md5 + goal `Complete`; the engine downloads it (which may itself hedge). Trash-on-replace fires off the *winning* leg's completion exactly as it does for a normal `Done`. |

The scheduler's `cancels` map is keyed by md5 today. With hedging, a book's race
may involve several md5s/hosts; the controller keeps a **per-book race registry**
keyed by the book/variation so `pause`/`cancel` resolve to *all* legs of that
book. (Per-md5 cancel still works for the legacy single-leg path.)

---

## 7. Failure & edge cases

| Case | Handling |
|------|----------|
| **All mirrors slow.** | Each stalled leg may spawn one hedge (subject to the global cap). If *every* lane is slow, the legs simply race to the least-slow finish; no leg is cancelled until one verifies. We never end up with *zero* legs running. |
| **A hedge 404s / errors.** | The hedge leg goes through normal `download_on_host` retry → `process_one` failover. A *permanent* error (404, gone) on a hedge leg just ends that leg (frees its hedge slot + host permit); the other legs continue. The book only `Failed`s if **all** legs are exhausted with no winner — same terminal rule as today, generalized over legs. |
| **Primary recovers after a hedge started.** | Fine — both keep running; whichever verifies first wins via the `OnceCell`. If the primary wins, the hedge is cancelled (silent loss). No special "cancel the hedge because the primary sped up" logic — first-finisher-wins subsumes it. (Optional refinement: if the primary's ETA drops below the hedge's, we *could* proactively cancel the hedge to save bandwidth; left as an open decision, §10.) |
| **md5 mismatch on a finisher.** | Returns `Permanent` from `download_with_client_cancellable`; that leg cannot win (it never reaches the `Ok` path). Its temp `.part` is removed by the existing mismatch cleanup. Other legs continue; if it was a *distinct-md5* hedge, that md5 is marked bad for the book. |
| **IPFS gateway lane.** | A perfect hedge target: genuinely independent bytes, and the `IpfsChainResolver` already rotates gateways on each `resolve` for built-in failover. IPFS legs are **not** resumable (gateways ignore Range) — but that only matters on *resume*, and we keep the *Range-capable* leg as the resume seed on pause. An IPFS leg that wins is promoted like any other. |
| **Resumability of the survivor.** | The winner is a complete, verified file — nothing to resume. On *pause* mid-race we keep the leading Range-capable leg's `.part` as the resume offset (§5). On *relaunch*, `reset_inflight_for_resume` clears `job.hedges` and rewinds to a single normal attempt; if a `.part` survives for the persisted host, the normal resume path uses it. |
| **Non-resumable host as the only fast leg.** | If a `libgen.pw`/IPFS leg (no Range) is leading and the user pauses, it cannot resume from offset — we drop its `.part` and resume as a fresh single attempt (today's behavior when a server ignores Range; `download_with_client_cancellable` already restarts from 0 on a 200-to-a-Range request). |

---

## 8. Validation — scenario walkthroughs

1. **Fast download → no hedge.** Leg streams above `stall_min_bps`; the stall
   detector never arms; `download_on_host` completes normally; `job.hedges` stays
   empty; UI shows a single host. ✓ (Identical to today.)
2. **Stalled primary → hedge → hedge wins → primary cancelled.** Primary on host
   A drops below threshold for `stall_window` → `Progress::Stalled` → controller
   spawns a hedge on host B (free slot, independent lane). B verifies first →
   `OnceCell::set(B)` → `group_cancel` cancels A → A returns `Cancelled` (silent)
   → B's temp renamed to `dest` → A's temp removed → `Progress::Done { host: B }`.
   Neither host ever exceeded its cap (each leg held one permit). ✓
3. **Primary wins after hedge launched → hedge cancelled.** A recovers and
   finishes before B → `set(A)` wins → B cancelled, B's temp removed →
   `Done { host: A }`. ✓
4. **All slow.** A and B both crawl; both keep streaming; the global hedge cap
   prevents a third leg; eventually the first to verify wins, the other is
   cancelled. No premature failure. ✓
5. **Hedge 404s.** B (a stale alternate md5) 404s permanently → B's leg ends,
   frees its hedge slot; A continues; if A later stalls again and a *different*
   lane is free, a new hedge may launch. Book `Failed` only if every leg is
   exhausted. ✓
6. **Pause mid-race.** User pauses → leading Range-capable leg kept (`.part` +
   offset persisted), others hard-cancelled; resume later as one normal attempt
   from the kept offset. ✓
7. **Single mirror only.** Book resolves to one host/md5 → no alternate lane →
   stall detector may fire but the controller finds no eligible hedge target →
   falls back to today's retry/failover on the one host. ✓

---

## 9. Implementation stages (each independently testable)

Each stage ships behind green tests + the existing headless harnesses, mirroring
the EXECUTION_MODEL staging discipline.

1. **Model + persistence (no behavior).** Add `HedgeLeg` and
   `DownloadJob.hedges: Vec<HedgeLeg>` (`#[serde(default, skip_serializing_if =
   "Vec::is_empty")]`). No SQL change (candidate/job are JSON blobs). Clear
   `hedges` in `reset_inflight_for_resume`. *Tests:* serde round-trip with/without
   the field; a v4 DB with no `hedges` decodes to empty.
2. **Stall detection signal.** In `download_on_host`, compute the windowed
   stall condition from the already-tracked `speed_bps`, emit a new
   `Progress::Stalled` event. No hedging yet — just the signal. *Tests:* extend
   the mock server (it already supports `delay`) with a *sustained*-slow path;
   assert a `Progress::Stalled` is emitted after the window and **not** for a fast
   path. (The mock's two-halves-with-`delay` write already models a slow stream;
   add a "trickle" mode that writes many tiny chunks with a per-chunk sleep so the
   windowed rate stays low without erroring.)
3. **Race group + first-finisher-wins (single host, no caps).** Introduce
   `RaceGroup`, distinct per-leg temp paths, `OnceCell` winner, promotion +
   loser cleanup, `group_cancel`. Wire a hedge launch off `Progress::Stalled`
   that resolves the *same md5 on a different host*. *Tests (the headline test):*
   register the same md5 under two `LabeledResolver` hosts — host A **slow**
   (sustained trickle), host B **fast** — assert (a) B wins, (b) A is cancelled,
   (c) the final file is byte-exact + md5-verified, (d) `sa.peak ≤ cap` and
   `sb.peak ≤ cap` (no host exceeds its concurrency), (e) exactly one file at
   `dest`, no stray `.part`/temp left behind. This is a direct extension of
   `spills_to_idle_host_when_preferred_is_saturated` + `host_wide_concurrency_cap`.
4. **Caps + controller.** Add `max_legs_per_book` + global `max_concurrent_hedges`
   semaphore; defer hedges when exhausted; exclude in-race hosts when picking a
   hedge target; prefer independent lanes. *Tests:* N stalled books with a global
   cap of 2 → at most 2 hedge legs in flight at once (assert via a peak counter on
   the hedge semaphore); un-started books still get served (no starvation).
5. **Distinct-md5 hedges.** Allow a hedge to draw an alternate sibling
   `Candidate` md5 (same format) when no distinct *host* is free for the same md5.
   *Tests:* two md5s for one book, host for md5#1 slow → hedge races md5#2; winner
   promoted; book records the winning md5.
6. **Engine/UI integration.** Surface `job.hedges` + a "trying N mirrors" badge;
   make pause/stop/cancel/replace resolve to *all* legs of a book (per-book race
   registry). *Tests:* the Tauri-boot harness + a "hedge then pause keeps the
   leading leg" check; re-verify/replace/trash unchanged after a hedged
   completion.
7. **Adaptive / default-off switch.** Config flag (`hedge: off | on | adaptive`)
   + the thresholds; default `off`. *Tests:* with `off`, no `Stalled`-triggered
   hedge ever spawns even on a slow path (parity with today).

### Testing strategy (existing mock-host harness)

The `download_queue.rs` harness already provides everything needed:

- **Slow host:** `PathConfig.delay` (split-body with a sleep). Add a *trickle*
  mode (many tiny writes with per-chunk sleeps) so the **windowed** rate — not
  just a one-off pause — stays below `stall_min_bps`, which is what the detector
  measures.
- **Fast host:** a plain `PathConfig::new(body)` on a second `LabeledResolver`
  host. Two `LabeledResolver`s pointing at the same mock server but with distinct
  `host` labels already route to distinct per-host queues (as
  `spills_to_idle_host...` does).
- **Assertions:** reuse `PathStats` (`peak`, `total`, `in_flight`, `timestamps`)
  to assert (a) the fast host wins, (b) the slow host is cancelled (its `total`
  may be 1, its bytes incomplete, its temp gone), (c) **no host exceeds its
  `max_concurrency`** (`peak ≤ cap`), (d) rate-limit spacing still holds, (e) no
  duplicate final files. md5 verification is already end-to-end via
  `download_with_client_cancellable`.

All offline, deterministic, no live mirrors — same posture as the current suite.

---

## 10. Open decisions (for the human)

| # | Decision | Proposed default | Notes |
|---|----------|------------------|-------|
| 1 | **On / off / adaptive** | **off** | Ship off; opt-in per the politeness/bandwidth posture. "adaptive" = only hedge when the book is large *and* the queue is not backlogged. |
| 2 | `stall_window` (how long slow before hedging) | **15 s** | Long enough to ignore slow starts/TLS/resolver dance; short enough to matter. |
| 3 | `stall_min_bps` (slow threshold) | **8 KB/s** | Below this for the window ⇒ stalled. 0-byte hang is the degenerate case. |
| 4 | `min_hedge_file_bytes` | **256 KB** | Don't hedge files that finish inside the window anyway. |
| 5 | `max_legs_per_book` | **2** | One hedge per stalled variation. |
| 6 | `max_concurrent_hedges` (global) | **2** | Keep hedging a small tax on host budgets. |
| 7 | **Proactively cancel a hedge when the primary recovers?** | **no** (rely on first-finisher-wins) | Saves bandwidth if yes, but adds ETA-comparison complexity; first-finisher-wins is already correct. |
| 8 | **Hedge host preference** | independent lane first (IPFS / `libgen.download` / `booksdl.lc` family — whichever lane is not already racing) | Same-family hosts share a CDN, so they rarely add bandwidth. |
| 9 | **Distinct-md5 hedges (Stage 5) in v1?** | yes, behind the same flag | Cheap given `keep_top` already retains alternates; helps when only one *host* serves a given md5. |
| 10 | **Bias `keep_top` retention toward distinct hosts?** | optional, later | Makes the alternate pool more lane-diverse; not required for v1. |
