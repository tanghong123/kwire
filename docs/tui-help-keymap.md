# Kwire TUI — verified keymap (spec for the context-paged Help redesign)

Every key/command verified against the reducer handlers (`crates/tui/src/app.rs`, dispatch `crates/tui/src/main.rs`). Descriptions are derived from what the handler ACTUALLY does, not the old Help text.

**Routing precedence** (`app.rs` `on_input`): (1) command line active → command-input handler; (2) any modal open → modal handler; (3) else the `Focus`-based reducer. **Key consequence:** several "List" actions (`d o R a e x m`) are NOT focus-gated — they act on `self.selected` from any pane. Only `r p c`, `space`, `←/→`, `s`, `D`, Enter branch on focus.

## GLOBAL (every page)
- `?` open Help (also closes it in Help) · `:` command line · `q`/`esc` quit · `Ctrl-C` quit
- `[` / `]` prev / next reading list — **rotation includes the aggregate "All" stop**
- `Tab` / `S-Tab` cycle focus Header→List→Activity (and back)
- `/` cycle status filter · `1`–`6` jump to filter (All/Needs you/Check/Cannot/In progress/Done) — **global, not focus-gated** (unlike `←/→`)

## LIST pane
NAVIGATE: `↑↓`/`jk` move (top→Header, bottom→Activity) · `S-↓`/`J` page down · `S-↑`/`K` page up · `⏎` open book (picker if needs-selection, else detail)
ACT (any pane, on selected book): `d` detail · `a` fetch all preferred formats · `e` edit title/author · `x`/`Del` remove (confirm) · `m` mark not-found · `o` open file · `R` reveal in Finder
ACT (List focus only): `r` retry/re-download · `p` pause · `c` cancel

## HEADER pane (filter chips)
`←`/`→` prev/next filter chip (**Header-focus only; no-op elsewhere**) · `↑↓`/`jk` leave to book list · `r` re-query whole list · `p` pause list · `s` start/resume list · `D` delete list (confirm)

## ACTIVITY pane (transfer legs)
`↑↓`/`jk` select leg (top→List) · `⏎` leg snapshot · `space` collapse/expand pane · `r` resume leg · `p` pause leg · `c` cancel leg

## DETAIL modal
NAVIGATE: `Tab` Variations↔History · `↑↓`/`jk` move (cross at edges) · `⏎` snapshot of focused row · `esc` close
ACT: `d` **download focused variation** · `r` retry · `s` re-query (inline search) · `e` edit · `x`/`Del` remove · `m` mark not-found · `S` download series · `o` open · `R` reveal

## PICKER modal (choose a copy)
`↑↓`/`jk` move · `⏎`/`d` pick this copy (arm download) · `a` all preferred formats · `v` candidate metadata · `esc` close

## SETTINGS modal
Viewing: `↑↓`/`jk` move field · `←`/`→` **nudge focused number field** (NOT filter chips) · `space` toggle bool · `⏎` edit/sub-editor · `s` save · `esc`/`q`/`Ctrl-G` discard · `r` refresh mirrors · `o` reorganize · `c` cleanup
Format editor: `↑↓`/`jk` move · `space` include/exclude · `S-J`/`S-K` move format down/up in priority · `esc`/`⏎` commit
Language picker: `↑↓`/`jk` move · `⏎` commit · `esc` cancel
Inline edit: type/`Backspace` · `⏎` commit · `esc` cancel

## `:` COMMAND LINE
Editing: type (space accepts wildmenu candidate) · `Backspace` · `Tab`/`S-Tab` completion/wildmenu · `↑`/`↓` history · `⏎` submit/accept · `esc` close
Advertised commands (5): `:settings` · `:import <path>` · `:add <title|md5>` · `:start-all` · `:pause-all`
Unadvertised but dispatched (for the "Cmds" page): `:requery` `:pause [list]` `:start/:resume [list]` `:delete [list]` `:refresh-mirrors` `:cleanup` `:reorganize` `:download-series`/`:series` `:mouse` `:help` `:quit`/`:q`

## CORRECTIONS — stale/wrong in the current Help (must fix in the rebuild)
| Key · context | Old/stale | CORRECT |
|---|---|---|
| `[ ]` global | "prev/next list" | …rotation **includes the "All" stop** |
| `← →` | "move filter chip" (one global meaning) | **Header only**: filter chip · **Settings**: nudge number field · **else: no-op**. Not global. |
| `d` | "book detail & history" (one meaning) | **List**: open detail · **Detail**: download variation · **Picker**: pick copy (per-context) |
| `⏎` | "open · choose a copy" | **List** open detail/picker · **Activity** leg snapshot · **Detail** row snapshot · **Picker** pick copy |
| `/` `1–6` | bucketed as "FILTER (Header)" | actually **global** (any focus); only `←/→` is Header-gated — don't bucket all four as Header |
| Activity `space` | MISSING | collapse/expand Activity pane |
| List `e x m` | MISSING | edit / remove / mark-not-found |
| Header `r p s D` | MISSING | requery / pause / start / delete list |
| Detail `S m s e x d Tab` | MISSING (no Detail page) | series / mark-nf / re-query / edit / remove / download-variation / switch sub-pane |
| Settings `r o c` | MISSING | refresh-mirrors / reorganize / cleanup |
| Picker `v` | MISSING | candidate metadata |
| `S-J`/`S-K` | MISSING | page down/up (List); reorder format priority (Settings format editor) |
| `:` Tab/`↑↓` | MISSING | completion/wildmenu + command history |
| command list | only 5 shown | 11 more dispatched-but-unadvertised exist |
