# Stuck-download reconciliation — keeping `downloading` honest

Status: **accepted, implemented** (design B below). Owners: download/engine.

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
dispatch and the live transports, so it *knows*. Anything persisted in-flight that
the engine isn't actually running is drift to reconcile. The reconciliation verdict
is the same in any design:

- variation has a **live session** (its md5 is in the engine's `inflight` set) → **leave it** (even if quiet — no false-killing a slow-but-alive transfer);
- **no live session**, final file present + full size + **md5 verifies** → **`Done`**;
- **no live session**, file partial/absent, `attempts < 3` → **re-queue** (`Pending`, keep the `.part`/`resume_offset`, `attempts += 1`) so the engine resumes it;
- **no live session**, `attempts ≥ 3` → **`Failed`** (stop thrashing a dead source).

The two designs below differ only in **where that verdict is computed and how it's
triggered** — not in the verdict itself.

## 3. The two designs we weighed

### Design A — engine *dispatch guard*
Split the work across the normal download path:
- a periodic engine sweep does **one** thing — reset any sessionless in-flight job to
  `Pending`;
- the completeness verdict lives **inside `begin_download`**: when the engine goes to
  (re)dispatch a `Pending` variation, it first checks "does my own final file already
  exist + verify?" → if yes, mark `Done` and skip the fetch; otherwise resume from the
  `.part`.
- Startup needs no separate scan: `resume_on_launch` already rewinds in-flight →
  `Pending`, so the first dispatch reconciles everything.

Appeal: one reconciliation point, sitting on the path the download already takes.

### Design B — standalone reconcile function  ✅ shipped
A single function makes the **whole** verdict and is invoked from two triggers:
- `reconcile_completed_inflight(orch, live_md5s, ctx)` — for one list, walks every
  persisted in-flight variation, applies the §2 verdict, and persists. `begin_download`
  is untouched.
- **Triggers:** the **startup** integrity scan calls it with an *empty* `live_md5s`
  (nothing is dispatched yet, so every zombie qualifies); the **running engine** calls
  it every 30 s (`RECONCILE_CADENCE`) after a `tick()`, passing the live set, and emits
  `library://refresh` when anything changed (the view clears without a relaunch).

Appeal: the verdict lives in exactly one place; nothing is added to the hot dispatch
path.

## 4. Why we shipped B (and rejected A)

Both designs are **engine-authoritative on liveness** — both key off the engine's
`inflight: HashSet<BookKey>` (a download key is `(list, group_path, book_index,
Some(md5))`; the live set is those `md5`s). So A's headline appeal — "single source of
truth for liveness" — is *already* true of B; it's not a differentiator.

A's other appeal was "no completeness check living outside `begin_download`." But
`begin_download` only does **cross-list** md5 dedup — it never checks whether a
variation's *own* output is already complete. So B's check isn't a duplicate of
anything; it's already single-location. A would have *added* that logic into the hot
dispatch path for **no behavioral gain**.

That leaves the only real difference as **placement** — a sweep-called helper (B) vs a
dispatch guard (A) — which is aesthetic, not behavioral. B was already built and
tested, so we shipped B.

## 5. Observability

Independent of the reconciliation: in `apply_progress`, a `Progress::Done` that
matches no current variation now emits a `warn!` ("Done received for md5 … but no
matching variation — completion dropped; likely an edit/re-query race"), so the next
occurrence of the **root-cause race** is a log lookup rather than a re-investigation.

## 6. Code map (design B)
- `crates/core/src/orchestrator.rs` — `InflightVariation`, `inflight_variations()`,
  `promote_variation` / `requeue_variation` / `fail_inflight_variation`, the
  `apply_progress` warn.
- `app/src-tauri/src/commands.rs` — `reconcile_completed_inflight` (the §2 verdict;
  off-lock hashing), `RECONCILE_MAX_ATTEMPTS = 3`, the startup-scan call.
- `app/src-tauri/src/engine.rs` — `RECONCILE_CADENCE` (30 s), the sweep, `emit_refresh`.
- `app/src-tauri/tests/reconcile_inflight.rs` — complete→Done, partial→requeue+attempts,
  over-cap→Failed, live-session→untouched.
