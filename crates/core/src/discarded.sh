# arena set-branch --hard: keep what the reset discards, on the pod, where nothing overwrites it.
#
# Rendered by arena_core::backup::checkout_command (--hard only), which runs `discarded_save` from
# the repo's top after the fetch and after unredacted.sh has set the real keys aside, right before
# `checkout -f` + `reset --hard`. Review (ai-review claude #2, codex #2): the only copy of a pod's
# uncommitted work was set-branch's rsync into the day's snapshot label and the big mirror — the
# very paths the next `pods pull` re-rsyncs from the reset pod, overwriting it. So first it is saved
# in $1/arena-discarded/<UTC time>/ (in .git: the reset never touches it, no push carries it, and
# every later copy — snapshot, big, archive — carries it under a name no later save reuses):
#   changes.patch   `git diff --binary HEAD`: every uncommitted change to tracked files, staged or
#                   not (the conflicted files' too, markers and all)
#   conflicted/     the files with unresolved conflicts, as they were
#   state/          the operation's own files (MERGE_HEAD, MERGE_MSG, rebase-merge/, sequencer/, …)
#   untracked/      what the reset removes that no patch has: untracked or ignored files, links and
#                   directories at a path <ref> tracks — or in its way (a file or link where <ref>
#                   has a directory)
#   README          what this is and how to get it back
# Nothing to keep, no folder. Prints, on stdout:
#   DISCARDED_SAVED <path>   the folder, relative to the repo's top
# `discarded_save <git dir> <ref>`: non-zero, said on stderr, when it couldn't save — the caller then
# doesn't reset, and the part saved so far is removed (the work is all still in place; a copy cut
# short by a full disk mustn't stay to fill it). A <ref> that isn't a commit saves nothing: the
# switch fails on it, discarding nothing.
discarded_save() {
    if ! git rev-parse -q --verify "$2^{commit}" >/dev/null 2>&1; then return 0; fi
    ds_head=$(git rev-parse -q --verify HEAD 2>/dev/null) || ds_head=
    ds_on=$(git rev-parse --abbrev-ref HEAD 2>/dev/null) || ds_on=HEAD
    ds_d="$1/arena-discarded/$(date -u +%Y%m%dT%H%M%SZ)"
    ds_made=
    # Never into an earlier save's folder: a second reset in the same second gets its own.
    mkdir -p "$1/arena-discarded" 2>/dev/null || true
    if ! mkdir "$ds_d" 2>/dev/null; then
        ds_d="$ds_d-$$"
        if ! mkdir "$ds_d"; then ds_fail "$ds_d" "anything"; return 1; fi
    fi
    ds_made=1
    ds_w="$ds_d/.lists"
    if ! mkdir "$ds_w"; then ds_fail "$ds_d" "its lists"; return 1; fi
    # Every uncommitted change to tracked files.
    if [ -n "$ds_head" ]; then
        if ! git diff --binary --full-index HEAD > "$ds_d/changes.patch"; then ds_fail "$ds_d" "the diff"; return 1; fi
        if [ ! -s "$ds_d/changes.patch" ]; then rm -f "$ds_d/changes.patch"; fi
    fi
    # The conflicted files, as they are on disk.
    if ! { git ls-files -u -z > "$ds_w/u" && tr '\0' '\n' < "$ds_w/u" | cut -f2- | LC_ALL=C sort -u > "$ds_w/conflicted"; } \
        || ! ds_copy "$ds_w/conflicted" "$ds_d/conflicted"; then
        ds_fail "$ds_d" "the conflicted files"
        return 1
    fi
    # The operation's state: what it was merging or applying, its message, its todo list.
    for f in MERGE_HEAD MERGE_MSG MERGE_MODE MERGE_AUTOSTASH AUTO_MERGE CHERRY_PICK_HEAD REVERT_HEAD SQUASH_MSG \
        rebase-merge rebase-apply sequencer; do
        if [ -e "$1/$f" ]; then
            if ! { mkdir -p "$ds_d/state" && cp -pR "$1/$f" "$ds_d/state/"; }; then ds_fail "$ds_d" "$f"; return 1; fi
        fi
    done
    # What the reset removes that no patch has. `checkout -f` writes each path <ref> tracks (a file,
    # a link or a gitlink: `ls-tree -r` lists no directories), and whatever is in its way goes:
    # - at the path itself, an untracked or ignored file or link; or a directory — whole, ignored
    #   contents, a nested repo's .git and all (git's remove_subtree; review ai-review codex #1,
    #   claude #7: an ignored `checkpoints/` where staff now track a `checkpoints` file was deleted
    #   unsaved) — whether the index tracks that path or not (a tracked file the participant
    #   replaced with a directory goes the same way). Not a directory where <ref> has a gitlink:
    #   git leaves a submodule's directory be;
    # - at a leading path, where <ref> has a directory: a file or a link the index doesn't track (a
    #   link itself, never what it points to: git removes the link, not its target's files).
    # A file the index tracks, at its own path, is in the patch. The walk goes from the top, so a
    # link on the way is what's saved, not the files reached through it. Directories are copied
    # whole (`cp -pPR`: links kept as links, never followed), so a big one costs its size here.
    if ! { git ls-tree -r -z "$2" > "$ds_w/t" && git ls-files -z > "$ds_w/i" \
        && tr '\0' '\n' < "$ds_w/t" | cut -f2- | LC_ALL=C sort -u > "$ds_w/ref" \
        && tr '\0' '\n' < "$ds_w/t" | awk 'index($0, "160000 ") == 1 { print substr($0, index($0, "\t") + 1) }' | LC_ALL=C sort -u > "$ds_w/gitlinks" \
        && tr '\0' '\n' < "$ds_w/i" | LC_ALL=C sort -u > "$ds_w/index" \
        && LC_ALL=C comm -2 "$ds_w/ref" "$ds_w/index" > "$ds_w/walk" && : > "$ds_w/hit"; }; then
        ds_fail "$ds_d" "the paths $2 tracks"
        return 1
    fi
    # $ds_w/walk: every path <ref> tracks — the ones the index has after a tab (comm's 3rd column).
    ds_tab=$(printf '\t')
    while IFS= read -r ds_l; do
        case $ds_l in "$ds_tab"*) ds_r=${ds_l#"$ds_tab"} ds_k=1 ;; *) ds_r=$ds_l ds_k= ;; esac
        ds_q=
        while [ -n "$ds_r" ]; do
            case $ds_r in */*) ds_q=${ds_q:+$ds_q/}${ds_r%%/*} ds_r=${ds_r#*/} ;; *) ds_q=${ds_q:+$ds_q/}$ds_r ds_r= ;; esac
            # $ds_q: the path so far; $ds_r: what's left of it (empty: $ds_q is the tracked path).
            if [ -L "$ds_q" ] || [ -f "$ds_q" ]; then
                if { [ -z "$ds_r" ] && [ -z "$ds_k" ]; } || { [ -n "$ds_r" ] && ! grep -qxF -- "$ds_q" "$ds_w/index"; }; then
                    if ! printf '%s\n' "$ds_q" >> "$ds_w/hit"; then ds_fail "$ds_d" "the untracked files"; return 1; fi
                fi
                break
            fi
            if [ ! -d "$ds_q" ]; then break; fi
            if [ -z "$ds_r" ] && ! grep -qxF -- "$ds_q" "$ds_w/gitlinks"; then
                if ! printf '%s\n' "$ds_q" >> "$ds_w/hit"; then ds_fail "$ds_d" "the untracked files"; return 1; fi
            fi
        done
    done < "$ds_w/walk"
    if ! { LC_ALL=C sort -u "$ds_w/hit" > "$ds_w/untracked" && ds_copy "$ds_w/untracked" "$ds_d/untracked"; }; then
        ds_fail "$ds_d" "the untracked files"
        return 1
    fi
    rm -rf "$ds_w"
    if [ -z "$(ls -A "$ds_d")" ]; then
        rmdir "$ds_d"
        rmdir "$1/arena-discarded" 2>/dev/null || true
        return 0
    fi
    if ! ds_readme "$ds_d" "$2" "$ds_on" "$ds_head" > "$ds_d/README"; then ds_fail "$ds_d" "its README"; return 1; fi
    printf 'DISCARDED_SAVED %s\n' "${ds_d#"$(pwd)/"}"
}

