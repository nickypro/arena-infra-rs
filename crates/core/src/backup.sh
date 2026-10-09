# arena backup: commit + push one pod's ARENA checkout on the branch it is on.
#
#   sh -c "$THIS_SCRIPT" arena-backup <repo> <commit message> <max file bytes> <push limit bytes> [<git ssh key>]
#
# Rendered and parsed by arena_core::backup (backup_command / parse_backup_report); runs under
# plain `sh` (dash on the images), whatever the pod's login shell. Prints, on stdout:
#   SKIP <branch>              on main/master/a detached HEAD: nothing done (exit 0)
#   NO_CHANGES <branch>        clean tree and HEAD == its upstream: nothing to push (exit 0)
#   PUSHED <branch>            committed what there was and/or pushed unpushed commits (exit 0)
#   BIGSKIP <bytes> <path>     a file over <max file bytes>, kept out of the commit (stays on disk)
#   NESTED_ADOPTED <path>      a nested clone whose files this repo now tracks (its .git untouched)
#   NESTED_KEPT <path>         a nested repo left as it is and kept out of the commit
#   BIGSTUCK <bytes> <path>    a file over <push limit bytes> already in unpushed commits (exit 3)
#   BIGSTUCK_BASE <sha>|none   what `git reset` to, to rewrite those commits without it
# Any other failure exits non-zero via `set -e` (a rejected push is a failure).
set -e
REPO=$1
MSG=$2
MAX=$3
LIMIT=$4
KEY=${5:-}
cd "$REPO"
cd "$(git rev-parse --show-toplevel)"
TOP=$(pwd)
if [ -n "$KEY" ]; then
    export GIT_SSH_COMMAND="ssh -i $KEY -o StrictHostKeyChecking=accept-new -o BatchMode=yes"
fi
# Stay on the current branch; never push a protected/detached one.
B=$(git rev-parse --abbrev-ref HEAD)
if [ "$B" = main ] || [ "$B" = master ] || [ "$B" = HEAD ]; then echo "SKIP $B"; exit 0; fi
# A committer identity, in case the pod has none configured.
git config user.name >/dev/null 2>&1 || git config user.name 'Arena Autocommit'
git config user.email >/dev/null 2>&1 || git config user.email 'autocommit@arena.education'
G=$(git rev-parse --absolute-git-dir)
T="$G/arena-backup.$$"
trap 'rm -f "$T".*' EXIT

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
if ! git diff --cached --quiet; then git commit -q -m "$MSG"; fi

# --- Push -----------------------------------------------------------------------------------
# Ahead of the upstream (or no upstream yet) → there is something to push, whether it was
# committed just now or by an earlier run whose push failed: "nothing staged" isn't "nothing to
# do". Only a clean tree *and* HEAD == @{u} is NO_CHANGES.
if git rev-parse --abbrev-ref --symbolic-full-name '@{u}' >/dev/null 2>&1; then
    AHEAD=$(git rev-list --count '@{u}..HEAD')
    set -- '@{u}..HEAD'
else
    AHEAD=new
    set -- HEAD --not --remotes
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
        base=$(git rev-parse '@{u}')
    fi
    echo "BIGSTUCK_BASE $base"
    echo "a file over $((LIMIT / 1048576)) MB is in the unpushed commits of $B: GitHub refuses every push of it (GH001) until they're rewritten." >&2
    if [ "$base" != none ]; then
        echo "To keep all the work and drop the file from git (it stays on disk): git -C '$REPO' reset -q $base — the next backup re-commits everything without it." >&2
    fi
    exit 3
fi
git push -u origin "$B"
echo "PUSHED $B"
