#!/usr/bin/env bash
# Shared Docker / bare-VM provisioning. Call arena_provision after supplying the baked assets.
arena_provision() {
set +e
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
VENV="${VENV:-/opt/arena-env}"
BIN_DIR="${ARENA_BIN_DIR:-/usr/local/bin}"
INFRA_DIR="${INFRA_DIR:-$HOME/.arena_infra}"

echo "### 1/6 system packages"
# apt normally succeeds even when it kept old indexes. Error-Mode=any makes that
# fatal; retain the log so a mirror TLS failure is reported at this step.
apt_update() {
    apt-get -o APT::Update::Error-Mode=any update -qq > "$SETUP_TMP/apt.log" 2>&1
    local rc=$?
    cat "$SETUP_TMP/apt.log"
    [ "$rc" = 0 ] && ! grep -Eq '^Err:|Some index files failed' "$SETUP_TMP/apt.log"
}
if ! apt_update; then
    if grep -qi 'mirror\.hetzner\.com' "$SETUP_TMP/apt.log" && \
        grep -Eqi 'certificate verification failed|certificate.*expired|handshake' "$SETUP_TMP/apt.log"; then
        # Change only this mirror, retaining apt's signed Release verification.
        APT_DIR="${ARENA_APT_DIR:-/etc/apt}"
        for src in "$APT_DIR/sources.list" "$APT_DIR"/sources.list.d/*.list "$APT_DIR"/sources.list.d/*.sources; do
            [ -f "$src" ] || continue
            sed -i 's#https://mirror\.hetzner\.com/#http://mirror.hetzner.com/#g' "$src" || die "couldn't update $src"
        done
        echo "arena-warning: Hetzner apt mirror TLS failed; retrying that mirror over HTTP (apt signatures remain required)"
        apt_update || die "apt-get update failed after the Hetzner mirror retry — see the index error above"
    else
        die "apt-get update failed — see the index error above"
    fi
fi
apt-get install -y --no-install-recommends \
    ncdu vim nano htop net-tools iputils-ping tree ffmpeg sudo fzf \
    figlet curl wget git ca-certificates btop rsync lsof tmux zip unzip \
    build-essential libosmesa6 jq pkg-config zsh python3-dev || die "apt-get install of the system packages failed"

if [ "$GPU" = 1 ]; then
    apt-get install -y --no-install-recommends nvtop || echo "WARN: optional nvtop install failed"
fi

echo "### 2/6 docker + docker compose"
if [ "${ARENA_CONTAINER:-0}" != 1 ]; then
if ! command -v docker >/dev/null 2>&1; then
    curl -fsSL https://get.docker.com | sh
fi
systemctl enable --now docker 2>/dev/null || true
docker --version || echo "WARN: docker missing (its installer failed?) — continuing; the ARENA env doesn't need it"
docker compose version || echo "WARN: docker compose plugin missing"
fi

echo "### 3/6 uv"
if ! command -v uv >/dev/null 2>&1 && [ ! -x "$HOME/.local/bin/uv" ]; then
    curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="$HOME/.local/bin:$PATH"
mkdir -p "$BIN_DIR"
for b in uv uvx; do
    [ ! -x "$HOME/.local/bin/$b" ] || ln -sf "$HOME/.local/bin/$b" "$BIN_DIR/$b"
done
uv --version || die "uv isn't installed (its installer failed?)"

echo "### 4/6 ARENA repo + Python env"
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
    git clone ${ARENA_BRANCH:+--branch "$ARENA_BRANCH"} "$REPO_URL" "$REPO_DIR" || die "couldn't clone $REPO_URL into $REPO_DIR"
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
    if [ "$GPU" = 1 ] && [ "${ARENA_CONTAINER:-0}" != 1 ]; then
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

# Use the course's requirements directly, as the image does. Its install_uv.sh hardcodes
# <repo>/.venv and cannot apply CPU substitutions, so both platforms share this installer.
if env_ok; then
    INSTALLER="none (env already present)"
    echo "ARENA env already present (torch + transformer_lens import) — skipping the install"
else
    INSTALLER="requirements.txt (shared uv install)"
    [ -f requirements.txt ] || die "no requirements.txt in $REPO_DIR to install the ARENA env from"
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
    # uv creates or repairs this environment without deleting a participant's files.
    uv venv --python "$PY" --seed "$VENV" || die "uv couldn't create a Python $PY venv at $VENV"
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
python -m ipykernel install --user --name arena-env --display-name arena-env || die "couldn't register the arena-env Jupyter kernel"

echo "### 5/6 coding agents (claude code + codex)"
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
for b in claude codex; do [ -e "$HOME/.local/bin/$b" ] && ln -sf "$HOME/.local/bin/$b" "$BIN_DIR/$b"; done

echo "### 6/6 zsh + oh-my-zsh + dotfiles + MOTD (match the GPU pods)"
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

# Both paths use the same baked dotfiles; no runtime clone of the old infra repo.
ln -sf "$INFRA_DIR/dotfiles/.vimrc" "$HOME/.vimrc"
ln -sf "$INFRA_DIR/dotfiles/.p10k.zsh" "$HOME/.p10k.zsh"
sed "s#/opt/arena-env#${VENV}#g" "$INFRA_DIR/dotfiles/.zshrc.uv" > "$HOME/.zshrc"
touch "$HOME/.bashrc"
if ! grep -qF ARENA_ENV_ACTIVATE "$HOME/.bashrc"; then
    printf '\n# ARENA_ENV_ACTIVATE\nexport PATH="$HOME/.local/bin:$PATH"\n[ ! -f "%s/bin/activate" ] || source "%s/bin/activate"\n' "$VENV" "$VENV" >> "$HOME/.bashrc"
fi
# Keep an existing local venv intact; only fill a missing compatibility path.
if [ "$REPO_DIR/.venv" != "$VENV" ] && [ ! -L "$REPO_DIR/.venv" ]; then
    if [ -e "$REPO_DIR/.venv" ]; then
        mkdir -p "$HOME/.cache/arena-setup/old-venvs" || die "couldn't preserve the old repo-local venv"
        old=$(mktemp -d "$HOME/.cache/arena-setup/old-venvs/env.XXXXXX") || die "couldn't preserve the old repo-local venv"
        mv "$REPO_DIR/.venv" "$old/venv" || die "couldn't preserve the old repo-local venv"
        echo "arena-warning: old repo-local venv kept at $old/venv; .venv now uses $VENV"
    fi
    ln -s "$VENV" "$REPO_DIR/.venv" || die "couldn't link the repo-local venv to $VENV"
fi
[ "$REPO_DIR" = "$HOME/ARENA_3.0" ] || [ -e "$HOME/ARENA_3.0" ] || [ -L "$HOME/ARENA_3.0" ] || ln -s "$REPO_DIR" "$HOME/ARENA_3.0"
mkdir -p "$HOME/.ssh"
ssh-keyscan github.com >> "$HOME/.ssh/known_hosts" 2>/dev/null || true
chmod 600 "$HOME/.ssh/known_hosts" 2>/dev/null || true
touch "$HOME/.no_auto_tmux"
# Give hand commits and merge recovery an identity before the first backup.
git config --global user.name >/dev/null 2>&1 || git config --global user.name "ARENA participant" || true
git config --global user.email >/dev/null 2>&1 || git config --global user.email "arena@localhost" || true

# Shell-independent Python startup hook and editor/kernel configuration.
SITE=$("$VENV/bin/python" -c 'import sysconfig; print(sysconfig.get_paths()["purelib"])')
if [ -n "$SITE" ] && [ -d "$SITE" ]; then
    install -m644 "$INFRA_DIR/scripts/arena_cpu_threads.py" "$SITE/arena_cpu_threads.py" || die "couldn't install the CPU thread hook"
    printf 'import arena_cpu_threads\n' > "$SITE/arena_cpu_threads.pth"
fi
mkdir -p "$HOME/.vscode"
cat > "$HOME/.vscode/settings.json" <<EOF
{"python.defaultInterpreterPath":"$VENV/bin/python","python.analysis.extraPaths":["$REPO_DIR/chapter0_fundamentals/exercises","$REPO_DIR/chapter1_transformer_interp/exercises","$REPO_DIR/chapter2_rl/exercises","$REPO_DIR/chapter3_llm_evals/exercises","$REPO_DIR/chapter4_alignment_science/exercises"]}
EOF

# Optional exercise checkouts: a failed clone is visible and never stops the env gate.
clone_course() {
    [ -d "$2/.git" ] && return
    mkdir -p "$(dirname "$2")"
    git clone --depth 1 "$1" "$2" || echo "WARN: couldn't clone $1 (continuing)"
}
clone_course https://github.com/callummcdougall/arena-llm-context.git "$HOME/arena-llm-context"
C1="$REPO_DIR/chapter1_transformer_interp/exercises"
clone_course https://github.com/saprmarks/geometry-of-truth.git "$C1/geometry-of-truth"
clone_course https://github.com/ApolloResearch/deception-detection.git "$C1/deception-detection"
clone_course https://github.com/decoderesearch/circuit-tracer.git "$C1/circuit-tracer"
clone_course https://github.com/neelnanda-io/Grokking.git "$C1/part52_grokking_and_modular_arithmetic/Grokking"
clone_course https://github.com/PascalPons/connect4 "$REPO_DIR/chapter2_rl/exercises/part5_mcts_alphazero/pascal_pons/solver"
C4="$REPO_DIR/chapter4_alignment_science/exercises"
for item in clarifying-EM/model-organisms-for-EM PalisadeResearch/shutdown_avoidance interp-reasoning/thought-anchors safety-research/assistant-axis safety-research/petri tim-hua-01/ai-psychosis; do
    clone_course "https://github.com/$item.git" "$C4/${item##*/}"
