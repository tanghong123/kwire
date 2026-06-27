# Demo tour

`tour.tape` is a [VHS](https://github.com/charmbracelet/vhs) script that drives
the kwire TUI end to end and renders a high-resolution screenshot set + video —
the assets used for the README / release pages.

## Regenerate

```sh
brew install vhs
cargo build --release --bin kwire     # produces ./target/release/kwire
vhs demo/tour.tape                     # run from the repo root
```

Outputs land in `demo/out/` (git-ignored):

| file | view |
|------|------|
| `01-empty-first-run.png` | empty first-run splash |
| `02-import-command.png`  | `:import` command line |
| `03-list-populated.png`  | populated list, chips, activity |
| `04-book-detail.png`     | book detail (variations + history) |
| `05-help.png`            | context-paged help overlay |
| `06-picker.png`          | "choose a copy" picker |
| `07-second-list.png`     | two lists in the strip |
| `08-all-aggregate.png`   | ★ All aggregate view |
| `09-manual-add.png`      | manual `:add` |
| `10-filter-done.png`     | a status-filter chip |
| `11-activity-pane.png`   | focused activity pane |
| `12-delete-confirm.png`  | delete-list confirm |
| `kwire-tour.mp4`         | the full ~80 s tour |

## How it works

- Runs against a throwaway `HOME` (`/tmp/kwire-demo-home`), so it always starts
  from an empty database and never touches your real library.
- Imports the repo's public-domain fixture lists (`fixtures/*.md`) via a neutral
  `/tmp/kwire-demo/` path — no usernames or home paths ever appear on screen.
- Live network downloads drive the progress shown; the `Sleep` steps allow for
  them. On a slow connection, increase the Sleeps.

## Extending

Add more `Screenshot "demo/out/NN-name.png"` steps where you want new views.
Keep any shell setup inside a `Hide` block and re-enter the app's alt-screen
before `Show`, so no shell prompt or path renders into a frame. See the comments
at the top of `tour.tape` for the keymap gotchas (Esc-at-top-quits, close Help
with `?`, `D` acts on the active concrete list — not the ★ All aggregate).
