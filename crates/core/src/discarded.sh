# arena set-branch --hard: keep what the reset discards, on the pod, where nothing overwrites it.
#
# Rendered by arena_core::backup::checkout_command (--hard only), which runs `discarded_save` from
# the repo's top after the fetch and after unredacted.sh has set the real keys aside, right before
# `checkout -f` + `reset --hard`. Review (ai-review claude #2, codex #2): the only copy of a pod's
# uncommitted work was set-branch's rsync into the day's snapshot label and the big mirror — the
# very paths the next `pods pull` re-rsyncs from the reset pod, overwriting it. So first it is saved
# in $1/arena-discarded/<UTC time>/ (in .git: the reset never touches it, no push carries it, and
# every later copy — snapshot, big, archive — carries it under a name no later save reuses):
#   changes.patch   every uncommitted change to tracked files, staged or not, in one: HEAD -> the
#                   files (the conflicted files' too, markers and all)
#   staged.patch    what was staged: HEAD -> the index (review ai-review codex #3: a file staged in
#                   one version and then changed again — or put back to HEAD's — in the work tree has
#                   that staged version nowhere else). The files with unresolved conflicts have no one
#                   staged version: it lists them as `* Unmerged path …`, which `git apply` passes over
#   unstaged.patch  what wasn't: the index -> the files, the conflicted files left out (their diff
#                   is a combined one no `git apply` takes; conflicted/ has them)
#   conflicted/     the files with unresolved conflicts, as they were
#   state/          the operation's own files (MERGE_HEAD, MERGE_MSG, rebase-merge/, sequencer/, …)
#   untracked/      what the reset removes that no patch has: untracked or ignored files, links and
#                   directories at a path <ref> tracks — or in its way (a file or link where <ref>
#                   has a directory)
#   README          what this is and how to get it back
# The patches are plumbing (`diff-index`, `diff-files`, binary and full-index): no participant's diff
# config (colour, no prefix, an external diff) gets into them, and the index is never refreshed.
# The commits the reset can leave on no branch are kept from git's gc by refs (no reflog of their
# own: once a branch's reflog expires, nothing else may keep them):
#   refs/arena-discarded/<the folder's name>  the commit it was on (review ai-review claude #11): when
#                   anything else is saved (the patches apply to it), or when it has commits no
#                   remote has (a detached HEAD's, or a branch's the reset moves)
#   refs/arena-discarded/<the folder's name>-<branch>  the local <branch>'s tip, when it has commits
#                   <ref> doesn't (review ai-review codex #3): `checkout -f -B <branch> <ref>` moves
#                   it to <ref>, whichever branch the checkout was on (a pod on a clean, pushed
#                   autocommit branch with an unpushed commit on its local main lost that commit)
# A save of refs alone is a folder with just the README, naming them. Nothing to keep, no folder
# and no ref. Prints, on stdout:
#   DISCARDED_SAVED <path>   the folder, relative to the repo's top
#   DISCARDED_PINNED <ref>   each ref it made
# `discarded_save <git dir> <ref> <branch> <max KiB>`: non-zero, said on stderr, when it couldn't
# save — the caller then doesn't reset, and the part saved so far is removed, refs and all (the
# work is all still in place; a copy cut short by a full disk mustn't stay to fill it). So when
# what untracked/ would hold is over <max KiB> (DISCARD_MAX_GB) or over half what's free on the
# filesystem of <git dir> (review ai-review claude #2): totalled first, nothing of it copied, and
# the line says what was too big and how to move it out of the way (`ds_fits`). A save the switch
# is stopped in the middle of (a TERM from its bound, a dropped connection: an exit, so the EXIT
# trap runs) is removed by `ds_abandon`, the switch's EXIT trap; only a completed one ($ds_done,
# set after its last step) stays. A <ref> that isn't a commit saves nothing: the switch fails on
# it, discarding nothing.
discarded_save() {
    if ! git rev-parse -q --verify "$2^{commit}" >/dev/null 2>&1; then return 0; fi
    ds_head=$(git rev-parse -q --verify HEAD 2>/dev/null) || ds_head=
    ds_on=$(git rev-parse --abbrev-ref HEAD 2>/dev/null) || ds_on=HEAD
    ds_d="$1/arena-discarded/$(date -u +%Y%m%dT%H%M%SZ)"
    ds_made= ds_refs= ds_done= ds_big= ds_what=
    # Never into an earlier save's folder: a second reset in the same second gets its own.
    mkdir -p "$1/arena-discarded" 2>/dev/null || true
    if ! mkdir "$ds_d" 2>/dev/null; then
        ds_d="$ds_d-$$"
        if ! mkdir "$ds_d"; then ds_fail "$ds_d" "anything"; return 1; fi
    fi
    ds_made=1
    # What it's doing ($ds_step), for the line of a save stopped in the middle (`ds_abandon`).
    ds_step='saving the conflicted files'
    ds_w="$ds_d/.lists"
    if ! mkdir "$ds_w"; then ds_fail "$ds_d" "its lists"; return 1; fi
    # The conflicted files, as they are on disk.
    if ! { git ls-files -u -z > "$ds_w/u" && tr '\0' '\n' < "$ds_w/u" | cut -f2- | LC_ALL=C sort -u > "$ds_w/conflicted"; } \
        || ! ds_copy "$ds_w/conflicted" "$ds_d/conflicted"; then
        ds_fail "$ds_d" "the conflicted files"
        return 1
    fi
    # Every uncommitted change to tracked files: in one, and staged and not apart.
    ds_step='saving the diffs of the tracked files'
    if [ -n "$ds_head" ]; then
        if ! git diff-index -p --binary --full-index HEAD > "$ds_d/changes.patch"; then ds_fail "$ds_d" "the diff"; return 1; fi
        if ! git diff-index -p --binary --full-index --cached HEAD > "$ds_d/staged.patch"; then ds_fail "$ds_d" "the staged diff"; return 1; fi
    fi
    # Unstaged: each changed path but the conflicted ones, named (literally; xargs may split the
    # list, and patches put one after the other are one patch).
    if ! { git diff-files --name-only -z > "$ds_w/f" && tr '\0' '\n' < "$ds_w/f" | LC_ALL=C sort -u > "$ds_w/changed" \
        && LC_ALL=C comm -23 "$ds_w/changed" "$ds_w/conflicted" > "$ds_w/unstaged" \
        && : > "$ds_d/unstaged.patch" \
        && { [ ! -s "$ds_w/unstaged" ] || tr '\n' '\0' < "$ds_w/unstaged" \
            | GIT_LITERAL_PATHSPECS=1 xargs -0 git diff-files -p --binary --full-index -- > "$ds_d/unstaged.patch"; }; }; then
        ds_fail "$ds_d" "the unstaged diff"
        return 1
    fi
    # No diff in it, no patch: `git apply` refuses one with nothing to apply (a staged.patch of
    # `* Unmerged path` lines alone, say).
    for f in changes staged unstaged; do
        if [ -f "$ds_d/$f.patch" ] && ! grep -q '^diff --git ' "$ds_d/$f.patch"; then rm -f "$ds_d/$f.patch"; fi
    done
    # The operation's state: what it was merging or applying, its message, its todo list.
    ds_step="saving the operation's state"
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
    ds_step='listing what the reset would remove'
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
    if ! LC_ALL=C sort -u "$ds_w/hit" > "$ds_w/untracked"; then ds_fail "$ds_d" "the untracked files"; return 1; fi
    # Whether it fits, before any of it is copied: a directory goes whole.
    if [ -s "$ds_w/untracked" ]; then
        ds_step="totalling the size of what the reset would remove ($(wc -l < "$ds_w/untracked") path(s))"
        if ! ds_fits "$ds_w/untracked" "$1" "$4"; then ds_remove "$ds_d"; return 1; fi
        ds_step="copying $ds_what into the save"
    fi
    if ! ds_copy "$ds_w/untracked" "$ds_d/untracked"; then ds_fail "$ds_d" "the untracked files"; return 1; fi
    ds_step='writing its README and refs' ds_big=
    rm -rf "$ds_w"
    # The refs (above), whether or not anything else was saved.
    ds_name=$(basename -- "$ds_d")
    ds_pin=
    if [ -n "$ds_head" ] && { [ -n "$(ls -A "$ds_d")" ] || [ -n "$(git rev-list -n1 "$ds_head" --not --remotes 2>/dev/null)" ]; }; then
        ds_pin="refs/arena-discarded/$ds_name"
    fi
    ds_bpin=
    if [ -n "$3" ] && ds_tip=$(git rev-parse -q --verify "refs/heads/$3^{commit}" 2>/dev/null) \
        && [ -n "$(git rev-list -n1 "$2..$ds_tip" 2>/dev/null)" ]; then
        ds_bpin="refs/arena-discarded/$ds_name-$3"
    else
        ds_tip=
    fi
    if [ -z "$ds_pin" ] && [ -z "$ds_bpin" ] && [ -z "$(ls -A "$ds_d")" ]; then
        rmdir "$ds_d"
        rmdir "$1/arena-discarded" 2>/dev/null || true
        ds_made=
        return 0
    fi
    if ! ds_readme "$ds_d" "$2" "$ds_on" "$ds_head" "$ds_pin" "$3" "$ds_tip" "$ds_bpin" > "$ds_d/README"; then
        ds_fail "$ds_d" "its README"
        return 1
    fi
    # Each ref named in $ds_refs before it's made (review ai-review claude #7): a save stopped
    # between the two then removes it all the same (`ds_remove` passes over one never made), where
    # one named after was left pinning its commits, unnamed by any README.
    if [ -n "$ds_pin" ]; then
        ds_refs=$ds_pin
        if ! git update-ref "$ds_pin" "$ds_head"; then ds_fail "$ds_d" "the ref $ds_pin"; return 1; fi
    fi
    if [ -n "$ds_bpin" ]; then
        ds_refs="$ds_refs $ds_bpin"
        if ! git update-ref "$ds_bpin" "$ds_tip"; then ds_fail "$ds_d" "the ref $ds_bpin"; return 1; fi
    fi
    ds_done=1
    printf 'DISCARDED_SAVED %s\n' "${ds_d#"$(pwd)/"}"
    for ds_r in $ds_refs; do printf 'DISCARDED_PINNED %s\n' "$ds_r"; done
}

