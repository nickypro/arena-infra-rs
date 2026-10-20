# arena backup: commit + push one pod's ARENA checkout on the branch it is on.
#
#   sh -c "$THIS_SCRIPT" arena-backup <repo> <commit message> <max file bytes> <push limit bytes> [<git ssh key> [<owner/name>]]
#
# Rendered and parsed by arena_core::backup (backup_command / parse_backup_report); runs under
# plain `sh` (dash on the images), whatever the pod's login shell. Prints, on stdout:
#   WRONG_ORIGIN <owner/name> [<url>]  `origin` isn't the configured repo <owner/name> (or there is
#                              none): checked first, before the SKIP; nothing committed or pushed (exit 5).
#                              The URL is printed with any credentials taken out.
#   SKIP <branch>              on main/master/a detached HEAD: nothing done (exit 0)
#   UNCOMMITTED <branch> <what> [unmerged]  before a SKIP, when the checkout has work no backup
#                              saves there (what a `set-branch --hard` copies first, .git and all):
#                              <what> is the first of BUSY's word, `changes` (to tracked files),
#                              `commits` (that no remote has), `untracked` (files not ignored)
#   NO_CHANGES <branch>        clean tree and HEAD == origin's <branch>: nothing to push (exit 0)
#   PUSHED <branch>            committed what there was and/or pushed unpushed commits (exit 0)
#   BIGSKIP <bytes> <path>     a file over <max file bytes>, kept out of the commit (stays on disk)
#   NESTED_ADOPTED <path>      a nested clone whose files this repo now tracks (its .git untouched)
#   NESTED_KEPT <path>         a nested repo left as it is and kept out of the commit
#   BIGSTUCK <bytes> <path>    a file over <push limit bytes> already in unpushed commits (exit 3)
#   BIGSTUCK_BASE <sha>|none   what `git reset` to, to rewrite those commits without it
#   BRANCH <branch>            the branch it backs up (before anything else, unless SKIP)
#   BUSY <branch> <what> [unmerged]  an operation is in progress on it — <what> is rebase, am, merge,
#                              cherry-pick, revert, sequence (a multi-commit cherry-pick or revert paused
#                              between commits), or conflicts (unmerged paths with none of those: a `git
#                              stash pop` that conflicted); `unmerged` after an operation while some of its
#                              conflicts aren't resolved and added yet: nothing staged, committed or
#                              pushed, so it stays resumable and abortable (exit 6)
#   REDACTED <n> <path>        a file committed with its n key(s) redacted; the file on disk keeps them
#   REDACTED_HISTORY <n> <old> <new>  a key in older unpushed commits, in a file or a commit message (an
#                              earlier backup's, or one the participant made): those n commits rewritten
#                              redacted, pushed as a fast-forward
#   HEAL_ABORTED <why>         that rewrite stopped short (the branch moved meanwhile): the next run ends it (exit 1)
#   PUSH_BLOCKED <reason>      GitHub's push protection still refuses the push — a secret the redaction
#                              doesn't know; the reason names GitHub's secret type and path:line (exit 4)
#   PUSH_FAILED                the push failed otherwise (the network, a non-fast-forward…; exit 1)
# Any other failure exits non-zero via `set -e`.
set -e
# A hangup, an interrupt, a TERM or a broken pipe (the ssh connection gone, met at the next write)
# ends the run as an exit, so the EXIT trap that removes its temporary files runs: dash runs none
# for a signal left at its default. (The trap runs once the command running then returns.)
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 141' PIPE
REPO=$1
MSG=$2
MAX=$3
LIMIT=$4
KEY=${5:-}
ORIGIN=${6:-}
cd "$REPO"
cd "$(git rev-parse --show-toplevel)"
TOP=$(pwd)
# Whose repo this pushes to, before anything else (the SKIP on main too): a pod nobody ran `pods
# setup` on kept the image's upstream `origin` and had no deploy key, so it was never backed up to
# git and nothing said so (live, 2026-10-17). An empty ORIGIN (no repo configured) checks nothing.
# @ARENA_ORIGIN_SH@
origin_check "$ORIGIN"
if [ -n "$KEY" ]; then
    export GIT_SSH_COMMAND="ssh -i $KEY -o StrictHostKeyChecking=accept-new -o BatchMode=yes"
