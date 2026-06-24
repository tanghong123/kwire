# Kwire — Design

A personal tool to download books from Library Genesis. One **UI-agnostic core
engine** (`libgen-core`) drives everything: CLI harnesses for development and
regression-testing, and a **native macOS GUI** (Tauri) over a self-contained web
UI. A ratatui TUI can be added later against the same engine.

Status: **built and tested** — the engine, the CLI harnesses, and the Tauri app
are implemented and exercised headlessly (`cargo test`).

---

## 1. Goals & non-goals

**Goals**
- Add books by manual entry, or by importing a Markdown / JSON list.
- Manage all books as a persistent, resumable task queue with status + retries.
- Auto-download confident matches; let the user pick when a match is ambiguous.
- Track each *variation* (format/copy) of a book independently — an epub can be
  `Done` while its pdf is still `Downloading`.
- Organize downloads into per-list folders, sequence-numbered, sanitized names.
- Support sub-grouping (Markdown subsections / JSON groups) → subfolders.
- Concurrent queries and downloads, **politely** — per-host concurrency + rate
  limits, with pause / cancel / resume and resume-on-launch.

**Non-goals (for v1)**
- No account/login or paid mirrors.
- No full-text search or in-app reader.
- No sync across machines (local SQLite only).
- Tauri GUI is "system-webview native", not AppKit. Acceptable for a personal tool.

---

## 2. Architecture overview

One **UI-agnostic core engine** behind every front end. The CLI harnesses are not
throwaway — they are a permanent front door used to develop and regression-test the
engine offline.

```
┌──────────────────────────┬───────────────────────────┐
│ Tauri desktop app        │ CLI harnesses             │
│ (app/src-tauri + web UI) │ parse/query/download/run  │
├──────────────────────────┴───────────────────────────┤
│ core engine (Rust library `libgen-core`, no UI deps)  │
│  model · parse · search · matching · download · queue │
│  · store · naming · orchestrator                      │
└───────────────────────────────────────────────────────┘
```

The orchestrator is the single command/event surface every front end drives; no
front end touches the network or the database directly.

### Workspace layout

```
kwire/                 (cargo workspace)
├── crates/
│   ├── core/      the engine library (see §3)
│   └── cli/       harness binaries over core (see §9)
├── app/
│   ├── src-tauri/ Tauri desktop backend (builds the .app / .dmg)
│   ├── ui/        the self-contained web UI the app ships
│   └── ui-mock/   the design prototype + headless UI tests
├── fixtures/      sample .md/.json, recorded search responses, ipfs fixtures
├── mirrors.toml   editable search-mirror config
└── DESIGN.md / docs/CODEBASE.md
```

---

## 3. Core engine modules

| module        | responsibility |
|---------------|----------------|
| `model`       | domain types + per-request/per-variation state (§4) |
| `parse`       | Markdown / JSON → normalized `DownloadList` (§7) |
| `search`      | mirror config, search client, record/replay transport, candidate fetch (§6) |
| `matching`    | score candidates, format ranking, diversity-aware variation keep, decide auto vs. needs-selection (§5) |
| `download`    | per-mirror resolvers (md5 → URL), resumable/md5-verified fetch, cancellation (§6, §8) |
| `queue`       | per-host queues, scheduler, rate limits, retry/failover, pause/cancel (§8) |
| `store`       | SQLite persistence (lists, groups, requests, candidates, jobs); schema v2 |
| `naming`      | pure filename/foldering: template, sanitize, length cap, collision suffix (§10) |
| `orchestrator`| parse→query→match→persist→naming→download behind a command/event API (§3a) |

### 3a. Orchestrator: the command / event surface

`orchestrator::Orchestrator` ties the pipeline together behind a UI-agnostic
surface. A front end hands it a parsed `DownloadList`, a `SearchClient`, and an
output dir, then drives it:

- **Commands** (methods / the `Command` enum): `query_all`, `start_downloads`,
  `select_candidate`, `retry`, `request_variation` / `cancel_variation`,
  `pause_variation` / `resume_variation` / `cancel_download`, `pause_all` /
  `resume_all`, `reset_inflight_for_resume`, `set_format_pref`.
