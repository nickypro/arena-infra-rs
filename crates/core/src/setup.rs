//! Pod provisioning: make a freshly-created pod ready to commit/back up.
//!
//! Faithful to the legacy `setup_em.sh`. After the caller copies the git deploy key
//! (scp), the on-pod script:
//!   1. `chmod 600` the key,
//!   2. add a `github.com` block to `~/.ssh/config` pointing at it (idempotent),
//!   3. add the deploy key's *public* half to `~/.ssh/authorized_keys` (idempotent),
//!      so anyone holding that key can also SSH into the pod — derived on the pod via
//!      `ssh-keygen -y` from the key we just copied, so no extra file is shipped,
//!   3b. when `/workspace` is a real mount (a persistent volume), put the repo ON it —
//!      `/workspace/<repo dir>`, with the configured path a symlink to it; after a reset the
//!      volume copy wins ([`crate::volume::relocation_command`]) — so a restart keeps the
//!      participants' work and everything below acts on the volume copy,
//!   4. point the ARENA repo's `origin` at the GitHub SSH URL, fetch *only the default
//!      branch* (no tags), and update (default: stay on the current branch, pull /
//!      reset-if-on-main; `--force`: check out the default branch and `reset --hard`),
//!      then update submodules,
//!   5. write `~/.name` as `export MACHINE_NAME='<short>'`.
//!
//! Then, best-effort (never failing a pod's setup), the VS Code Remote-SSH warm-up
//! ([`crate::vscode`]): server + course extensions pre-installed, the arena env as the
//! default interpreter.
//!
//! Renders the on-pod shell command (pure, unit-tested). The per-provider step list
//! ([`provisioning_steps`]) is data, and [`provision`] runs it over a [`Remote`] with a
//! time budget per step — so one wedged pod reports `timed out at <step>` instead of
//! blocking a whole fleet `setup`, and the runner is testable with `FakeRemote`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::config::Config;
use crate::error::{human_duration, Error, Result};
use crate::remote::Remote;
use crate::ssh::SshTarget;

/// Settings for provisioning, from the `GIT_SSH_KEY_*` / `ARENA_REPO_*` config keys.
#[derive(Debug, Clone)]
pub struct SetupConfig {
    /// Local path to the git deploy key (the scp *source*).
    pub key_local: String,
    /// Where the key lands on the pod (`GIT_SSH_KEY_REMOTE`).
    pub key_remote: String,
    /// The ARENA checkout path on the pod.
    pub repo_path: String,
    /// `git@github.com:owner/name.git`.
    pub repo_url: String,
    /// Default branch (e.g. "main").
    pub branch: String,
    /// Machine-name prefix, used to derive the short name for `~/.name`.
    pub prefix: String,
    /// Public keys to ensure in `~/.ssh/authorized_keys` (shared key + deploy key).
    pub authorized_pubkeys: Vec<String>,
    /// Broadcast token exports `(env name, value)` to write into the login shells
    /// (e.g. Hugging Face + Claude Code, from config). Empty => the token step is skipped.
    pub broadcast_exports: Vec<(String, String)>,
    /// `--zsh-install`: install zsh, make it the login shell, and drop an idempotent
    /// `.zshrc` that activates the arena env. Off by default (the prebuilt arena image
    /// already ships zsh); useful on a bare/non-arena base image.
    pub zsh_install: bool,
    /// The VS Code warm-up (on by default; `None` = `VSCODE_PREINSTALL=0` / `--no-vscode`).
    pub vscode: Option<crate::vscode::VscodeSetup>,
}

impl SetupConfig {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let owner = cfg
            .get("ARENA_REPO_OWNER")
            .ok_or_else(|| Error::Config("missing ARENA_REPO_OWNER".into()))?;
        let name = cfg
            .get("ARENA_REPO_NAME")
            .ok_or_else(|| Error::Config("missing ARENA_REPO_NAME".into()))?;
        let key_local = cfg
            .get("GIT_SSH_KEY_LOCAL")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Config("missing GIT_SSH_KEY_LOCAL (the key to copy)".into()))?;
        let repo_path = cfg
            .get("BACKUP_REPO_PATH")
            .map(String::from)
            .unwrap_or_else(|| format!("/root/{name}"));
        let vscode = crate::vscode::VscodeSetup::from_config(cfg, &repo_path)?;
        Ok(Self {
            // Resolve to a readable copy (prefer ~/.ssh/<name> if the configured path
            // isn't readable), same as the shared key — so `setup` works when run as a
            // user that can't read the configured /root path.
            key_local: crate::ssh::resolve_key_path(key_local),
            key_remote: cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519").to_string(),
            repo_path,
            repo_url: format!("git@github.com:{owner}/{name}.git"),
            branch: cfg.get("DEFAULT_BRANCH").unwrap_or("main").to_string(),
            prefix: cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string(),
            authorized_pubkeys: crate::ssh::authorized_pubkeys(cfg),
            broadcast_exports: crate::apikeys::broadcast_env_vars(|k| cfg.get(k).map(String::from)),
            zsh_install: false,
            vscode,
        })
    }

    /// The short machine name (the part after `{prefix}-`), e.g. arena8-apple -> apple.
    pub fn short_name<'a>(&self, machine_name: &'a str) -> &'a str {
        short_name(&self.prefix, machine_name)
    }

    /// The on-pod provisioning command, run *after* the key has been scp'd. `force`
    /// mirrors `setup_em.sh --force` (hard-reset onto the default branch).
    pub fn remote_command(&self, machine_name: &str, force: bool) -> String {
        let q = shell_quote;
        let key = &self.key_remote;

        // Idempotent github.com block in ~/.ssh/config pointing at the deploy key.
        let ssh_config = format!(
            "mkdir -p \"$HOME/.ssh\" && chmod 700 \"$HOME/.ssh\" && touch \"$HOME/.ssh/config\" && \
             sed -i '/^# BEGIN arena-infra github.com/,/^# END arena-infra github.com/d' \"$HOME/.ssh/config\" && \
             printf '%s\\n' '# BEGIN arena-infra github.com' 'Host github.com' '    AddKeysToAgent yes' \
             '    IdentityFile {key}' '# END arena-infra github.com' >> \"$HOME/.ssh/config\" && \
             chmod 600 \"$HOME/.ssh/config\""
        );

        // Ensure the shared key + deploy key are in authorized_keys (so the tool and
        // participants can SSH in with whichever they hold). Each is appended only if
        // not already present. (The provider's account key is injected automatically.)
        let mut ak = String::from(
            "touch \"$HOME/.ssh/authorized_keys\" && chmod 600 \"$HOME/.ssh/authorized_keys\"",
        );
        for pk in &self.authorized_pubkeys {
            let qpk = q(pk);
            ak.push_str(&format!(
                " && (grep -qxF {qpk} \"$HOME/.ssh/authorized_keys\" || echo {qpk} >> \"$HOME/.ssh/authorized_keys\")"
            ));
        }
        // Also re-derive the on-pod deploy key's public half and add it (covers the case
        // where the local deploy-key .pub differs from what's on the pod).
        ak.push_str(&format!(
            " && PUB=$(ssh-keygen -y -f {key} 2>/dev/null) && \
             (grep -qxF \"$PUB\" \"$HOME/.ssh/authorized_keys\" || echo \"$PUB\" >> \"$HOME/.ssh/authorized_keys\")"
        ));
        let authorized_keys = ak;

        let mut steps = vec!["set -e".to_string(), format!("chmod 600 {}", q(key)), ssh_config, authorized_keys];
        // The repo onto the persistent volume, when the pod has one — before the update, so
        // the fetch/reset below (and every later git operation) acts on the volume copy.
        // Never fails setup itself: anything odd is an `arena-warning:` line (see `provision`).
        steps.extend(crate::volume::relocation_command(&self.repo_path));
        steps.push(self.repo_update_command(force));
        steps.push(name_file_command(self.short_name(machine_name)));
        // Coding agents (claude code + codex) + tmux. All idempotent; the whole block runs in
        // a `set +e` subshell ending in `|| true`, so a flaky installer never aborts setup.
        // Node-free curl installers drop into ~/.local/bin; symlink into /usr/local/bin since
        // the dotfiles .zshrc doesn't put ~/.local/bin on PATH. Codex's official installer has
        // a SHA-digest bug on some hosts, so fall back to the prebuilt GitHub release.
        steps.push(
            r#"( set +e
export PATH="$HOME/.local/bin:$PATH"
command -v tmux >/dev/null 2>&1 || apt-get install -y -qq tmux >/dev/null 2>&1 || { apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq tmux >/dev/null 2>&1; }
command -v claude >/dev/null 2>&1 || curl -fsSL https://claude.ai/install.sh | bash >/dev/null 2>&1
command -v codex >/dev/null 2>&1 || curl -fsSL https://chatgpt.com/codex/install.sh | sh >/dev/null 2>&1
if ! command -v codex >/dev/null 2>&1; then case "$(uname -m)" in aarch64|arm64) a=aarch64;; *) a=x86_64;; esac; mkdir -p "$HOME/.local/bin" /tmp/cx; url=$(curl -fsSL https://api.github.com/repos/openai/codex/releases/latest | grep -oE 'https://[^"]*codex-'"$a"'-unknown-linux-musl\.tar\.gz' | head -1); [ -n "$url" ] && curl -fsSL "$url" | tar xz -C /tmp/cx 2>/dev/null && bin=$(find /tmp/cx -type f -name 'codex*' | head -1) && [ -n "$bin" ] && install -m755 "$bin" "$HOME/.local/bin/codex"; fi
for b in claude codex; do [ -e "$HOME/.local/bin/$b" ] && ln -sf "$HOME/.local/bin/$b" /usr/local/bin/$b; done
) || true"#.to_string(),
        );
        // Optional (`--zsh-install`): the same shell the GPU pods get — zsh + oh-my-zsh +
        // powerlevel10k + the shared `nickypro/arena-infra` dotfiles, with zsh as the login
        // shell. Mirrors the hetzner `hetzner_setup.sh` "6/6" block, minus the ARENA_3.0
        // checkout and venv/conda activation (this path is for non-arena base images that
        // don't have conda). The whole thing is a `set +e … || true` subshell so a missing
        // apt mirror / network blip never aborts the rest of setup. The cp'd dotfiles
        // .zshrc ends in a bare `conda activate arena-env`; we guard it with `command -v
        // conda` so a no-conda image doesn't print a "command not found" on every shell.
        if self.zsh_install {
            steps.push(
                r#"( set +e
export DEBIAN_FRONTEND=noninteractive
command -v zsh >/dev/null 2>&1 || apt-get install -y -qq zsh figlet >/dev/null 2>&1 || { apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq zsh figlet >/dev/null 2>&1; }
[ -d "$HOME/.arena_infra" ] || git clone --depth 1 https://github.com/nickypro/arena-infra "$HOME/.arena_infra" >/dev/null 2>&1
if [ ! -d "$HOME/.oh-my-zsh" ]; then RUNZSH=no CHSH=no sh -c "$(curl -fsSL https://raw.githubusercontent.com/ohmyzsh/ohmyzsh/master/tools/install.sh)" "" --unattended >/dev/null 2>&1; fi
ZSH_CUSTOM="${ZSH_CUSTOM:-$HOME/.oh-my-zsh/custom}"
cl() { [ -d "$2" ] || git clone --depth 1 "$1" "$2" >/dev/null 2>&1; }
cl https://github.com/romkatv/powerlevel10k.git              "$ZSH_CUSTOM/themes/powerlevel10k"
cl https://github.com/zsh-users/zsh-autosuggestions.git      "$ZSH_CUSTOM/plugins/zsh-autosuggestions"
cl https://github.com/zsh-users/zsh-syntax-highlighting.git  "$ZSH_CUSTOM/plugins/zsh-syntax-highlighting"
cl https://github.com/zsh-users/zsh-history-substring-search "$ZSH_CUSTOM/plugins/zsh-history-substring-search"
cl https://github.com/zsh-users/zsh-completions              "$ZSH_CUSTOM/plugins/zsh-completions"
[ -f "$HOME/.arena_infra/dotfiles/.vimrc" ]    && ln -sf "$HOME/.arena_infra/dotfiles/.vimrc"    "$HOME/.vimrc"
[ -f "$HOME/.arena_infra/dotfiles/.p10k.zsh" ] && ln -sf "$HOME/.arena_infra/dotfiles/.p10k.zsh" "$HOME/.p10k.zsh"
[ -f "$HOME/.arena_infra/dotfiles/.zshrc" ]    && cp -f "$HOME/.arena_infra/dotfiles/.zshrc"     "$HOME/.zshrc"
[ -f "$HOME/.zshrc" ] && sed -i 's/^conda activate arena-env$/command -v conda >\/dev\/null 2>\&1 \&\& conda activate arena-env/' "$HOME/.zshrc"
ZSH_BIN=$(command -v zsh); [ -n "$ZSH_BIN" ] && chsh -s "$ZSH_BIN" "$(id -un)" >/dev/null 2>&1
) || true"#.to_string(),
            );
        }
        // Optional: export broadcast tokens (Hugging Face, Claude Code) into the login
        // shells, idempotently, so participants get gated-repo / Claude Code access.
        if !self.broadcast_exports.is_empty() {
            let mut block = String::from("touch \"$HOME/.bashrc\" \"$HOME/.zshrc\"");
            for (name, value) in &self.broadcast_exports {
                let line = q(&format!("export {name}=\"{value}\""));
                for file in ["\"$HOME/.bashrc\"", "\"$HOME/.zshrc\""] {
                    block.push_str(&format!(" && (grep -qxF {line} {file} || echo {line} >> {file})"));
                }
            }
            steps.push(block);
        }
        steps.join("; ")
    }

    /// The ARENA-repo part of [`Self::remote_command`]: re-point `origin`, fetch, update
    /// the branch, update submodules. Separate so it can be exercised against real git
    /// repos in tests (the rest of the command touches `$HOME` and installs agents).
    ///
    /// Must run under the caller's `set -e` (a real git failure on an arena pod fails
    /// setup loudly).
    pub fn repo_update_command(&self, force: bool) -> String {
        let q = shell_quote;
        let branch = &self.branch;

        // Fetch ONLY the default branch, without tags. A bare `git fetch origin` pulls
        // every branch and tag of the cohort repo — which gains one autocommit branch per
        // participant per day, so it gets slower all programme long. The explicit
        // `+src:dst` refspec updates exactly `origin/<branch>`, which is all the reset /
        // checkout below reads. Deliberately NOT `--depth` (makes the clone shallow, which
        // risks breaking the backup pushes) and NOT `--filter=blob:none`: on an existing
        // full clone git either ignores it or silently re-configures `origin` as a
        // promisor remote — a lasting change to the participants' checkout.
        let fetch = format!(
            "git fetch --no-tags origin {}",
            q(&format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"))
        );

        // Branch update: force => checkout default + hard reset; else stay put.
        let git_update = if force {
            format!(
                "git checkout {b} && git reset --hard origin/{b}",
                b = q(branch)
            )
        } else {
            // Stay on the current branch. On the default branch, hard-reset to origin;
            // otherwise (e.g. an autocommit-wNdM branch with no upstream) only fast-
            // forward if it actually tracks a remote — never fail the whole setup. The
            // pull names the upstream (remote + ref) explicitly: a bare `git pull` runs a
            // bare `git fetch`, i.e. exactly the all-branches-and-tags fetch avoided above.
            format!(
                "CUR=$(git rev-parse --abbrev-ref HEAD); \
                 if [ \"$CUR\" = {b} ]; then git reset --hard origin/{b}; \
                 else (R=$(git config \"branch.$CUR.remote\") && M=$(git config \"branch.$CUR.merge\") && \
                 git pull --ff-only --no-tags \"$R\" \"$M\") || true; fi",
                b = q(branch)
            )
        };

        // The repo update only applies when the ARENA checkout is actually on the image
        // (the prebuilt arena image bakes it in at `repo_path`). On a non-arena base image
        // (e.g. an NVIDIA NGC image brought up with `--bootstrap`) it's absent — skip the
        // git steps with a notice instead of `cd`-ing into a missing dir and aborting the
        // whole setup under `set -e`. Everything else (SSH keys, ~/.name, agents, zsh,
        // tokens) still runs. The then-branch runs under the outer `set -e`, so a real git
        // failure on an arena pod still fails loudly.
        format!(
            "if [ -d {repo}/.git ]; then cd {repo}; git remote set-url origin {url}; \
             {fetch}; {git_update}; git submodule update --init --recursive; \
             else echo {skip} >&2; fi",
            repo = q(&self.repo_path),
            url = q(&self.repo_url),
            skip = q(&format!(
                "arena setup: {} not present — skipping repo update (non-arena image)",
                self.repo_path
            )),
        )
    }
}

