# kwire — EXTENDED guided tour tape for VHS (separate from the proven tour.tape).
#
# A longer, story-driven run:
#   import Jeremy (showcasing :import Tab-completion) -> browse the books (arrow
#   keys) -> a book detail -> the download pane (scroll the legs) -> manually
#   add Pride and Prejudice -> import Avery (loaded LATE so Jeremy's downloads
#   settle first) -> page-scroll the combined ★ All list (Shift-J / Shift-K) ->
#   needs-you: choose a copy -> check-download: choose a copy ->
#   Jeremy / all 20 -> Ozma of Oz detail -> S = download series ->
#   delete-list confirm -> the about panel.
#
# Uses TRIMMED demo copies of the reading lists (demo/lists/*.md, 20 books each
# instead of 40) so the live downloads settle quickly enough on camera for the
# needs-you / check-download / series beats to land. The full fixtures under
# fixtures/ are left untouched.
#
# Live network downloads drive the progress shown; several beats depend on live
# results settling in time, so the Sleeps are generous. Outputs to demo/out/
# under the `full2-` prefix + kwire-full-2.mp4 so it clobbers neither tour.tape's
# media nor tour-full.tape's `full-` set.
#
# This is a variant of tour-full.tape whose ONLY difference is the Jeremy import:
# it picks jeremy.md by cycling the wildmenu with Tab (rather than typing "j").
#
# RUN (from the repo root):  vhs demo/tour-full-2.tape

Output "demo/out/kwire-full-2.mp4"

Set Shell "zsh"
Set FontSize 26
Set Width 2560
Set Height 1600
Set Padding 40

Env HOME "/tmp/kwire-demo-home"
Env XDG_DOWNLOAD_DIR "/tmp/kwire-demo-home/Downloads"

# ---- Setup + launch (all Hidden) -------------------------------------------
Hide
Type "export PS1='$ '"
Enter
Type "rm -rf /tmp/kwire-demo-home && mkdir -p /tmp/kwire-demo-home/Downloads"
Enter
Type "mkdir -p /tmp/kwire-demo && cp demo/lists/jeremy.md /tmp/kwire-demo/jeremy.md && cp demo/lists/avery.md /tmp/kwire-demo/avery.md"
Enter
Type "clear"
Enter
Type "./target/release/kwire"
Enter
Sleep 3s
Show
Sleep 3s
Down
Sleep 500ms
Up
Sleep 2s

# 01 - empty first-run splash
Screenshot "demo/out/full2-01-empty.png"

# ---- 02 import Jeremy — showcase :import Tab-completion (wildmenu cycling) ----
# Type the directory, Tab to reveal the candidates (wildmenu row) with the first
# one (avery.md) highlighted, Tab again to MOVE the highlight to jeremy.md, then
# Enter to accept it (closes the wildmenu, fills the buffer) and Enter to submit.
Type ":import /tmp/kwire-demo/"
Sleep 1500ms
Tab
Sleep 2s
Screenshot "demo/out/full2-02-import-complete.png"
Tab
Sleep 1500ms
Screenshot "demo/out/full2-03-import-completed.png"
Enter
Sleep 800ms
Enter
# (+5s after #2)
Sleep 5s

# ---- 03 Jeremy populated (let books resolve & download) ----
Sleep 25s
Screenshot "demo/out/full2-04-jeremy.png"
# (+10s after #3)
Sleep 10s

# ---- 04 walk down the book list and back (keyboard) ----
# The trimmed 20-book list fits on one screen, so this travels the selection
# highlight down through the groups and back rather than scrolling a viewport.
Down 8
Sleep 1s
Screenshot "demo/out/full2-05-scroll.png"
Up 8
Sleep 1s
# (+5s after #4)
Sleep 5s

# ---- 05 a book detail ----
Down
Sleep 600ms
Down
Sleep 600ms
Enter
Sleep 3s
Screenshot "demo/out/full2-06-detail.png"
Escape
Sleep 1500ms
# (+5s after #5)
Sleep 5s

# ---- 07 the download pane: focus Activity, scroll the legs ----
Tab
Sleep 2s
Screenshot "demo/out/full2-07-activity.png"
Down
Sleep 500ms
Down
Sleep 500ms
Down
Sleep 500ms
Screenshot "demo/out/full2-08-activity-scroll.png"
# return focus to the book list (Activity -> Header -> List)
Tab
Sleep 400ms
Tab
Sleep 600ms
# (+5s after the original #7)
Sleep 5s

# ---- 08 manually add Pride and Prejudice ----
Type ":add Pride and Prejudice, Jane Austen"
Sleep 1s
Enter
Sleep 8s
Screenshot "demo/out/full2-09-manual-add.png"
# (the 20s "let states settle" sleep is intentionally removed)

# ---- 06 import Avery (loaded LATE so Jeremy finishes first) ----
# Tab-complete the unique "a" match.
Type ":import /tmp/kwire-demo/a"
Sleep 800ms
Tab
Sleep 1s
Enter
Sleep 12s
Screenshot "demo/out/full2-10-avery.png"

