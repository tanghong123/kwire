# Codebase guide (for newcomers)

This guide explains the project for someone who is **not familiar with Rust**. It
covers just enough Rust to *read* the code, then tours how the pieces fit. If you know
Python or JavaScript, the analogies in §2 will get you oriented fast.

> TL;DR: there is one **engine library** (`libgen-core`) that does all the real work —
> parsing book lists, searching Library Genesis, picking the right match, downloading
> files politely, and persisting everything so it can resume. Every user interface (the
> command-line harnesses, and a native Mac desktop app) is a thin shell around that one
> library, driving it through a single command/event surface (the **orchestrator**).

---

## 1. The big picture

```
              what the user sees                    what does the work
   ┌──────────────────────┬──────────────┐   ┌────────────────────────┐
   │ Tauri desktop app    │ CLI harnesses│──▶│  libgen-core (engine)  │
   │ (app/src-tauri + ui) │   (libgen)   │   │  one Rust library      │
   └──────────────────────┴──────────────┘   └────────────────────────┘
```

- **`libgen-core`** — the engine. No user-interface code lives here. (folder: `crates/core`)
- **`libgen-cli`** — small command-line programs ("harnesses") that call the engine so we
  can build and test each feature on its own, headlessly. (folder: `crates/cli`)
- **`app/src-tauri`** — the native desktop app's Rust backend (Tauri); **`app/ui`** is the
  self-contained web UI it ships, and **`app/ui-mock`** is the clickable design prototype
  (plain HTML/CSS/JS) used to agree on the look & flows.

A ratatui terminal UI could be added later against the same engine; none exists yet.

The whole thing is a **Cargo workspace**: one repo containing several packages
("crates") that build together. See the top-level `Cargo.toml`.

The data flows in a pipeline, tied together and persisted by the **orchestrator**:

```
 parse  ──▶  search  ──▶  match  ──▶  queue  ──▶  download  ──▶  files on disk
 (read a    (ask libgen  (is this   (schedule    (fetch the     (renamed, in
  list)      mirrors)     the right  downloads    file, verify    per-list /
                          book?)     politely)    it)             per-group folders)

   store (SQLite) records every step so a quit/crash resumes on the next launch.
```

---

## 2. Just enough Rust to read this code

You don't need to *write* Rust to follow along. Here are the constructs you'll see,
each mapped to something you already know.