- **Events** (`Event`): `StatusChanged`, `Planned`, `Download(Progress)`, `Done`.

Every state transition is persisted through `store` so quit/crash resumes
cleanly. The whole surface is driveable headlessly: tests use a replay
`SearchClient` and a mock resolver with no live network.

---

## 4. Data model & the per-variation lifecycle

```
DownloadList                       → one destination folder
├─ title, settings (ListSettings)
└─ Group (nestable)                → subfolder
   └─ BookRequest
      ├─ input:  BookInput (title, authors[], isbn?, year?, publisher?,
      │          edition?, language?, format_pref[])
      ├─ status: RequestStatus (rolled up from the variations — see below)
      ├─ candidates[]: Candidate { md5, title, authors, year, publisher,
      │                language, extension(Format), size_bytes, source_host,
      │                score, job? }
      ├─ selected: md5 of the chosen candidate (legacy single-best path)
      └─ seq:     stable, persisted per-book sequence number (assigned once)
```

### Per-variation model + acquisition roll-up

Downloads are tracked **per candidate (variation)**, not per book. Each
`Candidate` carries its own optional `DownloadJob` (`job`). A variation is
"requested for download" exactly when it has a job. Because each variation has
its own job state, several variations of one book can be in different states at
once — e.g. the epub `Done` while the pdf is still `Downloading`.

`BookRequest::acquisition()` rolls those per-variation `JobState`s up into an
`Acquisition { requested, done, active, failed, paused, cancelled }`, which is
what a book row summarizes (e.g. "Downloading 1/2"). The book's `RequestStatus`
is derived from this roll-up (`Downloading` while any is active, `Paused` while
any is paused, `Done` once all requested variations finish, etc.).

```
JobState: Pending → Resolving → Downloading → Verifying → Done
                                     │
                       Failed ◄──────┤
                       Paused ◄──────┤   (kept .part + resume_offset, resumable)
                       Cancelled ◄───┘   (will not resume on its own)
```

```
RequestStatus (per book): Queued → Querying ─┬→ Matched ────────► (auto request best)
                                             ├→ NeedsSelection ─(pick)→ Ready
                                             └→ NotFound
              Ready → Downloading → Verifying → Done / Failed / Paused / Cancelled
```

- On `Matched`, `query_all` pre-selects the top candidate **and** auto-requests
  the single best variation for download (the "download one best copy" default).
- `NeedsSelection` / `NotFound` request nothing until the user acts.
- State + progress are persisted on every transition so quit/crash resumes.

`ListSettings` holds the knobs: `format_pref` (default `[epub, pdf]`),
`auto_threshold` / `near_threshold`, `naming_template`, `seq_per_group`, and
`keep_top` (how many variations to keep per book for later swapping).

---

## 5. Matching, format ranking & diversity

Auto-download above a configurable **confidence band**; otherwise present the
kept variations for selection.

Scoring signals, combined into 0..=1 confidence (`matching::score_candidate`):
- **ISBN exact** → near-1.0 (strongest single signal; nudged by format).
- Normalized **title** similarity (token-set ratio blended with edit distance).
- Normalized **author** similarity (best pairwise; redistributed onto title when
  the request has no authors).
- **Year / publisher / language** as small additive boosters.
- **Format penalty**: a candidate lacking any preferred format is demoted so it
  can't auto-match (but stays above the not-found floor if otherwise good).

Decision (`decide`):
- `score ≥ auto_threshold` (default 0.85) **and** a preferred format present →
  `Matched` (auto).
- `near_threshold ≤ score < auto_threshold` → `NeedsSelection`.
- below `near_threshold` (default 0.45) → `NotFound`.

Tie-breaks: format-preference rank → language match → larger (saner) size → md5.

**Default format preference: `[epub, pdf]`** — friendly to BOTH Kindle (incl.
Send-to-Kindle) and iPad (Apple Books). EPUB reflows on both; PDF is the
universal fallback. MOBI/AZW3 are Kindle-only, so they're excluded from the
default (but still selectable / requestable explicitly).

