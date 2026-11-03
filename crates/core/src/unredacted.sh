# arena set-branch: keep the participant's real keys across a branch switch.
#
# Rendered after redact.sh by arena_core::backup::checkout_command, which runs these around the
# switch, from the repo's top. The backup commits a file holding a key redacted and leaves the
# file on disk as it is (its real key in it), so the file reads as modified for good — and a
# switch to a branch with another version of it would refuse (`would be overwritten`) or, with
# --hard, throw the key away, which no commit has. So, before the switch, each such file is set
# aside — copied into a folder of this switch's own, $1/<UTC time>-<pid>/<path>, with the branch
# it was on in $1/<UTC time>-<pid>.branch (in .git: the rsync tiers back it up, no push carries
# it) — and put back to the version git has. A copy is never overwritten: the same path set aside
# again (another day, another branch, another key) is another copy. After the switch, a copy made
# on the branch now checked out goes back where the path holds exactly the version the set-aside
# left there. Prints, on stdout:
#   UNREDACTED_RESTORED <path>   its real keys are back in the file
#   UNREDACTED_KEPT <path>       its real keys are still set aside: this switch's own (the branch
#                                has its own version), or one of this branch's that couldn't go back
#                                (the file changed since) — every such one, every time
#
# However a switch ends — a TERM (the bound's; the EXIT trap then restores) or a KILL anywhere (a
# later switch's restore, or init-branches', recovers) — no version of a file is ever lost, and a
# key is only ever in its file or in a copy that is said (KEPT):
# - the run's branch is written before its first copy (a run is restored by its branch);
# - a copy is made beside the runs and renamed in: whole, or not there;
# - a file is only replaced through [`unredacted_place`]: in its own directory, entered once and
#   checked to be the work tree's (a link swapped in for it later is never followed), the live
#   file is first moved aside into the git dir — recorded with its path, so a KILL right then is
#   recovered — and the new one linked in only if what was moved aside is exactly what was
#   expected, bytes and mode, and nothing appeared in its place meanwhile; anything else goes
#   back (or, its place taken again, is kept as a copy, said).

# Each tracked file whose only difference from the index is keys the redaction knows: set aside.
# A file with any other uncommitted change is left alone — the switch treats it as it always did,
# a chmod included — and so is one that can't be copied, one under a symlinked directory, one
# whose directory isn't on the git dir's filesystem, and one that changes while it's set aside
# (each said on stderr): the loop never stops halfway. The file's git version is its redacted
# text — derived from the copy, and checked to be git's — so the index's stat data is all that's
# refreshed, which stages nothing.
unredacted_aside() {
    UR_RUN=$(date -u +%Y%m%dT%H%M%SZ)-$$
    UR_TOP=$(pwd -P)
    UR_BRANCH=$(git rev-parse --abbrev-ref HEAD) || UR_BRANCH=
    UR_MODES=$(git config --bool core.fileMode 2>/dev/null) || UR_MODES=true
    unredacted_settle "$1"
    git diff --name-only -z | tr '\0' '\n' | while IFS= read -r f; do
        if [ ! -f "$f" ] || [ -L "$f" ] || ! unredacted_in_place "$1" "$f"; then continue; fi
        e=$(git ls-files -s -- ":(literal)$f" | awk '$1 == "100644" || $1 == "100755" { print $1, $2 }')
        m=${e%% *}
        i=${e#* }
        if [ -z "$e" ] || [ "$(git hash-object -- "$f")" = "$i" ]; then continue; fi
        if [ -x "$f" ]; then x=100755; else x=100644; fi
        if [ "$UR_MODES" != false ] && [ "$m" != "$x" ]; then continue; fi
        if [ "$(LC_ALL=C sed -E "$REDACT_SED" < "$f" | git hash-object --path="$f" --stdin)" != "$i" ]; then continue; fi
        c=$1/$UR_RUN/$f
        if ! { mkdir -p -- "$1/$UR_RUN/$(dirname -- "$f")" \
            && { [ -f "$1/$UR_RUN.branch" ] || printf '%s\n' "$UR_BRANCH" > "$1/$UR_RUN.branch"; } \
            && cp -p -- "$f" "$1/$UR_RUN.part" && mv -f -- "$1/$UR_RUN.part" "$c" \
            && cp -p -- "$c" "$1/$UR_RUN.redacted" && LC_ALL=C sed -E "$REDACT_SED" < "$c" > "$1/$UR_RUN.redacted" \
            && [ "$(git hash-object --path="$f" -- "$1/$UR_RUN.redacted")" = "$i" ]; }; then
            rm -f -- "$c" "$1/$UR_RUN.redacted"
            echo "arena set-branch: couldn't set $f aside - left as it is" >&2
            continue
        fi
        if unredacted_place "$1" "$f" "$1/$UR_RUN.redacted" "$c" "$UR_RUN"; then r=0; else r=$?; fi
        case $r in
            0) ;;
            3) echo "arena set-branch: $f changed while it was set aside - it keeps its own version, and the one before is kept too" >&2 ;;
            *)
                rm -f -- "$c"
                echo "arena set-branch: couldn't set $f aside (it changed meanwhile, or its place moved) - left as it is" >&2
                ;;
        esac
        rm -f -- "$1/$UR_RUN.redacted"
    done
    git update-index -q --refresh > /dev/null 2>&1 || :
}

