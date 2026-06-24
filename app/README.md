# Kwire — desktop app (Tauri v2)

A native-feeling desktop front end for `libgen-core`, built with **Tauri v2** and a
**static frontend** (no npm / no bundler). The UI is a single self-contained
`ui/index.html` (graduated from `ui-mock/`); the Rust side (`src-tauri/`) wraps the
engine and exposes it over Tauri commands + events.

```
app/
├── ui-mock/        the standalone web UX prototype (unchanged; still file://-openable)
├── ui/             the app frontend (a wired copy of the mock → real invoke()/listen())
└── src-tauri/      the Tauri Rust crate (libgen-app) over libgen-core
```

## How it works

- The frontend calls Tauri **commands** via `window.__TAURI__.core.invoke(...)`:
  - `parse_preview(text, isJson)` — parse-only preview for the Import sheet.
  - `load_list(text, isJson)` — parse + persist (SQLite) → returns the view model.
  - `query_and_match()` — run the orchestrator query/match pass → updated model.
  - `select_candidate(bookId, md5)`, `retry(bookId)`.
  - `start_downloads(site)` — drive the scheduler; emits `download://progress` events.
  - `reveal(path)` — reveal a finished file in Finder.
- It listens for `download://progress` (carrying a flattened `queue::Progress`) to
  update rows live.
- If `window.__TAURI__` is absent (e.g. opened over `file://`), the UI falls back to
  the bundled demo data + simulation, so it stays inspectable and the headless test
  keeps passing.

## Configuration (env vars, read at startup)

- `LIBGEN_MIRRORS` — path to `mirrors.toml` (default: repo-root `mirrors.toml`).
- `LIBGEN_OUT` — download output dir (default: `~/Downloads`).
- `LIBGEN_REPLAY` — if set, search runs **offline** against this recorded-fixtures
  dir instead of live mirrors (great for a deterministic demo, e.g. `fixtures/search`).

## Build & run

The Rust command layer compiles against the engine with plain cargo:

```bash
cargo build -p libgen-app          # compiles the Tauri crate + libgen-core
cargo test  -p libgen-app          # unit tests (bridge indexing)
```

To run the full GUI you need the **Tauri CLI** (one-time install):

```bash
cargo install tauri-cli --version "^2" --locked   # provides `cargo tauri`
```

Then, from `app/src-tauri/`:

```bash
# Live mirrors:
cargo tauri dev

# Offline demo against recorded fixtures (no network):
LIBGEN_REPLAY=../../fixtures/search cargo tauri dev

# Production bundle (.app / .dmg on macOS):
cargo tauri build
```

> macOS uses the system **WKWebView** — no extra system libraries are required.
> On Linux you would additionally need `webkit2gtk` + `libsoup` dev packages.

## Tests

The existing headless UX test still passes against both the mock and the wired
frontend (it exercises the `file://` fallback path):

```bash
node app/ui-mock/headless-test.mjs                 # the original mock
node app/ui-mock/headless-test.mjs app/ui/index.html   # the app frontend
```