**Diversity-aware variation keep** (`select_variations`): after ranking, the
matcher chooses which variations to keep (`keep_top`, default 5) so the user can
later swap copies, without changing the auto/needs-selection decision:
1. filter to preferred formats (fall back to all if none match, so the book
   stays downloadable);
2. guarantee coverage — keep the best copy of each distinct format first;
3. fill remaining slots with size-diverse copies, dropping same-format
   near-duplicates whose sizes are within 15% of one already kept.

The `query-books` harness replays recorded searches so thresholds can be tuned
against real data with golden expectations.

---

## 6. Libgen integration reality

The fragile part; designed to be **config-driven** for search and a small
**code registry of resolvers** for download (because each mirror family needs a
bespoke multi-step resolve).

- **Search mirrors churn.** `mirrors.toml` lists search hosts with a
  `search_url` template, a `kind` (parser: `libgen_li_html`, `libgen_json`,
  `libgen_rs_html`), and a `priority`. Editable without rebuild. The
  `SearchClient` prefers JSON where offered and falls back to HTML scraping
  (`scraper`), going through a `Transport` so responses can be **recorded** and
  **replayed** offline for deterministic tests.
  - **TODO (deferred): remote mirror-config JSON.** Today `mirrors.toml` is
    bundled and hand-edited, so a mirror dying or a new one appearing requires a
    release. obsfx/libgen-downloader instead fetches its mirror list from a
    GitHub-hosted JSON (`raw.githubusercontent.com/.../config.vN.json`) at
    startup and uses the first responding host, decoupling mirror churn from
    release cadence. Worth borrowing the *pattern*: ship `mirrors.toml` as the
    offline default, but try to refresh it from a pinned remote URL on launch
    (cache last-good locally, fall back to the bundled copy on any fetch/parse
    failure, never block startup). Carries a supply-chain caveat — a
    compromised or MITM'd config could redirect downloads — so gate it behind
    HTTPS-only, schema validation, and ideally a signature or host allowlist
    before trusting fetched entries. Not urgent while our 5-host LG+ family is
    stable; revisit when hosts start churning.
- **Download resolvers** (`download::Resolver`, one per mirror family) turn an
  md5 into a concrete, directly-downloadable URL. `download::resolver_for_site`
  is the single registry shared by every front end; `download::ALL_SITES` lists
  what `--site` accepts:

  | `--site` | lane | how it resolves |
  |----------|------|-----------------|
  | `libgen.li`, `libgen.vg`, `libgen.la` | shared `booksdl.lc` CDN | `ads.php?md5=…` → scrape short-lived `get.php?md5=…&key=…` → 307 to CDN (Range-capable, resumable). The extra hosts buy front-door failover, not extra bandwidth. |
  | `libgen.pw`, `randombook.org` | independent `libgen.download` CDN | `GET /api/search/by-id?id={md5}` → numeric id → stream `/api/download?id=…`. Independent bandwidth; **not** Range-resumable. |
  | `ipfs` | public IPFS network | md5 → numeric id via libgen.li `index.php`, → `/ipfs/{CID}` via `file.php`, served by a chain of public gateways (`ipfs.io`, `dweb.link`, `gateway.pinata.cloud`) with resolve-time failover. Most independent lane. |

- **md5 is leverage**: returned up front → verify file integrity after download
  and **dedupe identical md5 across books** (download once, copy to the others).
- **Dead / removed sources:** `library.lol` is dead and has been removed. Anna's
  Archive is out — the real site is paywalled / captcha-gated, and a `.cc` clone
  injected ads; it is not used as a mirror.

---

## 7. Input parsing (reqs #1, #5)

Markdown and JSON both desugar into the **same** `DownloadList`. Manual entry
constructs the model directly. JSON is the canonical/explicit schema; Markdown is
the human-friendly form.

