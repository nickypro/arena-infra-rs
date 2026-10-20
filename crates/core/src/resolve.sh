# arena resolve: finish an operation left in progress in one pod's checkout once nothing in it has
# changed for a while — what `pods backup` runs on a pod whose backup said BUSY, after copying it.
#
#   sh -c "$THIS_SCRIPT" arena-resolve <repo> <quiet secs> <op> <print> <hours> <copy> <max file bytes>
#
# Rendered and parsed by arena_core::resolve (resolve_command / parse_resolve). <op> (`OP`'s
# `<what> <mtime> <commit>`) and <print> (`PRINT`) are what the look before the copy saw: the
# checkout is looked at again first, and if anything changed since — or anything in it is younger
# than <quiet secs>, or someone is at work in git there, or the state is unusual — nothing is
# touched. Then each conflicted path is decided, nothing changed yet:
#   - a regular text file with no conflict marker left (at the path's `conflict-marker-size`),
#     both sides there: resolved by hand — kept as it is on disk;
#   - one whose markers are still there but which isn't what the merge left (`as_left`), or which
#     can't be compared with it (a marker's label gone): being, or maybe being, resolved by hand —
#     the whole operation is left alone, and said;
#   - else the side whose last change to that path is the more recent (`git log -1 --format=%ct
#     <side> -- <path>`, HEAD's against the operation's other side; a tie keeps HEAD's) — for a
#     text file per hunk: `git merge-file --ours|--theirs` keeps the clean hunks of both sides and
#     takes that side of each conflict — but a notebook (`*.ipynb`) that isn't valid JSON merged so
#     (or can't be checked) whole; for a binary file, a symlink or a modify/delete, the whole side,
#     removed when that side deleted it. A notebook resolved by hand that isn't valid JSON leaves
#     the operation alone, said.
# A file over <max file bytes> (the backup's git limit) anywhere in that is never committed: left
# alone, said — nor, for a merge or a cherry-pick (whose commit takes the whole index), any path
# already staged: a blob over that limit, or a new gitlink. The state before is kept first in
# <git dir>/arena-resolved/<UTC stamp>/ (the conflicted files as they were, git's own state files,
# the old HEAD, the index, a README), then the state is read again — the operation and its commit,
# HEAD, the conflicts, the index, every file — and the pod looked at again for anyone at work
# (every condition of the first look: a terminal used, a git command running, …), and if any of it
# changed, nothing is touched. Then the paths are staged; for a merge or a cherry-pick every staged
# path is checked again, and each staged regular file that is neither HEAD's version nor the other
# side's (what the participant made) has its keys redacted in the index as the backup redacts them
# (the file on disk keeps its keys); and — HEAD and the operation checked once more, and no
# conflict marker staged — the operation is
# finished: a merge, a merge commit; a cherry-pick, `--continue`; a `git stash pop`'s conflicts,
# added (the stash entry kept) — in the repo's own identity. A revert is never finished: its other
# side is the commit it undoes, always older, so "the newer side" would undo nothing. <hours> and
# <copy> (today's copy, on the backup machine) are for the words. Prints, on stdout:
#   REFUSED <why>            nothing touched (exit 7)
#   SAVED <dir>              where the state before was kept
#   NEWER <side> <how> <HEAD's time> <the other side's time> <path>   taken from the newer side
#                            (<side> ours|theirs; <how> hunks: that side of each conflict, the
#                            clean hunks of both kept; file: the whole file; notjson, nocheck: the
#                            whole notebook, since merged per hunk it wasn't valid JSON / no
#                            python3 or jq was there to tell); the times are each side's last
#                            change to the path
#   GONE <side> <HEAD's time> <the other side's time> <path>   removed: the newer side deleted it
#   HAND <path>              kept as it is on disk (resolved by hand) and added
#   REDACTED <n> <path>      staged with its n key(s) redacted; the file on disk keeps them
#   RESOLVED <what>          finished (exit 0)
#   FAILED <step>            a step failed midway (exit 8): SAVED has the state before
# ARENA_TEST_HOOK_BEFORE_CHANGE and ARENA_TEST_HOOK_BEFORE_FINISH, commands run right before the
# re-reads, are set by tests only.
set -u
REPO=$1
SECS=$2
WANT_OP=$3
WANT_PRINT=$4
HOURS=$5
COPY=$6
MAX=$7
cd "$REPO" || exit 2
cd "$(git rev-parse --show-toplevel)" || exit 2
TOP=$(pwd)
G=$(git rev-parse --absolute-git-dir) || exit 2
# Until the changes start, no git run here may write the participant's index (not even the stat
# refresh a porcelain command does in passing): the re-read below compares it.
export GIT_OPTIONAL_LOCKS=0
# @ARENA_BUSY_OP_SH@
# @ARENA_LOOK_SH@
REVERT_WORDS=@ARENA_REVERT_WORDS@
T="$G/arena-resolve.$$"
trap 'rm -rf "$T".*' EXIT
# @ARENA_REDACT_SH@
refuse() { echo "REFUSED $*"; exit 7; }
fail() { echo "FAILED $*"; exit 8; }
LOOKED=$(arena_look)
# The value on line $2 of look $1 (its first such line); `val`: of the first look.
val_in() { printf '%s\n' "$1" | sed -n "s/^$2 //p" | head -n 1; }
val() { val_in "$LOOKED" "$1"; }