# Whether the work tree's $2 can be replaced from the folder $1 (in the git dir), as far as a look
# can tell: no directory on its way is a symlink, and its directory is on the git dir's
# filesystem (a link or rename there is one step; across filesystems `ln` refuses). Only a first
# look: [`unredacted_place`] is what holds while it acts.
unredacted_in_place() {
    _d=$(dirname -- "$2")
    _p=$_d
    while [ "$_p" != . ] && [ "$_p" != / ]; do
        if [ -L "$_p" ]; then return 1; fi
        _p=$(dirname -- "$_p")
    done
    [ -d "$_d" ] && [ "$(stat -c %d -- "$(dirname -- "$1")")" = "$(stat -c %d -- "$_d")" ]
}

# Replace the work tree's file $2 by $3 (in the folder $1, the git dir's), if it is now exactly
# $4 (bytes and mode); $5 is the run it's for. In $2's own directory, entered once (`cd -P`) and
# checked to be the work tree's: from there on only names in it are used, so a link swapped in for
# that directory is never followed. The live file is moved aside first ($1/$5.displaced, its path
# in $1/$5.displaced.path), so nothing it held is lost, and the new one hard-linked in, which never
# replaces a file that appeared meanwhile. 0: replaced. 1: not tried, nothing changed (it isn't
# where it was, or isn't a file). 2: left as it is (what was there wasn't $4, and went back; or the
# place was taken meanwhile, and that stays). 3: what was there wasn't $4, and its place was taken
# again before it could go back: it's kept as $1/$5/$2.displaced, said as KEPT.
unredacted_place() {
    _dir=$(dirname -- "$2")
    _want=$UR_TOP
    [ "$_dir" = . ] || _want=$UR_TOP/$_dir
    printf '%s\n' "$2" > "$1/$5.displaced.path" || return 1
    _r=0
    (
        _base=${2##*/}
        { cd -P -- "$_dir" 2>/dev/null && [ "$(pwd -P)" = "$_want" ]; } || exit 1
        { [ -f "$_base" ] && [ ! -L "$_base" ]; } || exit 1
        mv -f -- "$_base" "$1/$5.displaced" || exit 1
        if cmp -s -- "$1/$5.displaced" "$4" && [ "$(stat -c %a -- "$1/$5.displaced")" = "$(stat -c %a -- "$4")" ]; then
            ln -- "$3" "$_base" 2> /dev/null && r=0 || r=2
            rm -f -- "$1/$5.displaced"
            exit $r
        fi
        if ln -- "$1/$5.displaced" "$_base" 2> /dev/null; then
            rm -f -- "$1/$5.displaced"
            exit 2
        fi
        mkdir -p -- "$1/$5/$(dirname -- "$2")" && mv -f -- "$1/$5.displaced" "$1/$5/$2.displaced"
        exit 3
    ) || _r=$?
    rm -f -- "$1/$5.displaced.path"
    return $_r
}

# Link $1 in as the work tree's $2 if nothing is there: in $2's own directory, entered once and
# checked to be the work tree's (as [`unredacted_place`] does).
unredacted_link_in() {
    _dir=$(dirname -- "$2")
    _want=$UR_TOP
    [ "$_dir" = . ] || _want=$UR_TOP/$_dir
    (cd -P -- "$_dir" 2> /dev/null && [ "$(pwd -P)" = "$_want" ] && ln -- "$1" "${2##*/}" 2> /dev/null)
}

# What a KILL left between [`unredacted_place`]'s two steps: the live file moved aside, its path
# recorded. Gone from its place: it goes back. Else it's dropped if it's the run's copy or the
# version the set-aside left (nothing of its own), and kept as the run's `<path>.displaced` (said,
# KEPT) if not.
unredacted_settle() {
    for _d in "$1"/*.displaced; do
        [ -f "$_d" ] || continue
        _run=${_d##*/}
        _run=${_run%.displaced}
        _f=$(cat "$_d.path" 2> /dev/null) || _f=
        if [ -z "$_f" ]; then
            :
        elif [ ! -e "$_f" ] && [ ! -L "$_f" ] && unredacted_link_in "$_d" "$_f"; then
            rm -f -- "$_d"
        elif [ -f "$1/$_run/$_f" ] && { cmp -s -- "$_d" "$1/$_run/$_f" || LC_ALL=C sed -E "$REDACT_SED" < "$1/$_run/$_f" | cmp -s - "$_d"; }; then
            rm -f -- "$_d"
        else
            mkdir -p -- "$1/$_run/$(dirname -- "$_f")" && mv -f -- "$_d" "$1/$_run/$_f.displaced" || :
        fi
        rm -f -- "$_d.path"
    done
}

