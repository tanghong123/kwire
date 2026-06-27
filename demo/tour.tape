# kwire — guided tour tape for VHS (https://github.com/charmbracelet/vhs)
#
# Produces a high-resolution screenshot set + video of the whole app flow:
# empty first-run -> import a list -> browse/detail/help/picker -> a second
# list -> the aggregate "All" view -> a manual add -> filters -> activity ->
# delete confirm.
#
# PREREQUISITES
#   brew install vhs
#   cargo build --release --bin kwire     # produces ./target/release/kwire
#
# RUN (from the repo root)
#   vhs demo/tour.tape
#
# OUTPUT lands in demo/out/ (git-ignored): kwire-tour.mp4 + NN-*.png
#
# NOTES
#   - Runs against a throwaway HOME (/tmp/kwire-demo-home) so it always starts
#     from an empty database and never touches your real library.
#   - Imports the repo's public-domain fixture lists via a neutral /tmp path, so
#     no usernames or home paths appear on screen.
#   - Live network downloads drive the progress shown; the Sleeps allow for them.
#     Bump the Sleeps on a slow connection.
#   - Extend the tour by adding more Screenshot steps; keep setup inside Hide so
#     no shell prompt / path ever renders to a frame.

Output "demo/out/kwire-tour.mp4"

Set Shell "zsh"
Set FontSize 26
Set Width 2560
Set Height 1600
Set Padding 40

Env HOME "/tmp/kwire-demo-home"
Env XDG_DOWNLOAD_DIR "/tmp/kwire-demo-home/Downloads"

# ---- Setup + launch (all Hidden) -------------------------------------------
# Neutralize the prompt, prepare a fresh empty HOME + neutral fixture paths, and
# fully enter the app's alt-screen BEFORE Show, so no shell prompt / username /
# home path / binary path ever renders to a visible frame.
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
# nudge a paint
Down
Sleep 500ms
Up
Sleep 2s

# 01 - empty first-run splash
Screenshot "demo/out/01-empty-first-run.png"

# ---- 02 import command (capture the command line before Enter) ----
Type ":import /tmp/kwire-demo/jeremy.md"
Sleep 2s
Screenshot "demo/out/02-import-command.png"
Enter

# ---- 03 list populated (let books resolve & download) ----
Sleep 25s
Screenshot "demo/out/03-list-populated.png"

# ---- 04 book detail (variations + history) ----
Down
Sleep 800ms
Enter
Sleep 4s
Screenshot "demo/out/04-book-detail.png"

# ---- 05 help overlay from detail ----
Type "?"
Sleep 2s
Screenshot "demo/out/05-help.png"
# close help with '?' (NOT Escape) so a stray Esc doesn't fall through to the list (which quits)
Type "?"
Sleep 1s
# close detail with a single Escape -> back to list
Escape
Sleep 2s

# make sure filter is All so the full list is navigable
Type "1"
Sleep 1s

# ---- 06 picker ("choose a copy") ----
# "The Children's Homer" is a needs-selection book (4 candidates < 0.85). It is
# the 39th of 40; book #40 is "Tales from Shakespeare". Page to the bottom, then
# Up once lands on it; let its query resolve, then Enter opens the Picker.
Type "J"
Sleep 600ms
Type "J"
Sleep 600ms
Type "J"
Sleep 800ms
Up
Sleep 800ms
# wait for the live query to resolve to needs_selection before opening it;
# otherwise Enter would open the plain Book detail instead of the Picker.
Sleep 35s
Enter
Sleep 3s
Screenshot "demo/out/06-picker.png"
Escape
Sleep 1500ms
Type "1"
Sleep 1s

# ---- 07 second list ----
Type ":import /tmp/kwire-demo/avery.md"
Sleep 1s
Enter
Sleep 12s
Screenshot "demo/out/07-second-list.png"

# ---- 08 all aggregate (two '[' from Avery -> Jeremy -> ★ All) ----
Type "["
Sleep 1s
Type "["
Sleep 2s
Screenshot "demo/out/08-all-aggregate.png"

# ---- 09 manual add ----
Type ":add The Wonderful Wizard of Oz"
Sleep 1s
Enter
Sleep 6s
Screenshot "demo/out/09-manual-add.png"

# give earlier downloads time to finish so 'Done' has entries
Sleep 15s

# ---- 10 filter done (on the ★ All aggregate, which has many Done books) ----
# from Manual (last list) one ']' lands on the ★ All aggregate stop
Type "]"
Sleep 2s
Type "7"
Sleep 2s
Screenshot "demo/out/10-filter-done.png"
Type "1"
Sleep 1s

# switch to a concrete list so the delete demo has a real target
Type "]"
Sleep 2s

# ---- 11 activity pane (Tab focuses Activity) ----
Tab
Sleep 2s
Screenshot "demo/out/11-activity-pane.png"

# ---- 12 delete confirm ----
# return to List focus (Activity -> Header -> List), then Up walks up to the
# list strip (top of list -> chips -> ListStrip; extra Ups simply stop), then D.
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
Screenshot "demo/out/12-delete-confirm.png"
# decline the delete
Type "n"
Sleep 2s
