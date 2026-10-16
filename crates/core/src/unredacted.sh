# arena set-branch: keep the participant's real keys across a branch switch.
#
# Rendered after redact.sh by arena_core::backup::checkout_command, which runs these around the
# switch, from the repo's top. The backup commits a file holding a key redacted and leaves the
# file on disk as it is (its real key in it), so the file reads as modified for good — and a
# switch to a branch with another version of it would refuse (`would be overwritten`) or, with
# --hard, throw the key away, which no commit has. So, before the switch, each such file is set
# aside — copied to $1/<path> (in .git, which the rsync tiers back up and no push carries) — and
# put back to the version git has; after the switch, a set-aside file whose path holds exactly
# that version again (back on its branch) gets its real keys back. Prints, on stdout:
#   UNREDACTED_RESTORED <path>   its real keys are back in the file
#   UNREDACTED_KEPT <path>       still set aside in $1 (the branch has its own version of the file)

# Each tracked file whose only difference from the index is keys the redaction knows: set aside.
# A file with any other uncommitted change is left alone — the switch treats it as it always did.
unredacted_aside() {
    git diff --name-only -z | tr '\0' '\n' | while IFS= read -r f; do
        if [ ! -f "$f" ] || [ -L "$f" ]; then continue; fi
        i=$(git ls-files -s -- ":(literal)$f" | awk '$1 == "100644" || $1 == "100755" { print $2 }')
        if [ -z "$i" ] || [ "$(git hash-object -- "$f")" = "$i" ]; then continue; fi
        if [ "$(LC_ALL=C sed -E "$REDACT_SED" < "$f" | git hash-object --path="$f" --stdin)" != "$i" ]; then continue; fi
        mkdir -p -- "$1/$(dirname -- "$f")"
        cp -p -- "$f" "$1/$f"
        git checkout -q -- ":(literal)$f"
    done
}

# Each set-aside file whose path holds its redacted version again: its real keys back.
unredacted_restore() {
    if [ ! -d "$1" ]; then return 0; fi
    (cd "$1" && find . -type f) | sed 's#^\./##' | while IFS= read -r f; do
        if [ -f "$f" ] && [ ! -L "$f" ] \
            && [ "$(LC_ALL=C sed -E "$REDACT_SED" < "$1/$f" | git hash-object --path="$f" --stdin)" = "$(git hash-object -- "$f")" ]; then
            cp -p -- "$1/$f" "$f"
            rm -f -- "$1/$f"
            echo "UNREDACTED_RESTORED $f"
        else
            echo "UNREDACTED_KEPT $f"
        fi
    done
    find "$1" -depth -type d -empty -delete 2>/dev/null || true
}
