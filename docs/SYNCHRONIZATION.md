# Synchronization design

How concurrent commands and the background execution engine share state without
freezing the app or serializing per-list work. Companion to
`docs/EXECUTION_MODEL.md` (the goal-driven engine) — this doc is specifically the
locking/concurrency contract. **For review.**

## 1. The two failure modes we must avoid

1. **App freeze.** A command that holds a global lock across a live, rate-limited
   network run (search/download of ~100 books) blocks *every* other command.
   (Original bug: `requery`/`start_downloads` held the library mutex across the
   whole pass.)
2. **Per-list serialization.** Even with a global lock that's only briefly held,
   if the engine holds a *per-list* lock across one book's network op, every book
   in that list runs one-at-a-time. (Second bug: `run_item` held the per-list
   orchestrator lock across `query_one`'s `self.search.search(...)`.)

Root rule that prevents both: **no lock is ever held across network I/O.**

## 2. Lock layers (each with a strict, minimal scope)

| Lock | Type | Guards | Held for |
|------|------|--------|----------|
| **Library** | `Arc<Mutex<Library>>` | the SET of lists (`Vec<LoadedList>`) | only to clone per-list `Arc`s, then dropped |
| **Per-list orchestrator** | `Arc<Mutex<Orchestrator>>` (one per list) | that list's SQLite store + in-memory book state (current `status` + `goal`) | brief reads/writes of state only — **never across search/download** |
| **Per-book exclusion** | the engine's `inflight: HashSet<BookKey>` | "at most one transition per book in flight" | the duration of a book's transition (this is what the engine "holds" across the network, NOT the per-list lock) |
| **Download cancel registry** | `Scheduler.cancels: Mutex<HashMap<md5, CancelHandle>>` | per-md5 cancellation tokens for in-flight transfers | internal to the scheduler |

Why a per-list lock at all (not per-book)? Each list owns ONE SQLite connection
(`Store`), which is not concurrent — so store reads/writes for a list must
serialize. That's fine: they're sub-millisecond. The expensive part (network) is
deliberately outside the lock.

## 3. Lock-ordering rules (deadlock freedom)

- **Library → orchestrator, never the reverse.** Acquire the library lock, clone
  the `Arc`(s), DROP the library lock, *then* lock an orchestrator. A command/engine
  never holds the library lock while awaiting an orchestrator lock.
- **Never hold two orchestrator locks at once.** Cross-list work clones all Arcs
  first, then locks one at a time.
- **Never hold any lock across `.await` on network** (search, `scheduler.run`,
  per-chunk reads). Only across synchronous store ops.
- The scheduler's `cancels` lock and an orchestrator lock are never nested in a
  fixed-bad order: the engine drops the orch lock before awaiting scheduler
  progress; a cancelling command locks the orchestrator (to flip state) and
  *separately* calls `scheduler.cancel(md5)` (its own lock), not nested.

## 4. The transition decomposition (network off-lock)

Every transition is split into **brief-lock phases around a lock-free network
phase**. The orchestrator exposes the phases; the engine sequences them, releasing
the per-list lock in between.

### Query (DONE — committed)
```
begin_query(pos)         // LOCK: if Queued → mark Querying, return (input, settings, search_arc)
  → search.search(input) // NO LOCK — concurrent with every other book
  → matching::evaluate    // NO LOCK (pure)
finish_query(pos, outcome)// LOCK: if still Querying → write candidates+status
```
Guarded by `crates/core/tests/intra_list_concurrency.rs` (two books in one list,
slow transport, asserts total ≈ 1× not ≈ 2×).

### Download (DONE — committed)
```
begin_download(pos, scheduler)  // LOCK: plan, mark Downloading, build DownloadRequests,
                                //       spawn scheduler.run → return {rx, pending, run, md5s}
loop over rx (NO LOCK):         // awaits network progress
  for each Progress:
    LOCK briefly → apply_progress(pending, prog) → UNLOCK   // persist job state/offset
    emit download://progress
run.await (NO LOCK)
finish_download(pos, completed) // LOCK: trash-on-replace + settle status/goal
```
The per-list lock is held only for `apply_progress` (a single-row update) per
progress tick — never across the transfer. So multiple books in a list download
concurrently (bounded by the scheduler's per-host caps, not the per-list lock).

### Reverify (DONE — committed)
Same shape as Query (it's search + match against a Done book): `begin_reverify`
(LOCK: capture input + downloaded md5) → search+evaluate (NO LOCK) →
`finish_reverify` (LOCK: set `review`/recommended if the top changed).

## 5. Mutating state while a transition is in flight

A command (Stop / Re-query / Replace / Select) can change a book's `status`/`goal`
WHILE the engine is mid-network for that book. Two cases:

- **Query / Reverify — discard, no abort.** Search is the first network op and
  writes no file. `finish_query`/`finish_reverify` **re-read** and apply only if
  the book is still in the expected transient state (`Querying`); if a command
  rewound it to `Queued` or changed the goal, the stale result is dropped and the
  engine re-plans. Cheap and correct.
- **Download — abort the transfer.** An in-flight download holds a connection and
  is writing a `.part`, so discarding isn't enough. A command that takes a
  `Downloading` book out of "wanting to download" must call
  `scheduler.cancel(md5)` (which fires the `CancelHandle`'s token; the transfer
  loop in `download_on_host` is cancellation-aware). The kept-`.part` policy:
  - **Stop/pause** → cancel keeping the partial (`resume_offset` persisted) so a
    later Start resumes.
  - **Re-query of a non-Done book / hard cancel** → cancel removing the partial.
  - **Replace** → the recommended copy is enrolled and downloaded; the old file is
    trashed on the new one's success (existing `trash_on_replace`).
  After cancel, the scheduler emits `Progress::Cancelled`; the engine's drain loop
  applies it (job → `Paused`/`Cancelled`) and `finish_download` doesn't clobber.

Idempotence + monotonicity make this safe: a transition only advances a book FROM
the state it observed; if that changed, it no-ops.

## 6. The engine's per-book exclusion (`inflight`)

The driver, each tick: PLAN (clone Arcs under brief library lock; snapshot each
orch under its brief lock; collect actionable books) → reserve `inflight` keys
(one short lock) so a book is spawned once → EXECUTE detached workers. A worker
removes its key and `notify`s the driver on completion. So the network-spanning
"hold" is the `inflight` membership, not any mutex — which is what lets the
per-list lock stay brief.

## 7. What broke through before, and the test that now catches it

The original engine shipped a concurrency test that only asserted **cross-list**
non-blocking (list A busy doesn't block list B / the library). It passed while
**intra-list** work was fully serialized. Lesson encoded as required tests:

- `intra_list_concurrency.rs` — N books, ONE list, slow transport: total ≈ 1×.
  (Fails ≈ 2× on a lock-across-network regression.) **Done.**
- `intra_list_download_concurrency.rs` — `two_books_in_one_list_download_concurrently`
  (slow mock host, asserts overlap) + `cancelling_a_downloading_book_aborts_promptly_and_keeps_partial`.
  **Done.**
- The existing `download_queue.rs` host-cap/failover/rate tests stay green (no host
  exceeds its cap under the off-lock drive). **Verified.**

## 8. Invariants checklist (review against these)

1. No `.await` on network while any `Mutex` guard is alive.
2. Library lock: only `lib.all_arcs()` / lookup, then dropped.
3. Orchestrator lock: only store/state reads+writes; released between begin /
   per-progress-apply / finish.
4. Acquire order always library → orchestrator; at most one orchestrator lock at a
   time; orchestrator lock and scheduler `cancels` lock never nested.
5. Every transition re-reads current state before committing (monotonic).
6. A `Downloading` book taken out of intent triggers `scheduler.cancel(md5)`.
7. Per-book `inflight` prevents double-spawn; removed + `notify` on completion.
8. Tests assert intra-list concurrency for BOTH query and download, plus
   cancellation — not just cross-list.

## 9. Concurrency control — the full mechanism set

§2–§8 cover *mutual exclusion* (which lock guards what). But locks are NOT how
parallelism is controlled; they only prevent data races. Seven distinct
mechanisms work together — each solves a different problem:

| # | Mechanism | Type | Controls | Where |
|---|-----------|------|----------|-------|
| 1 | Library + per-list `Mutex` | mutual exclusion | data races on the list set / a list's store+state | `state.rs`, `engine.rs` |
| 2 | `inflight: HashSet<BookKey>` | per-book exclusion (set membership, NOT a held lock) | ≤1 transition per book at a time | `engine.rs` (insert before spawn, remove on done) |
| 3 | `Semaphore`s | parallelism bound | how MANY run at once: query gate = `query_concurrency`; per-host download = `max_concurrency`; resolve gate | `engine.rs` gate; `queue.rs` `HostQueue.semaphore`, `resolve_gate` |
| 4 | `RateLimiter` (token bucket + jitter) | request-rate bound | requests/sec per host (≠ concurrency) | `queue.rs` per `HostQueue` |
| 5 | `Notify` (`engine_wake`) | coordination / liveness | when to re-plan (goal change or worker finished, else 750 ms tick) | `engine.rs` driver `select!` |
| 6 | **monotonic re-read** (validate-before-commit) | optimistic concurrency | correctness when state changes during the off-lock network phase — a transition re-reads under the lock and no-ops if the book moved; the concurrent command wins | every `begin_*`/`finish_*` |
| 7 | `CancellationToken` per md5 | cancellation | abort an in-flight transfer when a command changes intent | `queue.rs` `Scheduler.cancels` |

Key insight: parallelism is bounded by the **semaphores** (#3), kept correct by
the **monotonic re-reads** (#6) and **per-book `inflight`** (#2), and kept live by
**`Notify`** (#5). The mutexes (#1) are necessary for safety but are deliberately
held only for sub-millisecond store ops — they are not the concurrency-control
mechanism. #6 is what makes "release the lock during the network" *safe*: instead
of pessimistically holding a lock, we optimistically act on a snapshot and
re-validate before committing.