# `ds_abandon`: for the switch's EXIT trap. A save this run began and didn't complete — the switch
# stopped in the middle of it (review ai-review claude #2: a 30 GB directory copied under the
# switch's 100 s bound was cut short by its TERM, and each try left another partial copy in
# .git) — is removed, folder and refs; the pod wasn't reset. And it says on stderr what the save
# was doing (review ai-review claude #6: "ran past its 100 s" alone said nothing of why, and each
# retry was stopped the same way): stopped mid-copy, what it was copying, how big, and how to move
# it out of the way (`ds_move`). A completed save always stays, whatever happens to the reset
# after it. (A KILL runs no trap: a save it cuts short stays.) The words are worked out first and
# said last, after the save is gone, a write that fails passed over (review ai-review claude #4):
# a stop that is a dropped connection has no stderr to write to, and the write — a SIGPIPE, or
# under `set -e` a failed redirection — ended the shell inside its trap, the partial save left in
# .git.
ds_abandon() {
    if [ -n "${ds_made:-}${ds_refs:-}" ] && [ -z "${ds_done:-}" ]; then
        ds_why="arena set-branch: stopped while ${ds_step:-saving}, before the --hard reset — the partial save removed, not reset"
        if [ -n "${ds_big:-}" ]; then ds_why="$ds_why: move it out of the checkout $(ds_move "$ds_big"), or delete it, then switch again"; fi
        ds_remove "$ds_d"
        printf '%s\n' "$ds_why" >&2 || true
    fi
    return 0
}

