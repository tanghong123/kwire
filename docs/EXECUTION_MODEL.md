# Execution Model: per-book state + goal, driven by a background engine

Status: **design (for review)** — supersedes the synchronous, lock-holding
command model in `app/src-tauri/src/commands.rs`.

## 1. Why

Today each heavy command (`query_and_match`, `requery`, `start_downloads`) holds
the global library mutex across its entire **live, rate-limited network** run
(re-searching/downloading ~100 books). While held, every other command blocks:
during a Re-query the UI is frozen, "Start queue" does nothing, the spinner never
clears. Discovery, re-query, download, and verify are also four ad-hoc operations
with overlapping logic and subtle interactions (what gets reset, what's
preserved).

The fix is to separate **intent** from **execution**:

- A book carries a **current state** (how far it has progressed) and a **goal**
  (how far we want it to go).
- Commands only *set goals* (a tiny critical section) and wake the engine.
- A long-lived **execution engine** continuously pushes each book from its
  current state toward its goal, doing all network I/O **outside** any lock.

This eliminates the freeze (no command holds a lock across I/O), unifies the four
operations into one reconciler, and makes resume trivial (on launch the engine
just reconciles current→goal).

## 2. Core concepts

```
        ┌───────── current state ─────────┐         goal
 new → querying → {matched|needs|nomatch} → downloading → complete
                       │ (needs choice: blocked on user)
```

- **Current state** = the furthest stage a book has reached.
- **Goal** = the stage the engine should drive it to.
- **Engine** = a background task; for every book where `state < goal` and the
  book is *actionable* (not blocked, within concurrency budget), it performs the
  next transition.

## 3. States (current)

| State        | Meaning                                                        | Advanceable by engine? |
|--------------|----------------------------------------------------------------|------------------------|
| `New`        | Entry only; not searched (or invalidated by re-query).         | yes → `Querying`       |
| `Querying`   | Search in flight.                                              | in progress            |
| `NoMatch`    | Searched; no acceptable candidate.                            | no (terminal until re-query) |
| `NeedsChoice`| Searched; candidates exist but ambiguous — **user must pick**.| no (blocked on user)   |
| `Matched`    | Searched; a best candidate auto-chosen; nothing downloaded.   | yes → `Downloading` (if goal=Complete) |
| `Downloading`| ≥1 chosen variation in flight.                               | in progress            |
| `Complete`   | All chosen variations verified on disk.                       | terminal (re-verify only) |
| `Failed`     | Download attempts exhausted (distinct from `NoMatch`).        | no (until retry/re-query) |

Per-variation download jobs (epub + pdf independently) are unchanged; the book
state is the roll-up. `Complete` books additionally retain a **downloaded
artifact** record `{md5, path}` independent of discovery, so re-verify can compare
without losing the file.

## 4. Goals

| Goal       | Drive the book to…                              | Set by                              |
|------------|--------------------------------------------------|-------------------------------------|
| `Idle`     | nothing (paused / parked)                        | **Stop** (per-list); **every launch** |
| `Match`    | `Matched`/`NeedsChoice`/`NoMatch` (discover only) | (reserved; no UI action today)     |
| `Complete` | `Complete` (discover **and** download)           | **Start**, **Re-query**, **Import** (per-list) |

`Idle < Match < Complete`. A goal never *kills* in-flight work; lowering the goal
to `Idle` (Stop) pauses downloads via the scheduler; raising it resumes.

### Resolved UX (per user)

- **Per-list control + a clarified global.** Start / Stop / Re-query are
  per-list (the sidebar right-click menu). The global top-bar button is
  **relabeled "Start downloading for all lists"** — it sets goal `Complete` for
  every list (a convenience fan-out of the per-list Start), removing the
  confusion of a single global paused/running state. (A paired "Stop all" → goal
  `Idle` for every list is the natural complement.)
- **Re-query means "push this list to completion."** It re-discovers with the
  current algorithm AND downloads — goal `Complete` (not just `Match`). So a
  Re-query of a stale list both fixes the matches and finishes the downloads.
