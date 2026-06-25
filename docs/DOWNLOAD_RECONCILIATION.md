# Stuck-download reconciliation — keeping `downloading` honest

Status: **accepted, implemented** (option B). Owners: download/engine.

This doc records why a download could appear stuck forever, the principle that fixes
it, the design we shipped, and the alternative we considered and rejected.

## 1. The bug

A download could finish — the `.part` promoted to the final `output_path` at full
size — while its persisted job stayed `Downloading` with stale `bytes_done`/`speed`.
The UI then showed a frozen in-progress transfer (e.g. 50 MB/90 MB at a stale
1.6 MB/s) even though the complete file already sat on disk. Confirmed live: two
manual-list books had their full-size finals present, jobs frozen at `downloading`,
and **no `counted pages` log** — i.e. `apply_progress`'s `Done` arm never ran.

**Root cause:** `Progress::Done` was emitted by the download layer (file promoted)
but never reconciled into job state — a race, almost certainly the **edit→re-query**
path clearing/recreating the book's candidates around an in-flight completion, so the
arriving `Done` matched no current variation and was dropped.

**Why the leg-TTL can't help.** The 60 s leg TTL governs only the UI's
*event-derived* `LEGS` map (active-panel rows). It is UI-only and has no authority
over backend job state. Worse, when event-legs age out, `legsFor()` *synthesizes* a
leg from the viewmodel — and the active panel includes a variation by its backend
`state === "downloading"` regardless of legs. So a stuck *job* is fundamentally
outside the TTL's reach.

## 2. Principle

**`downloading` must imply a live session, and the engine is the sole authority on
liveness.** Don't infer "is it alive" from disk or timers — the engine owns both job
dispatch and the live transports, so it *knows*. Anything persisted `downloading`
that the engine isn't actually running is drift to be reconciled.

## 3. Design shipped (option B)

A single reconciliation judgement, **gated by the engine's live-md5 set**, invoked
from two triggers.

- **Liveness authority:** the engine's `inflight: HashSet<BookKey>`; a download
  `BookKey` is `(list, group_path, book_index, Some(md5))`. The live set is every
  such `Some(md5)`. A variation whose md5 is in the set is **never touched** (a
  slow-but-alive transfer is safe).
- **Per sessionless in-flight variation** (`Pending`/`Resolving`/`Downloading`/
  `Verifying`, not in the live set):
  - final file present **+ full size + md5 verifies** → **`Done`**
    (`promote_variation`: `md5_verified`, `bytes_done = total`, keep `output_path`);
  - file **partial/absent** and `attempts < RECONCILE_MAX_ATTEMPTS (3)` → **re-queue**
    (`requeue_variation`: `state → Pending`, `attempts += 1`, **keep** the `.part`/
    `resume_offset`). The normal drive loop re-dispatches it and resumes;
  - `attempts ≥ 3` → **`Failed`** (`fail_inflight_variation`) — stop thrashing a dead
    source; becomes a visible, user-retryable state.
- **Triggers:**
  1. **Startup** — the background integrity scan calls it with an *empty* live set
     (nothing is dispatched yet), so every persisted zombie qualifies.
     `resume_on_launch` has already rewound in-flight → `Pending`, and the engine
     launches paused (goals `Idle`), so reconciliation runs *before* anything could
     re-download a complete file.
  2. **In-session** — `run_engine` sweeps every `RECONCILE_CADENCE` (30 s) after a
     `tick()`, passing the live set; it emits `library://refresh` when something
     changed, so a fixed job leaves the downloading view **without a relaunch**.
- **Cheap when idle:** a settled list yields an empty worklist and hashes nothing; a
  file is hashed only when a sessionless in-flight job's `output_path` is present at
  full size.

**Observability (root-cause visibility):** in `apply_progress`, a `Progress::Done`
that matched no current variation now emits a `warn!` ("Done received for md5 … but
no matching variation — completion dropped; likely an edit/re-query race"), so the
next occurrence of the underlying race is a log lookup, not a guess.

### Code map
- `crates/core/src/orchestrator.rs` — `InflightVariation`, `inflight_variations()`,
  `promote_variation` / `requeue_variation` / `fail_inflight_variation`, the
  `apply_progress` warn.
- `app/src-tauri/src/commands.rs` — `reconcile_completed_inflight(orch, live_md5s,
  ctx)` (the shared judgement; off-lock hashing), `RECONCILE_MAX_ATTEMPTS`, the
  startup-scan call.
- `app/src-tauri/src/engine.rs` — `RECONCILE_CADENCE`, the sweep + `emit_refresh`.
- `app/src-tauri/tests/reconcile_inflight.rs` — complete→Done, partial→requeue+attempts,
  over-cap→Failed, live-session→untouched.

## 4. Alternative considered — option A (engine-owned dispatch guard), rejected

A had the **sweep do nothing but re-queue** any sessionless in-flight job to
`Pending`, and folded the completeness decision into **`begin_download`**: before
fetching, if the variation's own `output_path` already exists + verifies → mark
`Done` and skip; else resume/fetch. Startup would fall out via `resume_on_launch`
without any scan.

It was attractive for "one reconciliation point on the normal download path." We
**did not pursue it** because, once B keys liveness off the engine's `inflight` set,
A's two selling points evaporate:

- **Engine-authoritative liveness** — A's main appeal — B *already* has (the
  `inflight` set), so A isn't more correct.
- **No duplication** — A worried about a completeness check living outside
  `begin_download`; but `begin_download` only does *cross-list* md5 dedup, never a
  *self*-completeness check, so B's check is already single-location. A would have
  added that logic to the hot dispatch path for no behavioral gain.

So the only remaining difference is placement (a sweep-called helper vs. a dispatch
guard) — aesthetic, not behavioral — and B was already built and tested. We shipped B.
