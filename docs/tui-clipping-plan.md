# Kwire TUI — clipping fixes: decisions log

Process: review each finding, discuss, record a decision. Implement only after all 14 are decided.
Status key: ⏳ pending · ✅ decided · ⏭️ deferred · ❌ won't fix

Cross-cutting root cause: no `unicode-width`; no `Paragraph.wrap()`; `…` ellipsis used in only one place (Help via `truncate_ellipsis`). A shared display-width-aware ellipsize/scroll helper underpins most fixes.

## CROSS-CUTTING RULE (applies to every clipped long value — confirmed at #2)
For any text that doesn't fit its region:
- **Not focused → show `…` ellipsis** (truncate, display-width aware).
- **Focused (not editing) → marquee-scroll** (ping-pong, like the existing detail title line).
- **Editing a field → scroll the buffer to keep the cursor visible** (text slides as you type past the edge).
This is the default for #1, #2, #4, #5, #6, #9, #12 and anywhere else text clips.

| # | Sev | View · Field | Decision |
|---|-----|--------------|----------|
| 1 | High | Detail variations table — fixed cols overflow → values crushed | ✅ see #1 |
| 2 | High | Settings modal width + long values + edit cursor | ✅ see #2 |
| 3 | High | Command line / :import input — no horiz scroll, cursor goes off-screen | ✅ see #3 |
| 4 | High | Book list — title/author/rest 3-region responsive layout | ✅ see #4 |
| 5 | Med | Detail per-variation Title·Author clipped | ✅ covered by #1 |
| 6 | Med | Detail breadcrumb subtitle (group/subgroup) clipped | ✅ ellipsize `…`, NO marquee — path not critical |
| 7 | Med | Bottom hint bar cut at width 80 | ✅ WRAP to a 2nd line (don't drop/ellipsize hints) |

> SIDE TASK (flagged at #7): `?` Help audit DONE. It surfaces ~21 of ~74 total bindings/commands (~30%). MISSING: all header list-ops (r/p/s/D), all detail actions (S/m/s/e/x/d/Tab), all settings keys (r/o/c), picker `v`, activity `space`, Shift-J/K paging, `:` completion/history, + 11 hidden `:` commands. STALE: `[ ]` label omits "All"; `d` labeled "book detail" but = download-variation inside Detail. The full keymap ~DOUBLES the content → does NOT fit one 80×24 screen in the current fixed 2-column design.
> → **SEPARATE DESIGN TASK: redesign the Help (scrolling / context-sensitive pages / grouped paging) so it can cover everything.** Deferred until after the clipping decisions.
| 8 | Med | Detail context hint footer cut at width 80 | ✅ WRAP to 2nd line (same as #7; apply to ALL footer hint rows) |
| 9 | Med | Activity pane transfer-leg title vs fmt/%/bar | ✅ pin status (fmt·%·bar·eta) fixed/right-aligned; title flexes (marquee focused, `…` else) |
| 10 | Med | Detail title marquee uses char-count not display width | ✅ use DISPLAY WIDTH (unicode-width) for ALL layout/scroll math; settled w/ #14 |
| 11 | Med | Picker modal title·author col + border-title | ✅ candidate rows = #1 treatment; border title = ellipsize `…` |
| 12 | Med | Book list group-header row name clipped | ✅ ellipsize `…` (non-focusable divider, like #6) |
| 13 | Low | Help modal section Head rows not ellipsized | ⏭️ DEFERRED into the Help redesign task |
| 15 | NEW | List strip (top reading-list row) responsive width | ✅ see #15 |
| 14 | Low | List strip CJK scroll/affordance positions use char-count | ✅ use DISPLAY WIDTH (settled together w/ #10) |

---

## #1 — Detail variations table (DECIDED)
- **Drop the Src column.**
- **Progress bar** → a **separate line below** the variation row, rendered **only for variations actively downloading**.
- **No MD5** in the detail variations table (MD5 stays in the variation/picker modal + the `v` snapshot).
- State label **"available" → "avail"**.
- Remaining columns (all kept, one line, none dropped): Title·Author, Fmt, Size, Match, State.
- **Responsive 60/40 split:**
  - 40% is a CAP, not a reservation. Mode A when the rest fields fit within ≤40%; Mode B when they'd need >40%.
  - **Mode A:** rest fields (Fmt, Size, Match, State) at NATURAL fixed width + fixed positions; Title·Author gets ALL remaining width (≥60%) — give slack back to Title·Author; marquee-scroll Title·Author if still too long (focused row).
  - **Mode B:** concatenate Title, Author, AND the rest into ONE comma-separated string; marquee-scroll the whole string if it doesn't fit.

## #2 — Settings modal (DECIDED)
- **Modal width = min(80, floor(0.9 × total_width), total_width − 10).** Stays 80 on wide terminals; shrinks on narrow ones so it never exceeds 90% of the screen and always leaves a ≥10-col margin.
- **Long values** (Download folder, Naming template, etc.) follow the CROSS-CUTTING RULE: `…` when the row isn't focused, marquee when focused, scroll-to-cursor while editing.

## #3 — Command line / :import input (DECIDED)
- The `:` command line and the empty-screen import box **scroll-to-cursor** (cross-cutting editing rule), with `‹` / `›` edge indicators when text is hidden to the left / right.
- **Path-completion wildmenu display:**
  - If the full `<parent>/<suggestion>` is short (≤ ~25 chars), show it in FULL.
  - Otherwise show `…<parent-last-10>/<suggestion>` — parent capped to its last 10 chars with a leading `…`; if the parent is ≤10 chars, show it whole.
  - Directory suggestions keep a trailing `/`.
  - The full path stays visible in the (scrolling) input line.

## #4 — Book list row (DECIDED) — differs from #1: the list reserves a SEPARATE author region (you scan authors here)
NOT focused — three regions, none ever starved:
- **Title: 60%** (the remaining width).
- **Author: 10% + any slack** the rest fields leave unused.
- **Rest fields (Fmt, Size, State): up to 30%.**
  - (a) if they fit in 30% → fixed-size columns (aligned for readability, no wasted space).
  - (b) if they don't fit in 30% → a comma-separated list packed into the 30%.
Focused:
- situation (a): marquee-scroll **title + author together** (rest fields stay fixed in place).
- situation (b): marquee-scroll **the whole line**.

## #5 — Detail per-variation Title·Author (DECIDED) — covered by #1
The #1 decision already governs the variations table title·author (Mode A/B + focused-row marquee / `…`). No separate work.

## #15 — List strip (top reading-list row) responsive width (DECIDED)
N = total strip width. **Each list has a MINIMUM of 30 columns.**
- If all lists fit at natural width → show them all, no capping.
- Otherwise per-list width = **max(30, N / min(#lists, 4))**:
  - **≤4 lists:** divide the strip EVENLY — each list = N/#lists, its own equal column. Floored at 30: if N/#lists < 30, each gets 30 and the strip overflows/scrolls.
  - **>4 lists:** each list capped at N/4 (≤ a quarter of the strip), floor 30; the strip OVERFLOWS and scrolls horizontally, packed tight — no slack between lists.
- Per-list clip rule: inactive clipped → `…`; active clipped → marquee within its own column. Display-width aware (per #14).
