#!/usr/bin/env bash
# Scrub a VM that `hetzner_setup.sh` has built, before `arena image build` snapshots it — then
# scan what's left for secrets. Rendered by arena_core::image::scrub_script (the @…@ fields).
#
# What a VM made from the snapshot must not inherit: the deploy key and its ssh config, the
# authorized keys (cloud-init writes the new server's own at its first boot), any token setup
# or copy-keys wrote (the bake never runs copy-keys; this is the belt to that), shell history,
# the machine's name, arena's state, its swap file (setup makes a new one), its identity
# (/etc/machine-id, the SSH host keys — cloud-init's first boot of a new instance makes new
# ones), /tmp and the logs. Then a scan of the home directories and /etc for the backup's
# secret patterns and private keys: any hit and the bake refuses to snapshot. The scan prints
# file names, never what matched.
#
# Run as root with REPO_DIR, KEY_REMOTE (the deploy key's path), NAME (the image's name) and BUILT
# (unix). Prints:
#   arena-image-commit: <the checkout's HEAD>
#   arena-image-req: <the hash of what the env install read>
#   arena-scan: <file>             (one per file with a secret in it)
#   arena-scrub: done (<n> hit(s))
# Exit 0: clean; 3: the scan found something; 1: the scrub itself failed.
# (ARENA_SCRUB_ROOT: a test's stand-in for /; never set on a VM.)
set -uo pipefail
R="${ARENA_SCRUB_ROOT:-}"
REPO_DIR="${REPO_DIR:?REPO_DIR}"
NAME="${NAME:?NAME}"
BUILT="${BUILT:?BUILT}"
die() { echo "FAILED: $*" >&2; exit 1; }

@REQ_HASH_FN@

# What the image is: read before anything goes (the hash function needs the repo).
commit=$(git -C "$R$REPO_DIR" rev-parse --short=12 HEAD 2>/dev/null) || die "no git checkout at $REPO_DIR"
req=$(arena_req_hash "$R$REPO_DIR")
echo "arena-image-commit: $commit"
echo "arena-image-req: $req"

# The deploy key, wherever GIT_SSH_KEY_REMOTE put it (under ~/.ssh it goes with the rest below).
[ -n "${KEY_REMOTE:-}" ] && rm -f "$R$KEY_REMOTE" "$R$KEY_REMOTE.pub"