# After the switch: the copies set aside on the branch now checked out, newest first and one per
# path, go back where the path holds exactly the version the set-aside left there (bytes and mode;
# through [`unredacted_place`]), and a copy the file already matches is dropped. Nothing else is
# ever overwritten: a file the participant changed, removed or moved under a link since keeps what
# they did, and its copy stays — said (KEPT), as are this switch's own copies. Copies from other
# branches wait for a switch back to theirs.
unredacted_restore() {
    if [ ! -d "$1" ]; then return 0; fi
    UR_TOP=$(pwd -P)
    cur=$(git rev-parse --abbrev-ref HEAD)
    unredacted_settle "$1"
    : > "$1.done"
    for run in $(ls -1 "$1" | sort -r); do
        if [ ! -d "$1/$run" ] || [ "$(cat "$1/$run.branch" 2>/dev/null)" != "$cur" ]; then continue; fi
        (cd "$1/$run" && find . -type f ! -name '*.displaced') | sed 's#^\./##' | while IFS= read -r f; do
            if grep -qxF -- "$f" "$1.done"; then continue; fi
            if [ ! -f "$f" ] || [ -L "$f" ] || ! unredacted_in_place "$1" "$f"; then continue; fi
            if cmp -s -- "$1/$run/$f" "$f"; then
                # Its keys are in the file already (a switch stopped before it put it back).
                rm -f -- "$1/$run/$f" && printf '%s\n' "$f" >> "$1.done"
                continue
            fi
            # What the set-aside left there, and the copy to put back, both beside the runs.
            cp -p -- "$1/$run/$f" "$1/$run.redacted" && LC_ALL=C sed -E "$REDACT_SED" < "$1/$run/$f" > "$1/$run.redacted" \
                && cp -p -- "$1/$run/$f" "$1/$run.back" \
                && unredacted_place "$1" "$f" "$1/$run.back" "$1/$run.redacted" "$run" \
                && rm -f -- "$1/$run/$f" && printf '%s\n' "$f" >> "$1.done" && echo "UNREDACTED_RESTORED $f"
            rm -f -- "$1/$run.redacted" "$1/$run.back"
        done
    done
    # A copy cut short (stopped before its rename into place): what it was for is as it was.
    rm -f -- "$1"/*.part "$1"/*.redacted "$1"/*.back
    # Every key still set aside here and not going back on its own: this switch's, and this branch's.
    for run in $(ls -1 "$1"); do
        if [ -d "$1/$run" ] && { [ "$run" = "${UR_RUN:-}" ] || [ "$(cat "$1/$run.branch" 2>/dev/null)" = "$cur" ]; }; then
            (cd "$1/$run" && find . -type f) | sed 's#^\./##'
        fi
    done | sort -u | while IFS= read -r f; do echo "UNREDACTED_KEPT $f"; done
    rm -f "$1.done"
    find "$1" -mindepth 1 -depth -type d -empty -delete 2>/dev/null || true
    for b in "$1"/*.branch; do
        if [ -f "$b" ] && [ ! -d "${b%.branch}" ]; then rm -f "$b"; fi
    done
    rmdir "$1" 2>/dev/null || true
}