- **Launch is paused.** A list is **not** auto-restarted on relaunch: every
  book's goal resets to `Idle` at startup (the resume normalizer also rewinds
  in-flight `Querying`/`Downloading` to their pre-flight state). The persisted
  state (matched / done / review) is shown, but nothing runs until the user
  Starts or Re-queries that list. `Match` stays in the enum for a possible future
  "discover-only" action but no command sets it today.

## 5. Command → intent mapping

Commands take a brief library lock, mutate state/goal, persist, and `notify` the
engine. None do network I/O.

| Command            | Effect                                                                                 |
|--------------------|----------------------------------------------------------------------------------------|
| **Import list**    | Create entries; **goal = Complete** (implicit Start) → engine discovers + downloads.   |
| **Start** (per-list) | goal = `Complete` for that list.                                                     |
| **Start downloading for all lists** (global) | goal = `Complete` for every list (fan-out of per-list Start). |
| **Stop** (per-list) / **Stop all** | goal = `Idle` (in-flight downloads pause via the scheduler).               |
| **Re-query** (per-list) | For non-`Complete` books: state→`New`. For `Complete` books: schedule **re-verify** (re-discover for comparison; keep the file; set `review` on mismatch). Sets **goal = Complete** for all of them — re-query means "push this list to completion." |
| **Select candidate** (NeedsChoice) | record the user's pick → state `Matched`; if goal=Complete, engine downloads it. |
| **Replace download** (verify) | chosen = recommended, goal=Complete, mark old file `trash_on_replace`; engine downloads the replacement, then trashes the old on success. |

Re-query sets `Complete` (not just `Match`): re-querying a list expresses "find the
right copies and get them" — re-discover with the current algorithm **and**
download. Discovery state is rewound (`New`) for not-yet-downloaded books;
`Complete` books are re-verified (file kept) and flagged if a better copy exists.

## 6. The engine (driver)

A single long-lived task owned by `AppState`, with a `tokio::sync::Notify` wake
handle. Pseudocode of one tick:

```
loop {
  // 1) PLAN — short critical section: read state, pick actionable work.
  let plan = {
    let lib = library.lock();              // brief
    collect_actionable(&lib, budgets)      // books where state<goal, not blocked,
  };                                       // within query/host concurrency budgets
  // 2) EXECUTE — NO lock held across these. Bounded concurrency.
  for item in plan {                       // each runs as a worker future
    spawn(async {
      let result = do_io(item).await;      // search OR download — network, no lock
      let lib = library.lock();            // brief
      apply(&mut lib, item, result);       // persist new state + emit event
    });
  }
  // 3) WAIT — until woken (goal/state change, search/download done) or a timer.
  select! { _ = notify.notified() => {}, _ = sleep(idle_tick) => {} }
}
```

Key invariants:

- **Network I/O never holds the library lock** → no command can be starved → no
  freeze. This is the whole point.
- **Idempotent, monotonic transitions** — applying a result re-checks the current
  state (it may have changed) before committing, so concurrent commands are safe.
- **Concurrency budgets** reuse what exists: query concurrency (parallel
  searches) and the per-host download `Scheduler` (slots + rate limits + failover
  + host-spill). The engine asks the scheduler for download slots exactly as
  `start_downloads` does today.
- **Backpressure / wakeups**: a worker finishing (search or download) notifies the
  engine so the next transition is planned promptly; commands notify on goal
  change; an idle timer is a safety net.

This subsumes the existing operations: search workers = `query_all`; download
workers = `start_downloads` + scheduler; state→`New` reset = `requery_unsettled`;
re-verify of `Complete` books = `reverify_downloads`.

## 7. Verify-downloads, expressed in the model

`Complete` book + Re-query ⇒ engine runs a **re-verify** transition: re-search +
re-match (network, no lock), compare the fresh top candidate against the retained
downloaded `{md5}`. If different ⇒ set `review = true`, `recommended_md5 = top`.
The book stays `Complete` (file intact) but is surfaced as "Check download". The
user's **Replace** sets the recommended as chosen with goal=`Complete` and
`trash_on_replace`; the engine downloads it and, on success, moves the old file to
Trash. (This is exactly the feature already merged — it just becomes a transition
the engine schedules rather than a bespoke command path.)