/// The short machine name for `prefix`: the part after `{prefix}-` (`arena8-apple` →
/// `apple`); a name without the prefix (an absolute `@name` entry) is returned whole.
/// Free-standing (as well as [`SetupConfig::short_name`]) because `pods rename` rewrites
/// `~/.name` without needing the repo/deploy-key settings a full [`SetupConfig`] demands.
pub fn short_name<'a>(prefix: &str, machine_name: &'a str) -> &'a str {
    machine_name.strip_prefix(&format!("{prefix}-")).unwrap_or(machine_name)
}

/// The command that writes `~/.name` (`export MACHINE_NAME='<short>'`, overwriting) — the
/// one place its format lives: setup's step 5, and `pods rename`, which must leave the
/// file exactly as a fresh setup under the new name would.
pub fn name_file_command(short: &str) -> String {
    format!(
        "echo {} > \"$HOME/.name\"",
        shell_quote(&format!("export MACHINE_NAME='{short}'"))
    )
}

/// Time budgets for one pod's provisioning steps. Per step, not per pod, because the
/// steps differ by orders of magnitude: pushing a key takes seconds, the image-based
/// config a minute or two, and the hetzner bare-VM script (apt, docker, a uv venv with
/// the ML packages) legitimately ~10+ minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupTimeouts {
    /// Each scp (deploy key, setup script) — small files; a minute means a sick host.
    pub copy: Duration,
    /// The image-based post-image config command (keys, repo fetch, agents).
    pub config: Duration,
    /// The hetzner bare-VM setup script.
    pub bare_vm: Duration,
    /// The best-effort VS Code warm-up — its own budget, untouched by `--timeout` /
    /// `SETUP_TIMEOUT_SECS` (those size the main step).
    pub vscode: Duration,
}

impl Default for SetupTimeouts {
    fn default() -> Self {
        Self {
            copy: Duration::from_secs(60),
            config: Duration::from_secs(300),
            bare_vm: Duration::from_secs(1800),
            vscode: crate::vscode::WARMUP_TIMEOUT,
        }
    }
}

/// The largest run-step budget `--timeout` / `SETUP_TIMEOUT_SECS` accept: a day. No
/// provisioning step legitimately runs that long (the slowest, the hetzner script, gets
/// 30 minutes), so a bigger value is a typo or an attempt at "no limit" — and an
/// unbounded one (e.g. `u64::MAX`) would overflow the per-pod budget arithmetic.
pub const MAX_STEP_TIMEOUT_SECS: u64 = 24 * 3600;

impl SetupTimeouts {
    /// Defaults, with the run-step budget overridden by `--timeout <secs>` (wins) or the
    /// `SETUP_TIMEOUT_SECS` config value. The override sets the budget of the main
    /// provisioning command for *every* provider (image config and hetzner script alike)
    /// — it's the operator saying "give each pod this long". The config value is only
    /// read (and validated) when no flag is given, so a bad value can be overridden.
    /// Either must be 1..=[`MAX_STEP_TIMEOUT_SECS`].
    pub fn resolve(config_value: Option<&str>, flag: Option<u64>) -> Result<Self> {
        let in_range = |n: u64| (1..=MAX_STEP_TIMEOUT_SECS).contains(&n);
        let secs = match flag {
            Some(n) if !in_range(n) => {
                return Err(Error::Config(format!(
                    "--timeout must be between 1 and {MAX_STEP_TIMEOUT_SECS} seconds (got {n})"
                )))
            }
            Some(n) => Some(n),
            None => match config_value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(raw) => Some(raw.parse::<u64>().ok().filter(|&n| in_range(n)).ok_or_else(|| {
                    Error::Config(format!(
                        "SETUP_TIMEOUT_SECS must be a whole number of seconds, 1..={MAX_STEP_TIMEOUT_SECS} \
                         (got `{raw}`)"
                    ))
                })?),
            },
        };
        let mut t = Self::default();
        if let Some(secs) = secs {
            t.config = Duration::from_secs(secs);
            t.bare_vm = Duration::from_secs(secs);
        }
        Ok(t)
    }

    /// [`Self::resolve`] against a loaded config.
    pub fn from_config(cfg: &Config, flag: Option<u64>) -> Result<Self> {
        Self::resolve(cfg.get("SETUP_TIMEOUT_SECS"), flag)
    }
}

