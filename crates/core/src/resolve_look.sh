# arena_look: from the checkout's top ($G its git dir, `busy_op` defined), what is in progress
# there and when anything in it last changed — read-only (no optional locks, nothing written).
# Shared by the look `pods backup` takes of a pod whose backup said BUSY and by the resolve, which
# looks again and goes ahead only if nothing changed. Prints, on stdout:
#   NOW <epoch>              the pod's own clock (every age is judged on it)
#   TOP <path>               the checkout's top
#   HOME <path>              the login's home
#   OP <what> [<mtime> <commit> <seq>]  busy_op's first word, or `none`. For a merge, cherry-pick or
#                            revert: the mtime of its *_HEAD file (written when it started) and that
#                            commit; for `conflicts` (a `git stash pop`'s): the index's mtime and the
#                            newest stash entry (`-`: none). <seq> 1 while sequencer/ is there (a
#                            multi-commit cherry-pick or revert)
#   NEWEST <epoch> <n>       the newest mtime of the n files git lists — tracked, and untracked but not
#                            ignored (`.git` never) — 0 0 for none
#   PRINT <crc>-<size>       a checksum of every such file's mtime, size and path: any edit, new file,
#                            deletion or rename changes it
#   INPUT <epoch>            the latest keyboard input on a terminal login (`who`; its tty's atime),
#                            when there is a login
arena_look() {
    echo "NOW $(date +%s)"
    echo "TOP $(pwd)"
    echo "HOME ${HOME:-}"
    set -- $(busy_op "$G")
    case ${1:-none} in
        merge) _f=MERGE_HEAD ;;
        cherry-pick) _f=CHERRY_PICK_HEAD ;;
        revert) _f=REVERT_HEAD ;;
        conflicts) _f=index ;;
        *) _f= ;;
    esac
    if [ -n "$_f" ]; then
        if [ "$_f" = index ]; then _h=$(git rev-parse -q --verify refs/stash 2>/dev/null) || _h=-
        else _h=$(git rev-parse -q --verify "$_f" 2>/dev/null) || _h=-; fi
        if [ -d "$G/sequencer" ]; then _s=1; else _s=0; fi
        echo "OP $1 $(stat -c %Y -- "$G/$_f") ${_h:--} $_s"
    else
        echo "OP ${1:-none}"
    fi
    _l=$(git --no-optional-locks ls-files -z --cached --others --exclude-standard \
        | xargs -0 -r stat -c '%Y %s %n' -- 2>/dev/null | LC_ALL=C sort -u)
    printf '%s\n' "$_l" | awk 'NF { n++; if ($1 + 0 > m) m = $1 + 0 } END { print "NEWEST " m + 0 " " n + 0 }'
    echo "PRINT $(printf '%s\n' "$_l" | cksum | tr ' ' '-')"
    _i=$(who 2>/dev/null | awk '{ print $2 }' | while read -r _t; do stat -c %X -- "/dev/$_t" 2>/dev/null; done | sort -n | tail -n 1)
    if [ -n "$_i" ]; then echo "INPUT $_i"; fi
}
