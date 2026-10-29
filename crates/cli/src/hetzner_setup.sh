#!/usr/bin/env bash
# Provision a bare Hetzner Ubuntu 24.04 x86 VM to look like an ARENA pod:
#   - system deps (build tools, ffmpeg, libosmesa6 for headless mujoco, zsh, …)
#   - docker + docker compose
#   - the configured cohort repo (REPO_URL) and its env, built by the repo's own install_uv.sh
#     (uv, Python 3.13, CPU torch wheels) — or, for a repo without one, by this script's uv
#     install of its requirements.txt (CPU substitutions); the public course repo (UPSTREAM_URL)
#     becomes the checkout's `upstream` remote
# ARENA_GPU=1 is the same for a bare **GPU** VM (Shadeform, a CUDA image with its driver): the
# requirements keep their CUDA wheels, uv matches torch to the host's driver (`--torch-backend
# ${ARENA_TORCH_BACKEND:-auto}`), and the env gate also asks torch for the GPU. ARENA_MACHINE_NAME
# names the machine for ~/.name and the banner where the hostname doesn't (Shadeform's).
#   - a ~/.zshrc / ~/.bashrc that auto-activates the venv, so `arena pods run`
#     (which does `zsh -c 'source ~/.zshrc; …'`) lands in the right Python.
#
# The arena8 SSH key is attached at CREATE time by the provider (MACHINE_NAME_PREFIX),
# so this script assumes you can already SSH in. Run as root on the pod (`arena pods setup`
# passes REPO_URL/REPO_DIR/REPO_KEY and UPSTREAM_URL from the config; by hand, name the repo):
#   ssh arena8-<name> 'REPO_URL=git@github.com:<owner>/<name>.git bash -s' < hetzner_setup.sh
# Idempotent: safe to re-run. The ARENA env is (re)installed unless it already passes the
# gate below, and the script exits non-zero — with the reason as its last line — whenever the
# env can't be made to pass it: a VM whose venv can't `import torch` is not set up, and must
# never be reported as if it were (it once was: no `set -e`, so a failed install was a ✓).
# Only the ARENA env is fatal; the shell niceties after it stay best-effort (WARN lines).
set -uo pipefail
export DEBIAN_FRONTEND=noninteractive

# Stop with the reason as the last line of stderr (what `arena pods setup` shows).
die() { echo "FAILED: $*" >&2; exit 1; }

# The installers' scratch — uv's above all — goes to a temp dir of this script's own, removed
# when it ends (the EXIT trap: after a `die` too). uv leaves a 0-byte lock per setuptools build
# in its temp dir (`uv-setuptools-<hash>.lock`, mode 666) and never removes it: left in /tmp, those
# read as data written outside the home since the VM was created to the auto-reaper's probe
# (`data_outside_home`) until a reboot empties /tmp — a freshly set-up VM never read as idle
# (live W7 B-H0). A uv that ignored TMPDIR is caught too: the `/tmp/uv-*` files made since this
# script began (empty, regular) go with it — never an older one, which isn't this run's.
# (ARENA_SETUP_TMP_ROOT: a test's stand-in for /tmp; never set on a pod.)
TMP_ROOT=${ARENA_SETUP_TMP_ROOT:-/tmp}
SETUP_TMP=$(mktemp -d "$TMP_ROOT/.arena-setup.XXXXXX") || die "couldn't make a temp dir in $TMP_ROOT"
touch "$SETUP_TMP/.started" || die "couldn't write in $SETUP_TMP"
cleanup() {
    find "$TMP_ROOT" -maxdepth 1 -type f -name 'uv-*' -empty -newer "$SETUP_TMP/.started" -delete 2>/dev/null
    rm -rf "$SETUP_TMP"
}
trap cleanup EXIT
export TMPDIR="$SETUP_TMP"