/// One step of provisioning a pod over SSH: push a file, or run a command. Each carries
/// a short human label (what a ✗ line names) and its own time budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionStep {
    Scp { label: &'static str, local: String, remote: String, timeout: Duration },
    Run { label: &'static str, cmd: String, timeout: Duration },
    /// A best-effort extra (the VS Code warm-up): run like `Run`, but when it fails or runs
    /// out of time the pod is still set up — the outcome carries a warning instead. Always
    /// last, so nothing required waits on it. `summary` is what the dry-run prints in place
    /// of the command (which carries a base64 script).
    Optional { label: &'static str, cmd: String, summary: String, timeout: Duration },
}

impl ProvisionStep {
    /// Short human name, e.g. "copy deploy key" — used in `timed out at <label>`.
    pub fn label(&self) -> &'static str {
        match self {
            ProvisionStep::Scp { label, .. } | ProvisionStep::Run { label, .. } | ProvisionStep::Optional { label, .. } => {
                label
            }
        }
    }

    /// This step's time budget.
    pub fn timeout(&self) -> Duration {
        match self {
            ProvisionStep::Scp { timeout, .. }
            | ProvisionStep::Run { timeout, .. }
            | ProvisionStep::Optional { timeout, .. } => *timeout,
        }
    }
}

/// The ordered provisioning steps for a pod, chosen by provider — pure and unit-tested,
/// so the runner never has to know what a provider *is*. Adding a backend = add its
/// steps here; the runner and the dry-run preview stay generic.
///   - bare-VM (hetzner): push the deploy key + the full setup script, then run it.
///   - image-based (runpod/vast): push the git deploy key, then the post-image config.
///
/// Both then get the VS Code warm-up (unless `scfg.vscode` is off) as a best-effort last
/// step on its own budget: on hetzner it runs after the script has built the venv it
/// points VS Code at.
pub fn provisioning_steps(
    provider: &str,
    scfg: &SetupConfig,
    name: &str,
    force: bool,
    hetzner_script_local: &str,
    timeouts: &SetupTimeouts,
) -> Vec<ProvisionStep> {
    let copy_key = ProvisionStep::Scp {
        label: "copy deploy key",
        local: scfg.key_local.clone(),
        remote: scfg.key_remote.clone(),
        timeout: timeouts.copy,
    };
    let mut steps = match provider {
        "hetzner" => vec![
            // Copy the git deploy key first, so the script can clone (and later push to)
            // the PRIVATE cohort repo over SSH — not just the public mirror.
            copy_key,
            ProvisionStep::Scp {
                label: "copy hetzner setup script",
                local: hetzner_script_local.to_string(),
                remote: "/root/hetzner_setup.sh".into(),
                timeout: timeouts.copy,
            },
            ProvisionStep::Run {
                label: "hetzner setup script",
                cmd: format!(
                    "REPO_URL={} REPO_DIR={} REPO_KEY={} bash /root/hetzner_setup.sh",
                    shell_quote(&scfg.repo_url),
                    shell_quote(&scfg.repo_path),
                    shell_quote(&scfg.key_remote),
                ),
                timeout: timeouts.bare_vm,
            },
        ],
        _ => vec![
            copy_key,
            ProvisionStep::Run {
                label: "repo + keys config",
                cmd: scfg.remote_command(name, force),
                timeout: timeouts.config,
            },
        ],
    };
    if let Some(vscode) = &scfg.vscode {
        steps.push(ProvisionStep::Optional {
            label: "vscode warm-up",
            cmd: vscode.remote_command(timeouts.vscode),
            summary: vscode.summary(),
            timeout: timeouts.vscode,
        });
    }
    steps
}

/// How [`provision`] rides out the create-vs-sshd-up boot race: a just-created VM can
/// report an SSH endpoint before sshd answers (hetzner assigns the IP at create), so a
/// *connection* failure is retried every `every` until `window` has passed since the
/// first attempt. Real failures (auth, script errors) and step timeouts are never retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootRetry {
    pub window: Duration,
    pub every: Duration,
}

impl Default for BootRetry {
    fn default() -> Self {
        Self { window: Duration::from_secs(150), every: Duration::from_secs(6) }
    }
}

/// The hard ceiling on one pod's provisioning: the boot-race window (plus one retry
/// pause) and every step's budget. Step timeouts already bound each attempt, so this
/// only bites if a [`Remote`] fails to honour its timeout — the guarantee that no pod
/// can hold a fleet `setup` longer than this, whatever the transport does. Saturating:
/// the inputs are public, and `Duration`'s `+`/`sum` panic on overflow — which would
/// crash every pod's setup task instead of provisioning anything.
pub fn pod_budget(steps: &[ProvisionStep], boot: BootRetry) -> Duration {
    steps
        .iter()
        .map(ProvisionStep::timeout)
        .chain([boot.window, boot.every])
        .fold(Duration::ZERO, Duration::saturating_add)
}