fi
# Stay on the current branch; never push a protected/detached one.
B=$(git rev-parse --abbrev-ref HEAD)
# @ARENA_BUSY_OP_SH@
# `tracked_changed <git dir>`: whether any tracked file differs from HEAD (staged or not), with the
# participant's index never written — not even its stat cache (review ai-review claude #9). The
# check runs on a copy of the index: `update-index --refresh` brings the copy's stat data up to date
# (a file whose stat changed but whose content didn't — after a restore, an rsync, a `touch` — is
# hashed and then reads as clean), and `diff-index` then compares HEAD with the work tree through
# it. Plumbing alone, so no porcelain refresh and no index.lock of ours. The copy is gone after,
# its refresh with it: such a file is hashed again next run, until the participant's own git
# refreshes their index (an editor's `git status` does, often). Without an index to copy, the
# porcelain diff, read-only. The copy's removal is the EXIT trap, set before the copy exists
# (review ai-review claude #10): a run killed mid-check (a timeout, a dropped connection) left a
# full copy of the index, and its lock, in .git every time.
tracked_changed() {
    _ti="$1/arena-backup.$$.index"
    trap 'rm -f "$_ti" "$_ti.lock"' EXIT
    if cp "$1/index" "$_ti" 2>/dev/null; then
        GIT_INDEX_FILE="$_ti" git update-index -q --refresh >/dev/null 2>&1 || true
        if GIT_INDEX_FILE="$_ti" git diff-index --quiet HEAD -- 2>/dev/null; then _tc=1; else _tc=0; fi
        rm -f "$_ti" "$_ti.lock"
        return "$_tc"
    fi
    ! git --no-optional-locks diff --quiet HEAD -- 2>/dev/null
}
if [ "$B" = main ] || [ "$B" = master ] || [ "$B" = HEAD ]; then
    # No backup commits or pushes here, so the work a `set-branch --hard` could discard is said
    # first, and it copies the pod — .git and all — before resetting it (review fb2 F3). The first
    # of: an operation in progress; changes to tracked files; commits no remote has (review pr4:
    # work committed on main, its tree clean, was reset away uncopied — only the reflog kept it);
    # untracked files git doesn't ignore (review pr4: --hard overwrites one wherever the branch it
    # resets to tracks the same path, and which branch that is, or what origin's copy of it holds
    # since the switch's own fetch, nothing here knows — so any counts). A nested repo (listed as
    # `dir/`) doesn't: the image ships clones like that, and the switch leaves them be. Read-only
    # for the participant's git: no optional locks, the tracked files compared through a copy of
    # the index (`tracked_changed`), so their own git never meets an index.lock of ours. Commits
    # count only when *no* remote has them, `upstream` included: those a `git pull upstream main`
    # brought are the course's, which a reset doesn't lose.
    GD=$(git rev-parse --absolute-git-dir)
    W=$(busy_op "$GD")
    if [ -z "$W" ] && tracked_changed "$GD"; then W=changes; fi
    if [ -z "$W" ] && [ -n "$(git --no-optional-locks rev-list -n1 HEAD --not --remotes 2>/dev/null)" ]; then W=commits; fi
    if [ -z "$W" ] && [ -n "$(git --no-optional-locks ls-files --others --exclude-standard 2>/dev/null | grep -v '/$' | head -n 1)" ]; then
        W=untracked
    fi
    if [ -n "$W" ]; then echo "UNCOMMITTED $B $W"; fi
    echo "SKIP $B"; exit 0
fi
echo "BRANCH $B"
# A committer identity, in case the pod has none configured.
git config user.name >/dev/null 2>&1 || git config user.name 'Arena Autocommit'
git config user.email >/dev/null 2>&1 || git config user.email 'autocommit@arena.education'
G=$(git rev-parse --absolute-git-dir)

# --- An operation in progress ---------------------------------------------------------------
# A participant mid-way through a conflicted `git pull upstream main` (a merge), a cherry-pick or a
# revert — or with the conflicts a `git stash pop` left — has conflict markers in the tree: `add
# -A` would stage them as resolved and the commit would end the operation (a merge committed as a
# merge, markers and all, then pushed), after which `git merge --abort` and the editor's conflict
# view no longer work. So while one lasts nothing is staged, committed or pushed: the run says
# what is in progress and stops (exit 6), the operation stays where the participant left it, and
# the caller copies the pod by rsync instead. So does a `git am` (HEAD attached, often nothing
# unmerged: a patch that didn't apply) and a sequence paused between its commits. A rebase
# detaches HEAD (SKIP), unless it kept HEAD attached.
# (`busy_op` is defined above, before the SKIP.) Checked here, and again right before the staging
# and right before the commit (review ai-review codex #1): a merge the participant starts while
# the nested repos and the files are scanned is caught there, before `add -A` resolves its
# conflicts or the commit ends it. What's left is the moment between the last check and git
# taking its index lock — git has no lock that spans commands, so no check can close it.
busy_stop() {
    W=$(busy_op "$G")
    if [ -n "$W" ]; then echo "BUSY $B $W"; exit 6; fi
}
busy_stop