GPU="${ARENA_GPU:-0}"
REPO_DIR="${REPO_DIR:-/root/ARENA_materials}"
# The cohort repo: the configured one — arena passes git@github.com:<ARENA_REPO_OWNER>/
# <ARENA_REPO_NAME>.git, the remote RunPod/Vast setup points their image's checkout at. No
# default: the public ARENA-education repo used to be one, so a VM could quietly get a
# different course repo than the cohort's (and backups a remote they can't push to).
REPO_URL="${REPO_URL:-}"
# The public course repo (arena passes ARENA_UPSTREAM_REPO's https URL; empty = none): added
# as the checkout's `upstream` remote, so `git pull upstream main` gets the course's own fixes.
UPSTREAM_URL="${UPSTREAM_URL:-}"
VENV="$REPO_DIR/.venv"

# Swap, before anything heavy: a cx23 has 4 GB of RAM and a cx33 8 GB, and the course's notebooks
# load models — without swap the OOM killer ends a participant's kernel (or the install) at the
# first peak; with it the box just slows down. ARENA_SWAP_GB (arena passes VM_SWAP_GB, default 8;
# 0 = none) sizes a swap file at the filesystem's top, where the auto-reaper's idle probe never
# looks (it scans /tmp, /opt, /home/… for data written outside the home: a file there would make
# the VM look busy for ever). Never fatal: a VM without swap still works, so every failure is a
# WARN and the file is removed. Idempotent: one already on is kept — resizing it would need a
# `swapoff`, which can fail, or stall the box, under memory pressure; the line says so.
# (ARENA_SWAPFILE, ARENA_FSTAB, ARENA_SYSCTL_CONF, ARENA_PROC_SWAPS: a test's stand-ins; never set on a pod.)
echo "### 1/7 swap"
SWAP_GB="${ARENA_SWAP_GB:-8}"
SWAPFILE="${ARENA_SWAPFILE:-/swapfile}"
FSTAB="${ARENA_FSTAB:-/etc/fstab}"
SYSCTL_CONF="${ARENA_SYSCTL_CONF:-/etc/sysctl.d/99-arena-swap.conf}"
PROC_SWAPS="${ARENA_PROC_SWAPS:-/proc/swaps}"
# Free space to leave beside the swap: the env (~5 GB) and the participant's work.
SWAP_ROOM_GB=10
swap_in_fstab() {
    grep -qs "^$SWAPFILE[[:space:]]" "$FSTAB" \
        || printf '%s none swap sw 0 0\n' "$SWAPFILE" >> "$FSTAB" \
        || echo "WARN: couldn't add $SWAPFILE to $FSTAB — the swap is off again after a reboot"
}
setup_swap() {
    case "$SWAP_GB" in
        '' | *[!0-9]*) echo "WARN: swap size '$SWAP_GB' isn't a whole number of GB — no swap"; return ;;
    esac
    if [ "$SWAP_GB" -eq 0 ]; then
        echo "swap: none (VM_SWAP_GB=0)"
        return
    fi
    local want=$((SWAP_GB * 1024 * 1024 * 1024)) on have free
    # What's on now, `<file> <bytes>` a line (the kernel's own list: no util-linux version needed).
    # Every number awk hands the shell goes through %.0f: some awks print a big product as 1.8e+10,
    # which `[ -lt ]` and $(( )) can't read.
    on=$(awk 'NR > 1 { printf "%s %.0f\n", $1, $3 * 1024 }' "$PROC_SWAPS" 2>/dev/null)
    if printf '%s\n' "$on" | awk -v f="$SWAPFILE" '$1 == f { found = 1 } END { exit !found }'; then
        have=$(printf '%s\n' "$on" | awk -v f="$SWAPFILE" '$1 == f { printf "%.0f\n", int($2 / 1073741824) }')
        [ "$have" = "$SWAP_GB" ] && echo "swap: $SWAPFILE already on ($have GB)" \
            || echo "swap: $SWAPFILE already on ($have GB, not the $SWAP_GB GB asked: resizing a live swap file isn't safe here)"
        swap_in_fstab
        return
    fi
    have=$(printf '%s\n' "$on" | awk '{ s += $2 } END { printf "%.0f\n", s }')
    if [ "$have" -ge "$want" ]; then
        echo "swap: $((have / 1073741824)) GB already on (not arena's) — none added"
        return
    fi
    # A leftover file of ours (not on: a run cut short) is space we can take back.
    free=$(df -Pk "$(dirname "$SWAPFILE")" 2>/dev/null | awk 'NR == 2 { printf "%.0f\n", $4 * 1024 }')
    case "$have$free" in
        *[!0-9]*) echo "WARN: no swap — couldn't measure the swap on or the free disk ('$have', '$free')"; return ;;
    esac
    [ -f "$SWAPFILE" ] && free=$(( ${free:-0} + $(stat -c %s "$SWAPFILE" 2>/dev/null || echo 0) ))
    if [ -z "$free" ] || [ "$free" -lt $((want + SWAP_ROOM_GB * 1024 * 1024 * 1024)) ]; then
        echo "WARN: no swap — ${SWAP_GB} GB of it would leave under ${SWAP_ROOM_GB} GB free on $(dirname "$SWAPFILE") ($(( ${free:-0} / 1073741824 )) GB free)"
        return
    fi
    rm -f "$SWAPFILE"
    if ! { fallocate -l "$want" "$SWAPFILE" 2>/dev/null || dd if=/dev/zero of="$SWAPFILE" bs=1M count=$((SWAP_GB * 1024)) status=none; }; then
        rm -f "$SWAPFILE"
        echo "WARN: no swap — couldn't write the ${SWAP_GB} GB file $SWAPFILE"
        return
    fi
    chmod 600 "$SWAPFILE"
    if ! mkswap "$SWAPFILE" >/dev/null || ! swapon "$SWAPFILE"; then
        rm -f "$SWAPFILE"
        echo "WARN: no swap — mkswap/swapon refused $SWAPFILE"
        return
    fi
    swap_in_fstab
    # Swap as a safety net, not a second RAM: the kernel prefers dropping cache to swapping.
    { printf 'vm.swappiness = 10\n' > "$SYSCTL_CONF" && sysctl -q -p "$SYSCTL_CONF"; } >/dev/null 2>&1 \
        || echo "WARN: couldn't set vm.swappiness (the swap is on at the kernel's default)"
    echo "swap: ${SWAP_GB} GB at $SWAPFILE"
}
setup_swap

