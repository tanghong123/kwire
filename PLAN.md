# Kwire — Implementation Plan

Phased plan over the design in `DESIGN.md`. Each phase is independently runnable and
testable. Network-free phases come first so progress doesn't depend on live mirrors.

Status: **draft for review**.

---

## Phase 0 — Workspace scaffold
- Cargo workspace: `crates/core`, `crates/cli`; placeholders for `crates/tui`, `app/`.
- `fixtures/` dir, `mirrors.toml`, CI-friendly `cargo test` wiring.
- **Done when**: `cargo build` + `cargo test` pass on an empty skeleton.

## Phase 1 — Model + parsing (no network)  ← first real value
- `core::model`: `DownloadList / Group / BookRequest / Candidate`, state machine enums.
- `core::parse`: JSON (canonical) + Markdown (desugaring) → `DownloadList`.
- Forgiving Markdown item grammar; unparseable items become title-only + warnings.
- `cli parse-list <file>` harness.
- Golden-file tests in `fixtures/` (md & json → expected normalized JSON).
- **Done when**: sample lists round-trip; reqs #1 & #5 (grouping) provable offline.

## Phase 2 — UX prototype (parallel with Phase 1)
- Static web mock (the eventual Tauri UI is web) of four screens:
  List view · Queue/progress · Candidate-selection modal · Import.
- No engine wiring — clickable, for feel/iteration only.
- **Done when**: you can click through the core flows and give layout feedback.

## Phase 3 — Search + matching
- `core::search`: `mirrors.toml` loader, search client (JSON-first, HTML fallback).
- `core::match`: scoring + confidence band → matched / needs_selection / not_found.
- `cli query-books <request.json>` with `--record` / `--replay`.
- Record real responses → `fixtures/`; golden tests run offline against them.
- **Done when**: recorded searches produce stable ranked candidates; thresholds tunable.

## Phase 4 — Download engine + per-host queues
- `core::download`: pluggable resolvers (md5 → download URL), ranged/resumable fetch,
  md5 verify.
- `core::queue`: scheduler with **one `HostQueue` per download host** (per-host
  concurrency + token-bucket rate limit + jitter), retry/backoff, mirror failover.
- `cli download-books` with `--host-concurrency`, `--rate`, retry flags.
- **Done when**: a small list downloads end-to-end, resumes after kill, verifies md5,
  and respects per-host limits.

## Phase 5 — Persistence + orchestration
- `core::store` (SQLite): lists, requests, candidates, jobs, progress.
- Event stream + command API tying parse → query → match → queue → files together.
- File naming/foldering (§10): sequence, sanitize, subfolders, collisions, dedupe.
- **Done when**: full pipeline runs headless via CLI and survives restart.

## Phase 6 — Tauri GUI
- Wire the Phase-2 UI to the engine via Tauri commands/events.
- Import, queue dashboard with live progress, candidate selection, retry/pause/cancel.
- **Done when**: the four screens are functional against the real engine on macOS.

## Phase 7 — TUI (later)
- ratatui front end over the same engine + event stream.

---

## Sequencing
- **Now, in parallel**: Phase 1 (model+parse) and Phase 2 (UX mock) — neither needs network.
- Then Phase 3 → 4 → 5 (engine), then Phase 6 (GUI), Phase 7 (TUI) last.

## Testing strategy
- Golden files for parsing and matching (deterministic, offline via `--replay`).
- Network code tested against recorded fixtures; live calls gated behind a flag.
- Per-host queue limits verified with a mock host + timing assertions.

## Risks
- Mirror instability → config-driven mirrors + resolvers, health checks, failover.
- This sandbox may lack network → record fixtures whenever live access is available;
  all tests stay offline-deterministic.