T="$G/arena-backup.$$"
trap 'rm -f "$T".*' EXIT
# @ARENA_REDACT_SH@

# --- Repos nested in this one ---------------------------------------------------------------
# `git add -A` records a nested clone as a gitlink: a commit id, none of its files — so the
# work inside it never reached GitHub (live: the image ships 4 untracked clones inside the
# exercises). Two treatments:
# - ADOPT, once per checkout (the marker $G/arena-nested lists what it adopted): a nested clone
#   with no commits of its own (HEAD on one of its remote-tracking refs — the image's clones of
#   upstream repos) has its tracked files added to this repo's index, where they are. Once this
#   repo tracks files in a directory, git walks into it like into any other (a .git there
#   doesn't stop it), so every later `add -A` commits the participants' new and changed files
#   in it. The clone's own .git is never moved or touched: git run inside the exercise still
#   works on the clone alone — never on the whole course repo — and its stashes, branches and
#   reflog stay in it, where the rsync tiers back them up like every other .git. Nothing on disk
#   changes; to undo: `git rm -r -q --cached <dir>` and delete its line from $G/arena-nested.
# - KEEP, every run: any other nested repo (one with commits of its own — a participant's
#   project — or one appearing after the first run) is left exactly as it is, kept out of the
#   commit through a managed block in .git/info/exclude, and reported. The rsync tiers back it up.
E="$G/info/exclude"
mkdir -p "$G/info"
touch "$E"
# The block is rebuilt below from what is nested now.
sed -i '/^# BEGIN arena-backup$/,/^# END arena-backup$/d' "$E"

is_submodule() {
    [ -f .gitmodules ] && git config -f .gitmodules --get-regexp '^submodule\..*\.path$' 2>/dev/null \
        | cut -d' ' -f2- | grep -qxF -- "$1"
}
# Every nested repo, one per line: untracked ones (ls-files lists a nested repo as `dir/`, never
# its files) and gitlinks an earlier backup committed by mistake (not .gitmodules submodules).
# An adopted clone is neither: this repo tracks files in it.
nested() {
    { git ls-files -z -o --exclude-standard | grep -z '/$' | sed -z 's#/$##'
      git ls-files -z -s | grep -z '^160000 ' | cut -z -f2-
    } | tr '\0' '\n' | while IFS= read -r d; do
        if [ -n "$d" ] && [ -e "$d/.git" ] && ! is_submodule "$d"; then printf '%s\n' "$d"; fi
    done
}
# A gitlink still in the index (an earlier backup's) out of it — the index only, never a file —
# so its directory is committed as files (adopted) or not at all (kept).
unlink_gitlink() {
    if git ls-files -s -- ":(literal)$1" | grep -q '^160000 '; then
        git rm -q --cached -f -- ":(literal)$1" >/dev/null
    fi
}
# The clone's own tracked files into this repo's index: listed from inside the clone (so its
# path needs no quoting) and added from there through this repo's git dir — regular files only
# (never a gitlink of its own submodules), none over the size limit (they stay out like anywhere
# else). Its .git is only read.
adopt() {
    (cd "./$1" && git ls-files -z | sed -z 's#^#./#' \
        | xargs -0 -r sh -c 'find "$@" -prune -type f ! -size +"$0"c -print0 2>/dev/null; exit 0' "$MAX" \
        | sed -z 's#^\./##' | GIT_DIR="$G" GIT_WORK_TREE="$TOP" git update-index --add -z --stdin)
}