## 8. Mapping to existing code

- `model.rs`: `RequestStatus` → the state enum above (mostly a rename/trim; add
  `goal: Goal` to `BookRequest`, `#[serde(default)]`). The `review`,
  `recommended_md5`, `trash_on_replace`, and per-variation `DownloadJob` fields
  stay.
- `orchestrator.rs`: its transition helpers (`query_all`, `start_downloads`,
  `requery_unsettled`, `reverify_downloads`) become the engine's per-book
  *transition functions* (operate on one book, return a result to persist),
  rather than whole-list loops the command awaits under lock.
- `app/src-tauri`: a new `engine` module owns the driver task + `Notify`; commands
  shrink to "lock, set goal/state, persist, notify". `start_downloads` /
  `query_and_match` / `requery` become goal-setters.
- The per-host `Scheduler` (`queue.rs`) is reused as-is for download budgets.

## 9. Persistence & migration

- Add `goal` (and keep `review`/`trash_on_replace` from schema v3). Schema bump
  v4; old rows default `goal = Idle`. *(Done — Stage 1, with a v3→v4 migration
  test.)*
- **Launch is paused (no auto-restart).** `resume_on_launch` sets every book's
  goal to `Idle` and rewinds in-flight `Querying`/`Downloading` to their
  pre-flight state (`New`/`Matched`) via the normalizer (today's
  `reset_inflight_for_resume`). The persisted discovery/download state is shown,
  but the engine does nothing until the user Starts or Re-queries a list. So
  `goal` is effectively session-scoped intent (persisted for crash-resilience,
  reset on every clean launch).

## 10. Events / UI contract

The engine emits per-book transition events (extends today's `query://book` +
`download://progress`). The UI updates **live** as books move New→…→Complete —
no more "frozen until a command returns". The per-book view already carries
`discovery`, per-variation `state`+progress+speed/ETA, `review`,
`recommended_md5`; we add the explicit `state` + `goal` so the UI can show, e.g.,
"queued for query", "downloading 1/2", and a per-list "12 discovering · 3
downloading · 40 done" that ticks in real time.

## 11. Decisions (resolved)

