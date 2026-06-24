# Contributing

Thanks for your interest in Kwire. This is a small project; issues
and pull requests are welcome.

## Getting oriented

Start with **[docs/CODEBASE.md](docs/CODEBASE.md)** — a tour written for engineers
who don't know Rust yet. The `docs/` folder has focused design notes
(download scheduling, the execution model, covers, synchronization, …), and
[DESIGN.md](DESIGN.md) covers the overall architecture.

The workspace has three crates:

- `crates/core` (`libgen-core`) — the UI-agnostic engine (search, matching,
  naming, download scheduling, persistence). All the logic lives here.
- `crates/cli` (`libgen`) — command-line harnesses used for development/testing.
- `app/src-tauri` (`libgen-app`) — the Tauri desktop app over a static web UI
  (`app/ui/index.html`).

## Building & testing

```bash
cargo build                       # compile everything
cargo test --workspace            # run the Rust test suite
node app/ui/uitest.mjs            # run the web-UI test harness (no install step)

cargo tauri build                 # build the desktop .app / .dmg (needs the Tauri CLI)
```

CI runs `cargo fmt --all --check`, `cargo clippy --workspace --all-targets`,
`cargo test --workspace`, and the UI harness — please make sure those pass
locally before opening a PR.

## Conventions

- **Formatting:** `cargo fmt --all` (rustfmt) before committing.
- **Commits:** [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`, …).
- **Tests:** new behavior should come with a test. The engine is built to be
  testable offline (recorded fixtures / replay transports) — prefer that over
  hitting the network.
- Keep changes small and reviewable.