# --- The same, and quiet ---------------------------------------------------------------------
set -- $(val OP)
what=${1:-none}
case $what in
    merge | cherry-pick | conflicts) ;;
    none) refuse "changed: nothing is in progress any more" ;;
    revert) refuse "$REVERT_WORDS" ;;
    am) refuse "a git am is in progress: never finished automatically" ;;
    sequence) refuse "a cherry-pick or revert sequence is in progress: never finished automatically" ;;
    *) refuse "a $what is in progress: never finished automatically" ;;
esac
[ $# -eq 4 ] || refuse "its state couldn't be read"
mt=$2
head=$3
seq=$4
[ "$what $mt $head" = "$WANT_OP" ] || refuse "changed: not the $what seen before the copy"
[ "$(val PRINT)" = "$WANT_PRINT" ] || refuse "changed: files changed since the copy"
[ "$seq" = 0 ] || refuse "a cherry-pick or revert sequence: never finished automatically"
# Old enough (or not there): at least <quiet secs> before the look's clock ($now), never in its
# future.
old() { [ -z "$1" ] || { [ "$1" -le "$now" ] && [ $((now - $1)) -ge "$SECS" ]; }; }
# Why look $1 says someone may be at work here, in words — on that look's own clock: the operation
# (started at $mt) younger than <quiet secs>, a file git lists changed since, something staged
# since (the index), git's index locked, a terminal login typed or any terminal used since, a git
# command (or an editor one waits on) running; nothing: quiet. Asked of the first look, and again —
# every condition — of the one right before the changes (`again`): a participant back at a
# terminal while the plan was made, typing into an editor's buffer, touches no file (review
# ai-review codex #6).
unquiet() {
    _q=$1
    now=$(val_in "$_q" NOW)
    if ! old "$mt"; then echo "recent: the $what started less than ${HOURS}h ago"; return; fi
    set -- $(val_in "$_q" NEWEST)
    if ! old "${1:-0}"; then echo "recent: a file changed less than ${HOURS}h ago"; return; fi
    if ! old "$(val_in "$_q" INDEX)"; then echo "recent: git's index changed less than ${HOURS}h ago (something was staged)"; return; fi
    if [ -n "$(val_in "$_q" LOCKED)" ]; then echo "busy: git's index is locked (.git/index.lock): a git command is at work, or died holding it"; return; fi
    if ! old "$(val_in "$_q" INPUT)"; then echo "recent: a terminal login typed less than ${HOURS}h ago"; return; fi
    if ! old "$(val_in "$_q" TTY)"; then echo "recent: a terminal was used less than ${HOURS}h ago"; return; fi
    set -- $(val_in "$_q" PROC)
    if [ $# -ne 0 ]; then echo "busy: a git command, or an editor one waits on, is running (pid $1: ${2:-?})"; fi
}
why=$(unquiet "$LOOKED")
[ -z "$why" ] || refuse "$why"
[ -n "$(git config user.name)" ] && [ -n "$(git config user.email)" ] || refuse "no git identity configured in the repo"

# --- What is in progress, as it is now ------------------------------------------------------
case $what in
    merge) other=MERGE_HEAD ;;
    cherry-pick) other=CHERRY_PICK_HEAD ;;
    *) other='the stash' ;;
esac
[ "$head" != - ] || refuse "conflicts with no stash entry they came from"
if [ "$what" = merge ] && [ "$(wc -l < "$G/MERGE_HEAD")" -ne 1 ]; then refuse "a merge of several branches"; fi
HEAD0=$(git rev-parse -q --verify HEAD) || refuse "no HEAD commit"
ours_tip=$(git log -1 --no-show-signature --format=%ct HEAD 2>/dev/null) || refuse "no HEAD commit"
theirs_tip=$(git log -1 --no-show-signature --format=%ct "$head" 2>/dev/null) || refuse "$other's commit is missing"
INDEX0=$(stat -c '%y %s' -- "$G/index" 2>/dev/null) || refuse "git's index couldn't be read"
git ls-files -u -z > "$T.u" || refuse "the conflicts couldn't be listed"
[ "$(tr -d -c '\n' < "$T.u" | wc -c)" -eq 0 ] || refuse "a conflicted path has a newline in its name"
if tr '\0' '\n' < "$T.u" | grep -q '^160000 '; then refuse "a submodule is in conflict"; fi

