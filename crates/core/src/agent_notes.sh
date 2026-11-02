# The agent notes (arena_core::agent_notes): uv and uvx where every shell finds them, then this
# machine's Python facts in Claude Code's user memory (~/.claude/CLAUDE.md) and Codex's
# (~/.codex/AGENTS.md), between the markers: the block is replaced, or appended once, and
# everything else in the file stays as it was (an unchanged file isn't written at all; a changed
# one is swapped for the new one in one step, with its mode and owner, through a link the file it
# points at, and the old one is kept beside it).
# Run by `sh -c`, whatever the login shell. The @…@ placeholders are filled in by Rust; the
# block's text is Rust's too.
@LINK_UV@
E=
for c in @ENVS@; do
    case $c in "~/"*) c=$HOME/${c#"~/"} ;; esac
    if [ -x "$c/bin/python" ]; then E=$c; break; fi
done
if [ -z "$E" ]; then
    echo @NO_ENV@
    exit 0
fi
R=@REPO@
# A temp file a run cut short (its ssh dropped) would be left beside the participant's file.
t=
trap 'rm -f "$t"' EXIT
trap 'rm -f "$t"; exit 1' HUP INT TERM
if [ -f "$E/bin/activate" ]; then A=@ACTIVATE_VENV@; else A=@ACTIVATE_PATH@; fi
if command -v uv >/dev/null 2>&1; then I=@INSTALL_UV@; else I=@INSTALL_PIP@; fi
B=$(printf '%s\n' @BLOCK@)
# renameat2(2) with RENAME_EXCHANGE: $1 and $2 swapped in one step (as the VS Code warm-up's
# settings merge does). Status 2: no such swap here (no python3, a system or filesystem without
# it); 1: it failed.
arena_swap() {
    command -v python3 >/dev/null 2>&1 || return 2
    python3 - "$1" "$2" <<'PY'
import ctypes, errno, os, sys
try:
    fn = ctypes.CDLL(None, use_errno=True).renameat2
except (OSError, AttributeError):
    sys.exit(2)
fn.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
if fn(-100, os.fsencode(sys.argv[1]), -100, os.fsencode(sys.argv[2]), 2) == 0:
    sys.exit(0)
sys.exit(2 if ctypes.get_errno() in (errno.EINVAL, errno.ENOSYS, errno.EOPNOTSUPP) else 1)
PY
}
# The block into file $1. Nothing of the participant's is ever overwritten or deleted: a file
# that isn't there is created only if it still isn't; one that is gets the new text swapped in,
# and what came out is kept beside it ($1.arena-XXXXXX, never deleted: an editor holding it open
# may still write into it). If that isn't what was read (a save landed meanwhile: the
# participant, Claude Code's own memory, another setup), it goes straight back, and round again.
arena_notes_write() {
    f=$1
    mkdir -p "${f%/*}" || return 1
    # Through a link, the file it points at is rewritten (the link stays a link).
    if [ -L "$f" ]; then r=$(readlink -f "$f") || return 1; else r=$f; fi
    for arena_try in 1 2 3; do
        # $t holds only text of ours: the traps delete it, nothing else.
        t=$(mktemp "$r.arena-XXXXXX") || return 1
        if [ ! -e "$r" ]; then
            # A hard link never replaces a file: one made meanwhile is updated next time round.
            if printf '%s\n' "$B" > "$t" && chmod 644 "$t" && ln "$t" "$r" 2>/dev/null; then s=0; else s=1; fi
            rm -f "$t"
            t=
            if [ "$s" = 1 ] && [ -e "$r" ]; then continue; fi
            return "$s"
        fi
        was=$(cksum < "$r") || { rm -f "$t"; t=; return 1; }
        # The first marked block becomes the new one, any later one goes (one block, however many
        # runs wrote one); none: the block goes at the end, after a blank line. A BEGIN line with
        # no END after it leaves the file alone: where the participant's text starts again is
        # unknown. Every other byte is kept, a last line without a newline too (ARENA_NL: 0).
        ARENA_NL=$(tail -c 1 "$r" | wc -l) ARENA_BEGIN=@BEGIN@ ARENA_END=@END@ ARENA_BLOCK=$B awk '
            BEGIN { b = ENVIRON["ARENA_BEGIN"]; e = ENVIRON["ARENA_END"]; blk = ENVIRON["ARENA_BLOCK"]; nonl = ENVIRON["ARENA_NL"] + 0 == 0 }
            function out(s, nr) { if (have) print pend; pend = s; pendnr = nr; have = 1 }
            { l = $0; sub(/^[ \t]+/, "", l); sub(/[ \t\r]+$/, "", l) }
            inb { if (l == e) inb = 0; next }
            l == b { if (!done) out(blk, 0); done = 1; inb = 1; next }
            { out($0, NR); n = NR; last = l }
            END {
                if (inb) exit 3
                if (!done) { if (n && last != "") out("", 0); out(blk, 0) }
                if (have) { if (nonl && pendnr && pendnr == NR) printf "%s", pend; else print pend }
            }' "$r" > "$t"
        s=$?
        if [ "$s" = 3 ]; then
            rm -f "$t"
            t=
            echo "@WARN@$f has a BEGIN arena-infra python line with no END line after it - left as it is"
            return 0
        fi
        if [ "$s" != 0 ] || cmp -s "$t" "$r"; then
            rm -f "$t"
            t=
            return "$s"
        fi
        ours=$(cksum < "$t")
        if ! chmod --reference="$r" "$t"; then
            rm -f "$t"
            t=
            return 1
        fi
        chown --reference="$r" "$t" 2>/dev/null || true
        # From here $k may hold what the participant's file held: no trap deletes it.
        k=$t
        t=
        arena_swap "$k" "$r"
        s=$?
        if [ "$s" = 2 ]; then
            rm -f "$k"
            echo "@WARN@$f can't be swapped for the new one in one step here (no python3 or renameat2) - left as it is"
            return 0
        elif [ "$s" != 0 ]; then
            # Swapped or not, $k says: our text alone goes, anything else stays.
            if [ "$(cksum < "$k")" = "$ours" ]; then rm -f "$k"; else echo "@WARN@$f: the swap failed - what it held is kept as $k"; fi
            return 1
        fi
        [ "$(cksum < "$k")" = "$was" ] && return 0
        # A save landed between the read and the swap: it goes back, and round again. What comes
        # out (our text, or one saved in that instant) is kept too.
        if ! arena_swap "$k" "$r"; then
            echo "@WARN@$f changed while the notes were being written, and the swap back failed - what it held is kept as $k"
            return 1
        fi
        [ "$(cksum < "$k")" = "$ours" ] || echo "@WARN@$f changed while the notes were being written - a save made meanwhile is kept as $k"
    done
    echo "@WARN@$f kept changing while the notes were being written - left as it is (pods setup --notes-only writes them)"
}
rc=0
for f in "$HOME/.claude/CLAUDE.md" "$HOME/.codex/AGENTS.md"; do
    arena_notes_write "$f" || { echo "couldn't write the agent notes into $f" >&2; rc=1; }
done
exit "$rc"