/// How one pod's provisioning ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// Every required step succeeded. `warnings`: the best-effort steps that didn't
    /// (`<label>: <why>`) — reported, never a failure.
    Done { warnings: Vec<String> },
    /// A step failed: non-zero exit (`code`), a failed copy, or the call itself erroring.
    Failed { step: &'static str, code: Option<i32>, detail: String },
    /// A step (or, defensively, the whole pod budget) ran out of time.
    TimedOut { step: &'static str, after: Duration },
}

impl ProvisionOutcome {
    /// Set up, with nothing to report.
    pub fn done() -> Self {
        ProvisionOutcome::Done { warnings: Vec::new() }
    }

    pub fn is_done(&self) -> bool {
        matches!(self, ProvisionOutcome::Done { .. })
    }

    /// The best-effort steps that didn't work out on a pod that is set up (else none).
    pub fn warnings(&self) -> &[String] {
        match self {
            ProvisionOutcome::Done { warnings } => warnings,
            _ => &[],
        }
    }

    /// What happened, without the pod's name: `done` (`done (warning: …)`), `timed out at
    /// <step> after Ns`, or `failed at <step>, exit N: <stderr>` — for a line that already
    /// names the pod (the fleet runner's, or `up`'s per-pod `[name] FAILED setup: …`).
    pub fn describe(&self) -> String {
        match self {
            ProvisionOutcome::Done { warnings } if warnings.is_empty() => "done".to_string(),
            ProvisionOutcome::Done { warnings } => format!("done (warning: {})", warnings.join("; ")),
            ProvisionOutcome::TimedOut { step, after } => format!("timed out at {step} after {}", human_duration(after)),
            ProvisionOutcome::Failed { step, code, detail } => {
                let exit = code.map(|c| format!(", exit {c}")).unwrap_or_default();
                let tail = if detail.is_empty() { String::new() } else { format!(": {detail}") };
                format!("failed at {step}{exit}{tail}")
            }
        }
    }
}

/// The fleet runner's per-pod progress line: `[done/total] ✓ name` (`… ✓ name (warning:
/// <step>: <why>)` when a best-effort step didn't work out), or
/// `[done/total] ✗ name (timed out at <step> after Ns)` /
/// `[done/total] ✗ name (failed at <step>, exit N): <stderr>`.
pub fn progress_line(done: usize, total: usize, name: &str, outcome: &ProvisionOutcome) -> String {
    match outcome {
        ProvisionOutcome::Done { warnings } if warnings.is_empty() => format!("[{done}/{total}] ✓ {name}"),
        ProvisionOutcome::Done { warnings } => format!("[{done}/{total}] ✓ {name} (warning: {})", warnings.join("; ")),
        ProvisionOutcome::TimedOut { .. } => format!("[{done}/{total}] ✗ {name} ({})", outcome.describe()),
        ProvisionOutcome::Failed { step, code, detail } => {
            let exit = code.map(|c| format!(", exit {c}")).unwrap_or_default();
            let tail = if detail.is_empty() { String::new() } else { format!(": {detail}") };
            format!("[{done}/{total}] ✗ {name} (failed at {step}{exit}){tail}")
        }
    }
}

/// True if a failure message reads like a host that isn't reachable *yet* (worth
/// waiting on during the boot race) rather than a real provisioning failure. Matches
/// what ssh/scp print: "connect to host … Connection refused", "Connection timed out",
/// "kex_exchange_identification: Connection closed" / "… Connection reset by peer", "No
/// route to host", "Network is unreachable". Only ever applied to a provider/stderr
/// message — a step *timeout* is its own error variant and is never routed here.
///
/// An *authentication* failure is checked first and is never "unreachable": the host
/// answered and rejected our key, which waiting won't change. It needs the explicit
/// check because scp follows ssh's `Permission denied (publickey).` with its own
/// `scp: Connection closed` (sftp mode) / `lost connection` (`-O`), which a plain
/// connection-word match would read as the boot race — burning the whole window and then
/// misreporting a key problem as "still unreachable".
pub fn looks_unreachable(message: &str) -> bool {
    let m = message.to_lowercase();
    const REJECTED: &[&str] = &[
        "permission denied (", // ssh: "Permission denied (publickey,password)."
        "too many authentication failures",
        "no supported authentication methods",
        "host key verification failed",
    ];
    if REJECTED.iter().any(|needle| m.contains(needle)) {
        return false;
    }
    [
        "connect to host",
        "connection refused",
        "timed out",
        "connection closed",
        "connection reset",
        "no route",
        "unreachable",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

/// One pass over the steps.
enum Pass {
    /// Finished — success, a real failure, or a timeout. Not retried.
    Final(ProvisionOutcome),
    /// The host refused / dropped the connection before anything ran — retry while the
    /// boot window is open.
    Unreachable(ProvisionOutcome),
}

/// Run a pod's provisioning `steps` in order over `remote`, each within its own budget,
/// stopping at the first failure. Connection failures are retried for the boot window
/// (whole sequence from the top — every step is idempotent); everything is capped by
/// [`pod_budget`]. Never errors: the outcome says what happened and where, so the caller
/// can report it and carry on with the rest of the fleet.
pub async fn provision(
    remote: &dyn Remote,
    target: &SshTarget,
    steps: &[ProvisionStep],
    boot: BootRetry,
) -> ProvisionOutcome {
    if steps.is_empty() {
        return ProvisionOutcome::Failed {
            step: "provisioning",
            code: None,
            detail: "no provisioning steps".into(),
        };
    }
    // Index of the step in flight, so a budget expiry can still name it.
    let at = AtomicUsize::new(0);
    let attempts = async {
        let start = tokio::time::Instant::now();
        let mut retried = false;
        loop {
            match provision_once(remote, target, steps, &at).await {
                Pass::Unreachable(_) if start.elapsed() < boot.window => {
                    retried = true;
                    tokio::time::sleep(boot.every).await;
                }
                Pass::Unreachable(ProvisionOutcome::Failed { step, code, detail }) if retried => {
                    let detail = format!(
                        "{detail} (still unreachable after retrying for {})",
                        human_duration(&boot.window)
                    );
                    break ProvisionOutcome::Failed { step, code, detail };
                }
                Pass::Unreachable(outcome) | Pass::Final(outcome) => break outcome,
            }
        }
    };
    let budget = pod_budget(steps, boot);
    match tokio::time::timeout(budget, attempts).await {
        Ok(outcome) => outcome,
        Err(_) => {
            let step = &steps[at.load(Ordering::Relaxed).min(steps.len() - 1)];
            // Stuck in the best-effort tail: every required step is done, so the pod is
            // set up — the same as that step timing out on its own.
            if let ProvisionStep::Optional { label, .. } = step {
                let why = format!("{label}: timed out after {}", human_duration(&budget));
                return ProvisionOutcome::Done { warnings: vec![why] };
            }
            ProvisionOutcome::TimedOut { step: step.label(), after: budget }
        }
    }
}

/// Whether a provisioning failure is the pod refusing our SSH key — `Permission denied
/// (publickey…)`, from ssh or scp — the one failure a provider-side key repair
/// ([`crate::provider::Provider::authorize_ssh_keys`], Vast's per-instance attach) can fix.
/// A *file* permission error on the pod (`scp: …: Permission denied`) is not it. Pure.
pub fn key_rejected(outcome: &ProvisionOutcome) -> bool {
    matches!(outcome, ProvisionOutcome::Failed { detail, .. }
        if detail.to_lowercase().contains("permission denied (publickey"))
}

/// After a key repair, setup runs again up to this many times…
pub const KEY_REPAIR_TRIES: u32 = 3;
/// …this far apart (the first after one wait), while the pod still refuses the key: a
/// provider applies a key attached through its API to the running container on its own
/// schedule (Vast: asynchronously), not by the time the API call returns.
pub const KEY_REPAIR_EVERY: Duration = Duration::from_secs(20);

/// Re-run a pod's provisioning after its provider re-authorized our keys: wait
/// [`KEY_REPAIR_EVERY`], [`provision`], and repeat while the pod still refuses the key — at
/// most [`KEY_REPAIR_TRIES`] times. Every step is idempotent (the runner already re-runs the
/// sequence from the top on a connection failure), so running it again is safe. Returns the
/// last outcome; never errors.
pub async fn provision_after_key_repair(
    remote: &dyn Remote,
    target: &SshTarget,
    steps: &[ProvisionStep],
    boot: BootRetry,
) -> ProvisionOutcome {
    let mut tries = 0;
    loop {
        tokio::time::sleep(KEY_REPAIR_EVERY).await;
        let outcome = provision(remote, target, steps, boot).await;
        tries += 1;
        if tries >= KEY_REPAIR_TRIES || !key_rejected(&outcome) {
            return outcome;
        }
    }
}

async fn provision_once(
    remote: &dyn Remote,
    target: &SshTarget,
    steps: &[ProvisionStep],
    at: &AtomicUsize,
) -> Pass {
    let mut warnings = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        at.store(i, Ordering::Relaxed);
        let step_label = step.label();
        let result = match step {
            ProvisionStep::Scp { local, remote: dst, timeout, .. } => {
                remote.copy(target, local, dst, Some(*timeout)).await
            }
            ProvisionStep::Run { cmd, timeout, .. } => {
                let call = remote.exec(target, cmd, Some(*timeout)).await;
                // A required step that worked can still have something to say (the repo
                // couldn't go onto the volume, and why): its `arena-warning:` lines.
                if let Ok(out) = &call {
                    if out.success {
                        warnings.extend(step_warnings(&out.stdout).map(|w| format!("{step_label}: {w}")));
                    }
                }
                call
            }
            ProvisionStep::Optional { cmd, timeout, .. } => {
                // The required steps are done: whatever happens here, the pod is set up.
                let call = remote.exec(target, cmd, Some(*timeout)).await;
                if let Some(why) = optional_failure(call) {
                    warnings.push(format!("{step_label}: {why}"));
                }
                continue;
            }
        };
        match result {
            Ok(out) if out.success => {}
            Ok(out) => {
                let failed = ProvisionOutcome::Failed {
                    step: step_label,
                    code: out.code,
                    detail: out.stderr.trim().to_string(),
                };
                // A copy that never reached the host (scp exits with ssh's connect error)
                // is the boot race. A failed *command* already ran, at least in part —
                // report it rather than re-run it.
                let copy = matches!(step, ProvisionStep::Scp { .. });
                return if copy && looks_unreachable(&out.stderr) {
                    Pass::Unreachable(failed)
                } else {
                    Pass::Final(failed)
                };
            }
            Err(Error::Timeout { after, .. }) => {
                return Pass::Final(ProvisionOutcome::TimedOut { step: step_label, after })
            }
            Err(e) => {
                let detail = e.to_string();
                let unreachable = looks_unreachable(&detail);
                let failed = ProvisionOutcome::Failed { step: step_label, code: None, detail };
                return if unreachable { Pass::Unreachable(failed) } else { Pass::Final(failed) };
            }
        }
    }
    Pass::Final(ProvisionOutcome::Done { warnings })
}

/// The warnings a provisioning command printed: its stdout lines starting with
/// [`crate::volume::WARNING_PREFIX`], prefix stripped (clipped, so a runaway line stays one line).
fn step_warnings(stdout: &str) -> impl Iterator<Item = String> + '_ {
    stdout
        .lines()
        .filter_map(|l| l.trim_end_matches('\r').strip_prefix(crate::volume::WARNING_PREFIX))
        .map(|w| crate::fleet::clip(w.trim(), 300))
}

/// Why a best-effort step didn't work out (`None` = it did): the last stderr line of a
/// non-zero exit (where the warm-up script puts its one-line reason, after any ssh
/// chatter), clipped; a timeout; or the call's own error.
fn optional_failure(call: Result<crate::ssh::SshOutput>) -> Option<String> {
    match call {
        Ok(out) if out.success => None,
        Ok(out) => {
            let exit = out.code.map(|c| format!("exit {c}")).unwrap_or_else(|| "killed".into());
            let last = out.stderr.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("");
            Some(if last.is_empty() { exit } else { format!("{exit}: {}", crate::fleet::clip(last, 300)) })
        }
        Err(Error::Timeout { after, .. }) => Some(format!("timed out after {}", human_duration(&after))),
        Err(e) => Some(e.to_string()),
    }
}

/// Single-quote for safe inclusion in a `sh -c` string (POSIX `'\''` escaping).
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SetupConfig {
        SetupConfig {
            key_local: "/root/.ssh/arena_infra_key".into(),
            key_remote: "/root/.ssh/id_ed25519".into(),
            repo_path: "/root/ARENA_3.0".into(),
            repo_url: "git@github.com:styme3279/ARENA_3.0.git".into(),
            branch: "main".into(),
            prefix: "arena8".into(),
            authorized_pubkeys: vec!["ssh-ed25519 AAAASHARED shared".into()],
            broadcast_exports: Vec::new(),
            zsh_install: false,
            vscode: None,
        }
    }

    #[test]
    fn short_name_strips_prefix() {
        assert_eq!(cfg().short_name("arena8-apple"), "apple");
        assert_eq!(cfg().short_name("apple"), "apple");
        assert_eq!(short_name("arena9", "arena9-bloom"), "bloom");
        assert_eq!(short_name("arena9", "james-gpu"), "james-gpu"); // absolute entry
        assert_eq!(short_name("arena9", "arena8-bloom"), "arena8-bloom"); // other prefix: whole
    }

    /// `pods rename` rewrites `~/.name` with `name_file_command`: it must be byte-for-byte
    /// the step setup runs, so a renamed pod looks exactly like one set up under the name.
    #[test]
    fn name_file_command_is_setups_name_step() {
        let cmd = name_file_command("apple");
        assert_eq!(cmd, r#"echo 'export MACHINE_NAME='\''apple'\''' > "$HOME/.name""#);
        assert!(cfg().remote_command("arena8-apple", false).contains(&cmd));
    }

    #[test]
    fn renders_full_provisioning_in_order() {
        let c = cfg().remote_command("arena8-apple", false);
        assert!(c.contains("chmod 600 '/root/.ssh/id_ed25519'"));
        // github.com ssh config block
        assert!(c.contains("# BEGIN arena-infra github.com"));
        assert!(c.contains("IdentityFile /root/.ssh/id_ed25519"));
        // configured pubkeys + the on-pod deploy key added to authorized_keys (idempotent)
        assert!(c.contains("grep -qxF 'ssh-ed25519 AAAASHARED shared'"));
        assert!(c.contains("ssh-keygen -y -f /root/.ssh/id_ed25519"));
        assert!(c.contains(r#"grep -qxF "$PUB" "$HOME/.ssh/authorized_keys""#));
        // repo wiring — guarded so a missing checkout (non-arena image) skips, not aborts
        assert!(c.contains("if [ -d '/root/ARENA_3.0'/.git ]; then cd '/root/ARENA_3.0'"));
        assert!(c.contains("not present — skipping repo update"));
        assert!(c.contains("git remote set-url origin 'git@github.com:styme3279/ARENA_3.0.git'"));
        assert!(c.contains("git fetch --no-tags origin '+refs/heads/main:refs/remotes/origin/main'"));
        assert!(c.contains("git submodule update --init --recursive"));
        // .name uses the EXPORT form with the SHORT name
        assert!(c.contains(r#"echo 'export MACHINE_NAME='\''apple'\''' > "$HOME/.name""#));
        // non-force stays on current branch
        assert!(c.contains("git rev-parse --abbrev-ref HEAD"));
        assert!(!c.contains("git checkout 'main'"));
    }

    /// With a volume the repo moves onto it BEFORE the update (so the fetch/reset act on the
    /// volume copy); a path that can't be relocated (already on /workspace) gets no block.
    #[test]
    fn the_repo_goes_onto_the_volume_before_it_is_updated() {
        let c = cfg().remote_command("arena8-apple", false);
        let reloc = crate::volume::relocation_command("/root/ARENA_3.0").unwrap();
        let at = |needle: &str| c.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
        assert!(at("ssh-keygen -y") < at(&reloc), "after the keys");
        assert!(at(&reloc) < at("git remote set-url"), "before the repo update");
        let mut on_volume = cfg();
        on_volume.repo_path = "/workspace/ARENA_3.0".into();
        assert!(!on_volume.remote_command("arena8-apple", false).contains("arena-aside"));
    }

    #[test]
    fn force_hard_resets_to_default_branch() {
        let c = cfg().remote_command("arena8-apple", true);
        assert!(c.contains("git checkout 'main' && git reset --hard origin/'main'"));
    }

    #[test]
    fn fetch_is_narrow_on_both_paths() {
        // Only the default branch, no tags — never the whole cohort repo (one autocommit
        // branch per participant per day) — and nothing that changes the clone's shape.
        for force in [false, true] {
            let c = cfg().repo_update_command(force);
            assert!(c.contains("git fetch --no-tags origin '+refs/heads/main:refs/remotes/origin/main'"), "{c}");
            assert!(!c.contains("git fetch origin;"), "bare all-refs fetch is back: {c}");
            assert!(!c.contains("--depth") && !c.contains("--filter"), "{c}");
            assert!(cfg().remote_command("arena8-apple", force).contains(&c));
        }
        // The non-force pull on a tracked branch names its upstream: a bare `git pull`
        // would run a bare (all-branches + tags) `git fetch`.
        let c = cfg().repo_update_command(false);
        assert!(c.contains(r#"git pull --ff-only --no-tags "$R" "$M""#), "{c}");
        assert!(!c.contains("git pull --ff-only)"), "{c}");
        // Branch names are quoted into the refspec.
        let mut odd = cfg();
        odd.branch = "it's".into();
        assert!(odd.repo_update_command(true).contains(r"'+refs/heads/it'\''s:refs/remotes/origin/it'\''s'"));
    }

    #[test]
    fn zsh_install_off_by_default_on_when_set() {
        // Off: no zsh step at all.
        let off = cfg().remote_command("arena8-apple", false);
        assert!(!off.contains("install -y -qq zsh"));
        assert!(!off.contains(".arena_infra"));
        // On: installs zsh, clones the shared dotfiles + oh-my-zsh/p10k, sets the login
        // shell, and guards the dotfiles' conda activation for no-conda images.
        let mut c = cfg();
        c.zsh_install = true;
        let on = c.remote_command("arena8-apple", false);
        assert!(on.contains("install -y -qq zsh figlet"));
        assert!(on.contains("git clone --depth 1 https://github.com/nickypro/arena-infra"));
        assert!(on.contains("tools/install.sh")); // oh-my-zsh
        assert!(on.contains("themes/powerlevel10k"));
        assert!(on.contains(r#"cp -f "$HOME/.arena_infra/dotfiles/.zshrc""#));
        // bare `conda activate` is rewritten to a guarded form, never left unguarded.
        assert!(on.contains(r"s/^conda activate arena-env$/command -v conda"));
        assert!(on.contains(r#"chsh -s "$ZSH_BIN""#));
        // no ARENA_3.0 checkout / venv activation on this path.
        assert!(!on.contains("/opt/arena-env/bin/activate"));
    }

    #[test]
    fn no_token_export_without_any_token() {
        let c = cfg().remote_command("arena8-apple", false);
        assert!(!c.contains("HF_TOKEN"));
        assert!(!c.contains("CLAUDE_CODE_OAUTH_TOKEN"));
    }

    #[test]
    fn exports_broadcast_tokens_idempotently_when_present() {
        let mut sc = cfg();
        sc.broadcast_exports = vec![
            ("HF_TOKEN".into(), "hf_secret".into()),
            ("HUGGING_FACE_HUB_TOKEN".into(), "hf_secret".into()),
            ("CLAUDE_CODE_OAUTH_TOKEN".into(), "cc_secret".into()),
        ];
        let c = sc.remote_command("arena8-apple", false);
        // Each into both shells, guarded so a re-run doesn't duplicate.
        assert!(c.contains(r#"grep -qxF 'export HF_TOKEN="hf_secret"' "$HOME/.bashrc""#));
        assert!(c.contains(r#"export CLAUDE_CODE_OAUTH_TOKEN="cc_secret""#));
        assert!(c.contains("\"$HOME/.zshrc\""));
    }

    // ---- provisioning steps, timeouts, runner -------------------------------------------

    use crate::remote::{FakeRemote, FakeReply, RemoteCall};

    fn steps_cfg() -> SetupConfig {
        SetupConfig {
            key_local: "/local/key".into(),
            key_remote: "/root/.ssh/id_ed25519".into(),
            repo_path: "/root/ARENA_3.0".into(),
            repo_url: "git@github.com:o/r.git".into(),
            branch: "main".into(),
            prefix: "arena8".into(),
            authorized_pubkeys: vec![],
            broadcast_exports: vec![],
            zsh_install: false,
            vscode: None,
        }
    }

    fn target(port: u16) -> SshTarget {
        SshTarget {
            user: "root".into(),
            host: "10.0.0.1".into(),
            port,
            key_paths: vec![],
            connect_timeout_secs: 10,
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn provisioning_steps_branch_by_provider() {
        let t = SetupTimeouts::default();
        let scfg = steps_cfg();
        // bare-VM (hetzner): copy the deploy key, push the script, run it with the repo
        // URL/key passed in (so it clones the PRIVATE repo over SSH) — on the long budget.
        assert_eq!(
            provisioning_steps("hetzner", &scfg, "arena8-flutter", false, "/tmp/h.sh", &t),
            vec![
                ProvisionStep::Scp {
                    label: "copy deploy key",
                    local: "/local/key".into(),
                    remote: "/root/.ssh/id_ed25519".into(),
                    timeout: secs(60),
                },
                ProvisionStep::Scp {
                    label: "copy hetzner setup script",
                    local: "/tmp/h.sh".into(),
                    remote: "/root/hetzner_setup.sh".into(),
                    timeout: secs(60),
                },
                ProvisionStep::Run {
                    label: "hetzner setup script",
                    cmd: "REPO_URL='git@github.com:o/r.git' REPO_DIR='/root/ARENA_3.0' REPO_KEY='/root/.ssh/id_ed25519' bash /root/hetzner_setup.sh".into(),
                    timeout: secs(1800),
                },
            ]
        );
        // image-based (runpod/vast): scp the deploy key, then a config command that
        // re-points origin — i.e. the post-image flow, not the bare-VM script.
        for provider in ["runpod", "vast"] {
            let r = provisioning_steps(provider, &scfg, "arena8-apple", false, "/tmp/h.sh", &t);
            assert_eq!(r.len(), 2);
            assert!(matches!(&r[0], ProvisionStep::Scp { local, remote, .. } if local == "/local/key" && remote == "/root/.ssh/id_ed25519"));
            assert!(matches!(&r[1], ProvisionStep::Run { cmd, .. } if cmd.contains("git remote set-url")));
            assert_eq!(r[1].label(), "repo + keys config");
            assert_eq!(r[1].timeout(), secs(300));
        }
    }

    #[test]
    fn setup_timeouts_resolve_flag_over_config_over_defaults() {
        let d = SetupTimeouts::default();
        assert_eq!((d.copy, d.config, d.bare_vm), (secs(60), secs(300), secs(1800)));
        let run = |cfg: Option<&str>, flag: Option<u64>| {
            SetupTimeouts::resolve(cfg, flag).map(|t| (t.copy, t.config, t.bare_vm))
        };
        // (config value, --timeout) -> (copy, config, bare_vm)
        let table: &[(Option<&str>, Option<u64>, (Duration, Duration, Duration))] = &[
            (None, None, (secs(60), secs(300), secs(1800))),
            (Some(""), None, (secs(60), secs(300), secs(1800))),
            (Some(" 900 "), None, (secs(60), secs(900), secs(900))),
            (Some("900"), Some(120), (secs(60), secs(120), secs(120))),
            // The flag wins outright, so a bad config value can be overridden.
            (Some("soon"), Some(120), (secs(60), secs(120), secs(120))),
        ];
        for (cfg, flag, want) in table {
            assert_eq!(run(*cfg, *flag).unwrap(), *want, "{cfg:?} {flag:?}");
        }
        for (cfg, flag) in [(Some("soon"), None), (Some("0"), None), (Some("-5"), None), (None, Some(0))] {
            let e = run(cfg, flag).unwrap_err();
            assert!(matches!(e, Error::Config(_)), "{cfg:?} {flag:?}: {e}");
        }
        // Bounded above too: "no limit" spelled as a huge number is refused with a clear
        // message instead of overflowing the budget arithmetic later (inside every pod's
        // setup task). The cap itself is accepted.
        let day = MAX_STEP_TIMEOUT_SECS;
        assert_eq!(run(None, Some(day)).unwrap().1, secs(day));
        assert_eq!(run(Some("86400"), None).unwrap().2, secs(day));
        for (cfg, flag) in [
            (None, Some(u64::MAX)),
            (None, Some(day + 1)),
            (Some("18446744073709551400"), None),
            (Some("18446744073709551616"), None), // > u64::MAX: unparsable, same error
            (Some("86401"), None),
        ] {
            let e = run(cfg, flag).unwrap_err().to_string();
            assert!(e.contains("86400"), "{cfg:?} {flag:?}: {e}");
        }
        let cfg = Config::parse("SETUP_TIMEOUT_SECS=42");
        assert_eq!(SetupTimeouts::from_config(&cfg, None).unwrap().config, secs(42));
    }

    #[test]
    fn looks_unreachable_matches_ssh_connect_failures_only() {
        for msg in [
            "ssh: connect to host 1.2.3.4 port 22: Connection refused",
            "ssh: connect to host 1.2.3.4 port 22: Connection timed out",
            "kex_exchange_identification: Connection closed by remote host",
            "ssh: connect to host 1.2.3.4 port 22: No route to host",
            "ssh: connect to host 1.2.3.4 port 22: Network is unreachable",
        ] {
            assert!(looks_unreachable(msg), "{msg}");
        }
        for msg in ["fatal: couldn't find remote ref refs/heads/main", "bash: line 1: foo: command not found", ""] {
            assert!(!looks_unreachable(msg), "{msg}");
        }
        // The pod answered and rejected our key — what real scp (OpenSSH 9.6) prints on an
        // auth failure in sftp mode and in legacy `-O` mode. Its trailing "Connection
        // closed" / "lost connection" must not read as the boot race.
        for msg in [
            "root@10.0.0.1: Permission denied (publickey).\nscp: Connection closed\r\n",
            "root@10.0.0.1: Permission denied (publickey).\nlost connection\n",
            "root@10.0.0.1: Permission denied (publickey,password).",
            "Received disconnect from 10.0.0.1 port 22:2: Too many authentication failures\nscp: Connection closed",
            "Host key verification failed.\r\nlost connection",
        ] {
            assert!(!looks_unreachable(msg), "{msg}");
        }
        // Still the boot race: sshd up but not ready yet.
        assert!(looks_unreachable("kex_exchange_identification: read: Connection reset by peer\nscp: Connection closed"));
    }

    #[test]
    fn progress_lines() {
        use ProvisionOutcome::*;
        assert_eq!(progress_line(1, 3, "arena8-apple", &ProvisionOutcome::done()), "[1/3] ✓ arena8-apple");
        assert_eq!(
            progress_line(2, 3, "arena8-bloom", &TimedOut { step: "repo + keys config", after: secs(300) }),
            "[2/3] ✗ arena8-bloom (timed out at repo + keys config after 300s)"
        );
        assert_eq!(
            progress_line(3, 3, "arena8-cloud", &Failed { step: "copy deploy key", code: Some(1), detail: "scp: denied".into() }),
            "[3/3] ✗ arena8-cloud (failed at copy deploy key, exit 1): scp: denied"
        );
        assert_eq!(
            progress_line(3, 3, "c", &Failed { step: "repo + keys config", code: None, detail: String::new() }),
            "[3/3] ✗ c (failed at repo + keys config)"
        );
        // The same reasons without the name/counter, for a line that already names the pod.
        assert_eq!(ProvisionOutcome::done().describe(), "done");
        assert_eq!(
            TimedOut { step: "repo + keys config", after: secs(300) }.describe(),
            "timed out at repo + keys config after 300s"
        );
        assert_eq!(
            Failed { step: "copy deploy key", code: Some(1), detail: "scp: denied".into() }.describe(),
            "failed at copy deploy key, exit 1: scp: denied"
        );
    }

    #[test]
    fn pod_budget_is_steps_plus_boot_window() {
        let steps = provisioning_steps("runpod", &steps_cfg(), "arena8-apple", false, "", &SetupTimeouts::default());
        // 60 (key) + 300 (config) + 150 (boot window) + 6 (one retry pause)
        assert_eq!(pod_budget(&steps, BootRetry::default()), secs(516));
    }

    #[test]
    fn pod_budget_saturates_instead_of_panicking() {
        // Huge step budgets (the inputs are public) must not panic on overflow.
        let mut steps = image_steps();
        for s in &mut steps {
            match s {
                ProvisionStep::Scp { timeout, .. }
                | ProvisionStep::Run { timeout, .. }
                | ProvisionStep::Optional { timeout, .. } => *timeout = Duration::MAX,
            }
        }
        assert_eq!(pod_budget(&steps, BootRetry::default()), Duration::MAX);
        let boot = BootRetry { window: Duration::MAX, every: Duration::MAX };
        assert_eq!(pod_budget(&image_steps(), boot), Duration::MAX);
    }

    #[tokio::test(start_paused = true)]
    async fn provision_survives_an_unbounded_budget() {
        let mut steps = image_steps();
        if let ProvisionStep::Run { timeout, .. } = &mut steps[1] {
            *timeout = Duration::MAX;
        }
        assert_eq!(provision(&FakeRemote::new(), &target(22), &steps, BootRetry::default()).await, ProvisionOutcome::done());
    }

    fn image_steps() -> Vec<ProvisionStep> {
        provisioning_steps("runpod", &steps_cfg(), "arena8-apple", false, "", &SetupTimeouts::default())
    }

    #[tokio::test(start_paused = true)]
    async fn all_steps_succeed_in_order_with_their_budgets() {
        // (c) step order per provider, as the remote sees it.
        let fake = FakeRemote::new();
        let steps = provisioning_steps("hetzner", &steps_cfg(), "arena8-flutter", false, "/tmp/h.sh", &SetupTimeouts::default());
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::done());
        let calls = fake.calls();
        assert_eq!(calls.len(), 3);
        assert!(matches!(&calls[0], RemoteCall::Copy { local, timeout, .. } if local == "/local/key" && *timeout == Some(secs(60))));
        assert!(matches!(&calls[1], RemoteCall::Copy { remote, .. } if remote == "/root/hetzner_setup.sh"));
        assert!(matches!(&calls[2], RemoteCall::Exec { cmd, timeout, .. } if cmd.ends_with("bash /root/hetzner_setup.sh") && *timeout == Some(secs(1800))));

        let fake = FakeRemote::new();
        assert_eq!(provision(&fake, &target(22), &image_steps(), BootRetry::default()).await, ProvisionOutcome::done());
        let calls = fake.calls();
        assert!(matches!(&calls[..], [RemoteCall::Copy { .. }, RemoteCall::Exec { cmd, .. }] if cmd.contains("git remote set-url")));
    }

    #[tokio::test(start_paused = true)]
    async fn failing_scp_stops_the_pod_at_that_step() {
        // (b) a real scp failure (not a connection error) is final: no config run, no retry.
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::exit(1, "scp: /root/.ssh/id_ed25519: Permission denied\n")]);
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(
            out,
            ProvisionOutcome::Failed {
                step: "copy deploy key",
                code: Some(1),
                detail: "scp: /root/.ssh/id_ed25519: Permission denied".into()
            }
        );
        assert_eq!(fake.calls().len(), 1, "stopped at the failing step");
    }

    #[tokio::test(start_paused = true)]
    async fn rejected_key_fails_at_once_not_after_the_boot_window() {
        // A pod that refuses our key answers scp exactly like this. One attempt, reported
        // as the auth failure it is — not 150s of retries ending in "still unreachable".
        let fake = FakeRemote::new();
        let denied = "root@10.0.0.1: Permission denied (publickey).\nscp: Connection closed\r\n";
        fake.script("10.0.0.1:22", (0..50).map(|_| FakeReply::exit(255, denied)));
        let start = tokio::time::Instant::now();
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(
            out,
            ProvisionOutcome::Failed { step: "copy deploy key", code: Some(255), detail: denied.trim().into() }
        );
        assert_eq!(fake.calls().len(), 1, "an auth failure is never retried");
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[test]
    fn key_rejected_means_the_pod_refused_our_key() {
        let failed = |detail: &str| ProvisionOutcome::Failed { step: "copy deploy key", code: Some(255), detail: detail.into() };
        assert!(key_rejected(&failed("root@10.0.0.1: Permission denied (publickey).\nscp: Connection closed")));
        assert!(key_rejected(&failed("root@10.0.0.1: permission denied (publickey,password).")));
        // A file permission on the pod, a timeout, success: not a key problem.
        assert!(!key_rejected(&failed("scp: /root/.ssh/id_ed25519: Permission denied")));
        assert!(!key_rejected(&failed("ssh: connect to host 10.0.0.1 port 22: Connection refused")));
        assert!(!key_rejected(&ProvisionOutcome::TimedOut { step: "copy deploy key", after: secs(60) }));
        assert!(!key_rejected(&ProvisionOutcome::done()));
    }

    #[tokio::test(start_paused = true)]
    async fn after_a_key_repair_setup_reruns_until_the_key_is_accepted() {
        let denied = "root@10.0.0.1: Permission denied (publickey).\nscp: Connection closed\r\n";
        // Still refused 20s after the repair, accepted at 40s.
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::exit(255, denied), FakeReply::ok(), FakeReply::ok()]);
        let start = tokio::time::Instant::now();
        let out = provision_after_key_repair(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::done());
        assert_eq!(start.elapsed(), secs(40));
        assert_eq!(fake.calls().len(), 3, "refused copy, then copy + config");
        // Never accepted: bounded — the refusal stands after the last try.
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", (0..10).map(|_| FakeReply::exit(255, denied)));
        let start = tokio::time::Instant::now();
        let out = provision_after_key_repair(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert!(key_rejected(&out), "{out:?}");
        assert_eq!(start.elapsed(), KEY_REPAIR_EVERY * KEY_REPAIR_TRIES);
        assert_eq!(fake.calls().len(), KEY_REPAIR_TRIES as usize);
        // Any other failure ends it at once — the key was the only thing to wait for.
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::exit(128, "fatal: couldn't find remote ref")]);
        let out = provision_after_key_repair(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert!(matches!(&out, ProvisionOutcome::Failed { step: "repo + keys config", .. }), "{out:?}");
        assert_eq!(fake.calls().len(), 2);
    }

    /// The relocation reports a repo it couldn't put on the volume as an `arena-warning:` line:
    /// the pod is set up, with the warning — never a failed setup, never silent.
    #[tokio::test(start_paused = true)]
    async fn a_required_step_that_worked_can_still_warn() {
        let fake = FakeRemote::new();
        let said = "arena-volume: nothing\narena-warning: repo not moved onto the /workspace volume: in use (working directory of pid 42) - re-run setup when it is idle\r\n";
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::stdout(said)]);
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(
            out,
            ProvisionOutcome::Done {
                warnings: vec!["repo + keys config: repo not moved onto the /workspace volume: in use (working directory of pid 42) - re-run setup when it is idle".into()]
            }
        );
        // A failed step's stdout isn't mined for warnings: the failure is the report.
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::exit(1, "boom").with_stdout("arena-warning: x\n")]);
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert!(matches!(&out, ProvisionOutcome::Failed { detail, .. } if detail == "boom"), "{out:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn failing_command_is_reported_not_retried() {
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::exit(128, "fatal: couldn't find remote ref")]);
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert!(matches!(&out, ProvisionOutcome::Failed { step: "repo + keys config", code: Some(128), .. }), "{out:?}");
        assert_eq!(fake.calls().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_step_times_out_naming_the_step() {
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::hang()]);
        let start = tokio::time::Instant::now();
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::TimedOut { step: "repo + keys config", after: secs(300) });
        assert_eq!(start.elapsed(), secs(300));
        assert_eq!(fake.calls().len(), 2, "a timeout is never retried as a connection error");
    }

    #[tokio::test(start_paused = true)]
    async fn connection_refused_then_success_is_retried_within_the_boot_window() {
        // (d) sshd not up yet: scp exits 255 with ssh's connect error twice, then works.
        let refused = || FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22: Connection refused");
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [refused(), FakeReply::error("ssh: connect to host 10.0.0.1 port 22: No route to host"), FakeReply::ok(), FakeReply::ok()]);
        let start = tokio::time::Instant::now();
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::done());
        assert_eq!(start.elapsed(), secs(12), "two 6s retry pauses");
        let kinds: Vec<&str> = fake
            .calls()
            .iter()
            .map(|c| if matches!(c, RemoteCall::Copy { .. }) { "copy" } else { "exec" })
            .collect();
        assert_eq!(kinds, ["copy", "copy", "copy", "exec"], "restarts from the first step");
    }

    #[tokio::test(start_paused = true)]
    async fn unreachable_past_the_boot_window_gives_up() {
        let fake = FakeRemote::new();
        fake.script(
            "10.0.0.1:22",
            (0..100).map(|_| FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22: Connection refused").after(secs(10))),
        );
        let start = tokio::time::Instant::now();
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        let ProvisionOutcome::Failed { step, code, detail } = &out else { panic!("{out:?}") };
        assert_eq!((*step, *code), ("copy deploy key", Some(255)));
        assert!(detail.contains("Connection refused") && detail.contains("still unreachable after retrying for 150s"), "{detail}");
        // An attempt every 16s (10s to fail + 6s pause) while under 150s: starts at
        // 0,16,…,144; the one starting at 144 fails at 154 — past the window, so stop.
        assert_eq!(fake.calls().len(), 10);
        assert_eq!(start.elapsed(), secs(154));
        assert!(start.elapsed() <= pod_budget(&image_steps(), BootRetry::default()));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_window_means_no_retry() {
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::exit(255, "Connection refused")]);
        let boot = BootRetry { window: Duration::ZERO, every: secs(6) };
        let out = provision(&fake, &target(22), &image_steps(), boot).await;
        assert_eq!(out, ProvisionOutcome::Failed { step: "copy deploy key", code: Some(255), detail: "Connection refused".into() });
        assert_eq!(fake.calls().len(), 1);
    }

    /// A transport that ignores its timeout — the pod budget must still cap it.
    struct DeafRemote;

    #[async_trait::async_trait]
    impl Remote for DeafRemote {
        async fn exec(&self, _: &SshTarget, _: &str, _: Option<Duration>) -> Result<crate::ssh::SshOutput> {
            std::future::pending().await
        }
        async fn copy(&self, _: &SshTarget, _: &str, _: &str, _: Option<Duration>) -> Result<crate::ssh::SshOutput> {
            std::future::pending().await
        }
        async fn copy_recursive(&self, _: &SshTarget, _: &str, _: &str, _: Option<Duration>) -> Result<crate::ssh::SshOutput> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pod_budget_caps_a_remote_that_ignores_timeouts() {
        let steps = image_steps();
        let start = tokio::time::Instant::now();
        let out = provision(&DeafRemote, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::TimedOut { step: "copy deploy key", after: secs(516) });
        assert_eq!(start.elapsed(), secs(516));
    }

    /// `steps_cfg` with the VS Code warm-up on, as config enables it by default.
    fn vscode_cfg() -> SetupConfig {
        let mut c = steps_cfg();
        c.vscode = crate::vscode::VscodeSetup::from_config(&Config::parse(""), &c.repo_path).unwrap();
        assert!(c.vscode.is_some(), "on by default");
        c
    }

    #[test]
    fn the_vscode_warm_up_is_the_last_step_everywhere_unless_off() {
        let t = SetupTimeouts::default();
        assert_eq!(t.vscode, secs(300));
        for (provider, before) in [("runpod", 2), ("vast", 2), ("hetzner", 3)] {
            let steps = provisioning_steps(provider, &vscode_cfg(), "arena8-apple", false, "/tmp/h.sh", &t);
            assert_eq!(steps.len(), before + 1, "{provider}");
            let ProvisionStep::Optional { label, cmd, summary, timeout } = steps.last().unwrap() else {
                panic!("{provider}: {steps:?}")
            };
            assert_eq!((*label, *timeout), ("vscode warm-up", secs(300)));
            assert!(cmd.contains("arena-vscode-warmup") && cmd.starts_with("ARENA_VSCODE_BUDGET=270 "), "{cmd}");
            // Hetzner's interpreter is the venv its script builds under the repo.
            assert!(cmd.contains("'/root/ARENA_3.0/.venv/bin/python'"), "{cmd}");
            assert!(summary.contains("ms-python.vscode-pylance"), "{summary}");
            // The required steps are exactly what they were without it.
            assert_eq!(steps[..before], provisioning_steps(provider, &steps_cfg(), "arena8-apple", false, "/tmp/h.sh", &t)[..]);
        }
        // --timeout / SETUP_TIMEOUT_SECS size the main step, not the warm-up.
        let t = SetupTimeouts::resolve(Some("900"), Some(45)).unwrap();
        assert_eq!((t.config, t.vscode), (secs(45), secs(300)));
        // Its budget counts toward the pod's ceiling: 60 + 300 + 300 + 150 + 6.
        let steps = provisioning_steps("runpod", &vscode_cfg(), "arena8-apple", false, "", &SetupTimeouts::default());
        assert_eq!(pod_budget(&steps, BootRetry::default()), secs(816));
        // From config: on unless VSCODE_PREINSTALL=0; a bad extension list fails setup's config.
        let cfg = |extra: &str| {
            Config::parse(&format!("ARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_LOCAL=/k\n{extra}"))
        };
        assert!(SetupConfig::from_config(&cfg("")).unwrap().vscode.is_some());
        assert!(SetupConfig::from_config(&cfg("VSCODE_PREINSTALL=0")).unwrap().vscode.is_none());
        let e = SetupConfig::from_config(&cfg("VSCODE_EXTENSIONS=ms-python.python;true")).unwrap_err();
        assert!(e.to_string().contains("VSCODE_EXTENSIONS"), "{e}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_or_hung_warm_up_is_a_warning_on_a_set_up_pod() {
        let steps = provisioning_steps("runpod", &vscode_cfg(), "arena8-apple", false, "", &SetupTimeouts::default());
        let warned = |w: &str| ProvisionOutcome::Done { warnings: vec![w.to_string()] };

        // The script's own report: exit 3, its reason on the last stderr line (after ssh chatter).
        let fake = FakeRemote::new();
        fake.script(
            "10.0.0.1:22",
            [
                FakeReply::ok(),
                FakeReply::ok(),
                FakeReply::exit(3, "Warning: Permanently added '[10.0.0.1]:22'\nvscode warm-up incomplete: server: checksum mismatch\n"),
            ],
        );
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out, warned("vscode warm-up: exit 3: vscode warm-up incomplete: server: checksum mismatch"));
        assert!(out.is_done());
        assert_eq!(out.warnings().len(), 1);
        assert_eq!(
            progress_line(1, 1, "arena8-apple", &out),
            "[1/1] ✓ arena8-apple (warning: vscode warm-up: exit 3: vscode warm-up incomplete: server: checksum mismatch)"
        );
        assert_eq!(
            out.describe(),
            "done (warning: vscode warm-up: exit 3: vscode warm-up incomplete: server: checksum mismatch)"
        );
        assert!(matches!(&fake.calls()[2], RemoteCall::Exec { timeout, .. } if *timeout == Some(secs(300))));

        // Hung: its own budget runs out, the pod is still set up.
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::ok(), FakeReply::hang()]);
        let start = tokio::time::Instant::now();
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out, warned("vscode warm-up: timed out after 300s"));
        assert_eq!(start.elapsed(), secs(300));

        // The connection dropping under it: a warning too — never a retry of the whole pod.
        let fake = FakeRemote::new();
        fake.script(
            "10.0.0.1:22",
            [FakeReply::ok(), FakeReply::ok(), FakeReply::error("ssh: connect to host 10.0.0.1 port 22: Connection refused")],
        );
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert!(out.is_done() && out.warnings()[0].contains("Connection refused"), "{out:?}");
        assert_eq!(fake.calls().len(), 3);

        // A failure before it is the pod's failure, as ever — the warm-up never runs.
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::exit(128, "fatal: couldn't find remote ref")]);
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert!(matches!(&out, ProvisionOutcome::Failed { step: "repo + keys config", .. }), "{out:?}");
        assert!(out.warnings().is_empty());
        assert_eq!(fake.calls().len(), 2);

        // All well: no warnings.
        let fake = FakeRemote::new();
        assert_eq!(provision(&fake, &target(22), &steps, BootRetry::default()).await, ProvisionOutcome::done());
        assert_eq!(fake.calls().len(), 3);
    }

    /// Answers the first `ok` calls, then never returns — ignoring its timeout.
    struct DeafAfter {
        ok: usize,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Remote for DeafAfter {
        async fn exec(&self, _: &SshTarget, _: &str, _: Option<Duration>) -> Result<crate::ssh::SshOutput> {
            if self.calls.fetch_add(1, Ordering::SeqCst) < self.ok {
                return Ok(crate::ssh::SshOutput { stdout: String::new(), stderr: String::new(), code: Some(0), success: true });
            }
            std::future::pending().await
        }
        async fn copy(&self, t: &SshTarget, _: &str, _: &str, d: Option<Duration>) -> Result<crate::ssh::SshOutput> {
            self.exec(t, "", d).await
        }
        async fn copy_recursive(&self, t: &SshTarget, _: &str, _: &str, d: Option<Duration>) -> Result<crate::ssh::SshOutput> {
            self.exec(t, "", d).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_pod_budget_never_turns_a_stuck_warm_up_into_a_failure() {
        let steps = provisioning_steps("runpod", &vscode_cfg(), "arena8-apple", false, "", &SetupTimeouts::default());
        let stuck_in_warm_up = DeafAfter { ok: 2, calls: AtomicUsize::new(0) };
        let out = provision(&stuck_in_warm_up, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::Done { warnings: vec!["vscode warm-up: timed out after 816s".into()] });
        // Stuck before it: still the failure it always was.
        let stuck_in_config = DeafAfter { ok: 1, calls: AtomicUsize::new(0) };
        let out = provision(&stuck_in_config, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::TimedOut { step: "repo + keys config", after: secs(816) });
    }

    #[tokio::test]
    async fn no_steps_is_a_failure_not_a_success() {
        let out = provision(&FakeRemote::new(), &target(22), &[], BootRetry::default()).await;
        assert!(!out.is_done());
    }

    /// The rendered repo-update block, run against REAL git repos: a bare "origin" that
    /// gains commits, a new branch and a tag after the pod cloned it. Proves the narrow
    /// fetch works on an existing full clone on every branch path, and that it really is
    /// narrow (the new branch and tag are not fetched). Local only — no network.
    #[cfg(unix)]
    mod real_git {
        use super::steps_cfg;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Output};

        struct Fixture {
            root: PathBuf,
            seed: PathBuf,
            origin: PathBuf,
            pod: PathBuf,
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        fn isolated(cmd: &mut Command, home: &Path) {
            // Never read the operator's/system git config; deterministic identity.
            cmd.env("HOME", home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE");
        }

        impl Fixture {
            /// `None` when git isn't installed (the test then has nothing to check).
            fn new(tag: &str) -> Option<Self> {
                if Command::new("git").arg("--version").output().is_err() {
                    eprintln!("git not installed — skipping real-git check");
                    return None;
                }
                let root = std::env::temp_dir().join(format!("arena-setup-git-{}-{tag}", std::process::id()));
                let _ = std::fs::remove_dir_all(&root);
                std::fs::create_dir_all(&root).unwrap();
                let f = Fixture {
                    seed: root.join("seed"),
                    origin: root.join("origin.git"),
                    pod: root.join("pod"),
                    root,
                };
                f.git(&f.root, &["init", "-q", "-b", "main", "seed"]);
                f.commit(&f.seed, "a");
                f.git(&f.seed, &["branch", "feature"]);
                f.git(&f.seed, &["branch", "autocommit-arena8-w1d1-apple"]);
                f.git(&f.seed, &["tag", "v1"]);
                f.git(&f.root, &["clone", "-q", "--bare", "seed", "origin.git"]);
                Some(f)
            }

            fn git(&self, dir: &Path, args: &[&str]) -> String {
                let mut c = Command::new("git");
                c.arg("-C").arg(dir).args(args);
                isolated(&mut c, &self.root);
                let out = c.output().unwrap();
                assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }

            fn commit(&self, dir: &Path, name: &str) {
                std::fs::write(dir.join(name), name).unwrap();
                self.git(dir, &["add", name]);
                self.git(dir, &["commit", "-q", "-m", name]);
            }

            fn clone_pod(&self, extra: &[&str]) {
                let mut args = vec!["clone", "-q"];
                args.extend_from_slice(extra);
                args.extend_from_slice(&["origin.git", "pod"]);
                self.git(&self.root, &args);
            }

            /// After the pod cloned: new commits on main + feature, a brand-new branch and
            /// a tag — the stuff a narrow fetch must (main/feature) or must not pull.
            fn advance_origin(&self) -> (String, String) {
                self.git(&self.seed, &["checkout", "-q", "main"]);
                self.commit(&self.seed, "m2");
                self.git(&self.seed, &["checkout", "-q", "feature"]);
                self.commit(&self.seed, "f2");
                self.git(&self.seed, &["checkout", "-q", "-b", "autocommit-arena8-w2d1-bloom"]);
                self.commit(&self.seed, "n");
                self.git(&self.seed, &["tag", "v2"]);
                let origin = self.origin.to_string_lossy().into_owned();
                self.git(&self.seed, &["push", "-q", &origin, "main", "feature", "autocommit-arena8-w2d1-bloom", "v2"]);
                (self.git(&self.seed, &["rev-parse", "main"]), self.git(&self.seed, &["rev-parse", "feature"]))
            }

            fn run_update(&self, force: bool) -> Output {
                let mut scfg = steps_cfg();
                scfg.repo_path = self.pod.to_string_lossy().into_owned();
                scfg.repo_url = self.origin.to_string_lossy().into_owned();
                let script = format!("set -e; {}", scfg.repo_update_command(force));
                let mut c = Command::new("sh");
                c.arg("-c").arg(script);
                isolated(&mut c, &self.root);
                let out = c.output().unwrap();
                assert!(
                    out.status.success(),
                    "repo update (force={force}) failed:\n{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                out
            }

            fn head(&self) -> (String, String) {
                (
                    self.git(&self.pod, &["rev-parse", "--abbrev-ref", "HEAD"]),
                    self.git(&self.pod, &["rev-parse", "HEAD"]),
                )
            }

            fn rev(&self, r: &str) -> String {
                self.git(&self.pod, &["rev-parse", r])
            }

            /// The new branch and tag never arrive: the fetch was narrow.
            fn assert_narrow(&self) {
                let refs = self.git(&self.pod, &["for-each-ref", "--format=%(refname)"]);
                assert!(!refs.contains("autocommit-arena8-w2d1-bloom"), "fetched an unrelated branch:\n{refs}");
                assert!(!refs.contains("refs/tags/v2"), "fetched tags:\n{refs}");
            }
        }

        #[test]
        fn on_default_branch_resets_to_fetched_origin() {
            let Some(f) = Fixture::new("default") else { return };
            f.clone_pod(&[]);
            let (main, _) = f.advance_origin();
            f.run_update(false);
            assert_eq!(f.head(), ("main".into(), main.clone()));
            assert_eq!(f.rev("origin/main"), main);
            f.assert_narrow();
        }

        #[test]
        fn tracked_non_default_branch_fast_forwards_its_upstream_only() {
            let Some(f) = Fixture::new("tracked") else { return };
            f.clone_pod(&[]);
            f.git(&f.pod, &["checkout", "-q", "-b", "feature", "--track", "origin/feature"]);
            let (main, feature) = f.advance_origin();
            f.run_update(false);
            assert_eq!(f.head(), ("feature".into(), feature.clone()), "pulled its own upstream");
            assert_eq!(f.rev("origin/feature"), feature);
            assert_eq!(f.rev("origin/main"), main, "default branch fetched too");
            f.assert_narrow();
        }

        #[test]
        fn untracked_or_detached_checkout_is_left_alone() {
            let Some(f) = Fixture::new("untracked") else { return };
            f.clone_pod(&[]);
            f.git(&f.pod, &["checkout", "-q", "-b", "autocommit-arena8-w1d2-apple"]);
            f.commit(&f.pod, "participant-work");
            let before = f.head();
            let (main, _) = f.advance_origin();
            f.run_update(false);
            assert_eq!(f.head(), before, "no upstream => untouched, setup still succeeds");
            assert_eq!(f.rev("origin/main"), main);
            f.assert_narrow();

            f.git(&f.pod, &["checkout", "-q", "--detach"]);
            let detached = f.rev("HEAD");
            f.run_update(false);
            assert_eq!(f.rev("HEAD"), detached);
        }

        #[test]
        fn force_checks_out_and_resets_the_default_branch() {
            let Some(f) = Fixture::new("force") else { return };
            f.clone_pod(&[]);
            f.git(&f.pod, &["checkout", "-q", "-b", "feature", "--track", "origin/feature"]);
            let (main, _) = f.advance_origin();
            f.run_update(true);
            assert_eq!(f.head(), ("main".into(), main));
            f.assert_narrow();
        }

        #[test]
        fn force_creates_the_default_branch_when_missing_locally() {
            // A clone that never had a local `main` (cloned on another branch): checkout
            // DWIMs it from the freshly fetched origin/main.
            let Some(f) = Fixture::new("force-nolocal") else { return };
            f.clone_pod(&["-b", "feature"]);
            let (main, _) = f.advance_origin();
            f.run_update(true);
            assert_eq!(f.head(), ("main".into(), main));
            f.assert_narrow();
        }

        /// On a pod with a volume: setup's relocation puts the clone on the (fake) volume and
        /// the repo update that follows acts on THAT copy, through the symlink — the configured
        /// path stays a link, the volume copy gets the new commits.
        #[test]
        fn the_update_acts_on_the_volume_copy_through_the_link() {
            use std::os::unix::fs::PermissionsExt;
            let Some(f) = Fixture::new("volume") else { return };
            f.clone_pod(&[]);
            let (ws, bin) = (f.root.join("workspace"), f.root.join("bin"));
            std::fs::create_dir_all(&ws).unwrap();
            std::fs::create_dir_all(&bin).unwrap();
            let stub = bin.join("mountpoint");
            std::fs::write(&stub, format!("#!/bin/sh\n[ \"$2\" = '{}' ]\n", ws.display())).unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
            let reloc = crate::volume::relocation_script(
                &f.pod.display().to_string(),
                &ws.display().to_string(),
                &f.root.join("lock").display().to_string(),
            )
            .unwrap();
            let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
            let mut c = Command::new("sh");
            c.arg("-c").arg(reloc).env("PATH", path);
            isolated(&mut c, &f.root);
            assert!(c.output().unwrap().status.success());
            let on_volume = ws.join("pod");
            assert_eq!(std::fs::read_link(&f.pod).unwrap(), on_volume);
            let (main, _) = f.advance_origin();
            f.run_update(false);
            assert!(std::fs::symlink_metadata(&f.pod).unwrap().file_type().is_symlink(), "still a link");
            assert_eq!(f.git(&on_volume, &["rev-parse", "HEAD"]), main, "the volume copy was updated");
            f.assert_narrow();
        }

        #[test]
        fn missing_checkout_is_skipped_not_fatal() {
            let Some(f) = Fixture::new("missing") else { return };
            // No pod clone at all (a non-arena image).
            let out = f.run_update(false);
            assert!(String::from_utf8_lossy(&out.stderr).contains("skipping repo update"));
        }
    }
}