# The path's conflict-marker size: its `conflict-marker-size` attribute, else git's 7.
msize() {
    _m=$(git check-attr conflict-marker-size -- "$1" 2>/dev/null | sed -n 's/.*: conflict-marker-size: \([1-9][0-9]\{0,2\}\)$/\1/p')
    echo "${_m:-7}"
}
# Whether file $1 has a conflict marker line of size $2 (`<` or `>` that many times, then a space
# or the line's end) — `=======` alone is no marker: Markdown's setext headings use it.
markers() { LC_ALL=C grep -aqE "^(<{$2}|>{$2})( |\$)" -- "$1"; }
# The first path staged with conflict markers in it (`git add` of a file still in conflict): a
# marker line (at the path's size) staged against HEAD that the other side's version doesn't have
# either — a line like that the course itself has is no marker.
staged_markers() {
    git diff --cached --name-only -z HEAD -- > "$T.sn" || return 0
    _min=$(git check-attr -z --stdin conflict-marker-size < "$T.sn" 2>/dev/null | tr '\0' '\n' \
        | awk 'NR % 3 == 0 && $0 ~ /^[1-9][0-9]*$/ && $0 + 0 < m { m = $0 + 0 } END { print m }' m=7)
    git diff --cached -U0 --no-color --no-ext-diff --no-textconv HEAD -- 2>/dev/null \
        | LC_ALL=C grep -aqE "^\\+(<{$_min,}|>{$_min,})" || return 0
    tr '\0' '\n' < "$T.sn" | while IFS= read -r _p; do
        _n=$(msize "$_p")
        _re="^\\+(<{$_n}|>{$_n})( |\$)"
        git diff --cached -U0 --no-color --no-ext-diff --no-textconv HEAD -- ":(literal)$_p" | LC_ALL=C grep -aE "$_re" | LC_ALL=C sort -u > "$T.sm"
        [ -s "$T.sm" ] || continue
        git diff --cached -U0 --no-color --no-ext-diff --no-textconv "$head" -- ":(literal)$_p" | LC_ALL=C grep -aE "$_re" | LC_ALL=C sort -u > "$T.sm2"
        if [ -n "$(LC_ALL=C comm -12 "$T.sm" "$T.sm2" | head -n 1)" ]; then printf '%s\n' "$_p"; break; fi
    done
}
p=$(staged_markers)
[ -z "$p" ] || refuse "conflict markers were added in $p (staged with them still in it): resolve it, or abort, on the pod"

# The commit takes the whole index, not only the paths in conflict: a file the participant
# resolved, or added, and `git add`ed before this ran goes in too (review ai-review codex #4,
# claude #4). So every path staged against HEAD is checked as the backup checks what it commits —
# `unfit` says the first that can't go in, in words: a blob over <max file bytes> (GitHub refuses
# the push, and every later backup is stuck), or a gitlink new to HEAD (a nested repo's commit id,
# none of its files: the backup takes those back out of the index). Nothing: all fit. It leaves
# the staged changes in $T.st, one per line: `<HEAD's mode> <mode> <blob> <status><TAB><path>`.
unfit() {
    git diff-index --cached --no-renames -z HEAD -- > "$T.sz" || { echo "the staged changes couldn't be listed"; return; }
    if [ "$(tr -d -c '\n' < "$T.sz" | wc -c)" -ne 0 ]; then echo "a staged path has a newline in its name"; return; fi
    tr '\0' '\n' < "$T.sz" | awk 'NR % 2 == 1 { split(substr($0, 2), m, " "); next } { print m[1] " " m[2] " " m[4] " " m[5] "\t" $0 }' > "$T.st"
    _g=$(awk '{ i = index($0, "\t"); split(substr($0, 1, i - 1), m, " ") } m[2] == "160000" && m[1] != "160000" { print substr($0, i + 1); exit }' "$T.st")
    if [ -n "$_g" ]; then
        echo "$_g is staged as a nested repo (a gitlink: its commit id, none of its files): never committed — unstage it, or abort, on the pod"
        return
    fi
    awk '{ split($0, m, " ") } m[2] == "100644" || m[2] == "100755" || m[2] == "120000" { print m[3] }' "$T.st" | sort -u \
        | git cat-file --batch-check='%(objectname) %(objectsize)' > "$T.ss" || { echo "the staged files' sizes couldn't be read"; return; }
    awk -v max="$MAX" 'NR == FNR { s[$1] = $2; next } { i = index($0, "\t"); split(substr($0, 1, i - 1), m, " ") }
        (m[3] in s) && s[m[3]] + 0 > max + 0 {
            printf "%s is staged at %s bytes, over git'"'"'s %s-byte limit: never committed — unstage it, or abort, on the pod\n", substr($0, i + 1), s[m[3]], max
            exit
        }' "$T.ss" "$T.st"
}
# A stash pop's conflicts make no commit here: the backup after commits them, as it commits anything.
case $what in
    merge | cherry-pick)
        why=$(unfit)
        [ -z "$why" ] || refuse "$why"
        ;;