**JSON (canonical)**
```json
{
  "title": "Reading 2026",
  "settings": { "format_pref": ["epub","pdf"], "language": "English" },
  "groups": [
    { "name": "Adventure", "books": [
      { "title": "Treasure Island",
        "authors": ["Robert Louis Stevenson"], "isbn": "9780141321004" }
    ]}
  ]
}
```

**Markdown (desugars to the above)**
```markdown
# Reading 2026

## Adventure
- Treasure Island — Robert Louis Stevenson [9780141321004]
- Kidnapped by Robert Louis Stevenson (1886)
```
- `#` = list title, `##`/`###` = nested groups → nested subfolders.
- Item grammar is forgiving: `Title — Author`, `Title - Author`, `Title by
  Author`, optional `(Year)`, `[ISBN]`. Co-authors are split on ` and ` (with
  ` & ` and `;` normalized to it first); comma is NOT a separator (it appears as
  "Last, First"). Unparseable items are kept as title-only requests with a
  warning, never dropped silently.

Golden-file tests in `fixtures/expected/` pin parser behavior.

---

## 8. Queue, concurrency & lifecycle (req #6)

### Per-host download queues

Not one global pool — **one queue per resolved download host**. The scheduler
owns a `host → HostQueue` map, each with its own:
- **concurrency limit** (small; `HostLimits::max_concurrency`, default 2),
- **rate limiter** (minimum inter-request interval + jitter),
- **retry budget** (`max_attempts`).

```
                 ┌──────────────── scheduler ────────────────┐
ready jobs ─────▶│ resolve, then route by resolved host      │
                 │   libgen.li        → HostQueue(conc, rl)  │
                 │   libgen.download  → HostQueue(conc, rl)  │
                 │   ipfs.io          → HostQueue(conc, rl)  │
                 └────────────────────────────────────────────┘
```

- **Routing**: a job's host is known only after resolve picks a working URL, so
  resolution runs first; the job is then enqueued onto the matching `HostQueue`.
- **Failover**: on repeated failure, the `ResolverChain` re-resolves to an
  alternate mirror and re-enqueues onto that host's queue, so a slow/blocked host
  can't starve the others.
- **Queries** use their own concurrent pool (`Orchestrator::query_all`, bounded
  by `with_query_concurrency`, default 6), independent of downloads.

### Reliability & lifecycle

- **Retry**: exponential backoff + jitter, capped attempts, only on transient
  errors (timeout / 5xx / 429 / reset). Permanent errors (404, md5 mismatch) fail
  fast. `DownloadError` classifies transient vs. permanent vs. cancelled.
- **Resumable**: HTTP Range; `resume_offset` is persisted so restarts continue
  the partial `.part` file (servers that ignore Range restart cleanly from 0).
- **Verify**: the downloaded `.part`'s md5 is checked against the candidate's md5
  before the atomic rename to the final path.
- **Pause / cancel / resume in-flight**: a `CancellationToken` stops a streaming
  download. **Pause** keeps the `.part` + `resume_offset` (`JobState::Paused`,
  resumable); **cancel** removes it (`JobState::Cancelled`). `resume_*` re-pends
  the work for the next download pass.
- **Concurrent queries**: many searches run at once, bounded per orchestrator.
- **Resume-on-launch**: schema v2 persists a stable per-book `seq` and full job
  state. On relaunch, `reset_inflight_for_resume` moves any
  `Downloading/Resolving/Verifying` job back to `Pending` (keeping
  `resume_offset`) and `resume_all` re-pends paused work, so
  `start_downloads` continues cleanly (`run-list --resume`).
- **Dedupe identical md5**: if several books request the same md5, the file is
  downloaded once (and md5-verified once); the other destinations are filled by
  copying the verified file.
- **Sequence stability**: `seq` is assigned the first time a book is planned and
  reused thereafter, so inserting a book mid-list never renumbers existing files.
- **Persistence**: SQLite (`rusqlite`, bundled). Queue state survives quit/crash
  and resumes on launch.

---

## 9. Standalone harnesses (`crates/cli`)

Binary `libgen`. Each wraps the same core modules so experiments and the app
never diverge.

- **`parse-list <file.md|json>`** → prints the normalized `DownloadList` JSON.
  No network.
