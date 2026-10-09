# arena backup: commit + push one pod's ARENA checkout on the branch it is on.
#
#   sh -c "$THIS_SCRIPT" arena-backup <repo> <commit message> <max file bytes> [<git ssh key>]
#
# Rendered and parsed by arena_core::backup (backup_command / parse_backup_report); runs under
# plain `sh` (dash on the images), whatever the pod's login shell. Prints, on stdout:
#   SKIP <branch>              on main/master/a detached HEAD: nothing done (exit 0)
#   NO_CHANGES <branch>        clean tree and HEAD == its upstream: nothing to push (exit 0)
#   PUSHED <branch>            committed what there was and/or pushed unpushed commits (exit 0)
#   BIGSKIP <bytes> <path>     a file over the limit, kept out of the commit (stays on disk)
#   NESTED_FOLDED <path>       a nested clone folded into this repo (its .git moved aside)
#   NESTED_KEPT <path>         a nested repo left as it is and kept out of the commit
#   BIGSTUCK <bytes> <path>    a file over the limit already in unpushed commits (exit 3)
#   BIGSTUCK_BASE <sha>|none   what `git reset` to, to rewrite those commits without it
# Any other failure exits non-zero via `set -e` (a rejected push is a failure).
set -e
REPO=$1
MSG=$2
MAX=$3
KEY=${4:-}
cd "$REPO"
cd "$(git rev-parse --show-toplevel)"
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
# - FOLD, once per checkout (the marker $G/arena-nested lists what it folded): a nested clone
#   with no commits of its own (HEAD on one of its remote-tracking refs — the image's clones of
#   upstream repos) has its .git moved aside to .git.arena-nested — reversible (move it back);
#   its files stay where they are and become part of this repo, so the participants' work in
#   them is committed and pushed like everything else.
# - KEEP, every run: any other nested repo (one with commits of its own — a participant's
#   project — or one appearing after the fold) is left exactly as it is, kept out of the commit
#   through a managed block in .git/info/exclude, and reported. The rsync tiers back it up.
ASIDE=.git.arena-nested
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
nested() {
    { git ls-files -z -o --exclude-standard -x "$ASIDE/" | grep -z '/$' | sed -z 's#/$##'
      git ls-files -z -s | grep -z '^160000 ' | cut -z -f2-
    } | tr '\0' '\n' | while IFS= read -r d; do
        if [ -n "$d" ] && [ -e "$d/.git" ] && ! is_submodule "$d"; then printf '%s\n' "$d"; fi
    done
}
# A gitlink still in the index (an earlier backup's) out of it — the index only, never a file —
# so its directory is committed as files (folded) or not at all (kept).
unlink_gitlink() {
    if git ls-files -s -- ":(literal)$1" | grep -q '^160000 '; then
        git rm -q --cached -f -- ":(literal)$1" >/dev/null
    fi
}

if [ ! -e "$G/arena-nested" ]; then
    nested | while IFS= read -r d; do
        # A .git *file* (a worktree, an absorbed submodule) isn't a plain clone: keep it.
        if [ ! -d "$d/.git" ] || [ -e "$d/$ASIDE" ]; then continue; fi
        if ! git -C "$d" rev-parse -q --verify HEAD >/dev/null 2>&1; then continue; fi
        if ! own=$(git -C "$d" rev-list -n1 HEAD --not --remotes 2>/dev/null) || [ -n "$own" ]; then continue; fi
        if mv "$d/.git" "$d/$ASIDE"; then
            unlink_gitlink "$d"
            printf '%s\n' "$d"
        fi
    done > "$T.folded"
    mv -f "$T.folded" "$G/arena-nested"
    sed 's/^/NESTED_FOLDED /' "$G/arena-nested"
fi
# A clone folded before that has a .git again — the image's own, after `pods replace` copied
# this checkout (its index, the folded files and this list) onto a new pod — is folded again:
# this repo tracks its files already, and must not see them inside a nested repo. (To undo a
# fold for good: move the .git back and delete its line from $G/arena-nested.)
if [ -s "$G/arena-nested" ]; then
    while IFS= read -r d; do
        if [ -n "$d" ] && [ -d "$d/.git" ] && [ ! -e "$d/$ASIDE" ] && mv "$d/.git" "$d/$ASIDE"; then
            unlink_gitlink "$d"
            echo "NESTED_FOLDED $d"
        fi
    done < "$G/arena-nested"
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
    echo "$ASIDE/"
    sed 's#[][*?\\]#\\&#g; s#^#/#; s#$#/#' "$T.kept"
    echo '# END arena-backup'
} >> "$E"

# --- Files GitHub refuses -------------------------------------------------------------------
# One file over GitHub's 100 MiB limit in a commit and every push of the branch is refused
# (GH001) — even after the file is deleted, since the blob stays in the unpushed history (live:
# a 110 MB checkpoint stopped a pod's git backup for good). So a file over $MAX that `add -A`
# would stage (new or changed, not ignored) is left out of the commit, never staged — its blob
# never written — and reported; the rsync big tier keeps it.
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
# An over-limit file already in the unpushed commits (committed before this guard, or by
# hand): the push can't succeed, so say so — with how to fix it — instead of failing the same
# way every run.
git rev-list --objects "$@" | git cat-file --batch-check='%(objecttype) %(objectsize) %(rest)' \
    | awk -v max="$MAX" '$1 == "blob" && $2 + 0 > max + 0 { p = $0; sub(/^[^ ]+ [^ ]+ /, "", p); print "BIGSTUCK " $2 " " p }' \
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
    echo "a file over $((MAX / 1048576)) MB is in the unpushed commits of $B: GitHub refuses every push of it (GH001) until they're rewritten." >&2
    if [ "$base" != none ]; then
        echo "To keep all the work and drop the file from git (it stays on disk): git -C '$REPO' reset -q $base — the next backup re-commits everything without it." >&2
    fi
    exit 3
fi
git push -u origin "$B"
echo "PUSHED $B"