# ---- 06b page-scroll the combined list (Shift-J / Shift-K) ----
# Jump to the ★ All aggregate (Jeremy + Manual + Avery = 41 books, taller than
# the viewport) and page through it. Shift-J / Shift-K act on the BOOK LIST pane
# ONLY, so the list must be the active pane first: ']' switches the list (focus
# stays on the list, where it already is after the import) and '1' (All filter)
# puts the selection at the top.
Type "]"
Sleep 1s
Type "1"
Sleep 1500ms
Type "J"
Sleep 1500ms
Screenshot "demo/out/full2-11-page-scroll.png"
Type "J"
Sleep 1500ms
Type "K"
Sleep 1500ms
Type "K"
Sleep 1500ms

# ---- 09 needs-you: choose a copy (already on the ★ All aggregate) ----
# filter (2) shows the needs-you books across all lists.
Type "2"
Sleep 3s
Screenshot "demo/out/full2-12-needs-you-list.png"
# Open a needs-you book and pick its 2nd copy. Which of the (few) needs-you books
# lands here depends on live timing — any of them is a fine demo of the Picker.
Down
Sleep 1s
Enter
Sleep 3s
Screenshot "demo/out/full2-13-needs-you-picker.png"
Down
Sleep 800ms
Enter
Sleep 2s

# ---- 10a fill the wait by touring the lists ([ ]) and the filter chips (/) ----
# A Wonder-Book (Jeremy #10) must finish downloading and be page-verified before
# its "too few pages" check-download flag appears — that takes a while on the
# throttled downloads. Rather than wait statically, tour the UI with the GLOBAL
# hotkeys (no pane focus needed, so the List focus is never disturbed): [ ] cycle
# the 3 lists (+ the ★ All stop), / cycles the 7 filter chips. Steps are spaced
# ~8s apart so the download has time to land; we finish on ★ All + Check-download.

# cycle the lists with ] : All -> Jeremy -> Manual -> Avery -> back to ★ All
Type "]"
Sleep 8s
Type "]"
Sleep 8s
Type "]"
Sleep 8s
Type "]"
Sleep 8s
# cycle the 7 filter chips with / (currently on Needs you): Check, Can't, Queued,
# In prog, Done, All, Needs you, Check — visiting every chip, ending on Check.
Type "/"
Sleep 8s
Type "/"
Sleep 8s
Type "/"
Sleep 8s
Type "/"
Sleep 8s
Type "/"
Sleep 8s
Type "/"
Sleep 8s
Type "/"
Sleep 8s
Type "/"
Sleep 8s

# ---- 10 check-download: open A Wonder-Book, choose the 2nd variation ----
# The / tour ended on the Check-download filter. K (page-up) lands on the FIRST
# flagged book — A Wonder-Book for Girls and Boys (row 0); it clamps at row 0 and
# never crosses up into the header, robust whether 1 or 2 books are flagged. Enter
# opens its detail; Down to the 2nd variation, `d` to choose it (the detail stays
# open), Esc to dismiss, `1` back to the full list.
Screenshot "demo/out/full2-14-check-download-list.png"
Type "K"
Sleep 800ms
Enter
Sleep 3s
Screenshot "demo/out/full2-15-check-download-detail.png"
# choose the second variation (the alternate copy)
Down
Sleep 800ms
Type "d"
Sleep 2s
Escape
Sleep 1s
Type "1"
Sleep 1s
# (+5s after #10)
Sleep 5s

# ---- 11 Jeremy / all 20 -> Ozma of Oz -> detail -> S = series ----
# ']' from the ★ All aggregate lands on a concrete list (Jeremy); show all 20.
Type "]"
Sleep 2s
Type "1"
Sleep 1s
Screenshot "demo/out/full2-16-jeremy-all.png"
# Ozma of Oz is the 9th book (group 1's 5 + Fantasy & Wonder #4). The `1` filter
# just reset the selection to the top (book 1), so Down x8 lands exactly on Ozma.
Down
Sleep 150ms
Down
Sleep 150ms
Down
Sleep 150ms
Down
Sleep 150ms
Down
Sleep 150ms
Down
Sleep 150ms
Down
Sleep 150ms
Down
Sleep 150ms
Sleep 1s
Screenshot "demo/out/full2-17-ozma-selected.png"
# open the detail, then S to download the whole series
Enter
Sleep 3s
Screenshot "demo/out/full2-18-ozma-detail.png"
Type "S"
# The Open Library series lookup takes a while — wait for the
# "<series> (series)" list to be created and switched to.
Sleep 25s
# S leaves the seed book's detail modal open over the new list; close it (the
# modal IS open, so Esc is safe here) to show the clean series list, and to give
# the delete/about beats below a clean (no-modal) starting state.
Escape
Sleep 2s
Screenshot "demo/out/full2-19-series.png"
# (+5s after #11)
Sleep 5s

# ---- 12 delete-list confirm (kept from the proven tape) ----
# get to List focus, walk up to the list strip, then D.
Tab
Sleep 400ms
Tab
Sleep 400ms
Up
Sleep 200ms
Up
Sleep 200ms
Up
Sleep 200ms
Up
Sleep 200ms
Up
Sleep 200ms
Up
Sleep 200ms
Up
Sleep 200ms
Up
Sleep 200ms
Up
Sleep 200ms
Up
Sleep 200ms
Type "D"
Sleep 2s
Screenshot "demo/out/full2-20-delete-confirm.png"
# decline the delete
Type "n"
Sleep 2s
# (+5s after #12)
Sleep 5s

# ---- 13 about panel (ends the tour) ----
Type ":about"
Sleep 1s
Enter
Sleep 2s
Screenshot "demo/out/full2-21-about.png"
# (+5s after #13)
Sleep 5s
