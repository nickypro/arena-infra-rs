# arena resolve: finish an operation left in progress in one pod's checkout once nothing in it has
# changed for a while — what `pods backup` runs on a pod whose backup said BUSY, after copying it.
#
#   sh -c "$THIS_SCRIPT" arena-resolve <repo> <quiet secs> <op> <print> <hours> <copy>
#
# Rendered and parsed by arena_core::resolve (resolve_command / parse_resolve). <op> (`OP`'s
# `<what> <mtime> <commit>`) and <print> (`PRINT`) are what the look before the copy saw: the
# checkout is looked at again first, and anything changed since — or younger than <quiet secs>,
# or unusual — and nothing is touched. Then each conflicted path is kept as the participant
# resolved it by hand (a regular text file with no conflict markers left, both sides there),
# else taken from the side whose commit is newer (committer time; a tie keeps HEAD's) — removed
# when that side deleted it; the operation is finished (a merge: a merge commit; a cherry-pick or
# revert: `--continue`; a `git stash pop`'s conflicts: added, the stash entry kept), in the
# repo's own identity. The conflicted paths' files as they were are kept first in
# <git dir>/arena-resolved/<UTC stamp>/files/. <hours> and <copy> (where the backup copy is)
# are for the words. Prints, on stdout:
#   REFUSED <why>            nothing touched (exit 7)
#   SAVED <dir>              where the conflicted paths' files were kept as they were
#   NEWER <path>             taken from the newer side
#   GONE <path>              removed: the newer side deleted it
#   HAND <path>              kept as it is on disk (resolved by hand) and added
#   RESOLVED <what> <ours|theirs> <HEAD's committer time> <the other side's>   finished (exit 0)
#   FAILED <step>            a step failed midway (exit 8): SAVED has the files as they were
set -u
REPO=$1
SECS=$2
WANT_OP=$3
WANT_PRINT=$4
HOURS=$5
COPY=$6
cd "$REPO" || exit 2
cd "$(git rev-parse --show-toplevel)" || exit 2
TOP=$(pwd)
G=$(git rev-parse --absolute-git-dir) || exit 2
# @ARENA_BUSY_OP_SH@
# @ARENA_LOOK_SH@
refuse() { echo "REFUSED $*"; exit 7; }
fail() { echo "FAILED $*"; exit 8; }
LOOKED=$(arena_look)
val() { printf '%s\n' "$LOOKED" | sed -n "s/^$1 //p" | head -n 1; }

# --- The same, and quiet ---------------------------------------------------------------------
set -- $(val OP)
what=${1:-none}
case $what in
    merge | cherry-pick | revert | conflicts) ;;
    none) refuse "changed: nothing is in progress any more" ;;
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
now=$(val NOW)
set -- $(val NEWEST)
newest=${1:-0}
input=$(val INPUT)
[ $((now - mt)) -ge "$SECS" ] || refuse "recent: the $what started less than ${HOURS}h ago"
[ $((now - newest)) -ge "$SECS" ] || refuse "recent: a file changed less than ${HOURS}h ago"
[ -z "$input" ] || [ $((now - input)) -ge "$SECS" ] || refuse "recent: a terminal login typed less than ${HOURS}h ago"
[ -n "$(git config user.name)" ] && [ -n "$(git config user.email)" ] || refuse "no git identity configured in the repo"

# --- Which side is newer ---------------------------------------------------------------------
case $what in
    merge) other=MERGE_HEAD ;;
    cherry-pick) other=CHERRY_PICK_HEAD ;;
    revert) other=REVERT_HEAD ;;
    *) other='the stash' ;;
esac
[ "$head" != - ] || refuse "conflicts with no stash entry they came from"
if [ "$what" = merge ] && [ "$(wc -l < "$G/MERGE_HEAD")" -ne 1 ]; then refuse "a merge of several branches"; fi
ours_t=$(git log -1 --no-show-signature --format=%ct HEAD 2>/dev/null) || refuse "no HEAD commit"
theirs_t=$(git log -1 --no-show-signature --format=%ct "$head" 2>/dev/null) || refuse "$other's commit is missing"
if [ "$theirs_t" -gt "$ours_t" ]; then win=theirs; s=3; else win=ours; s=2; fi

# --- What to do with each conflicted path (nothing touched yet) ------------------------------
T="$G/arena-resolve.$$"
trap 'rm -f "$T".*' EXIT
TAB=$(printf '\t')
git ls-files -u -z > "$T.u" || refuse "the conflicts couldn't be listed"
[ "$(tr -d -c '\n' < "$T.u" | wc -c)" -eq 0 ] || refuse "a conflicted path has a newline in its name"
if tr '\0' '\n' < "$T.u" | grep -q '^160000 '; then refuse "a submodule is in conflict"; fi
# One line per path: its stages (e.g. 123, 13), stage 3's blob (- for none), the path.
tr '\0' '\n' < "$T.u" | awk '
    { i = index($0, "\t"); split(substr($0, 1, i - 1), m, " "); p = substr($0, i + 1)
      if (!(p in st)) { order[n++] = p; st[p] = "" }
      st[p] = st[p] m[3]; if (m[3] == 3) b3[p] = m[2] }
    END { for (k = 0; k < n; k++) { p = order[k]; printf "%s\t%s\t%s\n", st[p], (p in b3) ? b3[p] : "-", p } }' > "$T.p"