echo "### 2/7 system packages"
apt-get update -qq || die "apt-get update failed"
apt-get install -y --no-install-recommends \
    build-essential ffmpeg git curl wget ca-certificates libosmesa6 jq zsh tmux \
    python3-dev pkg-config || die "apt-get install of the system packages failed"

echo "### 3/7 docker + docker compose"
if ! command -v docker >/dev/null 2>&1; then
    curl -fsSL https://get.docker.com | sh
fi
systemctl enable --now docker 2>/dev/null || true
docker --version || echo "WARN: docker missing (its installer failed?) — continuing; the ARENA env doesn't need it"
docker compose version || echo "WARN: docker compose plugin missing"

echo "### 4/7 uv"
if ! command -v uv >/dev/null 2>&1 && [ ! -x "$HOME/.local/bin/uv" ]; then
    curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="$HOME/.local/bin:$PATH"
uv --version || die "uv isn't installed (its installer failed?)"

echo "### 5/7 ARENA repo + Python env"
# If a git deploy key was provided (REPO_KEY), wire github.com to it so we can clone — and
# later push backups to — the PRIVATE cohort repo over SSH. Without it the clone is anonymous
# (a public REPO_URL only).
if [ -n "${REPO_KEY:-}" ] && [ -f "$REPO_KEY" ]; then
    chmod 600 "$REPO_KEY"
    mkdir -p ~/.ssh && chmod 700 ~/.ssh
    grep -q github.com ~/.ssh/known_hosts 2>/dev/null || ssh-keyscan github.com >> ~/.ssh/known_hosts 2>/dev/null
    if ! grep -q "BEGIN arena github" ~/.ssh/config 2>/dev/null; then
        printf '%s\n' '# BEGIN arena github' 'Host github.com' "  IdentityFile $REPO_KEY" \
            '  IdentitiesOnly yes' '# END arena github' >> ~/.ssh/config
        chmod 600 ~/.ssh/config
    fi
fi
if [ ! -d "$REPO_DIR" ]; then
    [ -n "$REPO_URL" ] || die "no repo at $REPO_DIR and no REPO_URL to clone (arena passes the configured ARENA_REPO_OWNER/ARENA_REPO_NAME)"
    git clone "$REPO_URL" "$REPO_DIR" || die "couldn't clone $REPO_URL into $REPO_DIR"