- **`query-books <request.json>`** → ranked candidates JSON, with record/replay
  of search responses for deterministic offline tests.
- **`download-books <md5…>`** → resolve + ranged/resumable download + md5 verify
  via the scheduler, with `--host-concurrency`, `--rate`, `--max-attempts`,
  `--resume`, `--mock <url-template>`, and `--site libgen.li|libgen.vg|libgen.la|
  libgen.pw|randombook.org|ipfs`.
- **`run-list <file>`** → the whole pipeline: parse → persist → query/match →
  plan destinations → (dry run, or download with `--site`/`--mock`). Flags
  include `--all-formats` (request one best per preferred format), `--resume`
  (resume a persisted list), `--db`, `--replay`.

---

## 10. File naming & foldering (reqs #4, #5)

- Dest layout: `<list folder>/<group>/<subgroup>/<NN - Author - Title - <md5:6>.ext>`.
- Template configurable, default `{seq:02} - {authors} - {title}.{ext}`
  (`{seq}`/`{seq:0N}`, `{authors}`, `{title}`, `{ext}` placeholders); a short
  **6-hex md5 tag** is then appended to the stem (`… - <md5:6>.ext`), reserved out
  of the length cap.
- Names use the **request's CLEAN input** author/title (not the mirror's messy
  scraped metadata), so every variation of a book is named consistently;
  candidate values are only a fallback when the request omitted them.
- Sanitize: strip `/ \ : * ? " < > |` + control chars, collapse whitespace,
  trim trailing dots/spaces, char-length cap (180).
- Variations of one book share its sequence number; the **md5 tag makes every copy
  UNIQUE and DETERMINISTIC by construction** — two formats of one book, or two
  books that resolve to the same file, never collide, with no order-dependent
  rename. (A ` (2)` suffix remains only as an astronomically-rare last resort.)
- Sequence scope is `seq_per_group` (default per-group) vs per-list. Numbers are
  stable (see §8).
- `naming` is pure (no I/O): callers pass the set of already-taken paths.

---

## 11. Tech choices

- **Language**: Rust everywhere (engine + CLI + Tauri backend).
- **Async**: tokio. **HTTP**: reqwest. **HTML**: scraper. **DB**: rusqlite
  (bundled SQLite). **Hash**: md5. **Fuzzy match**: strsim. **Errors**: anyhow /
  thiserror. **Config**: TOML (`mirrors.toml`).
- **GUI**: Tauri (system webview) + a self-contained web UI.

---

## 12. Resolved questions (originally open for review)

1. **Mirror set** → a sane default registry in code (`download::ALL_SITES`):
   the libgen.li family (li/vg/la), libgen.pw / randombook.org, and ipfs. Search
   mirrors stay editable in `mirrors.toml`. `library.lol` is dead and removed;
   Anna's Archive is out (paywalled/captcha; a `.cc` clone injected ads).
2. **Resolve pool** → resolve just-in-time before download (and re-resolve on
   retry/failover), which keeps short-lived libgen.li keys fresh.
3. **Auto-threshold defaults** → moderate: `auto_threshold` 0.85,
   `near_threshold` 0.45, with the format penalty gating auto-match.
4. **Dedupe** → **done**: identical md5 across books downloads once and is copied
   to the other destinations (verified once).
5. **Sequence stability** → **done**: `seq` is assigned once and reused, so
   inserting a book mid-list appends rather than renumbering existing files.

Additional decisions since:
- **Default format preference `[epub, pdf]`** — Kindle/iPad-friendly.
- **Per-variation downloads + acquisition roll-up** — each candidate has its own
  job; one-best-on-match by default, `run-list --all-formats` for one best per
  preferred format.
- **Pause / cancel / resume + resume-on-launch** (store schema v2, stable `seq`).

---

## 13. Prior art & download-lane investigations

Two completed explorations that justify the current download strategy. Both
reinforce the same conclusion: **there is nothing to borrow on the download
mechanism, and the only genuinely CDN-independent lane is Anna's Archive** (the
stubbed `AnnaArchiveResolver`, pending the Cloudflare/cookie path).

