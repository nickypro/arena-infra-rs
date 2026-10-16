# arena secret redaction: shell functions the backup script and `set-branch` share.
#
# Rendered by arena_core::backup (redact_sh): the two patterns below are generated from
# arena_core::backup::SECRET_PATTERNS — edit them there, never here — so the Rust table tests
# and what runs on the pods can't drift. GNU sed/grep (`\b`), matched as bytes (LC_ALL=C).
# Nothing here prints a key: only blob ids, counts and paths.
#
# SECRET_RE   one key of any kind, with the boundary before it (a key inside a longer word is
#             no key: `task-or-v1-…` isn't `sk-or-v1-…`; a JSON escape like `\n` counts as one)
# REDACT_SED  each kind of key -> its REDACTED-… text, which no pattern matches (idempotent)
SECRET_RE='@SECRET_RE@'
REDACT_SED='@REDACT_SED@'

# A blob's redacted copy, written to the object store: `<blob> <redacted blob> <keys>` on
# stdout — nothing for a blob without a key, or a binary one (a NUL in its first 8000 bytes,
# git's own test): its bytes aren't text to rewrite, and git shows no diff of it anyway.
redact_blob() {
    git cat-file blob "$1" | LC_ALL=C grep -aqE -- "$SECRET_RE" || return 0
    _rs=$(git cat-file -s "$1")
    if [ "$_rs" -gt 8000 ]; then _rs=8000; fi
    if [ "$(git cat-file blob "$1" | head -c 8000 | tr -d '\000' | wc -c)" -ne "$_rs" ]; then return 0; fi
    _rn=$(git cat-file blob "$1" | LC_ALL=C grep -aoE -- "$SECRET_RE" | wc -l)
    _rr=$(git cat-file blob "$1" | LC_ALL=C sed -E "$REDACT_SED" | git hash-object -w --stdin)
    if [ "$_rr" != "$1" ]; then echo "$1 $_rr $_rn"; fi
}

# Point every regular-file entry of an index — $2, else the repo's own — that holds a blob of
# map $1 (redact_blob's lines) at its redacted copy, and print the entries it changed
# (`<mode> <redacted blob> 0<TAB><path>`, NUL-terminated). The index only: no file on disk
# is read or written. Symlinks (120000) and gitlinks (160000) never match.
redact_index() {
    if [ ! -s "$1" ]; then return 0; fi
    while read -r _ro _rr _rn; do
        printf 's/^\\(100644\\|100755\\) %s 0\\t/\\1 %s 0\\t/p\n' "$_ro" "$_rr"
    done < "$1" > "$1.sed"
    (
        if [ -n "${2:-}" ]; then export GIT_INDEX_FILE="$2"; fi
        git ls-files -s -z | sed -z -n -f "$1.sed" > "$1.upd"
        git update-index -z --index-info < "$1.upd"
        cat "$1.upd"
    )
}