# `ds_fits <list> <git dir> <max KiB>`: whether the paths listed (relative to the repo's top) fit
# in a save — `du -sk` of each, totalled — under <max KiB> and under half what's free on the
# filesystem of <git dir> (`df -Pk`; a `df` that can't tell leaves only the max). If not, says on
# stderr which path is biggest, how much is too much, and how to move it out of the way
# (`ds_move`).
ds_fits() {
    tr '\n' '\0' < "$1" | xargs -0 du -sk -- 2>/dev/null | LC_ALL=C sort -rn > "$1.du" || true
    ds_kb=$(awk '{ s += $1 } END { printf "%.0f\n", s + 0 }' "$1.du")
    # What it is, in words — set either way: the line of a save stopped mid-copy says it too.
    ds_big=$(head -n 1 "$1.du" | cut -f2-)
    ds_what="$ds_big ($(ds_size "$(head -n 1 "$1.du" | cut -f1)"))"
    ds_n=$(wc -l < "$1.du")
    if [ "$ds_n" -gt 1 ]; then ds_what="$ds_what and $((ds_n - 1)) more path(s), $(ds_size "$ds_kb") in all"; fi
    ds_free=$(df -Pk -- "$2" 2>/dev/null | awk 'NR == 2 && $4 ~ /^[0-9]+$/ { print $4 }')
    ds_over=
    if [ "$ds_kb" -gt "$3" ]; then ds_over="more than DISCARD_MAX_GB allows ($(ds_size "$3"))"; fi
    if [ -n "$ds_free" ] && [ "$((ds_kb * 2))" -gt "$ds_free" ]; then
        ds_over="${ds_over:+$ds_over, and }more than half the $(ds_size "$ds_free") free on its disk"
    fi
    if [ -z "$ds_over" ]; then return 0; fi
    printf '%s\n' "arena set-branch: not reset — the --hard reset would remove $ds_what, which is $ds_over to save first: move it out of the checkout $(ds_move "$ds_big"), or delete it, then switch again" >&2
    return 1
}