elif [ -n "$REPO_URL" ] && [ -d "$REPO_DIR/.git" ]; then
    # A checkout already there (a VM from a snapshot, a re-run) follows the configured repo
    # too, as RunPod/Vast setup re-points the image's: origin is where backups push.
    was=$(git -C "$REPO_DIR" remote get-url origin 2>/dev/null)
    if [ "$was" != "$REPO_URL" ]; then
        if git -C "$REPO_DIR" remote set-url origin "$REPO_URL" 2>/dev/null || git -C "$REPO_DIR" remote add origin "$REPO_URL" 2>/dev/null; then
            echo "origin: $REPO_URL (was ${was:-none})"
        else
            echo "WARN: couldn't point $REPO_DIR's origin at $REPO_URL (it stays ${was:-unset})"
        fi
    fi
fi
# Only when there's none: an `upstream` a participant set is theirs. Never fetched here, and
# never fatal — a convenience, not something the setup needs (a failed add is a warning on the
# pod's line). With two remotes sharing branch names (the cohort repo is a copy of the course's),
# a plain `git checkout <branch>` takes origin's, unless the participant set their own default.
if [ -n "$UPSTREAM_URL" ] && [ -d "$REPO_DIR/.git" ]; then
    if ! git -C "$REPO_DIR" remote get-url upstream >/dev/null 2>&1; then
        if git -C "$REPO_DIR" remote add upstream "$UPSTREAM_URL"; then
            echo "upstream: $UPSTREAM_URL"
        else
            echo "arena-warning: couldn't add the upstream remote $UPSTREAM_URL"
        fi
    fi
    git -C "$REPO_DIR" config --get checkout.defaultRemote >/dev/null 2>&1 \
        || git -C "$REPO_DIR" config checkout.defaultRemote origin || true
fi
cd "$REPO_DIR" || die "no repo at $REPO_DIR"

# The gate: the env is usable only if its python imports what the exercises need. It decides
# both whether to skip the slow (~4GB) install — e.g. on a pod booted from a pre-built snapshot,
# which is what makes those come up in ~1 min instead of 10 — and whether setup succeeded.
# On a GPU VM it must also see the GPU: a torch that can't (a CPU wheel, a driver/CUDA mismatch)
# is no GPU env, however well it imports.
env_ok() {
    [ -x "$VENV/bin/python" ] || return 1
    if [ "$GPU" = 1 ]; then
        "$VENV/bin/python" -c 'import torch, transformer_lens; assert torch.cuda.is_available()' >/dev/null 2>&1
    else
        "$VENV/bin/python" -c 'import torch, transformer_lens' >/dev/null 2>&1
    fi
}

# The venv's Python: ARENA_PYTHON if set, else 3.13 — raised to the lower bound of the repo's
# `requires-python` when that is newer. (3.11 broke: the requirements pin jax[cuda13]==0.11.2,
# which needs >= 3.12, so uv found "No solution" — and the old script carried on regardless.)
pick_python() {
    if [ -n "${ARENA_PYTHON:-}" ]; then echo "$ARENA_PYTHON"; return; fi
    local want=3.13 floor
    floor=$(sed -nE "s/^[[:space:]]*requires-python[[:space:]]*=[[:space:]]*[\"']?[[:space:]]*>=?[[:space:]]*([0-9]+\.[0-9]+).*/\1/p" pyproject.toml 2>/dev/null | head -n1)
    if [ -n "$floor" ] && [ "$(printf '%s\n%s\n' "$want" "$floor" | sort -V | tail -n1)" = "$floor" ]; then
        want=$floor
    fi
    echo "$want"
}

