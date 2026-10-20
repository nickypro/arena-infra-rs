# arena's verified course: which commits are the course's own — public already, and never to be
# rewritten (review ai-review claude #4: a redacted copy of a course commit broke its ancestry, so
# every later `git pull upstream main` conflicted on the redacted lines). Rendered into the backup
# script and the resolve's (arena_core::backup::course_sh), which ask it the same way and trust it
# by one rule (FIX7, claude #1 and #3).
#
# The proof is refs/arena/course-main, which nothing but `course_verify` writes: a fetch of the
# configured course repo's own `main`. What the pod's `upstream` holds, or its URLs say, proves
# nothing (review ai-review codex #1, FIX6: a participant's fork fetched as `upstream/main`, then
# `upstream` repointed at the course, left their own key-bearing commit trusted).
#
# Each script sets, before calling these: COURSE_URL (`course_url` of its course argument; empty:
# no course), COURSE_REFS= and COURSE_TRIED= (empty), COURSE_WHO (its name, for its stderr), and $T
# (its temporary files' prefix, $T.start made at its start).

# course_url COURSE: where the course's `main` is fetched from, for COURSE as the commands are given
# it: OWNER/NAME (arena_core::backup::course_repo: ARENA_UPSTREAM_REPO, always that spelling by
# then) is https://github.com/OWNER/NAME.git — public, so fetched with no credentials. A local path
# (`/…`) or a `file://` URL is used as it is: only tests pass one, a local bare repo standing in for
# the course, so that no test reaches GitHub. Anything else prints nothing: no course, no exemption.
course_url() {
    case $1 in
        /* | file://*) printf '%s\n' "$1" ;;
        ./* | ../* | */. | */..) ;;
        *) printf '%s\n' "$1" | LC_ALL=C sed -E -n 's#^([A-Za-z0-9._-]+)/([A-Za-z0-9._-]+)$#https://github.com/\1/\2.git#p' ;;
    esac
}

# `course_direct`: whether git, with the config the fetch reads, fetches $COURSE_URL from that very
# URL — `ls-remote --get-url` applies `url.<base>.insteadOf` (and a remote named by it), contacting
# nothing.
course_direct() {
    [ "$(GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null git ls-remote --get-url "$COURSE_URL" 2>/dev/null)" = "$COURSE_URL" ]
}

# `course_trust`: $COURSE_REFS — refs/arena/course-main, if it is there — whenever a course is
# configured and its URL isn't rewritten (`course_direct`). A ref an earlier run's fetch left (the
# backup's, or the resolve's) is trusted as one this run fetched: nothing else writes it, from no
# other URL. The backup and the resolve go by this one rule (review ai-review claude #1: the backup
# ignored a ref it hadn't fetched itself, the resolve trusted any). No course, or a rewritten URL:
# nothing is exempt.
course_trust() {
    COURSE_REFS=
    if [ -n "$COURSE_URL" ] && git rev-parse -q --verify 'refs/arena/course-main^{commit}' > /dev/null && course_direct; then
        COURSE_REFS=refs/arena/course-main
    fi
}

# `course_verify`: fetch the course's main from $COURSE_URL into refs/arena/course-main and trust
# what it holds ($COURSE_REFS). Over https with no credentials (no helper, no prompt: a private
# course can't be verified), bounded (`timeout 60`, where there is one), never touching the
# participant's tags — nor their FETCH_HEAD, which every fetch writes: it's kept aside and put back
# after, or removed when there was none (`--no-write-fetch-head` needs git 2.29, and Ubuntu 20.04's
# is 2.25: there every fetch failed, review ai-review claude #2). The fetch can't be steered
# (review ai-review claude #3): it reads no global or system config (`GIT_CONFIG_GLOBAL=/dev/null`,
# `GIT_CONFIG_NOSYSTEM`; a git older than 2.32 reads its global config still, and `course_direct`
# sees what it sees), and the URL it would use — the repo's own `url.<base>.insteadOf` applied —
# must be $COURSE_URL itself: else nothing is fetched or trusted, said (`COURSE_REWRITTEN <url>`).
# An insteadOf pointing at a fork filled the ref with the fork's commits, all then exempt; one
# turning https into ssh sent it with the deploy key, to fail every time. No submodules fetched. A
# ref lock a run before this one left (one the `timeout` killed mid-update) is removed first: it
# failed every later fetch. Tried once a run: a fetch that fails trusts nothing new and is said
# once (`COURSE_UNVERIFIED <url>`, git's own words on stderr). The outcome stays in $COURSE_TRIED:
# `ok`, `failed`, or `rewritten`.
course_verify() {
    if [ -n "$COURSE_TRIED" ]; then [ "$COURSE_TRIED" = ok ]; return; fi
    if ! course_direct; then
        COURSE_TRIED=rewritten COURSE_REFS=
        echo "COURSE_REWRITTEN $COURSE_URL"
        return 1
    fi
    COURSE_TRIED=failed
    _cvl=$(git rev-parse --git-path refs/arena/course-main.lock)
    if [ -e "$_cvl" ] && [ -n "$(find "$_cvl" ! -newer "$T.start" 2>/dev/null)" ]; then rm -f "$_cvl"; fi
    _cvh=$(git rev-parse --git-path FETCH_HEAD)
    _cvk=none
    if [ -e "$_cvh" ]; then
        if cp -p "$_cvh" "$T.fhead" 2>/dev/null; then _cvk=kept; else _cvk=; fi
    fi
    _cvt=
    if timeout 1 true >/dev/null 2>&1; then _cvt='timeout 60'; fi
    if GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 GIT_ASKPASS= SSH_ASKPASS= $_cvt \
        git -c credential.helper= fetch --no-tags --no-recurse-submodules --quiet \
        "$COURSE_URL" '+refs/heads/main:refs/arena/course-main' > /dev/null 2> "$T.cfetch" \
        && git rev-parse -q --verify 'refs/arena/course-main^{commit}' > /dev/null; then
        COURSE_TRIED=ok
    fi
    case $_cvk in
        kept) mv -f "$T.fhead" "$_cvh" || true ;;
        none) rm -f "$_cvh" ;;
    esac
    if [ "$COURSE_TRIED" = ok ]; then
        COURSE_REFS=refs/arena/course-main
        return 0
    fi
    echo "COURSE_UNVERIFIED $COURSE_URL"
    sed "s/^/$COURSE_WHO: fetching the course to verify its commits: /" "$T.cfetch" >&2 || true
    return 1
}
