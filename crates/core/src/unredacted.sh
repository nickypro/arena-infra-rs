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
#                                has its own version), one of this branch's that couldn't go back
#                                (the file changed since), or a version of the file a replacement
#                                couldn't finish with — every such one, every time, until it's gone
#   UNREDACTED_RECOVERED <folder> <path>   a version of the file a replacement moved aside, kept
#                                (never deleted on its own: a process could still write to it) until
#                                `arena pods clean-kept` drops it, once a file holds it byte for byte
#
# However a switch ends — a TERM (the bound's; the EXIT trap then restores) or a KILL anywhere (a
# later switch's restore, or init-branches', recovers) — no version of a file is ever lost, and a
# key is only ever in its file or in a copy that is said (KEPT):
# - the run's branch is written before its first copy (a run is restored by its branch);
# - a copy is made beside the runs and renamed in: whole, or not there;
# - a file is only replaced through [`unredacted_place`]: in its own directory, entered once and
#   checked to be the work tree's (a link swapped in for it later is never followed), the live
#   file is first moved into a recover.* folder of its own — its path and what it should be
#   written there before, so a KILL right then is recovered — and the new one linked in (`ln -T`:
#   never over anything that took the name) only if what was moved aside is exactly what was
#   expected, bytes and mode; else it goes back. What was moved aside is never deleted on its own
#   (a process that had it open could still write to it): its folder keeps it, said, until the
#   operator's `clean-kept` drops it, once a file holds it byte for byte ([`unredacted_clean`]).
#   A folder goes on its own only when nothing in it is unique — checked, never taken from a status.

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
    UR_HERE=$UR_BRANCH
    _ur=
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
        # Its git version in a temp folder of its own, made for this file alone: a name a KILL
        # left linked into the work tree is never written to again.
        t=
        if ! { mkdir -p -- "$1/$UR_RUN/$(dirname -- "$f")" \
            && { [ -f "$1/$UR_RUN.branch" ] || printf '%s\n' "$UR_BRANCH" > "$1/$UR_RUN.branch"; } \
            && cp -p -- "$f" "$1/$UR_RUN.part" && mv -f -- "$1/$UR_RUN.part" "$c" \
            && t=$(mktemp -d "$1/tmp.XXXXXX") \
            && cp -p -- "$c" "$t/redacted" && LC_ALL=C sed -E "$REDACT_SED" < "$c" > "$t/redacted" \
            && [ "$(git hash-object --path="$f" -- "$t/redacted")" = "$i" ]; }; then
            rm -f -- "$c"
            [ -z "$t" ] || rm -rf -- "$t"
            echo "arena set-branch: couldn't set $f aside - left as it is" >&2
            continue
        fi
        if unredacted_place "$1" "$f" "$t/redacted" "$c"; then r=0; else r=$?; fi
        rm -rf -- "$t"
        case $r in
            0) ;;
            4)
                # Replaced, but its folder couldn't be marked (no room?): no switch on top of that.
                echo "arena set-branch: couldn't mark $f's set-aside (is the disk full?) - not switched; the keys go back" >&2
                exit 4
                ;;
            3) echo "arena set-branch: $f changed while it was set aside, or couldn't go back: its versions are kept in $1" >&2 ;;
            *)
                rm -f -- "$c"
                echo "arena set-branch: couldn't set $f aside (it changed meanwhile, or its place moved) - left as it is" >&2
                ;;
        esac
    done || _ur=$?
    git update-index -q --refresh > /dev/null 2>&1 || :
    unredacted_settle "$1"
    return "${_ur:-0}"
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