# `ds_copy <list> <dir>`: each path listed (one per line, relative to the repo's top) copied into
# <dir> at the same path — a link as a link, a directory whole (links in it kept as links), modes
# and times kept. Non-zero at the first that fails.
ds_copy() {
    while IFS= read -r p; do
        if [ -z "$p" ] || { [ ! -e "$p" ] && [ ! -L "$p" ]; }; then continue; fi
        if ! { mkdir -p "$2/$(dirname -- "$p")" && cp -pPR -- "$p" "$2/$p"; }; then return 1; fi
    done < "$1"
}

# `ds_fail <dir> <what>`: says what couldn't be saved, and removes the save's own folder (made by
# this run: $ds_made) — the pod isn't reset, so nothing in it is needed.
ds_fail() {
    printf '%s\n' "arena set-branch: couldn't save $2 into $1 before the --hard reset - not reset" >&2
    if [ -n "$ds_made" ]; then
        rm -rf "$1"
        rmdir "$(dirname -- "$1")" 2>/dev/null || true
    fi
}

# `ds_readme <dir> <ref> <branch> <commit>`: the README, naming only the parts this save has.
ds_readme() {
    printf '%s\n' "What \`arena pods set-branch --hard\` discarded when it reset this checkout to $2 (UTC $(basename -- "$1"))." \
        "Saved here first; nothing here is pushed, and the rsync backups copy it with the rest of .git."
    if [ -n "$4" ]; then
        printf '%s\n' "It was on $3 at commit $4: \`git branch rescue $4\` brings that commit back, and any commits before it no branch has."
    fi
    if [ -f "$1/changes.patch" ]; then
        printf '%s\n' "- changes.patch: its uncommitted changes to tracked files. To get them back, from the checkout's top:" \
            "    git branch rescue $4 && git checkout rescue && git apply --index $1/changes.patch" \
            "  or onto the commit it is on now: git apply --3way $1/changes.patch"
    fi
    if [ -d "$1/conflicted" ]; then
        printf '%s\n' "- conflicted/: the files whose conflicts weren't resolved yet, as they were (conflict markers and all)."
    fi
    if [ -d "$1/state" ]; then
        printf '%s\n' "- state/: the git operation that was in progress (MERGE_HEAD: the commit being merged; MERGE_MSG; rebase-merge/, ...)."
    fi
    if [ -d "$1/untracked" ]; then
        printf '%s\n' "- untracked/: untracked or ignored files, links and directories the reset removed (it replaced them with $2's" \
            "  version, or a directory of $2's went where they were): copy them back from here, e.g. cp -pPR $1/untracked/<path> <path>"
    fi
}