esac

# --- Each conflicted path's way (nothing touched yet) ---------------------------------------
TAB=$(printf '\t')
# One line per path: its stages (e.g. 123, 23), each stage's mode and blob (- for none), the path.
tr '\0' '\n' < "$T.u" | awk '
    { i = index($0, "\t"); split(substr($0, 1, i - 1), m, " "); p = substr($0, i + 1)
      if (!(p in st)) { order[n++] = p; st[p] = "" }
      st[p] = st[p] m[3]; mode[p, m[3]] = m[1]; blob[p, m[3]] = m[2] }
    END { for (k = 0; k < n; k++) { p = order[k]; printf "%s", st[p]
            for (s = 1; s <= 3; s++) printf "\t%s\t%s", ((p, s) in mode) ? mode[p, s] : "-", ((p, s) in blob) ? blob[p, s] : "-"
            printf "\t%s\n", p } }' > "$T.p"
# git's binary test: a NUL in the first 8000 bytes (of a file, of a blob).
binary_file() { [ "$(head -c 8000 -- "$1" | tr -d -c '\000' | wc -c)" -ne 0 ]; }
binary_blob() { [ "$(git cat-file blob "$1" | head -c 8000 | tr -d -c '\000' | wc -c)" -ne 0 ]; }
# Whether file $1 is one JSON document, as Jupyter needs a notebook to be: `yes`/`no` by python3's
# json (the images have it), else jq's (one value, nothing after it); `none` when neither is here.
# ARENA_TEST_JSON_TOOLS, set by tests only, names the tools to try instead.
json_ok() {
    for _j in ${ARENA_TEST_JSON_TOOLS-python3 jq}; do
        command -v "$_j" > /dev/null 2>&1 || continue
        case $_j in
            python3) python3 -c 'import json, sys; json.load(open(sys.argv[1], "rb"))' "$1" ;;
            jq) jq -e -s 'length == 1' "$1" ;;
            *) false ;;
        esac > /dev/null 2>&1 && echo yes || echo no
        return 0
    done
    echo none
}
# Merged as text: not `-merge`/`merge=binary`, and no filter (an LFS pointer is no text to merge).
text_attrs() {
    _a=$(git check-attr merge filter -- "$1") || return 1
    if printf '%s\n' "$_a" | grep -qE ': merge: (unset|binary)$'; then return 1; fi
    printf '%s\n' "$_a" | grep -qE ': filter: (unspecified|unset)$'
}
# When side $1 last changed path $2: its last commit touching it. A side whose history has no such
# path — a rename: its commits name the file otherwise, and following that is no cheap lookup —
# counts from its own commit, $3 (the tip's time).
changed() {
    _c=$(git log -1 --no-show-signature --format=%ct "$1" -- ":(literal)$2" 2>/dev/null) || _c=
    echo "${_c:-$3}"
}
# The stages' blobs ($1 base or -, $2 ours, $3 theirs) as files, for git merge-file: an add/add's
# missing base is empty.
stages() {
    if [ "$1" = - ]; then : > "$T.s1"; else git cat-file blob "$1" > "$T.s1" || return 1; fi
    git cat-file blob "$2" > "$T.s2" && git cat-file blob "$3" > "$T.s3"
}
# File $1 with the label text on its marker lines (size $2) left out.
unlabelled() { LC_ALL=C sed -E "s/^(<{$2}|\\|{$2}|>{$2})( .*)?\$/\\1/" -- "$1"; }
# as_left <path> <marker size> <base blob|-> <ours blob> <theirs blob>: whether a file still in
# conflict is as the operation left it, markers and all (`same`), was begun by hand (`begun`), or
# can't be told (`unknown`). git's conflicted output is made again from the stages with `git
# merge-file`, given the labels on the file's own `<<<<<<<`/`>>>>>>>` lines, the path's marker
# size, and `--diff3`/`--zdiff3` when the file has base sections — the same bytes: as left.
# Different bytes get a second opinion: `merge-file` joins conflicts that only lines without
# letters or digits separate, where the merge itself (and `checkout --conflict`, which merges as it
# does) doesn't — so the merge is made again that way too, into a scratch tree through a copy of
# the index (nothing of the participant's written), its labels set aside. When there are no labels
# to read (a marker line deleted, or its label) or a stage is binary, it can't be made again, so
# nothing says the file is as git left it: `unknown`, and the operation stays BUSY. Never judged
# by the file's time instead: a participant who starts at once, deletes a marker and edits the
# hunk would have that edit overwritten (review ai-review codex #5).
as_left() {
    _p=$1 _n=$2
    _l1=$(LC_ALL=C grep -a -m1 -E "^<{$_n} " -- "$_p" | cut -c"$((_n + 2))"-)
    _l3=$(LC_ALL=C grep -a -m1 -E "^>{$_n} " -- "$_p" | cut -c"$((_n + 2))"-)
    _l2=$(LC_ALL=C grep -a -m1 -E "^\\|{$_n} " -- "$_p" | cut -c"$((_n + 2))"-)
    if LC_ALL=C grep -aqE "^\\|{$_n}( |\$)" -- "$_p"; then _styles='diff3 zdiff3'; else _styles=merge; fi
    if [ -z "$_l1" ] || [ -z "$_l3" ] || binary_blob "$4" || binary_blob "$5" || { [ "$3" != - ] && binary_blob "$3"; }; then
        echo unknown
        return 0
    fi
    for _s in $_styles; do
        if [ "$_s" = merge ]; then _o=; else _o=--$_s; fi
        git merge-file -p $_o --marker-size="$_n" -L "$_l1" -L "$_l2" -L "$_l3" "$T.s2" "$T.s1" "$T.s3" > "$T.g" 2>/dev/null
        if [ $? -lt 128 ] && cmp -s "$T.g" "$_p"; then echo same; return 0; fi
    done
    unlabelled "$_p" "$_n" > "$T.n1"
    for _s in $_styles; do
        rm -rf "$T.wt" && mkdir "$T.wt" && cp "$G/index" "$T.idx" || break
        if GIT_INDEX_FILE="$T.idx" git --work-tree="$T.wt" checkout --conflict="$_s" -- ":(literal)$_p" > /dev/null 2>&1 \
            && unlabelled "$T.wt/$_p" "$_n" > "$T.n2" && cmp -s "$T.n1" "$T.n2"; then
            echo same
            return 0
        fi
    done
    echo begun
}
i=0
: > "$T.plan"
while IFS="$TAB" read -r st m1 b1 m2 b2 m3 b3 p; do
    i=$((i + 1))
    if [ -d "$p" ] && [ ! -L "$p" ]; then refuse "a directory is where $p is in conflict"; fi
    if [ "$what" = conflicts ]; then
        # The other side must be that stash entry's (`git stash pop` keeps it on a conflict).
        want=$(git rev-parse -q --verify "$head:$p" 2>/dev/null) || want=-
        [ "$want" = "$b3" ] || refuse "conflicts that aren't the newest stash entry's ($p)"
    fi
    ot=$(changed HEAD "$p" "$ours_tip")
    tt=$(changed "$head" "$p" "$theirs_tip")
    if [ "$tt" -gt "$ot" ]; then side=theirs; s=3; wb=$b3; wm=$m3; else side=ours; s=2; wb=$b2; wm=$m2; fi
    n=$(msize "$p")
    act=
    # A regular text file on disk, both sides there: resolved by hand, or still in conflict.
    if [ -f "$p" ] && [ ! -L "$p" ] && [ "$b2" != - ] && [ "$b3" != - ] && ! binary_file "$p" && text_attrs "$p"; then
        if ! markers "$p" "$n"; then
            act=hand
        else
            stages "$b1" "$b2" "$b3" || refuse "$p's sides couldn't be read"
            case $(as_left "$p" "$n" "$b1" "$b2" "$b3") in
                same) ;;
                begun) refuse "you started resolving $p by hand — finish it (and git add it) or abort, on the pod" ;;
                *) refuse "can't tell whether you started resolving $p by hand (a marker line or its label is gone, or a side is binary: git's conflicted output can't be made again to compare) — finish it (and git add it) or abort, on the pod" ;;
            esac
            # Per hunk when both sides (and the base) are regular text files.
            case "$m1 $m2 $m3" in
                *120000* | *160000*) ;;
                *) if ! binary_blob "$b2" && ! binary_blob "$b3" && { [ "$b1" = - ] || ! binary_blob "$b1"; }; then act=hunks; fi ;;
            esac
        fi
    fi
    if [ -z "$act" ]; then
        case $st in
            *"$s"*) act=file ;;
            *) act=gone ;;
        esac
    fi
    case $act in
        hunks)
            git merge-file -p "--$side" --marker-size="$n" "$T.s2" "$T.s1" "$T.s3" > "$T.r$i" 2>/dev/null \
                || refuse "$p couldn't be merged per hunk"
            # A notebook merged per hunk must still be one JSON document, or Jupyter won't open it:
            # one side's conflict next to the other side's clean hunk can leave a bracket or a comma
            # wrong (review ai-review claude #3). If it doesn't parse — or nothing here can tell —
            # the whole notebook comes from the newer side instead, and the words say why.
            case $p in
                *.ipynb)
                    case $(json_ok "$T.r$i") in
                        yes) ;;
                        no) act=notjson ;;
                        *) act=nocheck ;;
                    esac
                    ;;
            esac
            ;;
        hand)
            # A notebook resolved by hand that doesn't parse is the participant's to fix: committed,
            # Jupyter would refuse it (with no parser here, it's taken as it is).
            case $p in
                *.ipynb) [ "$(json_ok "$p")" != no ] || refuse "$p isn't valid JSON: Jupyter can't open it as it is — fix it (and git add it) or abort, on the pod" ;;
            esac
            ;;
    esac
    case $act in
        hunks) size=$(wc -c < "$T.r$i") ;;
        hand) size=$(stat -c %s -- "$p") ;;
        file | notjson | nocheck) size=$(git cat-file -s "$wb") ;;
        *) size=0 ;;
    esac
    [ "$size" -le "$MAX" ] || refuse "$p would be $size bytes, over git's $MAX-byte limit: never committed — resolve it on the pod"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$act" "$side" "$ot" "$tt" "$i" "$wm" "$p" >> "$T.plan"