| Rust | What it means | Like in JS/Python |
|------|---------------|-------------------|
| `crate` | a package / library | an npm package / Python module |
| `mod foo;` | declares a module (a file `foo.rs`) | `import ./foo` |
| `pub` | public (visible outside the module) | `export` |
| `struct Foo { a: u16 }` | a record with named fields | a class with only data / a `dataclass` / a TS `interface` |
| `enum Status { Queued, Done }` | a type that is exactly one of several variants | a union type / Python `Enum`, but more powerful |
| `fn name(x: u16) -> bool` | a function: takes a `u16`, returns a `bool` | `function name(x): boolean` |
| `Vec<T>` | a growable list of `T` | `Array` / `list` |
| `Option<T>` | either `Some(value)` or `None` | nullable / `T \| undefined` |
| `Result<T>` | either `Ok(value)` or `Err(error)` | success-or-throw, but returned as a value |
| `String` / `&str` | owned text / a borrowed text slice | a string (don't overthink the distinction at first) |
| `async fn` + `.await` | a function that does I/O without blocking | `async`/`await` (same idea) |
| `impl Foo { ... }` | methods attached to `Foo` | methods inside a class body |
| `trait Bar` | a shared interface several types can implement | an interface / abstract base class |
| `?` (e.g. `read(x)?`) | "if this errored, return the error now" | `try/catch` that auto-rethrows |
| `#[derive(...)]` | auto-generate boilerplate (e.g. JSON, equality) | decorators / mixins |

Three ideas worth a second look because they shape this codebase:

1. **Errors are values, not exceptions.** Functions return `Result<T>`. The `?` operator
   means "unwrap the success, or bubble the error up." So `let s = read(path)?;` reads
   like normal code but quietly propagates failures. We use the `anyhow` crate for easy
   error handling.
2. **`Option<T>` instead of null.** A missing year is `None`, a present one is
   `Some(2019)`. The compiler forces us to handle the "missing" case, so there are no
   surprise null crashes.
3. **`enum` carries data.** `RequestStatus::Failed { error: String }` is one *variant*
   that also holds an error message. This is how we model a state machine cleanly (see §4).

**Serde** is the JSON library. `#[derive(Serialize, Deserialize)]` on a struct means "this
type can be turned into/from JSON automatically." That's why our model types round-trip to
JSON for the CLI and the GUI for free.

---

## 3. Where things live

```
kwire/
├── Cargo.toml                  # workspace: lists the crates + shared dependency versions
├── rust-toolchain.toml         # pins the Rust compiler version
├── mirrors.toml                # editable list of libgen SEARCH mirrors (no hardcoding in code)
├── DESIGN.md                   # the design rationale (read for "why")
├── PLAN.md                     # phased build plan
├── docs/CODEBASE.md            # this file
│
├── crates/core/                # THE ENGINE (libgen-core)
│   └── src/
│       ├── lib.rs              # lists the modules; the crate's front door
│       ├── model.rs            # ⭐ the shared data types — start here (see §4)
│       ├── parse.rs            # Markdown/JSON reading list  → model
│       ├── search.rs           # query libgen mirrors (record/replay) → candidates
│       ├── matching.rs         # score, format-rank, keep diverse variations; auto vs. ask
│       ├── download.rs         # per-mirror resolvers (md5→URL) + resumable, md5-verified fetch
│       ├── queue.rs            # per-host download scheduling, retry/failover, pause/cancel
│       ├── store.rs            # SQLite persistence (schema v2; resume-on-launch)
│       ├── naming.rs           # pure filename/foldering (template, sanitize, collisions)
│       └── orchestrator.rs     # ties the pipeline together behind a command/event API
│   └── tests/                  # headless integration tests + golden files
│
├── crates/engine/              # UI-AGNOSTIC DRIVER (libgen-engine) — shared by both frontends
│   └── src/
│       ├── engine.rs           # the concurrency driver loop (spawn_with) + EngineEmitter trait
│       ├── viewmodel.rs        # JSON-friendly projection of the library (ViewModel/ViewVariation)
│       ├── state.rs            # AppState/Config/Library shared between desktop + TUI
│       └── legs.rs             # ⭐ shared LegTracker: per-leg download state (see docs/LEG_LIFECYCLE.md)
│
├── crates/tui/                 # THE TUI FRONTEND (libgen-tui, binary `kwire`)
│   └── src/
│       ├── app.rs              # AppState + input/event handling (holds the LegTracker)
│       ├── ui.rs               # ratatui render (the Activity pane renders per-leg "· alt copy" rows)
│       └── theme.rs            # the "quiet" truecolor palette + shared widgets
│
├── crates/cli/                 # THE HARNESSES (libgen-cli, binary named `libgen`)
│   └── src/
│       ├── main.rs             # wires up the subcommands
│       ├── cmd_parse.rs        # `libgen parse-list <file>`
│       ├── cmd_query.rs        # `libgen query-books <input>`
│       ├── cmd_download.rs     # `libgen download-books <md5…> --site … | --mock …`
│       └── cmd_run.rs          # `libgen run-list <file>` — the whole pipeline
│
├── fixtures/                   # sample inputs + recorded responses + golden outputs
│   ├── jeremy_public_domain_list.md / .json   # a real sample list (Markdown / JSON)
│   ├── avery_public_domain_list.md / .json    # a second sample list
│   ├── expected/               # golden normalized parser outputs
│   ├── search/                 # recorded mirror responses for replay tests
│   └── ipfs/                   # captured libgen.li pages for the IPFS resolver tests
│
└── app/
    ├── src-tauri/              # native desktop app backend (Tauri) → .app / .dmg
    ├── ui/                     # the self-contained web UI the app ships
    └── ui-mock/                # clickable design prototype + headless UI tests
```

**If you read one file, read `crates/core/src/model.rs`.** Everything else moves these
types around.

---

## 4. The data model (the heart of it)

`crates/core/src/model.rs` defines the shared vocabulary. In plain language:

- **`DownloadList`** — a whole reading list. Has a `title`, some `settings`, and a list of
  `groups`. Maps to one destination folder on disk.
- **`Group`** — a named batch of books (e.g. "Batch 1 — Lift-Off"). Can contain
  `subgroups`, which become **subfolders**. This is how sub-grouping works.
- **`BookInput`** — what *you* asked for: title, authors, and optional isbn/year/
  publisher/edition/language/format preference. The optional fields are `Option<...>`.
- **`BookRequest`** — one tracked item: your `BookInput` **plus** its live `status`, the
  `candidates` we found (kept for swapping), which one is `selected`, and a stable, persisted
  `seq` (its sequence number, assigned once so inserting a book later never renumbers files).
- **`Candidate`** — one search result from a mirror: has an `md5` (a unique fingerprint
  libgen gives every file — we use it to fetch *and* to verify the download), title,
  authors, format, size, a `score` the matcher assigns, and **its own optional `job`**.
- **`DownloadJob`** — progress for one download: its `JobState`, host, attempts, bytes
  done, resume offset, whether the md5 verified, the final path.

### Per-variation downloads (important)

Downloads are tracked **per candidate**, not per book. Each `Candidate` carries its own
`job`; a candidate is "requested for download" exactly when it has one. That's how several
**variations** of one book can be in different states at once — e.g. the epub `Done` while
the same book's pdf is still `Downloading`.

`BookRequest::acquisition()` rolls those per-variation states up into a small summary
(`requested / done / active / failed / paused / cancelled`) — that's what a book row shows
(e.g. "Downloading 1/2"), and the book's overall `RequestStatus` is derived from it. By
default a `Matched` book auto-requests just its single best variation ("one best copy");
`run-list --all-formats` requests one best per preferred format instead.

### The lifecycle (state machines)

`RequestStatus` (per book) and `JobState` (per variation) are each an `enum` — always
exactly one state, moving between them:

```
per book:    queued → querying ─┬→ matched ───────────► (auto-request best variation)
                                ├→ needs_selection ─(you pick)→ ready
                                └→ not_found
             ready → downloading → verifying → done / failed / paused / cancelled

per variation (JobState): pending → resolving → downloading → verifying → done
                          (also: failed, paused [resumable], cancelled)
```

- `matched` = we're confident, download automatically.
- `needs_selection` = ambiguous, the UI asks you to choose (the candidate modal).
- `failed` = retried with backoff; if it keeps failing it waits for a manual retry.
- `paused` keeps the partial `.part` + resume offset so it can continue; `cancelled` does
  not. Interrupted downloads also resume automatically on the next launch (the orchestrator
  re-pends in-flight jobs from the persisted store).

`ListSettings` holds the knobs that drive this: `format_pref` (**default `[epub, pdf]`** —
friendly to both Kindle and iPad), `auto_threshold` / `near_threshold` (how confident is
"confident enough"), the filename template, whether sequence numbers reset per group, and
`keep_top` (how many variations to keep for swapping).

---

## 5. How the modules cooperate

Each engine module is a stage in the pipeline. Their public entry points:

| Module | Key entry point | Input → Output |
|--------|-----------------|----------------|
| `parse` | `parse_auto` / `parse_markdown` / `parse_json` | text → `DownloadList` |
| `search` | `SearchClient::search` (replay/live transport) | `BookInput` → `Vec<Candidate>` |
| `matching` | `evaluate` | candidates + settings → `MatchOutcome` (status + kept variations) |
| `download` | `resolver_for_site` → `Resolver::resolve`, then `download_with_client_cancellable` | an md5 → a file on disk (verified) |
| `queue` | `Scheduler::run` | many jobs → downloaded politely, per host, with retry/failover/pause |
| `store` | `Store::open` / `insert_list` / `update_request` | model ⇄ SQLite (resumes on launch) |
| `naming` | `destinations_for_variations` | book + variations → sanitized destination paths |
| `orchestrator` | `Orchestrator` (`query_all`, `start_downloads`, pause/cancel/resume …) | a list → driven end-to-end, persisted, with events |

**Why "per-host queues"?** (`queue.rs`) Libgen files come from several download hosts.
Instead of one global download limit, each host gets its *own* small queue with its own
speed limit and retry budget. That way we're polite to each server, and a slow/blocked host
can't hold up downloads from the others. On repeated failure the resolver chain fails over
to an alternate mirror and re-enqueues onto that host's queue. See `DESIGN.md §8`.

**Why several download sites?** (`download.rs`) `resolver_for_site` builds a per-mirror
resolver that turns an md5 into a real download URL: the **libgen.li family** (`libgen.li`,
`libgen.vg`, `libgen.la`) does an `ads.php → get.php` hop on a shared CDN; **`libgen.pw` /
`randombook.org`** use an independent CDN via a by-id JSON lookup; **`ipfs`** maps the md5
to an IPFS content id (via libgen.li) and serves it from public gateways — the most
independent lane. `download::ALL_SITES` lists everything `--site` accepts. (`library.lol` is
dead and removed; Anna's Archive is not used.)

---

## 6. The CLI harnesses (how to run things)

The harnesses let us exercise one feature at a time, no GUI needed. The binary is
`libgen`. From the repo root:

```bash
# Parse a reading list and print the normalized model as JSON (no network):
cargo run -p libgen-cli -- parse-list fixtures/jeremy_public_domain_list.md

# Search mirrors for a book and print ranked candidates (uses recorded fixtures offline):
cargo run -p libgen-cli -- query-books some_book.json --replay fixtures/search

# Resolve + download a file by md5 (resumable, md5-verified) from a chosen site:
cargo run -p libgen-cli -- download-books <md5> --site libgen.li --out downloads
#   …other sites: --site libgen.vg|libgen.la|libgen.pw|randombook.org|ipfs

# Run the WHOLE pipeline (parse → query → match → plan filenames). Dry run by
# default; add --site/--mock to actually download, --all-formats for epub+pdf,
# --db <path> to persist and --resume to continue interrupted downloads later:
cargo run -p libgen-cli -- run-list fixtures/jeremy_public_domain_list.md --replay fixtures/search
```

`-p libgen-cli` means "run the package named libgen-cli." Everything after `--` is passed
to our program.

---

## 7. Building, running, testing

```bash
cargo build                    # compile everything
cargo test                     # run ALL tests (headless, offline by design)
cargo test -p libgen-core      # just the engine's tests
cargo fmt                      # auto-format (run before committing)
cargo clippy                   # lint for common mistakes
```

Current status: the engine, the CLI harnesses, and the Tauri app are implemented and
tested (115 tests via `cargo test`, `cargo clippy --all-targets` clean). The `app/ui-mock/`
prototype is openable with `open app/ui-mock/index.html`.

**Testing philosophy (important):** everything is validated *headlessly* — no clicking, no
live servers required.
- **Parsing** is checked with **golden files**: we parse a fixture and compare to a saved
  expected JSON in `fixtures/expected/`. If behavior changes intentionally, regenerate the
  golden (`UPDATE_GOLDEN=1 cargo test`).
- **Search** is tested with **record/replay**: real HTTP responses are saved once (incl.
  the captured libgen.li pages in `fixtures/ipfs/` for the IPFS resolver), then replayed so
  tests are deterministic and need no network.
- **Downloads** are tested against a **local mock HTTP server** (so we can simulate
  resumes, corruption, rate limits, failover, and pause/cancel without touching real
  mirrors).
- **Orchestration / store** are tested with an in-memory SQLite store plus the replay
  search client and mock resolvers, end to end.

### The desktop app (Tauri) and its headless tests

The native app is `app/src-tauri` (a Rust [Tauri](https://tauri.app/) backend) over the
self-contained web UI in `app/ui`. The backend exposes the engine to the UI as **Tauri
commands** (`commands.rs`, called from JS via `invoke(...)`), translating the UI's flat
book ids to the orchestrator's `(group_path, book_index)` tree positions (`bridge.rs`).
Run it from `app/src-tauri` with `cargo tauri dev`; bundle a `.app`/`.dmg` with
`cargo tauri build`.

The UI is tested headlessly without a browser-clicking session: `app/ui/headless-test.mjs`
loads the page over `file://` through headless Chrome (DevTools Protocol, no extra deps),
asserts the JS ran with zero console errors, and exercises the core interactions (incl. the
per-variation rows and format ranking). `app/ui-mock/` has the same for the design
prototype. The real engine path is exercised at runtime via `cargo tauri dev`.

---

## 8. Glossary

- **Cargo / crate / workspace** — Rust's build tool / a package / a multi-package repo.
- **md5** — a short fingerprint of a file. Libgen gives one per file; we use it both as the
  download key and to confirm the file arrived intact.
- **search mirror** — one of several interchangeable libgen websites we *search*. They go up
  and down, so the list lives in `mirrors.toml` and is editable without recompiling.
- **download site (`--site`)** — a mirror we *download* from; each needs a bespoke resolver,
  so the set lives in code (`download::ALL_SITES`): the libgen.li family, libgen.pw /
  randombook.org, and ipfs.
- **variation** — one specific copy/format of a book (one `Candidate` with its own `job`);
  several variations of a book can download independently and be in different states.
- **golden file** — a saved "expected output" that a test compares against.
- **record/replay** — save real network responses once, then replay them in tests.
- **harness** — a small standalone program that drives one feature for development/testing.
- **resolve** — turn an md5 into an actual downloadable URL (libgen needs an extra hop).
- **backoff / jitter** — wait longer after each retry, with a little randomness, to be
  polite and avoid thundering-herd retries.

---

*Keep this file current as modules get implemented. If you add a public function or change
the model, update §4/§5 so the next newcomer stays oriented.*
