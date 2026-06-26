# Kwire TUI — Keyboard Navigation & Mouse Design

> **Status: Agreed** (design debate complete). **Target: a release after v1.2.0.**
> Companion to [`TUI.md`](TUI.md). Tracks tasks **#44** (keyboard nav) and **#55** (mouse).

## Core idea — one Focus model

Keyboard navigation and mouse support both flow from a single `Focus` state. Get the
focus model right and the mouse is mostly free: clicks and the wheel route to the **same
intents** the keyboard produces, through **one reducer and one test path**.

## Panes

Three `Tab`-focusable panes, top → bottom:

1. **Header** — the list strip + the status-filter row.
2. **List** — the book table (the hero pane). **Default focus on launch.**
3. **Activity** — the docked downloads pane.

Modals (detail / picker / settings / help) capture focus entirely until `Esc`. The `:`
command line is a transient input mode, not a pane.

Thin dim horizontal rules separate the panes (see #61): `Header ─ List ─ Activity ─
[:command-line] ─ Hint row`.

---

## Keyboard (#44)

### Focus cycling
- `Tab` / `Shift-Tab` — cycle focus **Header → List → Activity → (wrap)**.
- No jump-to-pane hotkeys (Tab cycling is enough for three panes).

### Active-pane affordance
- The **active** pane gets a bright accent on its title / left edge.
- **Inactive** panes **dim** their selection — exactly one bright cursor on screen; the
  others stay faint so you remember where each was when you `Tab` back.

### Keys within the active pane
| Pane | Keys |
|---|---|
| **List** | `↑↓` / `j k` select book · `⏎` open (detail / picker) · `d r e x m s …` act on the selection |
| **Activity** | `↑↓` select a transfer · `p` pause · `c` cancel · `r` resume / retry |
| **Header** | `←→` move across the **filter chips** (apply on move / `⏎`) |

### The `←/→` rule (resolves the historical overload)
- `←/→` have **no effect unless the Header pane is active** — then they move the **filter
  chips**.
- **Lists** are switched by a **dedicated global cycle** that works from **any** pane and
  **never** changes the active pane:
  - `[` — previous list  ·  `]` — next list  (maps to the strip's `‹ ›`; key adjustable).

So: **`←/→` = filters (Header only); `[` / `]` = lists (everywhere).** No key means two
things.

---

## Mouse (#55)

Mouse is **on by default** (`crossterm` `EnableMouseCapture`; teardown + the panic guard
disable it). Clicks are hit-tested against `app.last_rects` (the Rects the last `Layout`
produced) and handled as `Event::Mouse` inside `on_input`, so keyboard and mouse share one
reducer and one test path.

### Click semantics
- **Single click = select** the item under the cursor (move the selection there + focus
  that item's pane).
- **Click an already-selected item = perform its `Enter` action** (if it has one):
  - click a book → select it; click the selected book → open (detail / picker)
  - click a variation row in a modal → select; click the selected one → its activate /
    download
- **Items whose primary action is immediate** fire on the **first** click:
  - a **list chip** → switch to that list (+ focus Header)
  - a **filter chip** → set that filter
  - the **`▾ ACTIVITY`** header → collapse / expand
  - a **modal hint / button** → its action

Implementation: a click emits the **`select`** intent; a click on the already-selected item
emits the **`Enter`** intent. There is no separate double-click state to track.

### Wheel
- The scroll-wheel scrolls the pane under the cursor.

### Native text selection / copy
Mouse capture does **not** block native copy. Terminals (iTerm2, Terminal.app, Kitty, …)
let you hold **Shift** (or **Option** on macOS) to drag-select even with capture on — this
is exactly how Claude Code provides both. We document the hint; no app handling is needed.
A `:mouse` command toggles capture off as a fallback for the rare terminal lacking the
modifier override.

---

## Why it composes
- A single `Focus` enum + per-pane selection state drives **both** the render (accent
  active / dim inactive) and the input routing.
- Keyboard and mouse produce the **same intents** (`select`, `Enter`, `switch-list`,
  `set-filter`, …) → one reducer, one set of tests.
- `[` / `]` plus the Header-scoped `←/→` remove the `←/→` overload entirely.

## Implementation sketch
- `AppState`: add `focus: Focus { Header, List, Activity }` + a Header filter-chip index
  (List and Activity already track their selection).
- **Render:** accent the active pane; dim the inactive panes' selection highlights.
- **Input:** route keys/mouse by `focus`; `[` / `]` and the other globals bypass focus.
- **Mouse:** enable capture in setup (disable in teardown + panic guard); hit-test against
  `last_rects`; map to `select` / `Enter` / global intents in `on_input`.
- **Tests:** drive `on_input` with synthetic key **and** mouse events; assert focus
  transitions + emitted intents (the `TestBackend` path already used for the TUI).
- **Help + hint bars (ship with the change):** update the **Help** screen's two-column
  key reference **and** the contextual per-pane **hint bars** to document the new keymap —
  `Tab`/`Shift-Tab` pane cycle, `[` / `]` list cycle, Header-scoped `←/→` filters, the
  per-pane keys, and the mouse model. The nav change is not done until Help + hints match.

## Decisions (for the record)
- **A** — `←/→` scoped to Header; lists via global `[` / `]`. *(user's refinement of the original proposal)*
- **B** — `Tab` cycling only; no jump-to-pane hotkeys.
- **C** — inactive panes **dim** their selection (not hidden).
- **D** — mouse on by default; native copy via the terminal's Shift/Option-drag; `:mouse`
  toggle as a fallback.
- **Click** — single-click selects; click-on-selected fires `Enter`.