# Who builds the env: the repo's own install_uv.sh — uv, its Python (3.13), the venv at
# $REPO_DIR/.venv, its requirements.txt: the course's env as the course defines it, as the
# image RunPod/Vast pods boot from has it. torch's wheels are steered through uv's
# UV_TORCH_BACKEND (its `--torch-backend`; an older uv ignores it): CPU wheels on a CPU VM
# (PyPI's torch is a CUDA build — GBs of dead weight there), on a GPU VM matched to the host's
# driver or ARENA_TORCH_BACKEND, as below. A repo without install_uv.sh (an older checkout)
# gets this script's own uv install of its requirements.txt, as before — said as a warning,
# so `arena pods setup` shows which one ran. Not the repo's conda install.sh: it expects
# RunPod's layout (run from the repo's parent, which must be named ARENA_materials), has no
# `set -e` (a failed install passes), and its env isn't the venv the gate and shells use.
if env_ok; then
    INSTALLER="none (env already present)"
    echo "ARENA env already present (torch + transformer_lens import) — skipping the install"
elif [ -f install_uv.sh ]; then
    INSTALLER="install_uv.sh"
    if [ "$GPU" = 1 ]; then tb="${ARENA_TORCH_BACKEND:-auto}"; else tb=cpu; fi
    echo "installer: $REPO_DIR/install_uv.sh (the repo's own), torch backend $tb"
    # A venv that fails the gate is rebuilt from scratch, as below (uv's cache keeps it quick).
    rm -rf "$VENV"
    UV_TORCH_BACKEND="$tb" bash ./install_uv.sh \
        || die "the repo's install_uv.sh failed (exit $?) — see its output above"
