# Download legs & the active-downloads panel — lifecycle design

Status: **accepted, implementing**. Owners: download/queue + UI.

This doc specifies how the active-downloads panel models concurrent **legs** of a
single download (the primary plus any speculative **hedge** legs), and how legs
are added, kept alive, promoted, and removed. It supersedes the original
host-keyed, TTL-only reconstruction.

## 1. Problem

A download for one `md5` (one *variation*) may run on several transports at once:
the **primary** leg, plus **hedge** legs the scheduler launches when the primary
stalls (`queue.rs::try_launch_hedge`, `HedgeConfig`). The UI shows one line per
leg, badging non-primary legs.

The original UI reconstructed legs purely from the `download://progress` event
stream, keyed by **host**, with a 6-second silence TTL. That has three defects:

1. **Spurious legs from a single leg changing host.** One leg legitimately changes
   host two ways: a *failover* (`FailingOver{from_host}`, which the UI dropped) and
   **cdn edge-rotation** (`cdn1→cdn3.booksdl.lc`), which emits only a `Note` the UI
   ignored. So edge-rotation left the old edge as a phantom second leg for up to
   the TTL. A "two downloads on one line" report was most often this — *not* a real
   hedge. (Observed on `cdn3.booksdl.lc`.)
2. **False drops.** A leg that is connecting / awaiting first byte (TTFB climbs on a
   busy booksdl edge) or stalled is *alive* but silent; the 6s TTL dropped it.
3. **Progress flicker.** The variation's single `progress` field was written by
   whichever leg's byte event arrived last, so a winner at 100% flipped to a slower
   leg at 13%. (Already patched with a leading-leg heuristic; this design removes
   the root cause.)

Root cause: the UI **inferred** leg identity from `(md5, host)` and **inferred**
liveness from a short timer. Neither is authoritative.

## 2. Goals / non-goals

Goals: only **real** legs appear; a leg survives host changes (failover,
edge-rotation) as one line; alive-but-silent legs are never dropped; a dead leg is
removed promptly; the primary is unambiguous and promotes correctly when it dies.

Non-goals: changing the hedge *decision* (when to launch); changing download
mechanics; cross-list dedup (separate feature).

## 3. Design: identity + explicit lifecycle + soft-state backstop ("A + B")

Two authoritative facts move from *inferred* to *carried by the backend*:

- **`leg_id`** — a stable per-leg id, unique within an md5's race, assigned in
  start order (primary = 0, hedges = 1,2,…). The UI keys legs by `leg_id`, **not
  host**, so a host change within a leg stays one line.
- **`is_hedge`** — whether the backend launched this leg as a hedge. Authoritative
  label; the *displayed* primary is still the lowest-`leg_id` **live** leg (so a
  promoted survivor reads as primary).

Removal is **explicit (A)** with a **soft-state backstop (B)**:

- **A — `LegEnded{md5, leg_id}`**, emitted via an RAII **Drop guard** on the leg
  task, so *every* exit (return, `?`, cancel/abort, unwind, race-group cancel) emits
  it with one piece of code. This is the real removal path: prompt and complete.
- **B — a long TTL (~60s) backstop**, demoted from a correctness mechanism to pure
  insurance against the one residual loss: `Drop` cannot `await`, so it `try_send`s,
  which can fail if the bounded channel is momentarily full. 60s is long enough to
  never false-drop a connecting/TTFB leg.

**No heartbeat.** Once `LegEnded` is the death signal, *silence ≠ death*: an
alive-but-silent leg is simply kept (no `LegEnded` yet). TTFB/connect cannot exceed
the 60s backstop, so a heartbeat would do nothing. We additionally honor the
existing per-leg liveness events (`Stalled`/`Retrying`/`Resuming`) as keep-alives —
free, and it means even the 60s backstop never trips on an actively-reporting leg.

Why this shape (see the full debate): A wins on **clean semantics** (display = exact
projection of the backend's leg set) and **test determinism** (event-driven, no
timing in tests). B's only edge — self-healing against lost messages — is muted
because the channel is **in-process mpsc**, not a network; the Drop guard closes
A's "must instrument every exit" gap. So A is the mechanism and B is a thin
backstop.

## 4. Event contract (`queue.rs::Progress`)

Per-leg variants gain `leg_id: u64` and `is_hedge: bool`:
`Resolved`, `Resuming`, `Bytes`, `Stalled`, `Retrying`, `FailingOver`.

New variant:

```rust
/// A specific leg has ended (won, exhausted, cancelled, panicked, or was
/// abandoned when a sibling won). Emitted by the leg task's Drop guard so it
/// fires on EVERY exit path. The UI removes exactly this leg_id.
LegEnded { md5: String, leg_id: u64 },
```