done

# Machine name (MOTD + prompt) from the Hetzner hostname, e.g. arena8-flutter (ARENA_MACHINE_NAME
# where the hostname isn't the machine name: a Shadeform VM's).
[ -f "$HOME/.name" ] || printf 'export MACHINE_NAME=%s\n' "${ARENA_MACHINE_NAME:-$(hostname)}" > "$HOME/.name"
# ARENA figlet login banner, same script the GPU pods use.
MACHINE_NAME="${ARENA_MACHINE_NAME:-$(hostname)}" bash "$INFRA_DIR/scripts/motd.sh" 2>/dev/null || true
# Default shell -> zsh, so interactive SSH and `arena pods run` land in the configured env.
chsh -s "$(which zsh)" root 2>/dev/null || true
# Tokens (OPENROUTER_API_KEY, HF_TOKEN, …) are distributed separately by
# `arena pods copy-keys` (cross-provider), which appends exports to these same rc files.

# What built the env, for `arena pods setup`/`pods up` to put on the pod's ✓ line (stdout
# reaches the operator only through these `arena-` lines).
echo "arena-installer: $INSTALLER"
echo "### done — $(python --version 2>&1), torch $(python -c 'import torch; print(torch.__version__)' 2>/dev/null || echo '?'), installer: $INSTALLER, shell=$(getent passwd root | cut -d: -f7)"

}
