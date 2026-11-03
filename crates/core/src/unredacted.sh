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
# A file with any other uncommitted change is left alone — the switch treats it as it always did,
# a chmod included — and so is one that can't be copied (said on stderr), one under a symlinked
# directory, and one whose directory isn't on the git dir's filesystem: the loop never stops
# halfway. The file is put back to git's version — its redacted text, which was just checked to
# be that — by a rename, so it's whole either way; then the index's stat data is refreshed, which
# stages nothing.
# Stopped anywhere (the bound's TERM, the EXIT trap then restoring; or a KILL, and a later
# switch's restore), nothing is lost: the run's branch is written before its first copy (a run is
# restored by its branch: written at the end, a file already put back lost its key); every copy,
# here and in the restore, is made beside the runs and renamed into place, so a file is only ever
# the participant's version or git's, both of which any later restore recognises.
unredacted_aside() {
    UR_RUN=$(date -u +%Y%m%dT%H%M%SZ)-$$
    UR_BRANCH=$(git rev-parse --abbrev-ref HEAD) || UR_BRANCH=
    UR_MODES=$(git config --bool core.fileMode 2>/dev/null) || UR_MODES=true
    git diff --name-only -z | tr '\0' '\n' | while IFS= read -r f; do
        if [ ! -f "$f" ] || [ -L "$f" ] || ! unredacted_in_place "$1" "$f"; then continue; fi
        e=$(git ls-files -s -- ":(literal)$f" | awk '$1 == "100644" || $1 == "100755" { print $1, $2 }')
        m=${e%% *}
        i=${e#* }
        if [ -z "$e" ] || [ "$(git hash-object -- "$f")" = "$i" ]; then continue; fi
        if [ -x "$f" ]; then x=100755; else x=100644; fi
        if [ "$UR_MODES" != false ] && [ "$m" != "$x" ]; then continue; fi
        if [ "$(LC_ALL=C sed -E "$REDACT_SED" < "$f" | git hash-object --path="$f" --stdin)" != "$i" ]; then continue; fi
        if mkdir -p -- "$1/$UR_RUN/$(dirname -- "$f")" \
            && { [ -f "$1/$UR_RUN.branch" ] || printf '%s\n' "$UR_BRANCH" > "$1/$UR_RUN.branch"; } \
            && cp -p -- "$f" "$1/$UR_RUN.part" && mv -f -- "$1/$UR_RUN.part" "$1/$UR_RUN/$f" \
            && cp -p -- "$f" "$1/$UR_RUN.part" && LC_ALL=C sed -E "$REDACT_SED" < "$f" > "$1/$UR_RUN.part" \
            && mv -f -- "$1/$UR_RUN.part" "$f"; then
            :
        else
            echo "arena set-branch: couldn't set $f aside - left as it is" >&2
        fi
    done
    git update-index -q --refresh > /dev/null 2>&1 || :
}

# Whether the work tree's $2 can be replaced in place from the folder $1 (in the git dir): no
# directory on its way is a symlink (a copy would land where it points), and its directory is on
# the git dir's filesystem (a rename there is one step; across filesystems `mv` copies).
unredacted_in_place() {
    _d=$(dirname -- "$2")
    _p=$_d
    while [ "$_p" != . ] && [ "$_p" != / ]; do
        if [ -L "$_p" ]; then return 1; fi
        _p=$(dirname -- "$_p")
    done
    [ -d "$_d" ] && [ "$(stat -c %d -- "$(dirname -- "$1")")" = "$(stat -c %d -- "$_d")" ]
}

# After the switch: the copies set aside on the branch now checked out, newest first and one per
# path, go back where the path holds exactly their redacted version — renamed into place — and a
# copy the file already matches is dropped; this switch's own copies that did neither are said
# (KEPT). Nothing else is ever overwritten: a file the participant changed, removed or moved under
# a link since keeps what they did, and its copy stays. Copies from other branches wait for a
# switch back to theirs.
unredacted_restore() {
    if [ ! -d "$1" ]; then return 0; fi
    cur=$(git rev-parse --abbrev-ref HEAD)
    : > "$1.done"
    for run in $(ls -1 "$1" | sort -r); do
        if [ ! -d "$1/$run" ] || [ "$(cat "$1/$run.branch" 2>/dev/null)" != "$cur" ]; then continue; fi
        (cd "$1/$run" && find . -type f) | sed 's#^\./##' | while IFS= read -r f; do
            if grep -qxF -- "$f" "$1.done"; then continue; fi
            if [ ! -f "$f" ] || [ -L "$f" ] || ! unredacted_in_place "$1" "$f"; then continue; fi
            if cmp -s -- "$1/$run/$f" "$f"; then
                # Its keys are in the file already (a switch stopped before it put it back).
                rm -f -- "$1/$run/$f" && printf '%s\n' "$f" >> "$1.done"
            elif [ "$(LC_ALL=C sed -E "$REDACT_SED" < "$1/$run/$f" | git hash-object --path="$f" --stdin)" = "$(git hash-object -- "$f")" ]; then
                cp -p -- "$1/$run/$f" "$1/$run.part" && mv -f -- "$1/$run.part" "$f" \
                    && rm -f -- "$1/$run/$f" && printf '%s\n' "$f" >> "$1.done" && echo "UNREDACTED_RESTORED $f"
            fi
        done
    done
    # A copy cut short (stopped before its rename into place): what it was for is as it was.
    rm -f -- "$1"/*.part
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
