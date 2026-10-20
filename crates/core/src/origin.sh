# arena origin check: is this repo's `origin` the configured repo? Rendered into the backup script
# and into init-branches' command (arena_core::backup::origin_sh), which both push to `origin`.
#
# origin_key URL: what every spelling of one repo has in common: host/owner/name, lowercased. It
# accepts scp-style (git@github.com:o/n.git), ssh:// and https:// (with any user or credentials, and
# a port), a .git suffix, trailing slashes, and a query or fragment. `www.github.com` and
# `ssh.github.com` (GitHub's ssh over port 443) are github.com. Empty for anything else, such as a
# local path.
origin_key() {
    printf '%s\n' "$1" | LC_ALL=C sed -E -n '
        s#^[A-Za-z][A-Za-z0-9+.-]*://([^/]*@)?([^/:]+)(:[0-9]*)?/+#\2/#
        t path
        s#^([^/:@]*@)?([^/:]+):/*#\2/#
        t path
        b
        :path
        s|[?#].*$||
        s#/+$##
        s#\.git$##
        s#/+$##
        y/ABCDEFGHIJKLMNOPQRSTUVWXYZ/abcdefghijklmnopqrstuvwxyz/
        s#^(www|ssh)\.github\.com/#github.com/#
        /^[^/]+\/[^/]+\/[^/]+$/p'
}
# origin_shown URL: the URL as a report may print it. Any user or credentials (`user:token@`) are
# taken out, and so are a query and a fragment. In a URL with a scheme, everything up to the last
# `@` goes: a password with an unescaped `/` in it must not survive either, and a GitHub owner or
# repo name never has an `@`. scp-style can only carry a user name, which goes up to its `@`.
origin_shown() {
    printf '%s\n' "$1" | LC_ALL=C sed -E 's#^([A-Za-z][A-Za-z0-9+.-]*://).*@#\1#; t q; s#^[^/]*@##; :q; s|[?#].*$||'
}
# upstream_is_course OWNER/NAME: whether this repo's `upstream` remote is that GitHub repo — every
# URL it fetches from (`git remote get-url --all`, insteadOf applied), however each is spelled — so
# what refs/remotes/upstream/* hold is that repo's published history. No `upstream`, a URL elsewhere
# (a local path included: it has no key), or an OWNER/NAME with no key of its own: no.
upstream_is_course() {
    _cw=$(origin_key "git@github.com:$1.git")
    [ -n "$_cw" ] || return 1
    _cu=$(git remote get-url --all upstream 2>/dev/null) || return 1
    [ -n "$_cu" ] || return 1
    printf '%s\n' "$_cu" | while IFS= read -r _u; do [ "$(origin_key "$_u")" = "$_cw" ] || exit 1; done
}
# origin_check OWNER/NAME: stop the run unless every URL this repo fetches from or pushes to as
# `origin` is that GitHub repo. On a mismatch it prints `WRONG_ORIGIN OWNER/NAME <url>` (no URL when
# there is no origin), commits nothing, pushes nothing, and exits 5. An empty OWNER/NAME means no
# repo is configured, so there is nothing to compare and no check.
origin_check() {
    [ -n "$1" ] || return 0
    _want=$(origin_key "git@github.com:$1.git")
    _urls=$({ git remote get-url --all origin; git remote get-url --push --all origin; } 2>/dev/null) || true
    if [ -z "$_urls" ]; then
        printf 'WRONG_ORIGIN %s\n' "$1"
        exit 5
    fi
    # A configured repo with no key of its own (it can't be a GitHub repo) matches nothing.
    _bad=$(printf '%s\n' "$_urls" | while IFS= read -r _u; do
        if [ -n "$_u" ] && { [ -z "$_want" ] || [ "$(origin_key "$_u")" != "$_want" ]; }; then printf '%s\n' "$_u"; break; fi
    done)
    if [ -n "$_bad" ]; then
        printf 'WRONG_ORIGIN %s %s\n' "$1" "$(origin_shown "$_bad")"
        exit 5
    fi
}