else
    INSTALLER="requirements.txt (built-in uv install)"
    echo "arena-warning: no install_uv.sh in $REPO_DIR — installed its requirements.txt with the setup script's own uv install instead"
    [ -f requirements.txt ] || die "no install_uv.sh or requirements.txt in $REPO_DIR to install the ARENA env from"
    PY=$(pick_python)
    if [ "$GPU" = 1 ]; then
        # A GPU VM: the requirements as they are — CUDA wheels, `+cuNNN` pins and their index.
        REQ=requirements.gpu.txt
        cp requirements.txt "$REQ" || die "couldn't write $REQ"
    else
        REQ=requirements.cpu.txt
        # CPU substitutions, by pattern rather than by exact upstream line (those drift: jax went
        # cuda12 → cuda13, torch moved off the cu118 index): any `jax[cuda…]` extra → `jax[cpu]`;
        # bitsandbytes and the nvidia-* CUDA runtime wheels dropped (GPU-only — dead weight on a CPU
        # box); CUDA wheel indexes and `+cuNNN` local versions dropped — torch comes from the CPU
        # index via `--torch-backend cpu`.
        sed -E -e 's#^([[:space:]]*)jax\[cuda[^]]*\]#\1jax[cpu]#' \
            -e '/^[[:space:]]*bitsandbytes([^A-Za-z0-9_.-]|$)/d' \
            -e '/^[[:space:]]*nvidia-[A-Za-z0-9_.-]*cu[0-9]+/d' \
            -e '/^[[:space:]]*--(extra-)?index-url[[:space:]=]+https?:\/\/download\.pytorch\.org\/whl\/cu/d' \
            -e 's#\+cu[0-9]+##g' \
            requirements.txt > requirements.cpu.txt || die "couldn't write requirements.cpu.txt"
    fi
    # transformer_lens pins numpy<2, which conflicts with jax (numpy>=2). That pin is stale —
    # override it so the resolve succeeds (TL runs fine on numpy 2.x).
    printf 'numpy>=2.0\n' > overrides.txt || die "couldn't write overrides.txt"
    # A venv that fails the gate is rebuilt from scratch (it may be on the wrong Python).
    rm -rf "$VENV"
    uv venv --python "$PY" "$VENV" || die "uv couldn't create a Python $PY venv at $VENV"
    # shellcheck disable=SC1091
    source "$VENV/bin/activate" || die "couldn't activate $VENV"
    # `--torch-backend cpu`: uv picks torch's CPU wheels itself (PyPI's default torch is now a
    # CUDA build). An older uv without it gets the CPU index the old way —
    # --index-strategy unsafe-best-match: that index also carries some shared deps (e.g.
    # importlib-metadata) at versions that conflict with PyPI pins (circuitsvis); this lets uv
    # pick the best version across BOTH indexes instead of first-index-only.
    # A GPU VM: `--torch-backend auto` matches torch's CUDA wheels to the host's driver (or the
    # backend ARENA_TORCH_BACKEND names, e.g. cu130); unsafe-best-match lets the requirements' own
    # CUDA index and PyPI both serve, as for the CPU index. An older uv: the requirements' index.
    if uv pip install --help 2>/dev/null | grep -q -- '--torch-backend'; then
        if [ "$GPU" = 1 ]; then
            backend=(--torch-backend "${ARENA_TORCH_BACKEND:-auto}" --index-strategy unsafe-best-match)
        else
            backend=(--torch-backend cpu)
        fi
    elif [ "$GPU" = 1 ]; then
        backend=(--index-strategy unsafe-best-match)
    else
        backend=(--extra-index-url https://download.pytorch.org/whl/cpu --index-strategy unsafe-best-match)
    fi
    uv pip install --no-cache-dir "${backend[@]}" --override overrides.txt -r "$REQ" \
        || die "installing the ARENA packages ($REQ) into the Python $PY venv failed — see uv's error above"
fi
# The final gate, also after an install that "succeeded": no torch / transformer_lens, no setup.
if ! env_ok; then
    if [ "$GPU" = 1 ]; then
        why=$("$VENV/bin/python" -c 'import torch, transformer_lens; assert torch.cuda.is_available(), "torch.cuda.is_available() is False (driver/CUDA mismatch?)"' 2>&1 | tail -n1)
        die "the ARENA env at $VENV can't import torch + transformer_lens or use the GPU (${why:-no python in it}) — not set up"
    fi
    why=$("$VENV/bin/python" -c 'import torch, transformer_lens' 2>&1 | tail -n1)
    die "the ARENA env at $VENV can't import torch + transformer_lens (${why:-no python in it}) — not set up"
fi
# shellcheck disable=SC1091
source "$VENV/bin/activate"
# uv venv omits pip; install it INTO the venv so a bare `pip show <pkg>` works from the CLI
# (matches the GPU arena-env). Idempotent + cheap, so it also back-fills snapshot-booted pods.
python -m pip --version >/dev/null 2>&1 || uv pip install pip >/dev/null 2>&1 || true

echo "### 6/7 coding agents (claude code + codex)"
# Node-free curl installers drop a binary into ~/.local/bin (already on PATH + in the rc
# files). Idempotent: skip if already present. Auth is separate — the Claude Code OAuth
# token + ~/.claude.json onboarding come from `arena pods copy-keys`.
export PATH="$HOME/.local/bin:$PATH"
command -v claude >/dev/null 2>&1 || curl -fsSL https://claude.ai/install.sh | bash || echo "WARN: claude install failed (continuing)"
install_codex() {
    command -v codex >/dev/null 2>&1 && return 0
    # Try the official installer first…
    curl -fsSL https://chatgpt.com/codex/install.sh | sh >/dev/null 2>&1
    command -v codex >/dev/null 2>&1 && return 0
    # …it has a SHA-digest bug that fails on some hosts, so fall back to the prebuilt musl
    # binary from the latest GitHub release.
    case "$(uname -m)" in aarch64|arm64) a=aarch64 ;; *) a=x86_64 ;; esac
    mkdir -p "$HOME/.local/bin" /tmp/cx
    url=$(curl -fsSL https://api.github.com/repos/openai/codex/releases/latest \
        | grep -oE "https://[^\"]*codex-${a}-unknown-linux-musl\.tar\.gz" | head -1)
    [ -n "$url" ] || { echo "WARN: codex release asset not found (continuing)"; return 0; }
    curl -fsSL "$url" | tar xz -C /tmp/cx 2>/dev/null
    bin=$(find /tmp/cx -type f -name 'codex*' | head -1)
    [ -n "$bin" ] && install -m755 "$bin" "$HOME/.local/bin/codex" || echo "WARN: codex install failed (continuing)"
}
install_codex
# The shared dotfiles .zshrc does NOT put ~/.local/bin on PATH, so symlink the agents into
# /usr/local/bin — on PATH for every shell (login, non-login, bash, zsh, `arena pods run`).
for b in claude codex; do [ -e "$HOME/.local/bin/$b" ] && ln -sf "$HOME/.local/bin/$b" "/usr/local/bin/$b"; done