### 13a. Prior art: obsfx/libgen-downloader

`github.com/obsfx/libgen-downloader` — active (last commit 2026-03-08, v3.3.1),
a TypeScript/Ink TUI (migrated from Go), endpoints live. Studied as the closest
comparable project.

- **Same download pipeline as ours.**
  `GET https://<mirror>/ads.php?md5=<md5>` → scrape the short-lived
  `get.php?md5=…&key=<key>` link (they use a CSS selector
  `#main > tr:first-child > td:nth-child(2) > a`; we use a regex) → follow → 307
  → `cdnN.booksdl.lc` (they land on `cdn5`, we land on `cdn3` — same CDN family,
  different edges). **No CDN-independent lane.**
- **Remote mirror-config JSON.** Their mirror list is fetched at startup from a
  GitHub-hosted JSON (`config.v3.json` on a `configuration` branch); it currently
  lists exactly libgen.li/vg/gl/bz/la — the same 5 we use. `findMirror()` is
  first-alive-wins, sequential, with no health scoring. This is the pattern the
  §6 *remote mirror-config JSON* TODO proposes borrowing (see §6).
- **Strictly less robust than us.** Single serial download (its "bulk download"
  is a serial queue with per-item status rows that only *looks* concurrent); no
  Range/resume/`.part`; no md5 verify; fixed 5×2s retry; no per-download
  cross-mirror failover (it picks one mirror at startup). We have all of these.
- **`libgen.li/json.php` — investigated, not adopted.** It is an id-keyed
  object-map (`{"1":{…}}`), not the flat array our parser wants, and exposes no
  `req=`/`md5s=` search. Our existing `libgen_json` lane targets classic
  libgen.is/.rs (correct shape, but dead upstream — fails over gracefully), so
  there is nothing to gain here.
- **Conclusion:** nothing borrowable on the download mechanism. Confirms a
  CDN-independent lane must come from Anna's Archive / SLUM, not this project.

### 13b. Download-lane & concurrency investigation

A multi-agent probe of every candidate download lane and of whether multi-mirror
spread buys throughput. Harness: `crates/core/examples/concurrency_probe.rs`.

- **All 5 libgen+ mirrors share one CDN.** libgen.li/bz/la/gl/vg *all*
  307-redirect downloads to `cdn3.booksdl.lc` (verified for every one). Failing
  over among them is pointless when that CDN is down — this motivated the
  `cdn_group` same-CDN-skip in the scheduler.
- **libgen.pw & randombook.org are dead.** `/api/search/by-id?id=<md5>` →
  **HTTP 502** on both. (The §6 table still lists this lane; it is currently
  non-functional upstream.)
- **IPFS is a dead end.** The md5→CID lookup *is* fixable — a single
  `libgen.li/file.php?md5=<md5>` fetch exposes the CID (the current
  `index.php?req=<md5>` query is wrong) — **but** libgen's CIDs use a blake2b
  multihash hosted only on libgen's private node. 0 of 8 public gateways served
  real bytes for 3 distinct md5s (504s / 404s / timeouts), and libgen embeds a
  `localhost:8080` gateway link. Fixing the lookup will not make bytes
  retrievable; public-IPFS download against libgen is not viable.
- **Concurrency probe.** Single-mirror (libgen.li) vs. round-robin across all 5,
  at concurrency 3 / 5 / 8, with real `ads.php`→`get.php`→CDN downloads.
  Spreading across the 5 front-doors gives **no** concurrency or throughput
  benefit: one mirror sustains ~5–7 reliable concurrent downloads, same as five;
  the dominant failure (`no-headers-30s` from `cdn3.booksdl.lc` under load) hits
  both modes equally; aggregate throughput is ~1 MB/s regardless of concurrency
  or spread. **The CDN throttles per-client-IP, not per-hostname.** Multi-mirror
  only adds the flakier siblings' resolve/connect failures.
- **Implications:** a global download cap of ~5–6 is the sweet spot; keep
  multi-mirror failover for **resolve resilience, not throughput**.