# Replace the work tree's file $2 by $3, if it is now exactly $4 (bytes and mode). In $2's own
# directory, entered once (`cd -P`) and checked to be the work tree's: from there on only names in
# it are used, so a link swapped in for that directory is never followed. The live file is moved
# into a recover.* folder of its own in $1 first (its path written there before), and the new one
# hard-linked in with `ln -T`, which never replaces anything that took the name meanwhile (a file,
# a directory, a link to one). 0: replaced; the folder, renamed `<folder>.placed`, keeps what was
# there. 4: replaced, but that rename failed: the folder keeps it unmarked. 1: nothing moved. 2:
# left as it was (what was there wasn't $4, or the new one couldn't go in: it went back). 3: what
# was there couldn't go back (its name taken, or the link failed): its folder keeps it, said. A
# folder goes here only for 1 and 2, and only once that's checked: nothing in it, or its file the
# very one in its place again.
unredacted_place() {
    _dir=$(dirname -- "$2")
    _want=$UR_TOP
    [ "$_dir" = . ] || _want=$UR_TOP/$_dir
    mkdir -p -- "$1" || return 1
    _rd=$(mktemp -d "$1/recover.XXXXXX") || return 1
    if ! { printf '%s\n' "$2" > "$_rd/path" && printf '%s\n' "${UR_HERE:-}" > "$_rd/branch"; }; then
        rm -rf -- "$_rd"
        return 1
    fi
    _r=0
    (
        _base=${2##*/}
        { cd -P -- "$_dir" 2> /dev/null && [ "$(pwd -P)" = "$_want" ]; } || exit 1
        { [ -f "$_base" ] && [ ! -L "$_base" ]; } || exit 1
        mv -f -- "$_base" "$_rd/file" || exit 1
        if [ -f "$_rd/file" ] && [ ! -L "$_rd/file" ] && cmp -s -- "$_rd/file" "$4" \
            && [ "$(stat -c %a -- "$_rd/file")" = "$(stat -c %a -- "$4")" ] && ln -T -- "$3" "$_base" 2> /dev/null; then
            mv -T -- "$_rd" "$_rd.placed" 2> /dev/null || exit 4
            exit 0
        fi
        # Not what was expected, or the new one couldn't go in: what was there goes back.
        if [ -f "$_rd/file" ] && [ ! -L "$_rd/file" ]; then
            { ln -T -- "$_rd/file" "$_base" 2> /dev/null && [ "$_rd/file" -ef "$_base" ]; } || exit 3
        else
            { mv -n -T -- "$_rd/file" "$_base" 2> /dev/null && [ ! -e "$_rd/file" ] && [ ! -L "$_rd/file" ]; } || exit 3
        fi
        exit 2
    ) || _r=$?
    if [ "$_r" = 1 ] || [ "$_r" = 2 ]; then
        if { [ ! -e "$_rd/file" ] && [ ! -L "$_rd/file" ]; } || unredacted_is_back "$_rd/file" "$2"; then
            rm -rf -- "$_rd"
        else
            _r=3
        fi
    fi
    return $_r
}

# Whether $1 is the very file (one inode) the work tree's $2 is now: looked at from $2's own
# directory, entered once and checked to be the work tree's.
unredacted_is_back() {
    _dir=$(dirname -- "$2")
    _want=$UR_TOP
    [ "$_dir" = . ] || _want=$UR_TOP/$_dir
    (cd -P -- "$_dir" 2> /dev/null && [ "$(pwd -P)" = "$_want" ] && [ -f "${2##*/}" ] && [ ! -L "${2##*/}" ] && [ "$1" -ef "${2##*/}" ])
}

# Link $1 in as the work tree's $2 if nothing is there (`ln -T`): in $2's own directory, entered
# once and checked to be the work tree's (as [`unredacted_place`] does).
unredacted_link_in() {
    _dir=$(dirname -- "$2")
    _want=$UR_TOP
    [ "$_dir" = . ] || _want=$UR_TOP/$_dir
    (cd -P -- "$_dir" 2> /dev/null && [ "$(pwd -P)" = "$_want" ] && ln -T -- "$1" "${2##*/}" 2> /dev/null)
}

# The identities of every file a process has open (device:inode) or mapped (M:inode), a line
# each — and `?` when, as root (a pod's cleanup), a process still there couldn't be looked at:
# then nothing counts as free. As another user (a test), a process it may not look at is passed
# over. From /proc; without it, `?`. Only a look at one moment: the cleanup that uses it is the
# operator's, run when the pod is quiet.
unredacted_held() {
    if [ ! -d /proc/self/fd ]; then
        echo '?'
        return 0
    fi
    _me=$(id -u)
    for _p in /proc/[0-9]*; do
        if ! ls -- "$_p/fd" > /dev/null 2>&1; then
            if [ "$_me" = 0 ] && [ -d "$_p" ]; then echo '?'; fi
            continue
        fi
        for _fd in "$_p"/fd/*; do
            stat -L -c '%d:%i' -- "$_fd" 2> /dev/null || :
        done
        awk '$5 != 0 { print "M:" $5 }' "$_p/maps" 2> /dev/null || :
    done | sort -u
}

# Whether the file $1 is held, by [`unredacted_held`]'s list $2.
unredacted_is_held() {
    _id=$(stat -c '%d:%i' -- "$1" 2> /dev/null) || return 0
    printf '%s\n' "$2" | grep -qx -e "$_id" -e "M:${_id#*:}" -e '?'
}

# The recover.* folders [`unredacted_place`] left that need no one to decide: one with nothing in
# it (a KILL before the move) goes; one whose replacement never went in (a KILL between moving the
# file aside and linking its replacement in), its place still empty and on the branch it was moved
# aside on, puts the file back and goes — checked to be back. Every other one stays, said ([`unredacted_restore`]), for
# [`unredacted_clean`]: never one whose replacement went in, whatever it holds.
unredacted_settle() {
    for _rd in "$1"/recover.*; do
        [ -d "$_rd" ] || continue
        case $_rd in *.placed) continue ;; esac
        if [ ! -e "$_rd/file" ] && [ ! -L "$_rd/file" ]; then
            rm -rf -- "$_rd"
            continue
        fi
        _f=$(cat "$_rd/path" 2> /dev/null) || _f=
        # Only on the branch it was moved aside on: elsewhere an empty place proves nothing.
        [ "$(cat "$_rd/branch" 2> /dev/null)" = "${UR_HERE:-}" ] || continue
        if [ -n "$_f" ] && [ ! -e "$_f" ] && [ ! -L "$_f" ] && unredacted_link_in "$_rd/file" "$_f" && unredacted_is_back "$_rd/file" "$_f"; then
            rm -rf -- "$_rd"
        fi
    done
}

# `arena pods clean-kept`: of the versions recover.* folders keep, drop the ones a file now
# holds byte for byte — the work tree's at that path, or git's (HEAD's blob, as stored: no filter
# the repository configures is run) — and none a process has open or mapped; say each (dropped, or
# with $2 = dry what would be), and each one kept. A folder with nothing in it goes too.
unredacted_clean() {
    [ -d "$1" ] || return 0
    UR_TOP=$(pwd -P)
    _held=$(unredacted_held "$1")
    for _rd in "$1"/recover.*; do
        [ -d "$_rd" ] || continue
        _f=$(cat "$_rd/path" 2> /dev/null) || _f=
        _why=
        if [ ! -e "$_rd/file" ] && [ ! -L "$_rd/file" ]; then
            _why=empty
        elif [ -f "$_rd/file" ] && [ ! -L "$_rd/file" ] && ! unredacted_is_held "$_rd/file" "$_held" && [ -n "$_f" ]; then
            if [ -f "$_f" ] && [ ! -L "$_f" ] && unredacted_in_place "$1" "$_f" && cmp -s -- "$_rd/file" "$_f"; then
                _why=file
            elif git cat-file blob "HEAD:$_f" 2> /dev/null | cmp -s - "$_rd/file"; then
                _why=git
            fi
        fi
        if [ -n "$_why" ]; then
            [ "$2" = dry ] || rm -rf -- "$_rd"
            echo "RECOVERED_DROP $_why ${_rd##*/} ${_f:-?}"
        else
            echo "RECOVERED_KEEP ${_rd##*/} ${_f:-?}"
        fi
    done
    [ "$2" = dry ] || rmdir "$1" 2> /dev/null || true
}

