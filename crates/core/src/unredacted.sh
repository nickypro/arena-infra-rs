# arena set-branch: keep the participant's real keys across a branch switch.
#
# Rendered after redact.sh by arena_core::backup::checkout_command, which runs these around the
# switch, from the repo's top. The backup commits a file holding a key redacted and leaves the
# file on disk as it is (its real key in it), so the file reads as modified for good — and a
# switch to a branch with another version of it would refuse (`would be overwritten`) or, with
# --hard, throw the key away, which no commit has. So, before the switch, each such file is set
# aside — copied into a folder of this switch's own, $1/<UTC time>-<pid>/<path>, with the branch
# it was on in $1/<UTC time>-<pid>.branch (in .git: the rsync tiers back it up, no push carries
# it) — and put back to the version git has. A copy is never overwritten: the same path set aside
# again (another day, another branch, another key) is another copy. After the switch, a copy made
# on the branch now checked out goes back where the path holds exactly its redacted version (the
# commit the backup made). Prints, on stdout:
#   UNREDACTED_RESTORED <path>   its real keys are back in the file
#   UNREDACTED_KEPT <path>       set aside by this switch and kept (the branch has its own version)

# Each tracked file whose only difference from the index is keys the redaction knows: set aside.
# A file with any other uncommitted change is left alone — the switch treats it as it always did —
# and so is one that can't be copied (said on stderr): the loop never stops halfway.
unredacted_aside() {
    UR_RUN=$(date -u +%Y%m%dT%H%M%SZ)-$$
    git diff --name-only -z | tr '\0' '\n' | while IFS= read -r f; do
        if [ ! -f "$f" ] || [ -L "$f" ]; then continue; fi
        i=$(git ls-files -s -- ":(literal)$f" | awk '$1 == "100644" || $1 == "100755" { print $2 }')
        if [ -z "$i" ] || [ "$(git hash-object -- "$f")" = "$i" ]; then continue; fi
        if [ "$(LC_ALL=C sed -E "$REDACT_SED" < "$f" | git hash-object --path="$f" --stdin)" != "$i" ]; then continue; fi
        if mkdir -p -- "$1/$UR_RUN/$(dirname -- "$f")" && cp -p -- "$f" "$1/$UR_RUN/$f" && git checkout -q -- ":(literal)$f"; then
            :
        else
            echo "arena set-branch: couldn't set $f aside - left as it is" >&2
        fi
    done
    if [ -d "$1/$UR_RUN" ]; then git rev-parse --abbrev-ref HEAD > "$1/$UR_RUN.branch"; fi
}

# After the switch: the copies set aside on the branch now checked out, newest first and one per
# path, go back where the path holds exactly their redacted version; this switch's own copies
# that didn't are said (KEPT). Copies from other branches wait for a switch back to theirs.
unredacted_restore() {
    if [ ! -d "$1" ]; then return 0; fi
    cur=$(git rev-parse --abbrev-ref HEAD)
    : > "$1.done"
    for run in $(ls -1 "$1" | sort -r); do
        if [ ! -d "$1/$run" ] || [ "$(cat "$1/$run.branch" 2>/dev/null)" != "$cur" ]; then continue; fi
        (cd "$1/$run" && find . -type f) | sed 's#^\./##' | while IFS= read -r f; do
            if grep -qxF -- "$f" "$1.done"; then continue; fi
            if [ -f "$f" ] && [ ! -L "$f" ] \
                && [ "$(LC_ALL=C sed -E "$REDACT_SED" < "$1/$run/$f" | git hash-object --path="$f" --stdin)" = "$(git hash-object -- "$f")" ]; then
                cp -p -- "$1/$run/$f" "$f" && rm -f -- "$1/$run/$f" && printf '%s\n' "$f" >> "$1.done" && echo "UNREDACTED_RESTORED $f"
            fi
        done
    done
    if [ -n "${UR_RUN:-}" ] && [ -d "$1/$UR_RUN" ]; then
        (cd "$1/$UR_RUN" && find . -type f) | sed 's#^\./##' | while IFS= read -r f; do echo "UNREDACTED_KEPT $f"; done
    fi
    rm -f "$1.done"
    find "$1" -mindepth 1 -depth -type d -empty -delete 2>/dev/null || true
    for b in "$1"/*.branch; do
        if [ -f "$b" ] && [ ! -d "${b%.branch}" ]; then rm -f "$b"; fi
    done
    rmdir "$1" 2>/dev/null || true
}
