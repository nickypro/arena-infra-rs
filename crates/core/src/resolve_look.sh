# arena_look: from the checkout's top ($G its git dir, $REPO the repo path as configured, `busy_op`
# defined), what is in progress there, when anything in it last changed and whether anyone is at
# work in it — read-only (no optional locks, nothing written). Shared by the look `pods backup`
# takes of a pod whose backup said BUSY and by the resolve, which looks again and goes ahead only
# if nothing changed. Prints, on stdout:
#   NOW <epoch>              the pod's own clock (every age is judged on it)
#   TOP <path>               the checkout's top, every link resolved
#   REPO <path>              the configured repo path, when it names that top (setup links
#                            /root/ARENA_materials onto the /workspace volume): where a pull of
#                            the home finds the checkout, as the pull's own probe does
#   HOME <path>              the login's home
#   OP <what> [<mtime> <commit> <seq>]  busy_op's first word, or `none`. For a merge, cherry-pick or
#                            revert: the mtime of its *_HEAD file (written when it started) and that
#                            commit; for `conflicts` (a `git stash pop`'s): the index's mtime and the
#                            newest stash entry (`-`: none). <seq> 1 while sequencer/ is there (a
#                            multi-commit cherry-pick or revert)
#   NEWEST <epoch> <n>       the newest mtime of the n files git lists — tracked, and untracked but not
#                            ignored (`.git` never) — 0 0 for none
#   CHANGED <path>           that newest file, when there is one: what a pod's line names when a
#                            recent change keeps its operation from being finished
#   PRINT <crc>-<size>       a checksum of every such file's mtime, size, ctime (to the nanosecond) and
#                            path: any edit, new file, deletion or rename changes it — and so does a
#                            replacement that keeps the old mtime and size (`cp -p`, `rsync -a`),
#                            whose ctime is new. Ages are judged by mtime alone (NEWEST)
#   INDEX <epoch>            the index's mtime: something staged since the operation started
#   LOCKED index.lock        git's index is locked: a git command is at work (or died holding it)
#   INPUT <epoch>            the latest keyboard input on a terminal login (`who`; its tty's atime),
#                            when there is a login
#   TTY <epoch>              the latest use of any pseudo-terminal (/dev/pts/N: its atime or mtime),
#                            read directly: where there is no utmp (the images' containers) `who`
#                            lists nobody, and VS Code's, Jupyter's and tmux's terminals are no login
#   PROC <pid> <name>        a process at work in git, the first found: a `git` running (not a
#                            filesystem monitor's daemon, which lives as long as the repo is open),
#                            or anything holding one of git's message or todo files (MERGE_MSG,
#                            COMMIT_EDITMSG, git-rebase-todo, SQUASH_MSG) as an argument — an editor
#                            a git command waits on. Never the look's own: they share its session
# Unsaved work in an editor's or a notebook's buffers is seen by none of these: only what reaches
# the disk counts — and the copy and .git/arena-resolved/ hold the state before.
# ARENA_TEST_PTS_DIR and ARENA_TEST_PROC_DIR stand in for /dev/pts and /proc, in tests only.
arena_look() {
    echo "NOW $(date +%s)"
    echo "TOP $(pwd)"
    _r=$(cd -P "$REPO" 2>/dev/null && pwd -P) || _r=
    if [ -n "$_r" ] && [ "$_r" = "$(pwd -P)" ]; then echo "REPO ${REPO%/}"; fi
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
        | xargs -0 -r stat -c '%Y %s %z %n' -- 2>/dev/null | LC_ALL=C sort -u)
    # Each line: `<mtime> <size> <ctime: 3 words> <path>`.
    printf '%s\n' "$_l" | awk 'NF { n++; if (n == 1 || $1 + 0 > m) { m = $1 + 0; c = $0 } }
        END { print "NEWEST " m + 0 " " n + 0; if (n) { sub(/^[^ ]+ [^ ]+ [^ ]+ [^ ]+ [^ ]+ /, "", c); print "CHANGED " c } }'
    echo "PRINT $(printf '%s\n' "$_l" | cksum | tr ' ' '-')"
    if [ -e "$G/index" ]; then echo "INDEX $(stat -c %Y -- "$G/index")"; fi
    if [ -e "$G/index.lock" ]; then echo "LOCKED index.lock"; fi
    _i=$(who 2>/dev/null | awk '{ print $2 }' | while read -r _t; do stat -c %X -- "/dev/$_t" 2>/dev/null; done | sort -n | tail -n 1)
    if [ -n "$_i" ]; then echo "INPUT $_i"; fi
    _i=$(for _t in "${ARENA_TEST_PTS_DIR:-/dev/pts}"/[0-9]*; do
        if [ -e "$_t" ]; then stat -c '%X %Y' -- "$_t" 2>/dev/null; fi
    done | tr ' ' '\n' | sort -n | tail -n 1)
    if [ -n "$_i" ]; then echo "TTY $_i"; fi
    _i=$(arena_at_work)
    if [ -n "$_i" ]; then echo "PROC $_i"; fi
}
# The session of process dir $1 (/proc/<pid>): the 4th field after its `(name)` in `stat`.
arena_sid() {
    _s=$(cat "$1/stat" 2>/dev/null) || return 0
    _s=${_s##*") "}
    set -- $_s
    echo "${4:-}"
}
# `<pid> <name>` of the first process at work in git (PROC above), or nothing.
arena_at_work() {
    _pr=${ARENA_TEST_PROC_DIR:-/proc}
    _own=$(arena_sid "/proc/$$")
    { grep -lx git "$_pr"/[0-9]*/comm 2>/dev/null | sed 's#/comm$##'
      grep -lzE '(^|/)(MERGE_MSG|COMMIT_EDITMSG|git-rebase-todo|SQUASH_MSG)$' "$_pr"/[0-9]*/cmdline 2>/dev/null | sed 's#/cmdline$##'
    } | while IFS= read -r _d; do
        if [ -n "$_own" ] && [ "$(arena_sid "$_d")" = "$_own" ]; then continue; fi
        if tr '\0' '\n' < "$_d/cmdline" 2>/dev/null | grep -qx 'fsmonitor--daemon'; then continue; fi
        echo "${_d##*/} $(cat "$_d/comm" 2>/dev/null || echo '?')"
        break
    done
}