# After the switch: the copies set aside on the branch now checked out, newest first and one per
# path, go back where the path holds exactly the version the set-aside left there (bytes and mode;
# through [`unredacted_place`]), and a copy the file already matches is dropped. Nothing else is
# ever overwritten: a file the participant changed, removed or moved under a link since keeps what
# they did, and its copy stays — said (KEPT), as are this switch's own copies and every version a
# recover.* folder keeps. Copies from other branches wait for a switch back to theirs.
unredacted_restore() {
    if [ ! -d "$1" ]; then return 0; fi
    UR_TOP=$(pwd -P)
    cur=$(git rev-parse --abbrev-ref HEAD)
    UR_HERE=$cur
    unredacted_settle "$1"
    : > "$1.done"
    for run in $(ls -1 "$1" | sort -r); do
        case $run in recover.*) continue ;; esac
        if [ ! -d "$1/$run" ] || [ "$(cat "$1/$run.branch" 2>/dev/null)" != "$cur" ]; then continue; fi
        (cd "$1/$run" && find . -type f) | sed 's#^\./##' | while IFS= read -r f; do
            if grep -qxF -- "$f" "$1.done"; then continue; fi
            if [ ! -f "$f" ] || [ -L "$f" ] || ! unredacted_in_place "$1" "$f"; then continue; fi
            if cmp -s -- "$1/$run/$f" "$f"; then
                # Its keys are in the file already (a switch stopped before it put it back).
                rm -f -- "$1/$run/$f" && printf '%s\n' "$f" >> "$1.done"
                continue
            fi
            # What the set-aside left there, and the copy to put back, in a temp folder made for
            # this file alone (a name a KILL left linked into the work tree is never written to).
            if t=$(mktemp -d "$1/tmp.XXXXXX"); then
                if cp -p -- "$1/$run/$f" "$t/redacted" && LC_ALL=C sed -E "$REDACT_SED" < "$1/$run/$f" > "$t/redacted" \
                    && cp -p -- "$1/$run/$f" "$t/back"; then
                    if unredacted_place "$1" "$f" "$t/back" "$t/redacted"; then r=0; else r=$?; fi
                    if [ "$r" = 0 ] || [ "$r" = 4 ]; then
                        rm -f -- "$1/$run/$f" && printf '%s\n' "$f" >> "$1.done" && echo "UNREDACTED_RESTORED $f"
                    fi
                fi
                rm -rf -- "$t"
            fi
        done
    done
    # A copy cut short (stopped before its rename into place): what it was for is as it was. A
    # temp folder a KILL left: only its names go (a name in it may be a link to a file in place).
    rm -f -- "$1"/*.part
    rm -rf -- "$1"/tmp.*
    unredacted_settle "$1"
    # Every key still set aside here and not going back on its own: this switch's, this branch's,
    # and every version a recover.* folder keeps.
    for run in $(ls -1 "$1"); do
        case $run in
            recover.*.placed) ;;
            recover.*)
                if [ -d "$1/$run" ]; then
                    echo "$(cat "$1/$run/path" 2> /dev/null || echo '(a file)') (a version of it in .git/arena-unredacted/$run)"
                fi
                ;;
            *)
                if [ -d "$1/$run" ] && { [ "$run" = "${UR_RUN:-}" ] || [ "$(cat "$1/$run.branch" 2>/dev/null)" = "$cur" ]; }; then
                    (cd "$1/$run" && find . -type f) | sed 's#^\./##'
                fi
                ;;
        esac
    done | sort -u | while IFS= read -r f; do echo "UNREDACTED_KEPT $f"; done
    for run in "$1"/recover.*.placed; do
        if [ -d "$run" ]; then echo "UNREDACTED_RECOVERED ${run##*/} $(cat "$run/path" 2> /dev/null || echo '?')"; fi
    done
    rm -f "$1.done"
    find "$1" -mindepth 1 -depth -type d -empty -delete 2>/dev/null || true
    for b in "$1"/*.branch; do
        if [ -f "$b" ] && [ ! -d "${b%.branch}" ]; then rm -f "$b"; fi
    done
    rmdir "$1" 2>/dev/null || true
}
