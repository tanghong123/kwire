# kwire — EXTENDED guided tour tape for VHS (separate from the proven tour.tape).
#
# A longer, story-driven run:
#   import Jeremy (showcasing :import Tab-completion) -> scroll the books ->
#   a book detail -> import Avery -> the download pane (scroll the legs) ->
#   manually add Pride and Prejudice -> needs-you: choose a copy ->
#   check-download: choose a copy -> Jeremy / all 40 -> Ozma of Oz detail ->
#   S = download series -> delete-list confirm -> the about panel.
#
# Live network downloads drive the progress shown; several beats depend on live
# results settling in time, so the Sleeps are generous. Outputs to demo/out/
# under the `full-` prefix + kwire-full.mp4 so it never clobbers tour.tape's media.
#
# RUN (from the repo root):  vhs demo/tour-full.tape

Output "demo/out/kwire-full.mp4"

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
Type "mkdir -p /tmp/kwire-demo && cp fixtures/jeremy_public_domain_list.md /tmp/kwire-demo/jeremy.md && cp fixtures/avery_public_domain_list.md /tmp/kwire-demo/avery.md"
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
Screenshot "demo/out/full-01-empty.png"

# ---- 02 import Jeremy — showcase :import Tab-completion ----
# Type the directory, Tab to reveal the candidates (wildmenu row), then type "j"
# and Tab again to complete the unique match to jeremy.md.
Type ":import /tmp/kwire-demo/"
Sleep 1500ms
Tab
Sleep 2s
Screenshot "demo/out/full-02-import-complete.png"
Type "j"
Sleep 600ms
Tab
Sleep 1500ms
Screenshot "demo/out/full-03-import-completed.png"
Enter

# ---- 03 Jeremy populated (let books resolve & download) ----
Sleep 25s
Screenshot "demo/out/full-04-jeremy.png"

# ---- 04 scroll the list of books ----
Type "J"
Sleep 1s
Type "J"
Sleep 1s
Screenshot "demo/out/full-05-scroll.png"
# back to the top
Type "K"
Sleep 600ms
Type "K"
Sleep 1s

# ---- 05 a book detail ----
Down
Sleep 600ms
Down
Sleep 600ms
Enter
Sleep 3s
Screenshot "demo/out/full-06-detail.png"
Escape
Sleep 1500ms

# ---- 06 import Avery (Tab-complete the unique "a" match) ----
Type ":import /tmp/kwire-demo/a"
Sleep 800ms
Tab
Sleep 1s
Enter
Sleep 12s
Screenshot "demo/out/full-07-avery.png"

# ---- 07 the download pane: focus Activity, scroll the legs ----
Tab
Sleep 2s
Screenshot "demo/out/full-08-activity.png"
Down
Sleep 500ms
Down
Sleep 500ms
Down
Sleep 500ms
Screenshot "demo/out/full-09-activity-scroll.png"
# return focus to the book list (Activity -> Header -> List)
Tab
Sleep 400ms
Tab
Sleep 600ms

# ---- 08 manually add Pride and Prejudice ----
Type ":add Pride and Prejudice, Jane Austen"
Sleep 1s
Enter
Sleep 8s
Screenshot "demo/out/full-10-manual-add.png"

# let needs-you + check-download states settle across the lists
Sleep 20s

# ---- 09 needs-you: choose a copy ----
# From Manual, ']' lands on the ★ All aggregate; filter (2) shows needs-you books.
Type "]"
Sleep 2s
Type "2"
Sleep 3s
Screenshot "demo/out/full-11-needs-you-list.png"
# open the first needs-you book (needs_selection -> Picker), pick the 2nd copy
Down
Sleep 1s
Enter
Sleep 3s
Screenshot "demo/out/full-12-needs-you-picker.png"
Down
Sleep 800ms
Enter
Sleep 2s

# give A Wonder-Book (Jeremy #20) time to reach the front of the download queue,
# finish, and be page-count-verified — only then is it flagged "too few pages"
# (check-download). Downloads are throttled, so this needs a generous wait.
Sleep 60s

# ---- 10 check-download: open A Wonder-Book, choose the 2nd variation ----
# Filter (3) leaves the (single) flagged book selected, so press Enter DIRECTLY
# to open its detail — no Down first (a Down on a 1-row filter would cross into
# the Activity pane and steal focus). In the detail, Down to the 2nd variation
# and `d` to choose it (the detail stays open), then Esc to dismiss (safe — the
# modal is open) and `1` back to the full list.
Type "3"
Sleep 3s
Screenshot "demo/out/full-13-check-download-list.png"
# the filter leaves the LAST flagged book (The Heroes) selected; step up to the
# first one, A Wonder-Book for Girls and Boys, then open it.
Up
Sleep 800ms
Enter
Sleep 3s
Screenshot "demo/out/full-14-check-download-detail.png"
# choose the second variation (the alternate copy)
Down
Sleep 800ms
Type "d"
Sleep 2s
Escape
Sleep 1s
Type "1"
Sleep 1s

# ---- 11 Jeremy / all 40 -> Ozma of Oz (14th book) -> detail -> S = series ----
# ']' from the ★ All aggregate lands on a concrete list (Jeremy); show all 40.
Type "]"
Sleep 2s
Type "1"
Sleep 1s
Screenshot "demo/out/full-15-jeremy-all.png"
# Ozma of Oz is the 14th book (group 1's 10 + Fantasy & Wonder #4). The `1`
# filter just reset the selection to the top (book 1), so Down x13 lands exactly
# on Ozma — no `K` first (a `K` at the top overshoots up into the header and
# throws the count off by one).
Down
Sleep 120ms
Down
Sleep 120ms
Down
Sleep 120ms
Down
Sleep 120ms
Down
Sleep 120ms
Down
Sleep 120ms
Down
Sleep 120ms
Down
Sleep 120ms
Down
Sleep 120ms
Down
Sleep 120ms
Down
Sleep 120ms
Down
Sleep 120ms
Down
Sleep 120ms
Sleep 1s
Screenshot "demo/out/full-16-ozma-selected.png"
# open the detail, then S to download the whole series
Enter
Sleep 3s
Screenshot "demo/out/full-17-ozma-detail.png"
Type "S"
# The Open Library series lookup takes a while — wait for the
# "<series> (series)" list to be created and switched to.
Sleep 25s
# S leaves the seed book's detail modal open over the new list; close it (the
# modal IS open, so Esc is safe here) to show the clean series list, and to give
# the delete/about beats below a clean (no-modal) starting state.
Escape
Sleep 2s
Screenshot "demo/out/full-18-series.png"

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
Screenshot "demo/out/full-19-delete-confirm.png"
# decline the delete
Type "n"
Sleep 2s

# ---- 13 about panel (ends the tour) ----
Type ":about"
Sleep 1s
Enter
Sleep 2s
Screenshot "demo/out/full-20-about.png"
Sleep 1s