echo "### 7/7 zsh + oh-my-zsh + dotfiles + MOTD (match the GPU pods)"
apt-get install -y --no-install-recommends zsh figlet >/dev/null 2>&1 || true

# Same dotfiles repo the GPU pods clone — so the shell is byte-for-byte identical.
[ -d "$HOME/.arena_infra" ] || git clone --depth 1 https://github.com/nickypro/arena-infra "$HOME/.arena_infra"

# oh-my-zsh + powerlevel10k + the exact plugin set from the GPU image. RUNZSH/CHSH=no so the
# installer doesn't exec a shell or chsh mid-provision (we chsh explicitly below).
if [ ! -d "$HOME/.oh-my-zsh" ]; then
    RUNZSH=no CHSH=no sh -c \
        "$(curl -fsSL https://raw.githubusercontent.com/ohmyzsh/ohmyzsh/master/tools/install.sh)" \
        "" --unattended
fi
ZSH_CUSTOM="${ZSH_CUSTOM:-$HOME/.oh-my-zsh/custom}"
clone_plugin() { [ -d "$2" ] || git clone --depth 1 "$1" "$2"; }
clone_plugin https://github.com/romkatv/powerlevel10k.git             "$ZSH_CUSTOM/themes/powerlevel10k"
clone_plugin https://github.com/zsh-users/zsh-autosuggestions.git     "$ZSH_CUSTOM/plugins/zsh-autosuggestions"
clone_plugin https://github.com/zsh-users/zsh-syntax-highlighting.git "$ZSH_CUSTOM/plugins/zsh-syntax-highlighting"
clone_plugin https://github.com/zsh-users/zsh-history-substring-search "$ZSH_CUSTOM/plugins/zsh-history-substring-search"
clone_plugin https://github.com/zsh-users/zsh-completions             "$ZSH_CUSTOM/plugins/zsh-completions"

# p10k + vimrc symlinked verbatim. The shared .zshrc only knows conda, so we copy it and
# append a uv-venv activation (guarded by `! command -v conda`, so it's a no-op on GPU pods).
ln -sf "$HOME/.arena_infra/dotfiles/.vimrc"    "$HOME/.vimrc"
ln -sf "$HOME/.arena_infra/dotfiles/.p10k.zsh" "$HOME/.p10k.zsh"
cp -f  "$HOME/.arena_infra/dotfiles/.zshrc"    "$HOME/.zshrc"
for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
    touch "$rc"
    grep -qF "$VENV/bin/activate" "$rc" || \
        printf '\n# ARENA env: no conda on CPU pods — activate the uv venv (no-op on conda GPU pods).\nif ! command -v conda >/dev/null 2>&1 && [ -f "%s/bin/activate" ]; then\n    source "%s/bin/activate"\nfi\n' "$VENV" "$VENV" >> "$rc"
done

# Machine name (MOTD + prompt) from the Hetzner hostname, e.g. arena8-flutter (ARENA_MACHINE_NAME
# where the hostname isn't the machine name: a Shadeform VM's).
[ -f "$HOME/.name" ] || printf 'export MACHINE_NAME=%s\n' "${ARENA_MACHINE_NAME:-$(hostname)}" > "$HOME/.name"
# ARENA figlet login banner, same script the GPU pods use.
MACHINE_NAME="${ARENA_MACHINE_NAME:-$(hostname)}" bash "$HOME/.arena_infra/scripts/motd.sh" 2>/dev/null || true
# Default shell -> zsh, so interactive SSH and `arena pods run` land in the configured env.
chsh -s "$(which zsh)" root 2>/dev/null || true
# Tokens (OPENROUTER_API_KEY, HF_TOKEN, …) are distributed separately by
# `arena pods copy-keys` (cross-provider), which appends exports to these same rc files.

# What built the env, for `arena pods setup`/`pods up` to put on the pod's ✓ line (stdout
# reaches the operator only through these `arena-` lines).
echo "arena-installer: $INSTALLER"
echo "### done — $(python --version 2>&1), torch $(python -c 'import torch; print(torch.__version__)' 2>/dev/null || echo '?'), installer: $INSTALLER, shell=$(getent passwd root | cut -d: -f7)"