1. **Re-query goal** = `Complete` — re-query re-discovers AND downloads ("push the
   list to completion"). ✓
2. **Import** = implicit Start, goal `Complete`. ✓
3. **Controls** = per-list **Start / Stop / Re-query** (right-click) + a global
   **"Start downloading for all lists"** (and a "Stop all"). No single global
   paused/running state. ✓
4. **Launch = paused** — lists are not auto-restarted; goal resets to `Idle` on
   every launch. ✓
5. **NoMatch retry** = manual only (via Re-query); no background auto-retry. ✓

## 12. Validation — scenario walkthroughs

1. **Import** → entries, goal=Complete → engine: New→Querying→Matched→Downloading
   →Complete per book, concurrently, bounded. UI ticks live. ✓
2. **Re-query stale list** → non-complete books state→`New`, goal=`Complete` →
   re-discovered with the current algorithm (NoMatch/NeedsChoice greatly reduced)
   → and downloaded, all in one action. `Complete` books are re-verified (file
   kept) and flagged `review` if a better copy exists. ✓
3. **Concurrent Re-query + Start** (the bug): both are just state/goal writes +
   the engine churning in the background. Neither holds a lock across I/O, so
   Start takes effect immediately and matched books download *while* others are
   still being discovered. ✓ (root cause gone)
4. **NeedsChoice** → engine skips it (blocked on user); user picks → `Matched` →
   downloads (goal=Complete). ✓
5. **Stop / Start** (per-list) → goal Idle/Complete; in-flight respect the
   scheduler's pause; engine stops/resumes planning. ✓
6. **Restart mid-flight** → normalizer rewinds in-flight + **goal→Idle (paused)**;
   nothing runs until the user Starts/Re-queries. The persisted matched/done/
   review state is shown intact. ✓
7. **Verify wrong download (Oz)** → Re-query re-verifies the Complete book →
   `review` + recommended "Oz" #1 → Replace → downloads #1, trashes the #11
   PDF on success. ✓
8. **Failure** → search error: retry w/ backoff → `NoMatch`. download error:
   existing retry/failover/host-spill → `Failed`. Neither blocks others. ✓

## 13. Implementation stages (each independently tested + harness-green)

1. **Model**: state enum + `Goal` + fields; schema v4 + migration; map old status.
   No behavior change. (unit + round-trip + migration tests)
2. **Transition fns**: refactor orchestrator ops into single-book transition
   functions returning persistable results. (unit tests per transition)
3. **Engine**: the driver task + Notify + budgets; route discovery+download
   through it; commands become goal-setters; delete lock-across-I/O.
   (**concurrency integration test**: long op on list A never blocks library /
   list B; Start interleaves with Re-query)
4. **Events + UI**: live per-book state/goal/progress; per-list rollups.
   (file:// 48/48 + Tauri-boot harness + a new "live ticks" check)
5. **Verify/replace** re-expressed as engine transitions (feature already exists).

Each stage ships behind green tests + the two headless harnesses + a release-binary
smoke (boot + resume) before it reaches a build.

## 14. Backlog (post-engine)

- ✅ **DONE — Download progress %/ETA/bar seeding**: `DownloadRequest.expected_size`
  (from the candidate `size_bytes`) seeds the total when a host omits
  `Content-Length`, so %/ETA/bar render.
- ✅ **DONE — Author-corroborated low-title → "Needs you"**: `decide` surfaces a
  low-title candidate as `NeedsSelection` (not `NotFound`) when the author
  corroborates (field OR appears in the title) and a distinctive title token is
  shared — translations ("Pinocchio — Le avventure di Pinocchio …" by Carlo Collodi) + review rows.
  Tests in `matching.rs`.
- ☐ **"Download whole series"** (user-triggered): when a book is detected as part
  of a series (Open Library `series_key` via the validated harness), the detail
  view offers "Download whole series →" — seed a NEW list with the ordered members
  and run it. Includes the Percy-Jackson "0 members" fallback.
- ⏸️ **Speculative / hedged download** (resilience for slow mirrors) — design at
  `docs/SPECULATIVE_DOWNLOAD.md`, awaiting go-ahead. Cache ALL available mirror
  hosts per candidate; if a download stalls, race a competing download from
  another mirror under a temp name; first to finish wins and cancels the others.

## 15. Synchronization design (review + invariants)

Three lock layers, each with a strict scope. **No lock is ever held across network I/O.**

1. **Library mutex** (`Arc<Mutex<Library>>`) — guards the SET of lists. Held only
   to clone the per-list `Arc`s (commands + engine PLAN), then dropped.
2. **Per-list orchestrator mutex** (`Arc<Mutex<Orchestrator>>`) — guards that
   list's store/state ONLY. Held for brief reads/writes of book current-state +
   goal. **Never** held across a search or download.
3. **Per-book mutual exclusion** — the engine's `inflight` set: a book has at most
   one transition running at a time. This is what the engine "holds" across the
   network (not the per-list lock).

**Transitions are split** so the network runs lock-free:
- Query: `begin_query` (brief lock → mark `Querying`) → `search` + `evaluate`
  (NO lock) → `finish_query` (brief lock → write result). Many books in one list
  search concurrently. Guarded by `tests/intra_list_concurrency.rs`.
- Download: `begin` (brief lock → mark `Downloading`, build requests) → scheduler
  transfer (NO lock; progress persisted via brief locks per tick) → `finish`.

**Mutating state during an in-flight transition** (a command Stops/Re-queries a
book mid-network):
- **Query** needs no abort. `finish_query` re-reads and only applies if the book
  is still `Querying`; if a command rewound it to `Queued` / changed the goal, the
  stale result is discarded and the engine re-plans. (Search is the FIRST network
  op and writes no file, so it's safe to just drop.)
- **Download** DOES need an abort: an in-flight transfer holds a connection and is
  writing a `.part`. Stop/Re-query/Replace on a `Downloading` book must signal the
  scheduler's cancellation (`CancelHandle`/`CancellationToken`, keyed per md5) to
  abort the transfer; the partial is kept (pause) or removed (cancel) per the
  action. `finish` then sees the cancelled state and doesn't clobber.