Unchanged (per-**download**, no `leg_id`): `Done{host,…}` (a leg won → whole md5
done), `Failed{md5,error}` (whole download gave up), `Cancelled{md5,…}`,
`Note{md5,detail}`. The UI clears the *entire* md5 on `Done`/`Failed`/`Cancelled`.

## 5. Backend mechanics

- **`RaceGroup`** gains `next_leg_id: AtomicU64` (from 0). **`LegCtx`** gains
  `leg_id: u64` (already has `is_hedge`). The primary `LegCtx` takes id 0; each
  `try_launch_hedge` takes `next_leg_id.fetch_add(1)`.
- Every `Progress` emit in `process_one_inner` stamps `leg_id`/`is_hedge` from the
  `LegCtx`.
- **Drop guard:** at the top of the leg task,
  `let _end = LegEndGuard{ events: events.clone(), md5, leg_id };`
  whose `Drop` does `let _ = self.events.try_send(Progress::LegEnded{..});`.
  `LegEnded` after `Done`/`Cancelled` is harmless — the UI has already cleared the
  md5, so it is a no-op.

## 6. UI state machine (`index.html`)

```
LEGS: { [md5]: { byLeg: { [leg_id]: Leg }, } }
Leg:  { leg_id, host, bytes, total, speed, eta, is_hedge, ts }   // ts = last keep-alive
LEG_TTL = 60_000   // backstop only
```

`noteLeg(p)` transitions:

| event kind | action |
|---|---|
| `resolved`,`bytes`,`stalled`,`retrying`,`resuming` | upsert `byLeg[leg_id]`; set `host`, refresh `ts` (keep-alive); `bytes`/`resolved` also update progress fields |
| `failing_over` | same leg (`leg_id`) changes host; update `host`, refresh `ts` — **no drop** |
| `leg_ended` | delete `byLeg[leg_id]` (remove just this leg; survivors remain) |
| `done`,`failed`,`cancelled` | delete `LEGS[md5]` (whole race over) |

`legsFor(v)`: legs of `LEGS[md5].byLeg`, **sorted by `leg_id`**, dropping any whose
`ts` is older than `LEG_TTL` (backstop). The **lowest live `leg_id` = primary**
(`hedge=false` for display); the rest are badged hedge. `leg_id` order *is* start
order, so this gives "earliest start = primary" and promotes the survivor when a
lower leg is `LegEnded`. Fallback (no live legs / pre-events): synthesize one leg
from the viewmodel as today.

The variation's headline `progress` follows the **primary (lowest live leg_id)**,
which subsumes the leading-leg patch.

## 7. Failure modes

| failure | handled by |
|---|---|
| leg dies any way (return/err/cancel/unwind/race-cancel) | Drop guard → `LegEnded` |
| `LegEnded` lost (`try_send` on full channel) | 60s TTL backstop drops the stale leg |
| alive but silent (TTFB/connect) | kept (no `LegEnded`); `Stalled`/`Retrying` keep-alives; 60s backstop won't trip |
| host change within a leg (failover, edge-rotation) | keyed by `leg_id` → one line |
| primary dies, hedge survives | `LegEnded(primary)` → next-lowest live `leg_id` becomes primary |
| whole download ends | `Done`/`Failed`/`Cancelled` clears the md5 |

## 8. Implementation plan

1. **queue.rs** — `Progress` fields + `LegEnded`; `RaceGroup.next_leg_id`;
   `LegCtx.leg_id`; stamp every emit; `LegEndGuard`. Unit test: a leg run emits
   `LegEnded` once, on every exit path (incl. race-group cancel).
2. **bridge.rs** — carry `leg_id`/`is_hedge` and a `leg_ended` kind into the
   `download://progress` payload.
3. **index.html** — `LEGS` by `leg_id`; `noteLeg` table above; `legsFor` by
   `leg_id`; `LEG_TTL=60s`; honor keep-alives. Remove the host-order + leading-leg
   heuristics they replace. UI harness coverage for: edge-rotation = one leg, hedge
   = two legs, primary `LegEnded` promotes the hedge.

## 9. Test plan

- Backend: `LegEnded` emitted exactly once per leg on success, failover-exhaust,
  cancel, and race-group cancel (Drop runs). `leg_id` monotonic per race.
- UI (headless): rotation→1 line; hedge→2 lines, lowest id primary; `leg_ended` of
  the primary promotes the survivor; a silent-but-alive leg survives past 6s;
  `done`/`failed` clears all legs.