if [ ! -e "$G/arena-nested" ]; then
    nested | while IFS= read -r d; do
        # A .git *file* (a worktree, an absorbed submodule) isn't a plain clone: keep it.
        if [ ! -d "$d/.git" ]; then continue; fi
        if ! git -C "$d" rev-parse -q --verify HEAD >/dev/null 2>&1; then continue; fi
        if ! own=$(git -C "$d" rev-list -n1 HEAD --not --remotes 2>/dev/null) || [ -n "$own" ]; then continue; fi
        unlink_gitlink "$d"
        adopt "$d" || true
        # Adopted = this repo tracks files in it now (else it's kept, below).
        if [ -n "$(git ls-files -- ":(literal)$d/" | head -n 1)" ]; then printf '%s\n' "$d"; fi
    done > "$T.adopted"
    mv -f "$T.adopted" "$G/arena-nested"
    sed 's/^/NESTED_ADOPTED /' "$G/arena-nested"
fi
nested > "$T.kept"
while IFS= read -r d; do
    unlink_gitlink "$d"
    echo "NESTED_KEPT $d"
done < "$T.kept"
# A gitlink whose directory has no .git any more (its repo was removed by hand) hides the files
# there the same way: out of the index, so they're committed.
git ls-files -z -s | grep -z '^160000 ' | cut -z -f2- | tr '\0' '\n' | while IFS= read -r d; do
    if [ -n "$d" ] && [ -d "$d" ] && [ ! -e "$d/.git" ] && ! is_submodule "$d"; then unlink_gitlink "$d"; fi
done
{
    echo '# BEGIN arena-backup'
    sed 's#[][*?\\]#\\&#g; s#^#/#; s#$#/#' "$T.kept"
    echo '# END arena-backup'
} >> "$E"

# --- Files GitHub refuses -------------------------------------------------------------------
# One file over GitHub's 100 MiB limit ($LIMIT) in a commit and every push of the branch is
# refused (GH001) — even after the file is deleted, since the blob stays in the unpushed history
# (live: a 110 MB checkpoint stopped a pod's git backup for good). So a file over $MAX (with
# headroom under the limit) that `add -A` would stage (new or changed, not ignored) is left out
# of the commit, never staged — its blob never written — and reported; the rsync big tier keeps it.
git ls-files -z -o -m --exclude-standard | sed -z 's#^#./#' \
    | xargs -0 -r sh -c 'find "$@" -prune -type f -size +"$0"c -print0 2>/dev/null; exit 0' "$MAX" \
    | tr '\0' '\n' > "$T.big"