# `ds_move <path>`: the command that moves <path> (relative to the repo's top) out of the checkout,
# next to it, as `<its name>.kept-<UTC time>` (review ai-review claude #5): a name no earlier
# kept copy has, and `mv -n` besides, so a second switch's advice never replaces the first's copy.
# And `-T` (GNU's): the kept copy's name is the target itself, never a directory to move into —
# with `-n` alone, the same advice pasted again after the directory was made anew moved it inside
# the kept copy (review ai-review codex #4). A `mv` without `-T` (busybox's) gets `mv -n`: a probe
# asks (`-nT --help` is refused there, and only prints help where `-T` is known). Each word quoted
# by `ds_quote`: a path with a `'` in it pastes as it is.
ds_move() {
    if mv -nT --help > /dev/null 2>&1; then ds_mv='mv -nT'; else ds_mv='mv -n'; fi
    printf '(cd %s && %s -- %s %s)' "$(ds_quote "$(pwd)")" "$ds_mv" "$(ds_quote "$1")" \
        "$(ds_quote "../$(basename -- "$1").kept-$(date -u +%Y%m%dT%H%M%SZ)")"
}

# `ds_quote <word>`: <word> single-quoted for a POSIX shell, each `'` in it as `'\''` —
# arena_core::backup's shell_quote. (The `.` keeps a trailing newline from the substitution.)
ds_quote() {
    ds_qw=$(printf '%s.' "$1" | sed "s/'/'\\\\''/g")
    printf "'%s'" "${ds_qw%.}"
}