: > "$T.plan"
while IFS="$TAB" read -r st b3 p; do
    if [ -d "$p" ] && [ ! -L "$p" ]; then refuse "a directory is where $p is in conflict"; fi
    if [ "$what" = conflicts ]; then
        # The other side must be that stash entry's (`git stash pop` keeps it on a conflict).
        want=$(git rev-parse -q --verify "$head:$p" 2>/dev/null) || want=-
        [ "$want" = "$b3" ] || refuse "conflicts that aren't the newest stash entry's ($p)"
    fi
    act=
    case $st in
        *2*3*)
            # Resolved by hand: a regular text file (no NUL in its first 8000 bytes, as git judges;
            # not merged as binary) with no conflict marker left. Both sides are there: a path one
            # side deleted is left by git without markers, unresolved.
            if [ -f "$p" ] && [ ! -L "$p" ] && [ "$(head -c 8000 -- "$p" | tr -d -c '\000' | wc -c)" -eq 0 ] \
                && ! git check-attr merge -- "$p" | grep -qE ': merge: (unset|binary)$' \
                && ! grep -qE -e '^(<<<<<<<|>>>>>>>)( |$)' -- "$p"; then
                act=hand
            fi
            ;;
    esac
    if [ -z "$act" ]; then
        case $st in
            *"$s"*) act=$win ;;
            *) act=gone ;;
        esac
    fi
    printf '%s\t%s\n' "$act" "$p" >> "$T.plan"
done < "$T.p"

# --- The files as they were, kept first ------------------------------------------------------
S="$G/arena-resolved/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$G/arena-resolved" || refuse "the files as they were couldn't be kept"
if ! mkdir "$S" 2>/dev/null; then
    S="$S-$$"
    mkdir "$S" || refuse "the files as they were couldn't be kept"
fi
{
    echo "arena finished a $what left in progress for ${HOURS}h+ (the backup copy: $COPY)"
    echo "HEAD $(git rev-parse HEAD) ($ours_t)"
    echo "$other $head ($theirs_t)"
    echo "kept the newer side: $win"
} > "$S/README"
tr '\0' '\n' < "$T.u" > "$S/unmerged"
mkdir "$S/files" || refuse "the files as they were couldn't be kept"
if ! cut -f2- "$T.plan" | while IFS= read -r p; do
    if [ -e "$p" ] || [ -L "$p" ]; then
        case $p in */*) mkdir -p "$S/files/${p%/*}" || exit 1 ;; esac
        cp -P -p -- "$p" "$S/files/$p" || exit 1
    fi
done; then
    rm -rf "$S"
    refuse "the files as they were couldn't be kept"
fi
echo "SAVED $S"

# --- Resolve ---------------------------------------------------------------------------------
while IFS="$TAB" read -r act p; do
    case $act in
        hand)
            git add -- ":(literal)$p" || fail "git add $p"
            echo "HAND $p"
            ;;
        gone)
            git rm -q -f -- ":(literal)$p" > /dev/null || fail "git rm $p"
            echo "GONE $p"
            ;;
        *)
            git checkout "--$act" -- ":(literal)$p" || fail "git checkout --$act $p"
            git add -- ":(literal)$p" || fail "git add $p"
            echo "NEWER $p"
            ;;
    esac
done < "$T.plan"
[ -z "$(git ls-files -u | head -n 1)" ] || fail "conflicts were left"

# --- Finish ----------------------------------------------------------------------------------
day() { date -u -d "@$1" +%Y-%m-%d; }
# The paths of the plan's lines whose action is one of $1 (a regexp), joined with ", ".
paths() { awk -v a="^($1)\$" -F "$TAB" '$1 ~ a { printf "%s%s", (n++ ? ", " : ""), substr($0, index($0, "\t") + 1) } END { print "" }' "$T.plan"; }
newer=$(paths 'ours|theirs|gone')
hand=$(paths hand)
words=
if [ -n "$newer" ]; then words="kept the newer side (HEAD $(day "$ours_t") / $other $(day "$theirs_t")) for $newer"; fi
if [ -n "$hand" ]; then words="${words:+$words; }kept your hand-resolved $hand"; fi
words="${words:-nothing was left in conflict}; the state before is in the backup copy $COPY and on this machine in ${S#"$TOP"/}/"
case $what in
    merge)
        {
            echo "arena: finished a merge left in progress for ${HOURS}h+"
            echo
            echo "Nothing in this checkout changed for ${HOURS}h+ while a merge was in progress, so arena finished it: $words."
            if [ -s "$G/MERGE_MSG" ]; then
                echo
                grep -v '^#' "$G/MERGE_MSG"
            fi
        } > "$T.msg"
        GIT_EDITOR=true git commit -q --no-edit --cleanup=strip -F "$T.msg" || fail "git commit"
        ;;
    cherry-pick | revert)
        printf '\narena: finished this %s, left in progress for %sh+ — %s.\n' "$what" "$HOURS" "$words" >> "$G/MERGE_MSG"
        if git diff --cached --quiet HEAD --; then
            # Nothing left to change (the newer side was HEAD's everywhere): `--continue` refuses an
            # empty commit, so the commit it would make, empty — git's own advice.
            GIT_EDITOR=true git commit -q --allow-empty --no-edit --cleanup=strip || fail "git commit --allow-empty"
        else
            GIT_EDITOR=true git "$what" --continue > /dev/null || fail "git $what --continue"
        fi
        ;;
esac
case $what in
    merge) [ ! -e "$G/MERGE_HEAD" ] ;;
    cherry-pick) [ ! -e "$G/CHERRY_PICK_HEAD" ] ;;
    revert) [ ! -e "$G/REVERT_HEAD" ] ;;
esac || fail "the $what is still in progress"
echo "RESOLVED $what $win $ours_t $theirs_t"