set -- .
while IFS= read -r f; do
    f=${f#./}
    set -- "$@" ":(exclude,literal)$f"
    echo "BIGSKIP $(stat -c %s -- "$f" 2>/dev/null || echo 0) $f"
done < "$T.big"
busy_stop
git add -A -- "$@"
# One staged by hand (`git add` of a checkpoint) is taken back out of the index, before it's
# committed — the file itself untouched.
git diff --cached --name-only -z --diff-filter=AMT | sed -z 's#^#./#' \
    | xargs -0 -r sh -c 'find "$@" -prune -type f -size +"$0"c -print0 2>/dev/null; exit 0' "$MAX" \
    | tr '\0' '\n' > "$T.staged"
while IFS= read -r f; do
    f=${f#./}
    git reset -q -- ":(literal)$f"
    if ! grep -qxF -- "./$f" "$T.big"; then echo "BIGSKIP $(stat -c %s -- "$f" 2>/dev/null || echo 0) $f"; fi
done < "$T.staged"

# --- Keys pasted into files -----------------------------------------------------------------
# A key pasted into an exercise (live: an OpenRouter key in exercises.py, a notebook, a scratch
# file) was committed, and GitHub's push protection refused every push of the branch after it
# (GH013): ~20 hours of three pods' work never reached GitHub. So each new or changed regular file
# about to be committed is scanned for the keys redact.sh knows (rendered in above), and one that
# holds any is committed REDACTED: the redacted copy goes into the index only — the file on disk
# keeps the key, so the participant's code still runs. It shows as modified in `git status` from
# then on, and every run redacts it again to the blob HEAD already has: no new commit. Reported
# every run, like a file kept out for its size.
git diff --cached --raw --no-renames --diff-filter=d --no-abbrev \
    | awk '$2 == "100644" || $2 == "100755" { print $4 }' | sort -u \
    | while read -r b; do redact_blob "$b"; done > "$T.map"
redact_index "$T.map" > "$T.upd"
tr '\0' '\n' < "$T.upd" \
    | awk 'NR == FNR { n[$2] = $3; next } { i = index($0, "\t"); split(substr($0, 1, i - 1), m, " "); print "REDACTED " n[m[2]] " " substr($0, i + 1) }' "$T.map" -
busy_stop
if ! git diff --cached --quiet; then git commit -q -m "$MSG"; fi
# What this run pushes, fixed here (review ai-review codex #1, FIX7): every check below — what's
# ahead, the files over GitHub's limit, the keys — and the rewrite take this commit, never the live
# HEAD, and the push sends exactly it (or its rewrite) by an explicit refspec. The participant's
# own git is no party to the lock arena holds, so a commit they make while the scans run — a key
# pasted in after the scan, `git commit -am` — went out unscanned with a push of the branch by name.
# Now it stays local, and the next run scans it like any other.
TIP=$(git rev-parse HEAD)

# --- Push -----------------------------------------------------------------------------------
# Ahead of origin's copy of the branch (or origin has none yet) → there is something to push,
# whether it was committed just now or by an earlier run whose push failed: "nothing staged"
# isn't "nothing to do". Only a clean tree *and* HEAD == origin's branch is NO_CHANGES. Origin's
# — the remote this pushes to — never "any remote's" or the branch's configured upstream: setup
# adds the course repo as `upstream`, and a commit only it has (a `git pull upstream main`), or a
# branch made from `upstream/main` and tracking it, read as already pushed while origin had none
# of it (review ai-review claude #6).
if git rev-parse -q --verify "refs/remotes/origin/$B" >/dev/null; then
    AHEAD=$(git rev-list --count "refs/remotes/origin/$B..$TIP")
    set -- "refs/remotes/origin/$B..$TIP"
else
    AHEAD=new
    set -- "$TIP" --not --remotes=origin
fi
if [ "$AHEAD" = 0 ]; then echo "NO_CHANGES $B"; exit 0; fi
# A file over GitHub's own limit already in the unpushed commits (committed before this guard,
# or by hand): the push can't succeed, so say so — with how to fix it — instead of failing the
# same way every run. Judged by $LIMIT, never by $MAX: a file between the two (committed by hand)
# is one GitHub takes, and the push goes ahead.
git rev-list --objects "$@" | git cat-file --batch-check='%(objecttype) %(objectsize) %(rest)' \
    | awk -v max="$LIMIT" '$1 == "blob" && $2 + 0 > max + 0 { p = $0; sub(/^[^ ]+ [^ ]+ /, "", p); print "BIGSTUCK " $2 " " p }' \
    > "$T.stuck"
if [ -s "$T.stuck" ]; then
    cat "$T.stuck"
    if [ "$AHEAD" = new ]; then
        oldest=$(git rev-list "$@" | tail -n 1)
        base=$(git rev-parse -q --verify "$oldest^" 2>/dev/null) || base=none
    else
        base=$(git rev-parse "refs/remotes/origin/$B")
    fi
    echo "BIGSTUCK_BASE $base"
    echo "a file over $((LIMIT / 1048576)) MB is in the unpushed commits of $B: GitHub refuses every push of it (GH001) until they're rewritten." >&2
    if [ "$base" != none ]; then
        echo "To keep all the work and drop the file from git (it stays on disk): git -C '$REPO' reset -q $base — the next backup re-commits everything without it." >&2
    fi
    exit 3
fi
# What git push said, as GitHub's push protection words a refusal (GH013 since rulesets, GH009
# before): the refusal names the secret's type and where it is, never the secret — but it does
# carry a URL that *allows* the secret into the repo, which nothing here may print or pass on
# (`no_bypass_url`, shared with init-branches: arena_core::backup::NO_BYPASS_URL_SH).
push_protected() {
    LC_ALL=C grep -qiE 'GH013|GH009|push protection|push declined due to repository rule violations|cannot contain secrets|secrets? detected' "$1"
}
# @ARENA_NO_BYPASS_URL@
# The refusal in one line: GitHub's secret type(s) (the heading line above each `locations:`) and
# path:line(s), else the `[remote rejected]` reason. Never a URL.
push_block_reason() {
    _pt=$(LC_ALL=C sed -n '/locations:/{x;p;d;}; x' "$1" | LC_ALL=C sed 's/^remote://; s/^[^A-Za-z0-9]*//; s/[^A-Za-z0-9)]*$//' \
        | awk 'NF && !seen[$0]++' | head -n 3 | paste -sd ',' - | sed 's/,/, /g')
    _pp=$(LC_ALL=C sed -n 's/^remote: *path: *\([^ ]*\).*/\1/p' "$1" | tr -d '\r' | awk 'NF && !seen[$0]++' | head -n 3 \
        | paste -sd ',' - | sed 's/,/, /g')
    if [ -n "$_pt" ] && [ -n "$_pp" ]; then echo "$_pt at $_pp"; return 0; fi
    if [ -n "$_pt" ]; then echo "$_pt"; return 0; fi
    _pr=$(LC_ALL=C sed -n 's/.*\[remote rejected\][^(]*(\(.*\)).*/\1/p' "$1" | head -n 1)
    echo "${_pr:-refused by GitHub push protection}"
}

# --- Keys in the commits going out ----------------------------------------------------------
# The redaction above covers what this backup commits; a key can still be in the unpushed
# commits — committed by an older backup (live: three pods refused by GitHub for ~20 hours,
# 14-24 commits each, fixed by hand with filter-branch) or by the participant by hand ("commit
# all" on a file whose key the backup keeps out of git). Whether GitHub refuses it depends on
# the repo's push-protection settings; it must never leave the pod either way. So the blobs of
# the commits going out — only those on no branch of origin's, `$TIP --not --remotes=origin`,
# never anything this repo on GitHub already has (a commit only `upstream`, the course repo, has
# is scanned: origin has never had it) — are scanned before the push (one `cat-file --batch |
# grep -q`: usually a single commit this run made, already clean), and if one holds a key those
# commits are rewritten with it redacted, by plumbing alone: each commit's tree through a
# temporary index, then `commit-tree` with its own author, committer, dates, message and
# (mapped) parents, merges included. Deterministic, and a commit with nothing to redact and unchanged parents keeps its
# id. The participant's files are never read or written. The new tip is pushed as a
# fast-forward (never forced), and only then does the branch move — compare-and-swap, so a
# commit made meanwhile is never lost: the run stops there (HEAL_ABORTED), and the next one redoes
# the same rewrite (same ids: a fast-forward of what was pushed) with that commit on top.
# A key in a commit's *message* goes out the same way (review codex #1: `git commit -m '<a key>'`
# with clean files had nothing to redact, so the message went to GitHub as it was, and a rewrite
# for a file's key copied its message unchanged). So the same commits' objects are scanned too
# (one more `cat-file --batch | grep -q`), and on a hit the rewrite runs: each commit's message
# goes through the same sed as the files, and one whose message changes is rewritten — so are its
# descendants (their parents change). A hit outside every message (a key in an author's name) is
# no message's: the rewrite then changes nothing, every id kept. A rewrite for a file's key
# redacts the messages of the commits it writes the same way.
git rev-list --reverse --topo-order "$TIP" --not --remotes=origin > "$T.commits"
git rev-list --objects "$TIP" --not --remotes=origin | git cat-file --batch-check='%(objecttype) %(objectname) %(rest)' \
    | awk '$1 == "blob" { print $2 }' | sort -u > "$T.blobs"
# `scan_hits <list of objects>`: 0 when one of them holds a key, 1 when none does. A failed
# cat-file must not read as "no key" (the grep then saw only part of them): it stops the run.
scan_hits() {
    rm -f "$T.catfail"
    { git cat-file --batch < "$1" || : > "$T.catfail"; } | LC_ALL=C grep -aqE -- "$SECRET_RE" && _sk=0 || _sk=$?
    if [ "$_sk" = 1 ] && [ -e "$T.catfail" ]; then _sk=2; fi
    if [ "$_sk" -gt 1 ]; then echo "arena backup: scanning the outgoing commits for keys failed - nothing pushed" >&2; exit 2; fi
    return "$_sk"
}
: > "$T.hmap"
if [ -s "$T.blobs" ] && scan_hits "$T.blobs"; then
    while read -r b; do redact_blob "$b"; done < "$T.blobs" > "$T.hmap"
fi
: > "$T.cmap"
N=0
NEW=$TIP
if [ -s "$T.hmap" ] || { [ -s "$T.commits" ] && scan_hits "$T.commits"; }; then
    while read -r c; do
        ps=
        same=1
        for p in $(git log -1 --no-show-signature --format=%P "$c"); do
            np=$(sed -n "s/^$p //p" "$T.cmap")
            if [ -n "$np" ] && [ "$np" != "$p" ]; then same=; else np=$p; fi
            ps="$ps -p $np"
        done
        t=$(git rev-parse "$c^{tree}")
        nt=$t
        rm -f "$T.idx"
        GIT_INDEX_FILE="$T.idx" git read-tree "$t"
        redact_index "$T.hmap" "$T.idx" > "$T.changed"
        if [ -s "$T.changed" ]; then nt=$(GIT_INDEX_FILE="$T.idx" git write-tree); fi
        # Its message, each step its own command under `set -e`: a cat-file that fails stops the
        # run, never reads as an empty message.
        git cat-file commit "$c" > "$T.obj"
        sed '1,/^$/d' "$T.obj" > "$T.msg"
        mk=
        if key_in_file "$T.msg"; then
            LC_ALL=C sed -E "$REDACT_SED" "$T.msg" > "$T.rmsg"
            mv -f "$T.rmsg" "$T.msg"
            mk=1
        fi
        n=$c
        if [ "$nt" != "$t" ] || [ -z "$same" ] || [ -n "$mk" ]; then
            git log -1 --no-show-signature --date=raw --format='%an%n%ae%n%ad%n%cn%n%ce%n%cd' "$c" > "$T.who"
            { IFS= read -r an; IFS= read -r ae; IFS= read -r ad; IFS= read -r cn; IFS= read -r ce; IFS= read -r cdate; } < "$T.who"
            n=$(GIT_AUTHOR_NAME=$an GIT_AUTHOR_EMAIL=$ae GIT_AUTHOR_DATE=$ad GIT_COMMITTER_NAME=$cn GIT_COMMITTER_EMAIL=$ce \
                GIT_COMMITTER_DATE=$cdate git commit-tree --no-gpg-sign "$nt" $ps -F "$T.msg")
            N=$((N + 1))
        fi
        echo "$c $n" >> "$T.cmap"
    done < "$T.commits"
    NEW=$(sed -n "s/^$TIP //p" "$T.cmap")
fi

# The tip, or its redacted rewrite (as a fast-forward of the remote branch): that commit, by its
# id, to origin's branch — never the branch by name, which is wherever the participant has moved it
# since. That commit alone: `--no-follow-tags`, since a participant's `push.followTags` sent every
# annotated tag on the commits going out along with them, its message scanned by nothing.
if [ "$NEW" != "$TIP" ] && [ "$(git rev-parse HEAD)" != "$TIP" ]; then
    echo "HEAL_ABORTED $B moved while its unpushed commits were being redacted — the next backup redoes it"
    exit 1
fi
if git push --no-follow-tags origin "$NEW:refs/heads/$B" 2>"$T.push"; then pushed=1; else pushed=; fi
no_bypass_url < "$T.push" >&2
if [ -z "$pushed" ]; then
    # Still refused: a secret the patterns don't know (or a binary GitHub scans) — said, by type.
    if push_protected "$T.push"; then echo "PUSH_BLOCKED $(push_block_reason "$T.push")"; exit 4; fi
    echo "PUSH_FAILED"
    exit 1
fi
if [ "$NEW" != "$TIP" ]; then
    echo "REDACTED_HISTORY $N $TIP $NEW"
    if ! git update-ref -m "arena backup: keys redacted from unpushed commits" "refs/heads/$B" "$NEW" "$TIP"; then
        echo "HEAL_ABORTED $B moved while the redacted commits were pushed — the next backup puts the new commit on top"
        exit 1
    fi
    # The branch is checked out: its index entries holding a key now hold the redacted copy, so
    # the index matches the new tip and `git status` shows just the files on disk with their real
    # keys as modified — the state the redaction above leaves. No file on disk is touched.
    redact_index "$T.hmap" > /dev/null
fi
# The branch tracks origin's, as `push -u` made it (an id pushed sets no upstream), when it tracks
# nothing yet — written to the config itself: `--set-upstream-to=origin/$B` needs the
# remote-tracking ref, which a checkout whose `remote.origin.fetch` is narrowed (an image cloned
# `--single-branch`, `origin` repointed by setup) never gets, and that branch had no upstream for
# good — a bare `git push`/`git pull` saying so (review ai-review claude #7). An upstream the
# branch has already is left as it is.
if [ -z "$(git config "branch.$B.remote")" ]; then
    git config "branch.$B.remote" origin && git config "branch.$B.merge" "refs/heads/$B" || true
fi
echo "PUSHED $B"