# `ds_size <KiB>`: in words, GB to one decimal.
ds_size() {
    awk -v k="$1" 'BEGIN { if (k >= 1048576) printf "%.1f GB", k / 1048576; else if (k >= 1024) printf "%d MB", k / 1024; else printf "%d KB", k }'
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

# `ds_fail <dir> <what>`: says what couldn't be saved, and removes the save (`ds_remove`) — the pod
# isn't reset, so nothing in it is needed.
ds_fail() {
    printf '%s\n' "arena set-branch: couldn't save $2 into $1 before the --hard reset - not reset" >&2
    ds_remove "$1"
}

# `ds_remove <dir>`: removes the save's own folder (made by this run: $ds_made) and the refs it
# made ($ds_refs).
ds_remove() {
    if [ -n "$ds_made" ]; then
        rm -rf "$1"
        rmdir "$(dirname -- "$1")" 2>/dev/null || true
    fi
    for ds_r in $ds_refs; do git update-ref -d "$ds_r" 2>/dev/null || true; done
    ds_made= ds_refs=
}

# `ds_readme <dir> <ref> <branch> <commit> <pin> <target branch> <its tip> <its pin>`: the README,
# naming only the parts this save has (<commit> empty: there was no commit; <pin> or <its pin>
# empty: no such ref).
ds_readme() {
    printf '%s\n' "What \`arena pods set-branch --hard\` discarded when it reset this checkout to $2 (UTC $(basename -- "$1"))." \
        "Saved here first; nothing here is pushed, and the rsync backups copy it with the rest of .git."
    if [ -n "$5" ]; then
        printf '%s\n' "It was on $3 at commit $4 (the ref $5 keeps it, and the commits before it, from git's gc: any patches below apply to it)." \
            "\`git branch rescue $4\` brings that commit back, and any commits before it no branch has."
    elif [ -n "$4" ]; then
        printf '%s\n' "It was on $3 at commit $4."
    fi
    if [ -n "$8" ]; then
        printf '%s\n' "Its local $6 was at commit $7, with $(git rev-list --count "$2..$7") commit(s) $2 doesn't have: the reset moved $6 to $2," \
            "and the ref $8 keeps them from git's gc. \`git branch $6-rescue $7\` brings them back."
    fi
    if [ -f "$1/staged.patch" ] || [ -f "$1/unstaged.patch" ]; then
        _rd="git branch rescue $4 && git checkout rescue"
        if [ -f "$1/staged.patch" ]; then _rd="$_rd && git apply --index $1/staged.patch"; fi
        if [ -f "$1/unstaged.patch" ]; then _rd="$_rd && git apply $1/unstaged.patch"; fi
        printf '%s\n' "- staged.patch, unstaged.patch: its uncommitted changes to tracked files, what was staged (the commit -> the" \
            "  index) and what wasn't (the index -> the files); one is missing when it had nothing. To get the index and the" \
            "  files back exactly as they were, from the checkout's top:" \
            "    $_rd"
    fi
    if [ -f "$1/changes.patch" ]; then
        printf '%s\n' "- changes.patch: the same changes in one (the commit -> the files, staged or not) — simpler, but a file staged" \
            "  in one version and changed again after comes back as the files had it, its staged version lost:" \
            "    git branch rescue $4 && git checkout rescue && git apply --index $1/changes.patch" \
            "  or onto the commit it is on now: git apply --3way $1/changes.patch"
    fi
    if [ -d "$1/conflicted" ]; then
        printf '%s\n' "- conflicted/: the files whose conflicts weren't resolved yet, as they were (conflict markers and all). No one" \
            "  staged version of them exists, so staged.patch and unstaged.patch leave them out (staged.patch names them," \
            "  \"* Unmerged path ...\", lines git apply passes over); changes.patch has them, markers and all."
    fi
    if [ -d "$1/state" ]; then
        printf '%s\n' "- state/: the git operation that was in progress (MERGE_HEAD: the commit being merged; MERGE_MSG; rebase-merge/, ...)."
    fi
    if [ -d "$1/untracked" ]; then
        printf '%s\n' "- untracked/: untracked or ignored files, links and directories the reset removed (it replaced them with $2's" \
            "  version, or a directory of $2's went where they were): copy them back from here, e.g. cp -pPR $1/untracked/<path> <path>"
    fi
    _rd=
    for _r in $5 $8; do _rd="${_rd}git update-ref -d $_r && "; done
    printf '%s\n' "When none of it is wanted any more: ${_rd}rm -rf $1"
}
