# UX prototype (web mock)

A clickable mock of the Kwire desktop UI. The real app will be a
Tauri GUI over the Rust engine; this is a self-contained HTML prototype to settle
layout and flows first.

## Open it

Just double-click, or:

```bash
open app/ui-mock/index.html
```

Everything (markup, styles, demo data, logic) is **inline in `index.html`** and
uses a classic `<script>` — no ES modules, no server, no build step. This matters:
ES modules do **not** execute over `file://`, so a module-based mock silently does
nothing when opened by double-click. Keeping it single-file guarantees it runs.

## What it shows

A macOS-leaning, two-pane layout designed to balance **simplicity**, being
**informative**, and **aesthetics**:

- **Sidebar** — the list + status filters (All / Needs you / Downloading / Done /
  Queued) with live counts.
- **Main** — one primary view: books grouped by batch (collapsible), each row with
  format, status chip, and inline progress. A segmented overall-progress bar up top.
- **"Needs you" filter** — surfaces exactly the books requiring a decision
  (needs-selection or failed), so you never scan 100 rows to find what to act on.
- **Detail drawer** (click a book) — candidates to choose from, download details,
  saved path, or the error + Retry.
- **Per-host activity bar** — shows the polite per-host download queues at work.
- **Import sheet** — Markdown / JSON / manual entry with a live parsed preview.
- **Start queue** — animates the queue (querying → downloading → done) so flows are
  demoable.

It's a visual prototype: state is fake and resets on reload; nothing hits the
network or the engine yet.

## Headless test

`headless-test.mjs` drives the **real `file://` page** through headless Chrome
(via the DevTools Protocol; no dependencies — Node 22+ only). It asserts the script
actually ran, there are zero console errors/exceptions, and the core interactions
work (filters, drawer, import preview, group collapse).

```bash
node app/ui-mock/headless-test.mjs    # exit 0 = all checks pass
```

This is the regression guard for the class of bug above: it loads the page the
same way a user does, so "it silently does nothing over file://" fails the test.
