# The agent notes (arena_core::agent_notes): uv and uvx where every shell finds them, then this
# machine's Python facts in Claude Code's user memory (~/.claude/CLAUDE.md) and Codex's
# (~/.codex/AGENTS.md), between the markers: the block is replaced, or appended once, and
# everything else in the file stays as it was (an unchanged file isn't written at all; a changed
# one is rewritten in place, so its mode, owner and links stay). Run by `sh -c`, whatever the
# login shell. The @…@ placeholders are filled in by Rust; the block's text is Rust's too.
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
if command -v uv >/dev/null 2>&1; then I=@INSTALL_UV@; else I=@INSTALL_PIP@; fi
B=$(printf '%s\n' @BLOCK@)
arena_notes_write() {
    f=$1
    mkdir -p "${f%/*}" || return 1
    if [ ! -e "$f" ] && [ ! -L "$f" ]; then
        printf '%s\n' "$B" > "$f"
        return
    fi
    t=$(mktemp "$f.arena-XXXXXX") || return 1
    # The first marked block becomes the new one, any later one goes (one block, however many
    # runs wrote one); none: the block goes at the end, after a blank line. A BEGIN line with no
    # END after it leaves the file alone: where the participant's text starts again is unknown.
    ARENA_BEGIN=@BEGIN@ ARENA_END=@END@ ARENA_BLOCK=$B awk '
        BEGIN { b = ENVIRON["ARENA_BEGIN"]; e = ENVIRON["ARENA_END"]; blk = ENVIRON["ARENA_BLOCK"] }
        { l = $0; sub(/^[ \t]+/, "", l); sub(/[ \t\r]+$/, "", l) }
        inb { if (l == e) inb = 0; next }
        l == b { if (!done) print blk; done = 1; inb = 1; next }
        { print; n = NR; last = l }
        END {
            if (inb) exit 3
            if (!done) { if (n && last != "") print ""; print blk }
        }' "$f" > "$t"
    s=$?
    if [ "$s" = 3 ]; then
        rm -f "$t"
        echo "@WARN@$f has a BEGIN arena-infra python line with no END line after it - left as it is"
        return 0
    fi
    if [ "$s" != 0 ] || cmp -s "$t" "$f"; then
        rm -f "$t"
        return "$s"
    fi
    cat "$t" > "$f"
    s=$?
    rm -f "$t"
    return "$s"
}
rc=0
for f in "$HOME/.claude/CLAUDE.md" "$HOME/.codex/AGENTS.md"; do
    arena_notes_write "$f" || { echo "couldn't write the agent notes into $f" >&2; rc=1; }
done
exit "$rc"
