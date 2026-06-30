# Demo tour

`tour-full-2.tape` is a [VHS](https://github.com/charmbracelet/vhs) script that
drives the kwire TUI end to end and renders a high-resolution screenshot set +
video — the assets used for the README / release pages.

## Regenerate

```sh
brew install vhs
cargo build --release --bin kwire     # produces ./target/release/kwire
vhs demo/tour-full-2.tape             # run from the repo root
```

Outputs land in `demo/out/` (git-ignored): `NN-*.png` screenshots + `kwire-tour.mp4`.

| file | view |
|------|------|
| `01-empty.png`                 | empty first-run splash |
| `02-import-complete.png`       | `:import` Tab-completion wildmenu |
| `03-import-completed.png`      | wildmenu cycled to `jeremy.md` |
| `04-jeremy.png`                | populated list, chips, activity |
| `05-scroll.png`                | walking the book list |
| `06-detail.png`                | book detail (variations + history) |
| `07-activity.png`              | focused activity pane |
| `08-activity-scroll.png`       | scrolling the download legs |
| `09-manual-add.png`            | manual `:add` |
| `10-avery.png`                 | a second list in the strip |
| `11-page-scroll.png`           | ★ All aggregate, page-scrolled |
| `12-needs-you-list.png`        | the needs-you filter |
| `13-needs-you-picker.png`      | "choose a copy" picker |
| `14-check-download-list.png`   | check-download — "too few pages" |
| `15-check-download-detail.png` | choosing an alternate copy |
| `16-jeremy-all.png`            | back to a single list |
| `17-ozma-selected.png`         | a series seed book selected |
| `18-ozma-detail.png`           | the seed book's detail |
| `19-series.png`                | the generated ★ series list |
| `20-delete-confirm.png`        | delete-list confirm |
| `21-about.png`                 | the about panel |
| `kwire-tour.mp4`               | the full ~3 min tour |

The README embeds a curated subset (converted to JPG) under `docs/media/`, plus
a sped-up `kwire-tour.gif` rendered from the video.

## How it works

- Runs against a throwaway `HOME` (`/tmp/kwire-demo-home`), so it always starts
  from an empty database and never touches your real library.
- Imports TRIMMED demo copies of the reading lists (`demo/lists/*.md`, 20 books
  each) via a neutral `/tmp/kwire-demo/` path — no usernames or home paths ever
  appear on screen, and the shorter lists let the live downloads settle quickly
  enough on camera for the needs-you / check-download / series beats to land.
- Live network downloads drive the progress shown; the `Sleep` steps allow for
  them. On a slow connection, increase the Sleeps — especially the `[ ]` / `/`
  navigation tour, which doubles as the wait for *A Wonder-Book* to finish and be
  page-verified before its check-download flag appears.

## Extending

Add more `Screenshot "demo/out/NN-name.png"` steps where you want new views.
Keep any shell setup inside a `Hide` block and re-enter the app's alt-screen
before `Show`, so no shell prompt or path renders into a frame. See the comments
at the top of `tour-full-2.tape` for the keymap gotchas (Esc-at-top-quits, `S`
leaves the seed book's detail open, `[ ]` cycles lists and `/` cycles filter
chips globally, `D` acts on the active concrete list — not the ★ All aggregate).