# Homes: root's and any user's.
homes=("$R/root")
for h in "$R"/home/*; do [ -d "$h" ] && homes+=("$h"); done

# Swap: off and gone, and its fstab line and sysctl file (setup makes them again, sized for the
# new VM). A swap file would also put GBs of the bake VM's memory into the image.
swapoff "$R/swapfile" 2>/dev/null
rm -f "$R/swapfile" "$R/etc/sysctl.d/99-arena-swap.conf"
[ -f "$R/etc/fstab" ] && { sed -i '\#^/swapfile[[:space:]]#d' "$R/etc/fstab" || die "couldn't edit /etc/fstab"; }

for h in "${homes[@]}"; do
    # SSH: everything but known_hosts (github.com's public key, which the repo update needs).
    # The deploy key, the arena-infra blocks in ~/.ssh/config (github, fleet, archive), the
    # archive's read key, authorized_keys — cloud-init writes the new server's at first boot.
    if [ -d "$h/.ssh" ]; then
        find "$h/.ssh" -mindepth 1 ! -name known_hosts -exec rm -rf {} + 2>/dev/null
    fi
    # Tokens copy-keys or setup export in the shell rc files.
    for rc in .bashrc .zshrc .profile .bash_profile .zprofile .zshenv; do
        [ -f "$h/$rc" ] && { sed -i -E '/^[[:space:]]*export[[:space:]]+(@TOKEN_VARS@)=/d' "$h/$rc" || die "couldn't edit $h/$rc"; }
    done
    # Credentials the agents and libraries keep: Claude Code, Codex, Hugging Face, git, gh.
    rm -rf "$h/.claude.json" "$h/.claude.json.backup" "$h/.claude" "$h/.codex/auth.json" \
        "$h/.cache/huggingface" "$h/.huggingface" "$h/.git-credentials" "$h/.netrc" "$h/.config/gh" "$h/.docker/config.json"
    # History, the machine's name (setup writes the new VM's), arena's state, caches.
    rm -rf "$h/.bash_history" "$h/.zsh_history" "$h/.python_history" "$h/.lesshst" "$h/.viminfo" "$h/.wget-hsts" \
        "$h/.name" "$h/.arena" "$h/.cache/uv" "$h/.cache/pip" "$h/hetzner_setup.sh" "$h/.arena-scrub.sh"
done
# The MOTD's name line (setup writes it from the new VM's hostname).
rm -f "$R/etc/update-motd.d/20-arena"

# Machine identity. The SSH host keys: cloud-init makes new ones at the first boot of a new
# instance (after `cloud-init clean` every boot of the image is one). machine-id: empty, so
# systemd makes a new one at boot (never deleted: an absent file means "not booted yet" to some).
rm -f "$R"/etc/ssh/ssh_host_*
: > "$R/etc/machine-id" || die "couldn't empty /etc/machine-id"
[ -f "$R/var/lib/dbus/machine-id" ] && [ ! -L "$R/var/lib/dbus/machine-id" ] && rm -f "$R/var/lib/dbus/machine-id"
if [ -z "$R" ]; then
    cloud-init clean --logs >/dev/null 2>&1 || die "cloud-init clean failed (the image would boot as this VM)"
    apt-get clean >/dev/null 2>&1
    journalctl --rotate >/dev/null 2>&1
    journalctl --vacuum-time=1s >/dev/null 2>&1
fi

# /tmp and the logs.
find "$R/tmp" "$R/var/tmp" -mindepth 1 -delete 2>/dev/null
if [ -d "$R/var/log" ]; then
    find "$R/var/log" -type f \( -name '*.gz' -o -name '*.[0-9]' -o -name '*.old' \) -delete 2>/dev/null
    find "$R/var/log" -type f -exec truncate -s 0 {} + 2>/dev/null
fi

# What the VMs made from it read at setup (which image, how old, what its env was built from).
printf 'name=%s\nbuilt=%s\ncommit=%s\nreq=%s\n' "$NAME" "$BUILT" "$commit" "$req" > "$R@MARKER@" \
    || die "couldn't write @MARKER@"

# The scan: the backup's secret patterns and any private key file, over the homes and /etc. Not
# the venv (third-party packages, nothing of ours: their docs carry example keys such as AWS's)
# nor .git (packed, compressed objects). Binary files are skipped (-I). File names only.
# A private key is a PEM header on a line of its own — what a key file (the deploy key, a host
# key, one pasted into a file) has. Not the header inside a line: third-party code and docs
# quote it (live 2026-10-29: VS Code server's ssh2 module, oh-my-zsh's dotenv README).
SECRET_RE='@SECRET_RE@'
KEY_RE=$'^-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----\r?$'
hits=0
while IFS= read -r f; do
    [ -n "$f" ] || continue
    echo "arena-scan: ${f#"$R"}"
    hits=$((hits + 1))
done < <(LC_ALL=C grep -rlIE --exclude-dir=.git --exclude-dir=.venv -e "$SECRET_RE" -e "$KEY_RE" "${homes[@]}" "$R/etc" 2>/dev/null)
# Whatever the patterns miss: any key file or authorized_keys left in an .ssh directory.
while IFS= read -r f; do
    [ -n "$f" ] || continue
    echo "arena-scan: ${f#"$R"}"
    hits=$((hits + 1))
done < <(for h in "${homes[@]}"; do [ -d "$h/.ssh" ] && find "$h/.ssh" -mindepth 1 ! -type d ! -name known_hosts; done 2>/dev/null)
sync
echo "arena-scrub: done ($hits hit(s))"
[ "$hits" -eq 0 ] || exit 3