done < "$T.p"

# --- The words -------------------------------------------------------------------------------
# A time as the words give it: UTC, to the minute (`2026-10-20 13:05Z`).
day() { date -u -d "@$1" '+%Y-%m-%d %H:%MZ'; }
# The operation's other side, as a participant reading `git log` knows it.
case $what in
    merge) them="the merged branch" ref=MERGE_HEAD ;;
    cherry-pick) them="the picked commit" ref=CHERRY_PICK_HEAD ;;
    *) them="the stash" ref='stash@{0}' ;;
esac
# Each path's way, one line each — which side won, and both sides' last change to it: what the
# commit message, and the README of the state before, list.
choices() {
    while IFS="$TAB" read -r act side ot tt i wm p; do
        dates="last changed: yours (HEAD) $(day "$ot"), $them ($ref) $(day "$tt")"
        if [ "$side" = ours ]; then won="yours won"; its=your; else won="$them won"; its=its; fi
        case $act in
            hand) echo "- $p: kept as you resolved it by hand" ;;
            hunks) echo "- $p: $won ($its side of each conflict; the other changes of both sides kept) — $dates" ;;
            file) echo "- $p: $won ($its whole version) — $dates" ;;
            notjson) echo "- $p: $won ($its whole version: merged per hunk, the notebook wasn't valid JSON) — $dates" ;;
            nocheck) echo "- $p: $won ($its whole version: no python3 or jq here to check a notebook merged per hunk) — $dates" ;;
            gone) echo "- $p: $won (removed: deleted there) — $dates" ;;
        esac
    done < "$T.plan"
}
choices > "$T.words"
RULE="each conflicted file from the side whose last change to it is the more recent (a tie keeps yours), a file you resolved by hand kept as it is"

