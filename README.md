# Kwire

[![CI](https://github.com/tanghong123/kwire/actions/workflows/ci.yml/badge.svg)](https://github.com/tanghong123/kwire/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A desktop tool to download books from Library Genesis. One **UI-agnostic Rust
engine** (`libgen-core`) drives everything: command-line harnesses for
development/testing, and a **native macOS desktop app** (Tauri) over a
self-contained web UI.

**New to the codebase?** Read **[docs/CODEBASE.md](docs/CODEBASE.md)** — a guide
written for engineers who don't know Rust yet. For *why* things are designed this
way, see [DESIGN.md](DESIGN.md); for the build order, [PLAN.md](PLAN.md).

## What it does
1. Add books by manual entry or by importing a **Markdown / JSON** list.
2. Manage all books as one **persistent, resumable queue** with status + retries.
3. **Auto-download** confident matches (one best copy by default); ask you to
   pick when a match is ambiguous.
4. Track each **variation** (format/copy) independently — an epub can finish
   while the same book's pdf is still downloading. Default format preference is
   **`[epub, pdf]`** (Kindle/iPad-friendly; MOBI/AZW3 are Kindle-only and
   excluded by default).
5. Save into per-list folders with deterministic, **sequence-numbered** filenames
   built from your clean input metadata — `NN - Author - Title - <md5:6>.ext`. The
   trailing **6-hex md5 tag** makes every copy unique *by construction*, so two
   formats of one book (or two books that resolve to the same file) never collide.
   **Sub-grouping** (Markdown subsections / JSON groups) → subfolders.
6. **Concurrent but polite**: per-host download queues with rate limits, retries,
   and failover; **pause / cancel / resume**, dedupe of identical md5 across
   books, and **resume-on-launch** of interrupted downloads.

## Mirrors / download sites (`--site`)

The engine resolves an md5 to a concrete download via a small registry of
per-mirror resolvers (`download::resolver_for_site` / `download::ALL_SITES`):

| `--site` | lane |
|----------|------|
| `libgen.li`, `libgen.vg`, `libgen.la` | libgen.li family — shared `ads.php`/`get.php` CDN (front-door failover; Range-resumable) |
| `libgen.pw`, `randombook.org` | independent `libgen.download` CDN (by-id JSON lookup; not resumable) |
| `ipfs` | md5→CID via libgen.li, served by public IPFS gateways (most independent lane) |

Search mirrors (separate from download sites) live in editable
[`mirrors.toml`](mirrors.toml). `library.lol` is dead and removed; Anna's Archive
is not used (paywalled/captcha-gated; a `.cc` clone injected ads).

## Repository structure

```
kwire/
├── Cargo.toml              # Cargo workspace — lists crates + shared dependency versions
├── rust-toolchain.toml     # pins the Rust compiler version
├── mirrors.toml            # editable libgen SEARCH-mirror list (no hardcoding in source)
│
├── crates/
│   ├── core/               # libgen-core — THE ENGINE (no UI code)
│   │   ├── src/
│   │   │   ├── lib.rs           #   crate front door (declares the modules below)
│   │   │   ├── model.rs         #   ⭐ shared data types + per-variation state — start here
│   │   │   ├── parse.rs         #   Markdown/JSON reading list  → model
│   │   │   ├── search.rs        #   query libgen mirrors (record/replay) → candidates
│   │   │   ├── matching.rs      #   score, format-rank, diversity keep (auto vs. ask)
│   │   │   ├── download.rs      #   per-mirror resolvers + resumable md5-verified fetch
│   │   │   ├── queue.rs         #   per-host scheduling, retry/failover, pause/cancel
│   │   │   ├── store.rs         #   SQLite persistence (schema v2, resume-on-launch)
│   │   │   ├── naming.rs        #   pure filename/foldering (template, sanitize, collisions)
│   │   │   └── orchestrator.rs  #   parse→query→match→persist→naming→download (command/event API)
│   │   └── tests/              #   headless integration tests + golden files
│   │
│   └── cli/                # libgen-cli — THE HARNESSES (binary `libgen`)
│       └── src/
│           ├── main.rs         # wires up subcommands
│           ├── cmd_parse.rs    # `libgen parse-list <file>`
│           ├── cmd_query.rs    # `libgen query-books <input>`
│           ├── cmd_download.rs # `libgen download-books <md5…> --site … | --mock …`
│           └── cmd_run.rs      # `libgen run-list <file>` (whole pipeline)
│
├── app/
│   ├── src-tauri/          # Tauri desktop backend (builds the .app / .dmg)
│   ├── ui/                 # the self-contained web UI the app ships
│   └── ui-mock/            # clickable design prototype + headless UI tests
│
├── fixtures/               # sample inputs, recorded HTTP responses, golden outputs
│   ├── jeremy_public_domain_list.md / .json   # real sample list (Markdown / JSON)
│   ├── avery_public_domain_list.md / .json     # a second sample list
│   ├── expected/           # golden normalized outputs for parser tests
│   ├── search/             # recorded/synthetic mirror responses for replay tests
│   └── ipfs/               # captured libgen.li file/search pages for the IPFS resolver tests
│
└── docs/CODEBASE.md        # newcomer-friendly guide to reading the code
```

The **data pipeline** (each engine module is one stage):

```
parse ──▶ search ──▶ match ──▶ queue ──▶ download ──▶ files on disk
                       (orchestrator persists every step through `store`)
```

## Quick start (CLI)

```bash
cargo build                  # compile everything
cargo test                   # run all tests (headless, offline by design)

# Parse the sample reading list into the normalized model (no network):
cargo run -p libgen-cli -- parse-list fixtures/jeremy_public_domain_list.md

# Search (offline, against recorded mirror responses) and rank candidates:
echo '{"title":"The Adventures of Tom Sawyer","authors":["Mark Twain"]}' > /tmp/book.json
cargo run -p libgen-cli -- query-books /tmp/book.json --replay fixtures/search

# Run the whole pipeline as a dry run (parse → query → match → plan filenames):
cargo run -p libgen-cli -- run-list fixtures/jeremy_public_domain_list.md \
    --replay fixtures/search

# Request one best copy of EACH preferred format (epub AND pdf) per match:
cargo run -p libgen-cli -- run-list fixtures/jeremy_public_domain_list.md \
    --replay fixtures/search --all-formats

# Actually download by md5 against a real download site (pick any --site):
cargo run -p libgen-cli -- download-books <md5> --site libgen.li --out downloads
cargo run -p libgen-cli -- download-books <md5> --site libgen.pw
cargo run -p libgen-cli -- download-books <md5> --site ipfs

# Persist to a DB, then resume interrupted downloads on a later launch:
cargo run -p libgen-cli -- run-list list.md --db lib.db --site libgen.li
cargo run -p libgen-cli -- run-list list.md --db lib.db --site libgen.li --resume
```

## Desktop app (Tauri)

The native macOS app lives in `app/src-tauri` (Rust backend) over the web UI in
`app/ui`. From `app/src-tauri`:

```bash
cd app/src-tauri
cargo tauri dev      # run the app in development
cargo tauri build    # produce the .app / .dmg bundle
```

(Requires the Tauri CLI: `cargo install tauri-cli` or `cargo binstall tauri-cli`.)
The web UI is self-contained (no Node build step), and `app/ui-mock` holds the
design prototype plus headless UI tests.

See [docs/CODEBASE.md](docs/CODEBASE.md) for a guided tour of the code.

## Status

Engine, CLI harnesses, and the Tauri app are implemented and tested **headlessly
and offline** (300+ tests via `cargo test`, plus a self-contained web-UI harness;
`cargo fmt`, `cargo clippy`, and the full suite run in [CI](.github/workflows/ci.yml)):
- ✅ **Parser** — Markdown + JSON → normalized model (golden + grammar tests)
- ✅ **Search + matching** — config-driven mirrors, record/replay, format-ranking
  + diversity-aware variation keep
- ✅ **Download + per-host queues** — resumable, md5-verified, rate-limited,
  failover; pause/cancel/resume; IPFS + libgen.pw lanes
- ✅ **Persistence + orchestration** — SQLite (schema v2), per-variation jobs,
  dedupe, stable sequence numbers, resume-on-launch
- ✅ **Desktop app** — Tauri backend over a self-contained web UI (`app/ui`), with
  the `app/ui-mock` prototype + headless UI tests

Track remaining work in [PLAN.md](PLAN.md).

## Contributing

Issues and pull requests are welcome — see **[CONTRIBUTING.md](CONTRIBUTING.md)**
for how to build, test, and the project conventions.

## License

[MIT](LICENSE) © 2026 Hong Tang.

## Disclaimer

This software is a client for managing your own reading lists and downloads; it
does not host or distribute any content. You are responsible for ensuring your
use complies with copyright law and the terms of service of any site you access —
the authors assume no liability for misuse. Intended for personal and educational
use.