# --- The state before, kept first ------------------------------------------------------------
S="$G/arena-resolved/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$G/arena-resolved" || refuse "the state before couldn't be kept"
if ! mkdir "$S" 2>/dev/null; then
    S="$S-$$"
    mkdir "$S" || refuse "the state before couldn't be kept"
fi
unkept() {
    rm -rf "$S"
    rmdir "$G/arena-resolved" 2> /dev/null
    refuse "$@"
}
for f in MERGE_HEAD MERGE_MSG MERGE_MODE CHERRY_PICK_HEAD ORIG_HEAD AUTO_MERGE; do
    if [ -e "$G/$f" ]; then cp -p -- "$G/$f" "$S/$f" || unkept "the state before couldn't be kept"; fi
done
printf '%s\n' "$HEAD0" > "$S/HEAD" && cp -p -- "$G/index" "$S/index" && tr '\0' '\n' < "$T.u" > "$S/unmerged" \
    && mkdir "$S/files" || unkept "the state before couldn't be kept"
if ! cut -f7- "$T.plan" | while IFS= read -r p; do
    if [ -e "$p" ] || [ -L "$p" ]; then
        case $p in */*) mkdir -p "$S/files/${p%/*}" || exit 1 ;; esac
        cp -P -p -- "$p" "$S/files/$p" || exit 1
    fi
done; then
    unkept "the state before couldn't be kept"
fi
case $what in
    merge) redo="git reset --hard $HEAD0 && git merge $head" ;;
    cherry-pick) redo="git reset --hard $HEAD0 && git cherry-pick $head" ;;
    *) redo="git reset --hard $HEAD0 && git stash apply $head" ;;
esac
{
    echo "arena finished a $what left in progress for ${HOURS}h+ on $(date -u '+%Y-%m-%d %H:%M') UTC:"
    echo "$RULE."
    cat "$T.words"
    echo
    echo "The state before, here:"
    echo "  files/      the conflicted files as they were on disk (markers and all)"
    echo "  HEAD        the commit the $what was on: $HEAD0"
    echo "  index       git's index as it was, the conflicts in it; unmerged lists them"
    echo "  MERGE_HEAD, MERGE_MSG, CHERRY_PICK_HEAD, ORIG_HEAD …: git's own files as they were (those there were)"
    echo "To see a file as it was: cat files/<path>"
    echo "To do it again yourself (commit or copy anything newer first: this discards it): $redo"
    echo "Today's copy on the backup machine: $COPY (a later copy that day replaces it)."
} > "$S/README" || unkept "the state before couldn't be kept"
echo "SAVED $S"

# --- Nothing changed since? ------------------------------------------------------------------
# The changes start here. Right before, the state the look saw is read again — the operation and
# its commit, HEAD, the conflicts, the index, every file, no index lock — and the pod looked at
# again for anyone at work, every condition of the first look (`unquiet`: a terminal used, a git
# command running, …); nothing is touched if any of it changed: the participant may have aborted
# the merge and started a rebase meanwhile, and the plan made for the merge must never be applied
# to that — or come back to a terminal and be typing into a buffer not saved yet.
if [ -n "${ARENA_TEST_HOOK_BEFORE_CHANGE:-}" ]; then (sh -c "$ARENA_TEST_HOOK_BEFORE_CHANGE") > /dev/null 2>&1; fi
again() {
    _l=$(arena_look)
    if [ "$(val_in "$_l" OP)" != "$what $mt $head $seq" ]; then echo "changed: the $what isn't the one planned for any more"; return; fi
    if [ "$(val_in "$_l" PRINT)" != "$WANT_PRINT" ]; then echo "changed: files changed"; return; fi
    if [ "$(git rev-parse -q --verify HEAD)" != "$HEAD0" ]; then echo "changed: HEAD moved"; return; fi
    if [ -e "$G/index.lock" ]; then echo "changed: git's index is locked"; return; fi
    if [ "$(stat -c '%y %s' -- "$G/index" 2>/dev/null)" != "$INDEX0" ]; then echo "changed: git's index changed"; return; fi
    if ! git ls-files -u -z | cmp -s - "$T.u"; then echo "changed: the conflicts changed"; return; fi
    unquiet "$_l"
}
why=$(again)
[ -z "$why" ] || unkept "$why while the state before was kept"
unset GIT_OPTIONAL_LOCKS

# --- Resolve ---------------------------------------------------------------------------------
while IFS="$TAB" read -r act side ot tt i wm p; do
    case $act in
        hand)
            git add -- ":(literal)$p" || fail "git add $p"
            echo "HAND $p"
            ;;
        gone)
            git rm -q -f -- ":(literal)$p" > /dev/null || fail "git rm $p"
            echo "GONE $side $ot $tt $p"
            ;;
        file | notjson | nocheck)
            git checkout "--$side" -- ":(literal)$p" || fail "git checkout --$side $p"
            git add -- ":(literal)$p" || fail "git add $p"
            echo "NEWER $side $act $ot $tt $p"
            ;;
        hunks)
            cat "$T.r$i" > "$p" || fail "writing $p"
            # The winning side's mode (an executable bit either side may have changed).
            if [ "$wm" = 100755 ]; then chmod a+x -- "$p"; else chmod a-x -- "$p"; fi || fail "chmod $p"
            git add -- ":(literal)$p" || fail "git add $p"
            echo "NEWER $side hunks $ot $tt $p"
            ;;
    esac
done < "$T.plan"
[ -z "$(git ls-files -u | head -n 1)" ] || fail "conflicts were left"

# --- What the commit takes: every staged path, keys redacted ---------------------------------
# Every path staged against HEAD fits (`unfit`, as before anything was touched — now with what was
# staged above). Then each regular file the commit takes whose staged version is neither HEAD's
# nor the operation's other side's (MERGE_HEAD's, CHERRY_PICK_HEAD's) — content the participant
# made: a file resolved by hand, now or `git add`ed before this ran, one merged per hunk, one
# added mid-merge — goes in with the keys redact.sh knows redacted, in the index only: the file on
# disk keeps them, so the participant's code still runs. A key in it never enters a commit (the
# backup after this would redact what it commits, but this commit is made first). The course's or
# the picked commit's own content — a file only that side changed, one taken whole from it — goes
# in as it was published, and HEAD's is committed already. A stash pop's conflicts make no commit
# here: the backup after commits them, redacted.
case $what in
    merge | cherry-pick)
        why=$(unfit)
        [ -z "$why" ] || fail "$why"
        git diff-index --cached --no-renames --name-only -z "$head" -- > "$T.soz" || fail "listing the staged changes"
        tr '\0' '\n' < "$T.soz" > "$T.so"
        # Each such file: `<mode> <blob><TAB><path>`.
        awk 'NR == FNR { o[$0] = 1; next } { i = index($0, "\t"); p = substr($0, i + 1); split(substr($0, 1, i - 1), m, " ") }
            (m[2] == "100644" || m[2] == "100755") && (p in o) { print m[2] " " m[3] "\t" p }' "$T.so" "$T.st" > "$T.rc"
        (
            set -e
            cut -f1 "$T.rc" | cut -d' ' -f2 | sort -u > "$T.rcb"
            while read -r b; do redact_blob "$b"; done < "$T.rcb" > "$T.map"
            # Those paths alone (the same blob elsewhere, the course's, stays as it is).
            awk 'NR == FNR { r[$1] = $2; next } { i = index($0, "\t"); split(substr($0, 1, i - 1), m, " ") }
                m[2] in r { print m[1] " " r[m[2]] " 0\t" substr($0, i + 1) }' "$T.map" "$T.rc" > "$T.upd"
            tr '\n' '\0' < "$T.upd" | git update-index -z --index-info
        ) || fail "redacting keys in the staged files"
        awk 'NR == FNR { n[$2] = $3; next } { i = index($0, "\t"); split(substr($0, 1, i - 1), m, " "); print "REDACTED " n[m[2]] " " substr($0, i + 1) }' "$T.map" "$T.upd"
        ;;
esac
p=$(staged_markers)
[ -z "$p" ] || fail "conflict markers were added in $p"

# --- Finish ----------------------------------------------------------------------------------
# Once more, right before the commit: still this operation, on this HEAD, and git's index free.
if [ -n "${ARENA_TEST_HOOK_BEFORE_FINISH:-}" ]; then (sh -c "$ARENA_TEST_HOOK_BEFORE_FINISH") > /dev/null 2>&1; fi
kept="the state before is in ${S#"$TOP"/}/"
case $what in
    merge | cherry-pick)
        cur=$(busy_op "$G")
        if [ "${cur%% *}" != "$what" ] || [ "$(git rev-parse -q --verify "$other" 2>/dev/null)" != "$head" ]; then
            fail "changed before the commit: the $what isn't the one resolved any more — $kept"
        fi
        [ "$(git rev-parse -q --verify HEAD)" = "$HEAD0" ] || fail "changed before the commit: HEAD moved — $kept"
        [ ! -e "$G/index.lock" ] || fail "changed before the commit: git's index is locked — $kept"
        ;;
esac
where="The state before is in ${S#"$TOP"/}/ on this machine (its README says how to see it or do this again yourself); today's copy: $COPY."
case $what in
    merge)
        {
            echo "arena: finished a merge left in progress for ${HOURS}h+"
            echo
            echo "Nothing in this checkout changed for ${HOURS}h+ while a merge was in progress, so arena finished it: $RULE."
            cat "$T.words"
            echo
            echo "$where"
            if [ -s "$G/MERGE_MSG" ]; then
                echo
                grep -v '^#' "$G/MERGE_MSG"
            fi
        } > "$T.msg"
        GIT_EDITOR=true git commit -q --no-edit --cleanup=strip -F "$T.msg" || fail "git commit"
        ;;
    cherry-pick)
        {
            echo
            echo "arena: finished this cherry-pick, left in progress for ${HOURS}h+: $RULE."
            cat "$T.words"
            echo
            echo "$where"
        } >> "$G/MERGE_MSG"
        if git diff --cached --quiet HEAD --; then
            # Nothing left to change (the newer side was HEAD's everywhere): `--continue` refuses an
            # empty commit, so the commit it would make, empty — git's own advice.
            GIT_EDITOR=true git commit -q --allow-empty --no-edit --cleanup=strip || fail "git commit --allow-empty"
        else
            GIT_EDITOR=true git cherry-pick --continue > /dev/null || fail "git cherry-pick --continue"
        fi
        ;;
esac
case $what in
    merge) [ ! -e "$G/MERGE_HEAD" ] ;;
    cherry-pick) [ ! -e "$G/CHERRY_PICK_HEAD" ] ;;
esac || fail "the $what is still in progress"
echo "RESOLVED $what"
