//! Pod provisioning: make a freshly-created pod ready to commit/back up.
//!
//! Faithful to the legacy `setup_em.sh`. After the caller copies the git deploy key
//! (scp), the on-pod script:
//!   1. `chmod 600` the key,
//!   2. add a `github.com` block to `~/.ssh/config` pointing at it (idempotent),
//!   3. add the deploy key's *public* half to `~/.ssh/authorized_keys` (idempotent),
//!      so anyone holding that key can also SSH into the pod — derived on the pod via
//!      `ssh-keygen -y` from the key we just copied, so no extra file is shipped,
//!   4. point the ARENA repo's `origin` at the GitHub SSH URL, fetch *only the default
//!      branch* (no tags), and update (default: stay on the current branch, pull /
//!      fast-forward a clean main — one with its own changes or commits is left as it is,
//!      with a warning; `--force`: check out the default branch and `reset --hard`),
//!      then update submodules,
//!   5. give the checkout the backup's git identity where none is configured
//!      ([`GIT_IDENTITY_SH`]; the bare-VM script does the same),
//!   6. write `~/.name` as `export MACHINE_NAME='<short>'`.
//!
//! Before that script, on image-based pods, a best-effort step of its own (its own budget —
//! a first move copies the whole repo — and never a failed setup): when `/workspace` is a real
//! mount (a persistent volume), put the repo ON it — `/workspace/<repo dir>`, with the
//! configured path a symlink to it; after a reset the volume copy wins over the image's
//! untouched checkout ([`crate::volume::relocation_command`]) — so a restart keeps the
//! participants' work and the repo update acts on the volume copy.
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
    /// The public course repo, added to the checkout as its `upstream` remote when it has none
    /// ([`upstream_url`]; `None`: no such remote is added).
    pub upstream_url: Option<String>,
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
    /// Move the repo onto the pod's volume (image-based pods; on by default, `REPO_ON_VOLUME=0`
    /// turns it off).
    pub relocate: bool,
    /// GB of swap the bare-VM script makes (`VM_SWAP_GB`, [`swap_gb`]); 0 = none.
    pub swap_gb: u32,
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
        let relocate = crate::volume::relocation_enabled(cfg.get("REPO_ON_VOLUME"))?;
        let swap_gb = swap_gb(cfg)?;
        Ok(Self {
            // Resolve to a readable copy (prefer ~/.ssh/<name> if the configured path
            // isn't readable), same as the shared key — so `setup` works when run as a
            // user that can't read the configured /root path.
            key_local: crate::ssh::resolve_key_path(key_local),
            key_remote: cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519").to_string(),
            repo_path,
            repo_url: format!("git@github.com:{owner}/{name}.git"),
            upstream_url: upstream_url(cfg)?,
            branch: cfg.get("DEFAULT_BRANCH").unwrap_or("main").to_string(),
            prefix: cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string(),
            authorized_pubkeys: crate::ssh::authorized_pubkeys(cfg),
            broadcast_exports: crate::apikeys::broadcast_env_vars(|k| cfg.get(k).map(String::from)),
            zsh_install: false,
            vscode,
            relocate,
            swap_gb,
        })
    }

    /// The short machine name (the part after `{prefix}-`), e.g. arena8-apple -> apple.
    pub fn short_name<'a>(&self, machine_name: &'a str) -> &'a str {
        short_name(&self.prefix, machine_name)
    }

    /// Ensure every machine's keys ([`crate::ssh::authorized_pubkeys`]: the shared key, the deploy
    /// key and the admin key) are in `~/.ssh/authorized_keys`, so arena, participants and staff
    /// (from the proxy box, with the admin key) get in with whichever they hold. Each is appended
    /// only if not already present. Then the on-pod deploy key's public half, re-derived there
    /// (covers a local deploy-key .pub that differs from what's on the pod) — run after the key
    /// was copied. Only public halves: nothing here reads a private key but the pod's own deploy
    /// key. (The provider's account key is injected by the provider.) Part of
    /// [`Self::remote_command`]; a bare VM runs it as a step of its own ([`provisioning_steps`]).
    pub fn authorized_keys_command(&self) -> String {
        let q = shell_quote;
        let mut ak = String::from(
            "mkdir -p \"$HOME/.ssh\" && chmod 700 \"$HOME/.ssh\" && \
             touch \"$HOME/.ssh/authorized_keys\" && chmod 600 \"$HOME/.ssh/authorized_keys\"",
        );
        for pk in &self.authorized_pubkeys {
            let qpk = q(pk);
            ak.push_str(&format!(
                " && (grep -qxF {qpk} \"$HOME/.ssh/authorized_keys\" || echo {qpk} >> \"$HOME/.ssh/authorized_keys\")"
            ));
        }
        // The key as copied may keep an older file's mode (scp leaves an existing file's): 0600
        // first, which ssh-keygen wants of a private key (the config step's own chmod is before).
        let key = &self.key_remote;
        ak.push_str(&format!(
            " && chmod 600 {qkey} && PUB=$(ssh-keygen -y -f {key} 2>/dev/null) && \
             (grep -qxF \"$PUB\" \"$HOME/.ssh/authorized_keys\" || echo \"$PUB\" >> \"$HOME/.ssh/authorized_keys\")",
            qkey = q(key)
        ));
        ak
    }

    /// The on-pod provisioning command, run *after* the key has been scp'd. `force`
    /// mirrors `setup_em.sh --force` (hard-reset onto the default branch).
    pub fn remote_command(&self, machine_name: &str, force: bool) -> String {
        let q = shell_quote;
        let key = &self.key_remote;

        let ssh_config = self.github_ssh_config();

        let mut steps = vec!["set -e".to_string(), format!("chmod 600 {}", q(key)), ssh_config, self.authorized_keys_command()];
        // The relocation onto the volume is its own step, before this one; one whose ssh client
        // timed out can still be copying on the pod — wait for it (bounded) before the update.
        steps.push(crate::volume::relocation_wait_command());
        steps.push(self.repo_update_command(force));
        steps.push(git_identity_command(&self.repo_path));
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

    /// Idempotent github.com block in ~/.ssh/config pointing at the deploy key.
    fn github_ssh_config(&self) -> String {
        let key = &self.key_remote;
        format!(
            "mkdir -p \"$HOME/.ssh\" && chmod 700 \"$HOME/.ssh\" && touch \"$HOME/.ssh/config\" && \
             sed -i '/^# BEGIN arena-infra github.com/,/^# END arena-infra github.com/d' \"$HOME/.ssh/config\" && \
             printf '%s\\n' '# BEGIN arena-infra github.com' 'Host github.com' '    AddKeysToAgent yes' \
             '    IdentityFile {key}' '# END arena-infra github.com' >> \"$HOME/.ssh/config\" && \
             chmod 600 \"$HOME/.ssh/config\""
        )
    }

    /// A bare VM's repo update, before its setup script: on a VM that already has the checkout —
    /// one made from an arena image (`arena image build`), or a re-run — the same update an
    /// image-based pod gets ([`Self::repo_update_command`]: a clean default branch fast-forwards,
    /// anything of the participant's is left alone), so the script's env check reads the course's
    /// current requirements. Wires the deploy key to github.com first (an image carries no key)
    /// and knows github.com's host key. A fresh VM has no checkout yet: nothing to do (the
    /// script clones it).
    pub fn bare_vm_repo_update_command(&self, force: bool) -> String {
        let q = shell_quote;
        format!(
            "if [ -d {repo}/.git ]; then set -e; chmod 600 {key}; {ssh_config}; \
             (grep -qs '^github.com ' \"$HOME/.ssh/known_hosts\" || ssh-keyscan github.com >> \"$HOME/.ssh/known_hosts\" 2>/dev/null) || true; \
             {update}; else echo 'no checkout yet: the setup script clones it'; fi",
            repo = q(&self.repo_path),
            key = q(&self.key_remote),
            ssh_config = self.github_ssh_config(),
            update = self.repo_update_command(force),
        )
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
            // Stay on the current branch. On the default branch, move up to origin — but only
            // a checkout with nothing of its own: no tracked change (staged or not) and no
            // commit origin lacks, and then only as a fast-forward (which also refuses to
            // overwrite an untracked file). Anything else is the participant's work — setup
            // re-runs by itself after a reset (`restart`/`reimage`/`start`, on the repo the
            // volume kept), and a `reset --hard` there reverted their edits and dropped their
            // commits (review finding) — so it is left as it is, with a warning on the pod's
            // line. Otherwise (e.g. an autocommit-wNdM branch with no upstream) only fast-
            // forward if it actually tracks a remote — never fail the whole setup. The pull
            // names the upstream (remote + ref) explicitly: a bare `git pull` runs a bare
            // `git fetch`, i.e. exactly the all-branches-and-tags fetch avoided above.
            format!(
                "CUR=$(git rev-parse --abbrev-ref HEAD); \
                 if [ \"$CUR\" = {b} ]; then \
                 if git diff --quiet HEAD -- && git merge-base --is-ancestor HEAD origin/{b}; then \
                 git merge --ff-only --quiet origin/{b} || echo {ff_failed}; \
                 else echo {own_work}; fi; \
                 else (R=$(git config \"branch.$CUR.remote\") && M=$(git config \"branch.$CUR.merge\") && \
                 git pull --ff-only --no-tags \"$R\" \"$M\") || true; fi",
                b = q(branch),
                ff_failed = q(&format!(
                    "{}the repo couldn't fast-forward to origin/{branch} (an untracked file in the way?) - left as it was",
                    crate::volume::WARNING_PREFIX
                )),
                own_work = q(&format!(
                    "{}the repo on {branch} has its own changes or commits - left as it is, not reset to origin/{branch} \
                     (`pods setup --force` resets it)",
                    crate::volume::WARNING_PREFIX
                )),
            )
        };

        // The repo update only applies when the ARENA checkout is actually on the image
        // (the prebuilt arena image bakes it in at `repo_path`). On a non-arena base image
        // (e.g. an NVIDIA NGC image brought up with `--bootstrap`) it's absent — skip the
        // git steps with a notice instead of `cd`-ing into a missing dir and aborting the
        // whole setup under `set -e`. Everything else (SSH keys, ~/.name, agents, zsh,
        // tokens) still runs. The then-branch runs under the outer `set -e`, so a real git
        // failure on an arena pod still fails loudly.
        //
        // `upstream`: the public course repo, so a participant can `git pull upstream main` the
        // course's own fixes — `origin` is the cohort's private copy, where backups push. Added
        // only when the checkout has no `upstream` (one a participant set is theirs), never
        // fetched here, and never fatal: it's a convenience, not something setup needs.
        // With two remotes sharing branch names, a participant's plain `git checkout <branch>`
        // takes origin's (`checkout.defaultRemote`), unless they set their own. A failed add is a
        // warning on the pod's line.
        let upstream = self.upstream_url.as_ref().map_or_else(String::new, |u| {
            format!(
                "git remote get-url upstream >/dev/null 2>&1 || git remote add upstream {u} || echo {warn}; \
                 git config --get checkout.defaultRemote >/dev/null 2>&1 || git config checkout.defaultRemote origin || true; ",
                u = q(u),
                warn = q(&format!("{}couldn't add the upstream remote {u}", crate::volume::WARNING_PREFIX)),
            )
        });
        format!(
            "if [ -d {repo}/.git ]; then cd {repo}; git remote set-url origin {url}; {upstream}\
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

/// The git identity setup gives a checkout that has none: the backup's own — `backup.sh` sets the
/// same, the same way, before its first commit on a day branch.
pub const GIT_IDENTITY_NAME: &str = "Arena Autocommit";
/// See [`GIT_IDENTITY_NAME`].
pub const GIT_IDENTITY_EMAIL: &str = "autocommit@arena.education";

/// `arena_git_identity <repo>`, a shell function (sh, bash and zsh alike) — every provider's setup
/// runs it on the checkout: the image-based pods' config step ([`git_identity_command`]) and the
/// bare-VM script (`hetzner_setup.sh` carries this same text; a test holds them together). A fresh
/// machine had no identity until the first backup on a day branch set one, so a participant's
/// first `git commit` or `git merge` stopped at "Please tell me who you are" (live 7/9).
///
/// For each of `user.name` / `user.email`: set repo-local to [`GIT_IDENTITY_NAME`] /
/// [`GIT_IDENTITY_EMAIL`] only when no value is configured anywhere git looks (repo, global,
/// system — as the backup checks), so a participant's own is never overridden. A repo-local value
/// beats a global one, so one of ours (exactly our value) is unset again once a global or system
/// one exists — the participant set theirs since — and theirs takes over. No flip-flop with the
/// backup: it sets ours only when git finds no value at all, and that one is then there. A
/// failure is a warning (`arena-warning:`), never a failed setup.
pub const GIT_IDENTITY_SH: &str = r#"arena_git_identity() {
    [ -e "$1/.git" ] || return 0
    for arena_k in user.name user.email; do
        if [ "$arena_k" = user.name ]; then arena_v='Arena Autocommit'; else arena_v='autocommit@arena.education'; fi
        if ! git -C "$1" config "$arena_k" >/dev/null 2>&1; then
            git -C "$1" config "$arena_k" "$arena_v" || echo "arena-warning: couldn't give $1 a git $arena_k"
        elif [ "$(git -C "$1" config --local "$arena_k" 2>/dev/null)" = "$arena_v" ] &&
            [ -n "$(git -C "$1" config --global --includes "$arena_k" 2>/dev/null || git -C "$1" config --system --includes "$arena_k" 2>/dev/null)" ]; then
            git -C "$1" config --local --unset "$arena_k" || echo "arena-warning: couldn't hand $1's git $arena_k back to the participant's own"
        fi
    done
}"#;

/// The image-based config step's part that runs [`GIT_IDENTITY_SH`] on the checkout at `repo`
/// (none there — a non-arena image — and it does nothing).
pub fn git_identity_command(repo: &str) -> String {
    format!("{GIT_IDENTITY_SH}\narena_git_identity {}", shell_quote(repo))
}

/// The public course repo the pods' checkout gets as its `upstream` remote, unless
/// `ARENA_UPSTREAM_REPO` names another (`owner/name`) or `none`.
pub const DEFAULT_UPSTREAM_REPO: &str = "ARENA-education/ARENA_materials";

/// The swap a bare VM gets when set up when `VM_SWAP_GB` isn't set: a cx23 has 4 GB of RAM and a
/// cx33 8 GB, and a notebook that loads a model or two passes that — without swap the OOM killer
/// ends the participant's kernel; with it the box only slows down.
pub const DEFAULT_SWAP_GB: u32 = 8;

/// The most `VM_SWAP_GB` may ask for: more is a typo (a disk's worth), not a plan.
pub const MAX_SWAP_GB: u32 = 64;

/// `VM_SWAP_GB`: GB of swap the bare-VM setup script makes (Hetzner and other plain VMs — a
/// container pod can't swap); [`DEFAULT_SWAP_GB`] when unset or empty, 0 = none. Checked here,
/// before anything runs on a pod: it goes into the script's command line. Pure.
pub fn swap_gb(cfg: &Config) -> Result<u32> {
    let Some(raw) = cfg.get("VM_SWAP_GB").map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_SWAP_GB);
    };
    match raw.parse::<u32>() {
        Ok(gb) if gb <= MAX_SWAP_GB => Ok(gb),
        _ => Err(Error::Config(format!("VM_SWAP_GB must be a whole number of GB from 0 (no swap) to {MAX_SWAP_GB}, not {raw:?}"))),
    }
}

/// The URL of the `upstream` remote setup adds to each pod's checkout: the public course repo
/// (`ARENA_UPSTREAM_REPO`, default [`DEFAULT_UPSTREAM_REPO`]) over https — public, so no key —
/// while `origin` is the cohort's private copy that backups push to. `None` for `none` (or an
/// empty value), and when it is the cohort repo itself (`ARENA_REPO_OWNER/NAME`): a second name
/// for origin helps no one. Its GitHub URL (https or ssh) is taken as the same `owner/name`; an
/// error for anything else (it goes into a shell command and a URL). Pure.
pub fn upstream_url(cfg: &Config) -> Result<Option<String>> {
    let raw = cfg.get("ARENA_UPSTREAM_REPO").map(str::trim).unwrap_or(DEFAULT_UPSTREAM_REPO);
    if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    // The GitHub URL an operator pastes is the same repo (review fb2 R6).
    let bare = ["https://github.com/", "http://github.com/", "git@github.com:", "ssh://git@github.com/"]
        .iter()
        .find_map(|p| raw.strip_prefix(p))
        .unwrap_or(raw)
        .trim_end_matches('/');
    let plain = |p: &str| !p.is_empty() && p != "." && p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    let parts = bare.split_once('/').map(|(o, n)| (o, n.strip_suffix(".git").unwrap_or(n)));
    let Some((owner, name)) = parts.filter(|(o, n)| plain(o) && plain(n)) else {
        return Err(Error::Config(format!(
            "ARENA_UPSTREAM_REPO must be owner/name (e.g. {DEFAULT_UPSTREAM_REPO}) or its GitHub URL, or none, not {raw:?}"
        )));
    };
    let same = |key: &str, v: &str| cfg.get(key).is_some_and(|c| c.trim().eq_ignore_ascii_case(v));
    if same("ARENA_REPO_OWNER", owner) && same("ARENA_REPO_NAME", name) {
        return Ok(None);
    }
    Ok(Some(format!("https://github.com/{owner}/{name}.git")))
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
    /// The best-effort move of the repo onto the volume (image-based pods) — its own budget
    /// too: a first move copies the whole repo, which mustn't eat the config step's.
    pub relocate: Duration,
}

impl Default for SetupTimeouts {
    fn default() -> Self {
        Self {
            copy: Duration::from_secs(60),
            config: Duration::from_secs(300),
            bare_vm: Duration::from_secs(1800),
            vscode: crate::vscode::WARMUP_TIMEOUT,
            relocate: crate::volume::RELOCATE_TIMEOUT,
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
    /// A best-effort extra (the repo onto the volume, the VS Code warm-up): run like `Run`, but
    /// when it fails or runs out of time the pod is still set up — the outcome carries a
    /// warning instead (as it does for the `arena-warning:` lines it prints). The required
    /// steps after one still run. `summary` is what the dry-run prints in place of the command
    /// (a base64 blob, or a long script).
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

/// The bare-VM provisioning script (Hetzner CPU VMs, Shadeform GPU VMs with `ARENA_GPU=1`):
/// system deps, docker + compose, a uv venv with the ARENA packages, swap, and a zsh rc that
/// activates the venv. Embedded here, in core, so `arena pods setup` and the dashboard's setup
/// push the same script ([`provisioning_steps`] scps a staged copy to `/root/hetzner_setup.sh`);
/// image-based pods take the lighter post-image config path instead.
pub const HETZNER_SETUP: &str = include_str!("hetzner_setup.sh");

/// Which bare-VM setup a provider's pods get: they're plain VMs (not containers from the
/// cohort image), so the full setup script builds the ARENA env on them ([`provisioning_steps`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareVm {
    /// A CPU VM (Hetzner): the script swaps the CUDA requirements for CPU wheels.
    Cpu,
    /// A GPU VM (Shadeform): the script keeps the CUDA requirements (`ARENA_GPU=1`), lets uv
    /// match torch to the host's driver, and its env gate also asks for `torch.cuda`.
    Gpu,
}

/// The bare-VM setup `provider`'s pods get, if they're bare VMs at all (`None`: image-based
/// pods — RunPod, Vast). The one place a provider name decides this. Pure.
pub fn bare_vm(provider: &str) -> Option<BareVm> {
    match provider {
        "hetzner" => Some(BareVm::Cpu),
        "shadeform" => Some(BareVm::Gpu),
        _ => None,
    }
}

/// Whether `provider`'s pods are bare VMs ([`bare_vm`]).
pub fn is_bare_vm(provider: &str) -> bool {
    bare_vm(provider).is_some()
}

/// Whether `provider`'s pods are named by their hostname: a Hetzner VM's is its server name,
/// set by cloud-init at create, and its setup's login banner and `~/.name` fall back to it — so
/// a rename sets it too ([`hostname_command`]). Not a Shadeform VM's (the cloud it lands on
/// names it; setup is given the machine name instead), nor a RunPod or Vast container's (its
/// id). Pure.
pub fn hostname_is_machine_name(provider: &str) -> bool {
    provider == "hetzner"
}

/// Whether `provider`'s pods carry the machine name in their environment: a RunPod or Vast
/// container's `MACHINE_NAME` env var, set at create, which a rename can't change (only a
/// reimage, which wipes the disk, does). Bare VMs have none: their create sends no env. Pure.
pub fn machine_name_in_env(provider: &str) -> bool {
    !is_bare_vm(provider)
}

/// Whether `name` is a plain hostname (RFC 1123: letters, digits and hyphens, in dot-separated
/// parts of 1–63 that don't start or end with a hyphen; 253 at most) — what Hetzner takes as a
/// server name, and all [`hostname_command`] will write. Pure.
pub fn is_hostname(name: &str) -> bool {
    name.len() <= 253
        && name.split('.').all(|part| {
            (1..=63).contains(&part.len())
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !part.starts_with('-')
                && !part.ends_with('-')
        })
}

/// The command that makes `name` a VM's hostname, for VMs named by it
/// ([`hostname_is_machine_name`]). Converges on a state, whatever a run before it got to:
/// - the running and the static hostname (/etc/hostname, what the VM boots with) are `name` —
///   `hostnamectl set-hostname` unless both already are (else `hostname` + /etc/hostname);
/// - on the `127.0.1.1` lines of /etc/hosts (cloud-init's `127.0.1.1 <fqdn> <hostname>`), the
///   names the VM had (running and static) become `name`, every other alias and comment kept;
///   and if no such line names it then (a run before changed the hostname but not the file),
///   `name` goes first on the first one. Lines are found by their first field, so an indented
///   one counts (review r2). No `127.0.1.1` line, or no /etc/hosts: nothing written. Written in
///   place (same file) and only when it changes.
///
/// Exits non-zero unless both hostnames are `name` after it. A reboot keeps all of it with
/// nothing more (live, Hetzner ubuntu-24.04, cloud-init 26.1, 2026-10-30): Hetzner's metadata
/// and vendor-data keep the name the server was created with after a rename, but
/// `update_hostname` leaves a hostname changed since cloud-init set it alone ("assuming user
/// maintained hostname"), and `update_etc_hosts` (vendor-data's `manage_etc_hosts: true`) runs
/// once per instance. No cloud-init setting is written: vendor-data outranks
/// /etc/cloud/cloud.cfg.d anyway, and one naming the machine would misname a server made from a
/// snapshot of this one. `None`: `name` isn't a plain hostname ([`is_hostname`]). Pure.
pub fn hostname_command(name: &str) -> Option<String> {
    is_hostname(name).then(|| {
        format!(
            concat!(
                r#"n={n}; h=$(hostname); f=$(cat /etc/hostname 2>/dev/null); "#,
                r#"if [ "$h" != "$n" ] || [ "$f" != "$n" ]; then "#,
                r#"hostnamectl set-hostname "$n" || {{ hostname "$n" && echo "$n" > /etc/hostname; }} || exit 1; fi; "#,
                r#"if [ -f /etc/hosts ]; then t=$(mktemp) || exit 1; "#,
                r#"awk -v n="$n" -v h="$h" -v f="$f" '{{ l[NR] = $0 }} $1 == "127.0.1.1" {{ if (!first) first = NR; "#,
                r#"for (i = 2; i <= NF && $i !~ /^#/; i++) {{ if ($i == h || $i == f) $i = n; if ($i == n) has = 1 }} l[NR] = $0 }} "#,
                r#"END {{ if (first && !has) sub(/127\.0\.1\.1[ \t]+/, "127.0.1.1 " n " ", l[first]); for (i = 1; i <= NR; i++) print l[i] }}' "#,
                r#"/etc/hosts > "$t" && {{ cmp -s "$t" /etc/hosts || cat "$t" > /etc/hosts; }}; r=$?; rm -f "$t"; [ "$r" = 0 ] || exit 1; fi; "#,
                r#"[ "$(hostname)" = "$n" ] && [ "$(cat /etc/hostname 2>/dev/null)" = "$n" ]"#,
            ),
            n = shell_quote(name)
        )
    })
}

/// The ordered provisioning steps for a pod, chosen by provider — pure and unit-tested,
/// so the runner never has to know what a provider *is*. Adding a backend = add its
/// steps here; the runner and the dry-run preview stay generic.
///   - bare-VM ([`bare_vm`]): push the deploy key, authorize every machine's keys
///     ([`AUTHORIZE_LABEL`]), push the full setup script, then run it — on a CPU VM (hetzner) as
///     it always was, its hostname made the machine name first (best-effort, [`HOSTNAME_LABEL`]);
///     on a GPU VM (shadeform) in its GPU mode, with the machine name for `~/.name` (a Shadeform
///     VM's hostname isn't the machine name).
///   - image-based (runpod/vast): push the git deploy key, then the post-image config.
///
/// Image-based pods move the repo onto their volume first (unless `REPO_ON_VOLUME=0`, and when
/// the configured path can be relocated — see [`crate::volume::volume_repo_path`]; the script
/// itself checks the pod has a volume), best-effort on its own budget, before the config step
/// updates the repo. Both then get the VS Code warm-up (unless `scfg.vscode` is off) as a
/// best-effort last step on its own budget: on hetzner it runs after the script has built the
/// venv it points VS Code at.
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
    // The bare-VM script adds the `upstream` remote itself (as the image path's repo update does).
    let upstream = scfg.upstream_url.as_ref().map_or_else(String::new, |u| format!("UPSTREAM_URL={} ", shell_quote(u)));
    let mut steps = match bare_vm(provider) {
        Some(BareVm::Cpu) => vec![
            // Copy the git deploy key first, so the script can clone (and later push to)
            // the PRIVATE cohort repo over SSH — not just the public mirror.
            copy_key,
            ProvisionStep::Scp {
                label: "copy hetzner setup script",
                local: hetzner_script_local.to_string(),
                remote: "/root/hetzner_setup.sh".into(),
                timeout: timeouts.copy,
            },
            // A VM from an arena image (or a re-run) updates its checkout first, so the script
            // decides about the env on the course's current requirements.
            ProvisionStep::Run { label: "repo update", cmd: scfg.bare_vm_repo_update_command(force), timeout: timeouts.config },
            ProvisionStep::Run {
                label: "hetzner setup script",
                cmd: format!(
                    "ARENA_SWAP_GB={} {upstream}REPO_URL={} REPO_DIR={} REPO_KEY={} bash /root/hetzner_setup.sh",
                    scfg.swap_gb,
                    shell_quote(&scfg.repo_url),
                    shell_quote(&scfg.repo_path),
                    shell_quote(&scfg.key_remote),
                ),
                timeout: timeouts.bare_vm,
            },
        ],
        Some(BareVm::Gpu) => vec![
            copy_key,
            ProvisionStep::Scp {
                label: "copy bare-VM setup script",
                local: hetzner_script_local.to_string(),
                remote: "/root/hetzner_setup.sh".into(),
                timeout: timeouts.copy,
            },
            ProvisionStep::Run {
                label: "GPU VM setup script",
                cmd: format!(
                    "ARENA_GPU=1 ARENA_MACHINE_NAME={} ARENA_SWAP_GB={} {upstream}REPO_URL={} REPO_DIR={} REPO_KEY={} bash /root/hetzner_setup.sh",
                    shell_quote(name),
                    scfg.swap_gb,
                    shell_quote(&scfg.repo_url),
                    shell_quote(&scfg.repo_path),
                    shell_quote(&scfg.key_remote),
                ),
                timeout: timeouts.bare_vm,
            },
        ],
        None => {
            let mut v = vec![copy_key];
            let relocation = scfg.relocate.then(|| crate::volume::relocation_command(&scfg.repo_path, timeouts.relocate));
            if let Some(cmd) = relocation.flatten() {
                v.push(ProvisionStep::Optional {
                    label: RELOCATE_LABEL,
                    cmd,
                    summary: crate::volume::relocation_summary(&scfg.repo_path),
                    timeout: timeouts.relocate,
                });
            }
            v.push(ProvisionStep::Run {
                label: "repo + keys config",
                cmd: scfg.remote_command(name, force),
                timeout: timeouts.config,
            });
            v
        }
    };
    // A bare VM's script authorizes no key, and its create authorized only what the provider
    // was given (Hetzner: the project's cohort key, plus cloud-init's — which `--api-json
    // user_data` replaces): every machine's keys, right after the deploy key is there to derive
    // its public half from — before the long script, so a script that fails still leaves the
    // VM reachable with the deploy and admin keys. (Image-based pods do it in their config step.)
    if bare_vm(provider).is_some() {
        steps.insert(
            1,
            ProvisionStep::Run { label: AUTHORIZE_LABEL, cmd: scfg.authorized_keys_command(), timeout: timeouts.config },
        );
    }
    // A VM named by its hostname gets it re-asserted, best-effort, before the script reads it
    // for the login banner and `~/.name`: a no-op on a fresh VM (cloud-init named it), the
    // fix for one renamed while it couldn't be reached — what `pods rename` tells the operator
    // to run then.
    if let Some(cmd) = hostname_is_machine_name(provider).then(|| hostname_command(name)).flatten() {
        steps.insert(
            2,
            ProvisionStep::Optional {
                label: HOSTNAME_LABEL,
                cmd,
                summary: format!("set the hostname to {name} if it isn't (a Hetzner VM is named by it)"),
                timeout: timeouts.config,
            },
        );
    }
    if let Some(vscode) = &scfg.vscode {
        let low = low_memory(provider);
        steps.push(ProvisionStep::Optional {
            label: "vscode warm-up",
            cmd: vscode.remote_command(timeouts.vscode, low),
            summary: vscode.summary(low),
            timeout: timeouts.vscode,
        });
    }
    steps
}

/// Which of `provider`'s machines get VS Code's memory-saving settings
/// ([`crate::vscode::LowMemory`]): every CPU VM (Hetzner) — the course's kernels keep their models
/// in the RAM VS Code uses, and a cx33's 8 GB ran out — and any other machine only when it has
/// little RAM: a big GPU pod keeps Pylance's indexing. Pure.
pub fn low_memory(provider: &str) -> crate::vscode::LowMemory {
    match bare_vm(provider) {
        Some(BareVm::Cpu) => crate::vscode::LowMemory::Always,
        Some(BareVm::Gpu) | None => crate::vscode::LowMemory::BelowGib(crate::vscode::LOW_MEMORY_BELOW_GIB),
    }
}

/// The relocation step's label (what its warnings are prefixed with).
pub const RELOCATE_LABEL: &str = "repo onto volume";

/// A bare VM's step that authorizes every machine's keys ([`SetupConfig::authorized_keys_command`]).
pub const AUTHORIZE_LABEL: &str = "authorize keys";

/// The best-effort step that makes the machine name a Hetzner VM's hostname ([`hostname_command`]).
pub const HOSTNAME_LABEL: &str = "hostname";

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
    /// (`<label>: <why>`) — reported, never a failure. `installer`: what built the ARENA env,
    /// as the bare-VM setup script says it ([`INSTALLER_PREFIX`]; `None` from any other setup).
    /// `image`: the arena image the VM came from and how old it is, as the script says it the
    /// first time it sets up such a VM ([`IMAGE_PREFIX`]).
    Done { warnings: Vec<String>, installer: Option<String>, image: Option<String> },
    /// A step failed: non-zero exit (`code`), a failed copy, or the call itself erroring.
    Failed { step: &'static str, code: Option<i32>, detail: String },
    /// A step (or, defensively, the whole pod budget) ran out of time.
    TimedOut { step: &'static str, after: Duration },
}

impl ProvisionOutcome {
    /// Set up, with nothing to report.
    pub fn done() -> Self {
        ProvisionOutcome::Done { warnings: Vec::new(), installer: None, image: None }
    }

    /// Set up, with these warnings (and no installer said).
    pub fn warned(warnings: Vec<String>) -> Self {
        ProvisionOutcome::Done { warnings, installer: None, image: None }
    }

    pub fn is_done(&self) -> bool {
        matches!(self, ProvisionOutcome::Done { .. })
    }

    /// The best-effort steps that didn't work out on a pod that is set up (else none).
    pub fn warnings(&self) -> &[String] {
        match self {
            ProvisionOutcome::Done { warnings, .. } => warnings,
            _ => &[],
        }
    }

    /// What built the ARENA env on a pod that is set up (`install_uv.sh`, `none (env already
    /// present)`, …), when its setup said ([`INSTALLER_PREFIX`]).
    pub fn installer(&self) -> Option<&str> {
        match self {
            ProvisionOutcome::Done { installer, .. } => installer.as_deref(),
            _ => None,
        }
    }

    /// The arena image the VM came from, when its setup said ([`IMAGE_PREFIX`]).
    pub fn image(&self) -> Option<&str> {
        match self {
            ProvisionOutcome::Done { image, .. } => image.as_deref(),
            _ => None,
        }
    }

    /// What happened, without the pod's name: `done` (`done (warning: …)`), `timed out at
    /// <step> after Ns`, or `failed at <step>, exit N: <stderr>` — for a line that already
    /// names the pod (the fleet runner's, or `up`'s per-pod `[name] FAILED setup: …`).
    pub fn describe(&self) -> String {
        match self {
            ProvisionOutcome::Done { warnings, .. } if warnings.is_empty() => "done".to_string(),
            ProvisionOutcome::Done { warnings, .. } => format!("done (warning: {})", warnings.join("; ")),
            ProvisionOutcome::TimedOut { step, after } => format!("timed out at {step} after {}", human_duration(after)),
            ProvisionOutcome::Failed { step, code, detail } => {
                let exit = code.map(|c| format!(", exit {c}")).unwrap_or_default();
                let tail = if detail.is_empty() { String::new() } else { format!(": {detail}") };
                format!("failed at {step}{exit}{tail}")
            }
        }
    }
}

/// The fleet runner's per-pod progress line: `[done/total] ✓ name` (`… ✓ name (installer:
/// install_uv.sh)` when the bare-VM script said what built the env; `… (warning: <step>: <why>)`
/// when a best-effort step didn't work out; both: `(installer: …; warning: …)`), or
/// `[done/total] ✗ name (timed out at <step> after Ns)` /
/// `[done/total] ✗ name (failed at <step>, exit N): <stderr>`.
pub fn progress_line(done: usize, total: usize, name: &str, outcome: &ProvisionOutcome) -> String {
    match outcome {
        ProvisionOutcome::Done { warnings, installer, image } => {
            let mut said: Vec<String> = installer.iter().map(|i| format!("installer: {i}")).collect();
            said.extend(image.iter().map(|i| format!("image: {i}")));
            if !warnings.is_empty() {
                said.push(format!("warning: {}", warnings.join("; ")));
            }
            if said.is_empty() {
                format!("[{done}/{total}] ✓ {name}")
            } else {
                format!("[{done}/{total}] ✓ {name} ({})", said.join("; "))
            }
        }
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
            let i = at.load(Ordering::Relaxed).min(steps.len() - 1);
            let step = &steps[i];
            // Stuck in the best-effort tail: every required step is done, so the pod is
            // set up — the same as that step timing out on its own. (A best-effort step with
            // required ones after it — the relocation — is a timeout like any other: they
            // never ran.)
            let tail = steps[i..].iter().all(|s| matches!(s, ProvisionStep::Optional { .. }));
            if let (ProvisionStep::Optional { label, .. }, true) = (step, tail) {
                let why = format!("{label}: timed out after {}", human_duration(&budget));
                return ProvisionOutcome::warned(vec![why]);
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
    let mut installer = None;
    let mut image = None;
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
                // couldn't go onto the volume, and why): its `arena-warning:` lines — and, from
                // the bare-VM script, what built the env.
                if let Ok(out) = &call {
                    if out.success {
                        warnings.extend(step_warnings(&out.stdout).map(|w| format!("{step_label}: {w}")));
                        installer = step_installer(&out.stdout).or(installer);
                        image = step_said(&out.stdout, IMAGE_PREFIX, IMAGE_MAX_CHARS).or(image);
                    }
                }
                call
            }
            ProvisionStep::Optional { cmd, timeout, .. } => {
                // Best-effort: whatever happens here, setup carries on — a failure, a timeout or
                // what the step says (`arena-warning:` lines) is a warning on the pod.
                match remote.exec(target, cmd, Some(*timeout)).await {
                    Ok(out) if out.success => {
                        warnings.extend(step_warnings(&out.stdout).map(|w| format!("{step_label}: {w}")));
                    }
                    call => {
                        if let Some(why) = optional_failure(call) {
                            warnings.push(format!("{step_label}: {why}"));
                        }
                    }
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
    Pass::Final(ProvisionOutcome::Done { warnings, installer, image })
}

/// The line the bare-VM setup script (`hetzner_setup.sh`) prints after building — or finding —
/// the ARENA env: `arena-installer: install_uv.sh` (or `none (env already present)`,
/// `requirements.txt (built-in uv install)`). Its `### done` line said the same, but stdout
/// never reaches the operator on success, so which one ran was invisible (live hz F4).
pub const INSTALLER_PREFIX: &str = "arena-installer: ";

/// The longest installer name shown on a ✓ line: one short suffix, never a flood.
const INSTALLER_MAX_CHARS: usize = 80;

/// What a provisioning command said built the env: its last [`INSTALLER_PREFIX`] line, prefix
/// stripped, clipped to [`INSTALLER_MAX_CHARS`]; `None` when it says nothing. Pure.
fn step_installer(stdout: &str) -> Option<String> {
    step_said(stdout, INSTALLER_PREFIX, INSTALLER_MAX_CHARS)
}

/// The line the bare-VM setup script prints the first time it sets up a VM made from an arena
/// image: `arena-image: devtest-image-261029-1200, built 2026-10-29 (2 d ago), commit abc;
/// requirements unchanged`.
pub const IMAGE_PREFIX: &str = "arena-image: ";

/// The longest image line shown: its name, date, age, commit and what setup did with the env.
const IMAGE_MAX_CHARS: usize = 160;

/// A command's last `prefix` line, prefix stripped, clipped to `max` chars; `None` when it has
/// none. Pure.
fn step_said(stdout: &str, prefix: &str, max: usize) -> Option<String> {
    stdout
        .lines()
        .filter_map(|l| l.trim_end_matches('\r').strip_prefix(prefix))
        .map(str::trim)
        .rfind(|i| !i.is_empty())
        .map(|i| crate::fleet::clip_middle(i, max))
}

/// The longest a setup warning gets before its middle is cut ([`crate::fleet::clip_middle`]).
/// Generous: a warning is the one line that says what to do about a pod, and its end is the
/// instruction (a 300-char cut from the end once dropped "… then re-run setup").
pub const WARNING_MAX_CHARS: usize = 800;

/// The warnings a provisioning command printed: its stdout lines starting with
/// [`crate::volume::WARNING_PREFIX`], prefix stripped. Only a runaway line is shortened, and
/// from the middle, so its instruction at the end stays.
fn step_warnings(stdout: &str) -> impl Iterator<Item = String> + '_ {
    stdout
        .lines()
        .filter_map(|l| l.trim_end_matches('\r').strip_prefix(crate::volume::WARNING_PREFIX))
        .map(|w| crate::fleet::clip_middle(w.trim(), WARNING_MAX_CHARS))
}

/// Why a best-effort step didn't work out (`None` = it did): the last stderr line of a
/// non-zero exit (where the warm-up script puts its one-line reason, after any ssh
/// chatter), clipped; a timeout; or the call's own error.
fn optional_failure(call: Result<crate::ssh::SshOutput>) -> Option<String> {
    match call {
        Ok(out) if out.success => None,
        Ok(out) => {
            let exit = out.exit_label();
            let last = out.stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("");
            Some(if last.is_empty() { exit } else { format!("{exit}: {}", crate::fleet::clip_middle(last, WARNING_MAX_CHARS)) })
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
            upstream_url: None,
            branch: "main".into(),
            prefix: "arena8".into(),
            authorized_pubkeys: vec!["ssh-ed25519 AAAASHARED shared".into()],
            broadcast_exports: Vec::new(),
            zsh_install: false,
            vscode: None,
            relocate: true,
            swap_gb: DEFAULT_SWAP_GB,
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
        // The checkout's git identity where it has none: after the update, before the name.
        let (update, identity, name) = (
            c.find("git submodule update").unwrap(),
            c.find(&git_identity_command("/root/ARENA_3.0")).unwrap(),
            c.find("> \"$HOME/.name\"").unwrap(),
        );
        assert!(update < identity && identity < name, "{c}");
        assert!(c.contains(&format!("{GIT_IDENTITY_SH}\narena_git_identity '/root/ARENA_3.0'; ")));
        for v in [GIT_IDENTITY_NAME, GIT_IDENTITY_EMAIL] {
            assert!(GIT_IDENTITY_SH.contains(&format!("'{v}'")), "{v}");
        }
    }

    /// With a volume the repo moves onto it BEFORE the update (so the fetch/reset act on the
    /// volume copy): its own best-effort step, on its own budget, before the config step —
    /// which waits for a relocation still running on the pod. A path that can't be relocated
    /// (already on /workspace) gets no step; hetzner (no volume) none either.
    #[test]
    fn the_repo_goes_onto_the_volume_before_it_is_updated() {
        let t = SetupTimeouts::default();
        assert_eq!(t.relocate, secs(900));
        let steps = provisioning_steps("runpod", &cfg(), "arena8-apple", false, "", &t);
        let labels: Vec<&str> = steps.iter().map(ProvisionStep::label).collect();
        assert_eq!(labels, ["copy deploy key", RELOCATE_LABEL, "repo + keys config"]);
        let ProvisionStep::Optional { cmd, summary, timeout, .. } = &steps[1] else { panic!("{steps:?}") };
        assert_eq!(*cmd, crate::volume::relocation_command("/root/ARENA_3.0", secs(900)).unwrap());
        assert_eq!(*timeout, secs(900));
        assert!(summary.contains("/workspace/ARENA_3.0") && summary.contains("never a failed setup"), "{summary}");
        // --timeout / SETUP_TIMEOUT_SECS size the config step, not the relocation.
        let short = SetupTimeouts::resolve(None, Some(45)).unwrap();
        assert_eq!((short.config, short.relocate), (secs(45), secs(900)));
        // The config step waits for a relocation still running, then updates the repo.
        let c = cfg().remote_command("arena8-apple", false);
        let at = |needle: &str| c.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
        assert!(at(&crate::volume::relocation_wait_command()) < at("git remote set-url"), "{c}");
        assert!(!c.contains("arena-aside"), "the move itself isn't in the config step");
        let mut on_volume = cfg();
        on_volume.repo_path = "/workspace/ARENA_3.0".into();
        let labels: Vec<&str> =
            provisioning_steps("runpod", &on_volume, "arena8-apple", false, "", &t).iter().map(ProvisionStep::label).collect();
        assert_eq!(labels, ["copy deploy key", "repo + keys config"]);
        assert!(!provisioning_steps("hetzner", &cfg(), "a", false, "/h.sh", &t).iter().any(|s| s.label() == RELOCATE_LABEL));
        // REPO_ON_VOLUME=0: no step (the config step's wait is a no-op without a lock file).
        let off = SetupConfig { relocate: false, ..cfg() };
        let labels: Vec<&str> = provisioning_steps("runpod", &off, "a", false, "", &t).iter().map(ProvisionStep::label).collect();
        assert_eq!(labels, ["copy deploy key", "repo + keys config"]);
        let from = |extra: &str| SetupConfig::from_config(&Config::parse(&format!("ARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_LOCAL=/k\n{extra}")));
        assert!(from("").unwrap().relocate);
        assert!(!from("REPO_ON_VOLUME=0").unwrap().relocate);
        assert!(from("REPO_ON_VOLUME=sometimes").is_err());
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

    /// `ARENA_UPSTREAM_REPO`: the public course repo by default, another `owner/name`, or none;
    /// not the cohort repo itself; anything else refused (it goes into a shell command).
    #[test]
    fn the_upstream_remote_is_the_course_repo_unless_configured_otherwise() {
        let cohort = "ARENA_REPO_OWNER=Styme3279\nARENA_REPO_NAME=ARENA_materials\n";
        let url = |extra: &str| upstream_url(&Config::parse(&format!("{cohort}{extra}")));
        let ok: &[(&str, Option<&str>)] = &[
            ("", Some("https://github.com/ARENA-education/ARENA_materials.git")),
            ("ARENA_UPSTREAM_REPO=callummcdougall/ARENA_3.0\n", Some("https://github.com/callummcdougall/ARENA_3.0.git")),
            ("ARENA_UPSTREAM_REPO= o/r.git \n", Some("https://github.com/o/r.git")),
            ("ARENA_UPSTREAM_REPO=none\n", None),
            ("ARENA_UPSTREAM_REPO=NONE\n", None),
            ("ARENA_UPSTREAM_REPO=\n", None),
            // The cohort repo itself, any case: a second name for origin, so none.
            ("ARENA_UPSTREAM_REPO=styme3279/arena_materials\n", None),
            // Its GitHub URL, as pasted (review fb2 R6): the same repo.
            ("ARENA_UPSTREAM_REPO=https://github.com/ARENA-education/ARENA_materials\n", Some("https://github.com/ARENA-education/ARENA_materials.git")),
            ("ARENA_UPSTREAM_REPO=git@github.com:o/r.git\n", Some("https://github.com/o/r.git")),
            ("ARENA_UPSTREAM_REPO=ssh://git@github.com/o/r/\n", Some("https://github.com/o/r.git")),
            ("ARENA_UPSTREAM_REPO=https://github.com/Styme3279/ARENA_materials.git\n", None),
        ];
        for (extra, want) in ok {
            assert_eq!(url(extra).unwrap().as_deref(), *want, "{extra:?}");
        }
        for bad in ["o", "o/r/x", "a b/c", "o/;rm -rf ~", "../x", "o/..", "/r", "o/", "o/.git", "o/r$(id)", "https://gitlab.com/o/r", "https://github.com/o"] {
            let e = url(&format!("ARENA_UPSTREAM_REPO={bad}\n")).unwrap_err().to_string();
            assert!(e.contains("ARENA_UPSTREAM_REPO must be owner/name"), "{bad}: {e}");
        }
        // Read by setup's config: a bad value stops it before anything runs.
        let base = "ARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_LOCAL=/k\n";
        let from = |extra: &str| SetupConfig::from_config(&Config::parse(&format!("{base}{extra}")));
        assert_eq!(from("").unwrap().upstream_url.as_deref(), Some("https://github.com/ARENA-education/ARENA_materials.git"));
        assert!(from("ARENA_UPSTREAM_REPO=x y\n").is_err());
    }

    /// Both setup paths add it: the image's repo update (after origin's re-point, before the
    /// fetch; never fatal), and the bare-VM script, through `UPSTREAM_URL`.
    #[test]
    fn setup_adds_the_course_repo_as_upstream_on_both_paths() {
        let up = "https://github.com/ARENA-education/ARENA_materials.git";
        let with = SetupConfig { upstream_url: Some(up.into()), ..steps_cfg() };
        let cmd = with.repo_update_command(false);
        let add = format!(
            "git remote get-url upstream >/dev/null 2>&1 || git remote add upstream '{up}' || echo 'arena-warning: couldn'\\''t \
             add the upstream remote {up}'; git config --get checkout.defaultRemote >/dev/null 2>&1 || git config \
             checkout.defaultRemote origin || true; "
        );
        assert!(cmd.contains(&add), "{cmd}");
        let at = |s: &str| cmd.find(s).unwrap_or_else(|| panic!("{s}: {cmd}"));
        assert!(at("git remote set-url origin") < at(&add) && at(&add) < at("git fetch"), "{cmd}");
        assert!(!steps_cfg().repo_update_command(false).contains("upstream"));
        let script = |c: &SetupConfig| -> String {
            provisioning_steps("hetzner", c, "arena8-x", false, "/local/hetzner_setup.sh", &SetupTimeouts::default())
                .into_iter()
                .find_map(|s| match s {
                    ProvisionStep::Run { cmd, .. } if cmd.ends_with("bash /root/hetzner_setup.sh") => Some(cmd),
                    _ => None,
                })
                .unwrap()
        };
        assert!(script(&with).starts_with(&format!("ARENA_SWAP_GB=8 UPSTREAM_URL='{up}' REPO_URL='git@github.com:o/r.git' ")), "{}", script(&with));
        assert!(!script(&steps_cfg()).contains("UPSTREAM_URL"));
    }

    /// `VM_SWAP_GB`: 8 when unset or empty, 0 for none, refused when it isn't a whole number of
    /// GB up to the cap — it goes into the script's command line — and the VM commands carry it.
    #[test]
    fn vm_swap_gb_defaults_to_8_and_is_checked() {
        let gb = |extra: &str| swap_gb(&Config::parse(extra));
        assert_eq!(gb("").unwrap(), 8);
        assert_eq!(gb("VM_SWAP_GB=\n").unwrap(), 8);
        assert_eq!(gb("VM_SWAP_GB=0\n").unwrap(), 0);
        assert_eq!(gb("VM_SWAP_GB= 16 \n").unwrap(), 16);
        assert_eq!(gb("VM_SWAP_GB=64\n").unwrap(), 64);
        for bad in ["65", "-1", "8G", "8.5", "4 GB", "$(reboot)"] {
            let e = gb(&format!("VM_SWAP_GB='{bad}'\n")).unwrap_err().to_string();
            assert!(e.contains("VM_SWAP_GB must be a whole number of GB from 0 (no swap) to 64"), "{bad}: {e}");
        }
        let t = SetupTimeouts::default();
        let cmd = |provider: &str, swap: u32| -> String {
            let scfg = SetupConfig { swap_gb: swap, ..steps_cfg() };
            provisioning_steps(provider, &scfg, "devtest-alpha", false, "/tmp/h.sh", &t)
                .into_iter()
                .find_map(|s| match s {
                    ProvisionStep::Run { cmd, .. } if cmd.ends_with("bash /root/hetzner_setup.sh") => Some(cmd),
                    _ => None,
                })
                .unwrap()
        };
        assert!(cmd("hetzner", 0).starts_with("ARENA_SWAP_GB=0 REPO_URL="), "{}", cmd("hetzner", 0));
        assert!(cmd("shadeform", 16).contains(" ARENA_SWAP_GB=16 REPO_URL="), "{}", cmd("shadeform", 16));
        // A container pod can't swap: its steps never pass a size.
        let pod = provisioning_steps("runpod", &steps_cfg(), "devtest-alpha", false, "/tmp/h.sh", &t);
        assert!(!format!("{pod:?}").contains("ARENA_SWAP_GB"));
    }

    // ---- provisioning steps, timeouts, runner -------------------------------------------

    use crate::remote::{FakeRemote, FakeReply, RemoteCall};

    fn steps_cfg() -> SetupConfig {
        SetupConfig {
            key_local: "/local/key".into(),
            key_remote: "/root/.ssh/id_ed25519".into(),
            repo_path: "/root/ARENA_3.0".into(),
            repo_url: "git@github.com:o/r.git".into(),
            upstream_url: None,
            branch: "main".into(),
            prefix: "arena8".into(),
            authorized_pubkeys: vec![],
            broadcast_exports: vec![],
            zsh_install: false,
            vscode: None,
            relocate: true,
            swap_gb: DEFAULT_SWAP_GB,
        }
    }

    /// A bare VM's "authorize keys" step for [`steps_cfg`].
    fn authorize_step() -> ProvisionStep {
        ProvisionStep::Run { label: AUTHORIZE_LABEL, cmd: steps_cfg().authorized_keys_command(), timeout: secs(300) }
    }

    fn target(port: u16) -> SshTarget {
        SshTarget {
            user: "root".into(),
            host: "10.0.0.1".into(),
            port,
            key_paths: vec![],
            connect_timeout_secs: 10,
            host_keys: Default::default(),
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Shadeform's GPU VMs get the bare-VM steps in their GPU mode (`ARENA_GPU=1`, the machine
    /// name for `~/.name`) — the same script, on the same long budget; the only provider names
    /// that pick a bare-VM setup are Hetzner's (CPU) and Shadeform's (GPU).
    #[test]
    fn shadeform_gets_the_bare_vm_gpu_steps() {
        for (provider, want) in [("hetzner", Some(BareVm::Cpu)), ("shadeform", Some(BareVm::Gpu)), ("runpod", None), ("vast", None), ("lambda", None)] {
            assert_eq!(bare_vm(provider), want, "{provider}");
            assert_eq!(is_bare_vm(provider), want.is_some(), "{provider}");
        }
        let t = SetupTimeouts::default();
        let steps = provisioning_steps("shadeform", &steps_cfg(), "devtest-alpha", false, "/tmp/h.sh", &t);
        assert_eq!(
            steps,
            vec![
                ProvisionStep::Scp { label: "copy deploy key", local: "/local/key".into(), remote: "/root/.ssh/id_ed25519".into(), timeout: secs(60) },
                authorize_step(),
                ProvisionStep::Scp { label: "copy bare-VM setup script", local: "/tmp/h.sh".into(), remote: "/root/hetzner_setup.sh".into(), timeout: secs(60) },
                ProvisionStep::Run {
                    label: "GPU VM setup script",
                    cmd: "ARENA_GPU=1 ARENA_MACHINE_NAME='devtest-alpha' ARENA_SWAP_GB=8 REPO_URL='git@github.com:o/r.git' REPO_DIR='/root/ARENA_3.0' \
                          REPO_KEY='/root/.ssh/id_ed25519' bash /root/hetzner_setup.sh"
                        .into(),
                    timeout: secs(1800),
                },
            ]
        );
        // No relocation onto a volume (a VM has none), and --timeout sizes this step too.
        assert!(!steps.iter().any(|s| s.label() == RELOCATE_LABEL));
        let t = SetupTimeouts::resolve(None, Some(900)).unwrap();
        assert_eq!(provisioning_steps("shadeform", &steps_cfg(), "devtest-alpha", false, "/tmp/h.sh", &t)[3].timeout(), secs(900));
    }

    #[test]
    fn provisioning_steps_branch_by_provider() {
        let t = SetupTimeouts::default();
        let scfg = steps_cfg();
        // bare-VM (hetzner): copy the deploy key, authorize every machine's keys, make the
        // machine name its hostname (best-effort: a renamed VM's), push the script, run it with
        // the repo URL/key passed in (so it clones the PRIVATE repo over SSH) — on the long
        // budget.
        assert_eq!(
            provisioning_steps("hetzner", &scfg, "arena8-flutter", false, "/tmp/h.sh", &t),
            vec![
                ProvisionStep::Scp {
                    label: "copy deploy key",
                    local: "/local/key".into(),
                    remote: "/root/.ssh/id_ed25519".into(),
                    timeout: secs(60),
                },
                authorize_step(),
                ProvisionStep::Optional {
                    label: HOSTNAME_LABEL,
                    cmd: hostname_command("arena8-flutter").unwrap(),
                    summary: "set the hostname to arena8-flutter if it isn't (a Hetzner VM is named by it)".into(),
                    timeout: secs(300),
                },
                ProvisionStep::Scp {
                    label: "copy hetzner setup script",
                    local: "/tmp/h.sh".into(),
                    remote: "/root/hetzner_setup.sh".into(),
                    timeout: secs(60),
                },
                // A checkout already there (an arena image, a re-run) is updated first.
                ProvisionStep::Run { label: "repo update", cmd: scfg.bare_vm_repo_update_command(false), timeout: secs(300) },
                ProvisionStep::Run {
                    label: "hetzner setup script",
                    cmd: "ARENA_SWAP_GB=8 REPO_URL='git@github.com:o/r.git' REPO_DIR='/root/ARENA_3.0' REPO_KEY='/root/.ssh/id_ed25519' bash /root/hetzner_setup.sh".into(),
                    timeout: secs(1800),
                },
            ]
        );
        assert!(matches!(&provisioning_steps("hetzner", &scfg, "arena8-flutter", true, "/tmp/h.sh", &t)[4],
            ProvisionStep::Run { cmd, .. } if *cmd == scfg.bare_vm_repo_update_command(true)), "--force reaches it");
        // image-based (runpod/vast): scp the deploy key, move the repo onto the volume (best-
        // effort), then a config command that re-points origin — i.e. the post-image flow, not
        // the bare-VM script.
        for provider in ["runpod", "vast"] {
            let r = provisioning_steps(provider, &scfg, "arena8-apple", false, "/tmp/h.sh", &t);
            assert_eq!(r.len(), 3);
            assert!(matches!(&r[0], ProvisionStep::Scp { local, remote, .. } if local == "/local/key" && remote == "/root/.ssh/id_ed25519"));
            assert!(matches!(&r[1], ProvisionStep::Optional { label: RELOCATE_LABEL, cmd, .. } if cmd.contains("arena-aside")));
            assert_eq!(r[1].timeout(), secs(900));
            assert!(matches!(&r[2], ProvisionStep::Run { cmd, .. } if cmd.contains("git remote set-url")));
            assert_eq!(r[2].label(), "repo + keys config");
            assert_eq!(r[2].timeout(), secs(300));
        }
        // Only a Hetzner VM is named by its hostname: no such step elsewhere, nor for a name
        // that isn't a hostname (never written into /etc).
        for (provider, name) in [("shadeform", "arena8-flutter"), ("runpod", "arena8-apple"), ("hetzner", "arena8_bad name")] {
            let steps = provisioning_steps(provider, &scfg, name, false, "/tmp/h.sh", &t);
            assert!(steps.iter().all(|s| s.label() != HOSTNAME_LABEL), "{provider} {name}");
        }
    }

    /// The hostname a renamed Hetzner VM is given (`pods rename`, and setup's best-effort step),
    /// run against stand-in `hostname`/`hostnamectl` and files, under sh, dash and bash: both
    /// hostnames become the name, only the old names' tokens on `127.0.1.1` lines change (aliases,
    /// comments and other lines kept), a second run changes nothing, and it converges from a run
    /// that stopped half-way (review r1: a retry after a failed /etc/hostname write must not pass
    /// on the running name alone; a hosts line it never got to gets the name). Nothing for a name
    /// that isn't a plain hostname (it goes into /etc). And which providers are named by their
    /// hostname, and which carry MACHINE_NAME in env.
    #[test]
    fn the_hostname_command_converges_and_keeps_other_aliases() {
        use std::process::Command;
        for bad in ["", "a b", "a;rm -rf /", "-lead", "trail-", "a..b", "x_y", &"a".repeat(64), "'q'"] {
            assert!(hostname_command(bad).is_none(), "{bad:?}");
        }
        assert!(is_hostname("arena9-keepsake") && is_hostname("a.b-c.d9") && is_hostname(&"a".repeat(63)));
        assert!(hostname_is_machine_name("hetzner"));
        assert!(["shadeform", "runpod", "vast", ""].iter().all(|p| !hostname_is_machine_name(p)));
        assert!(machine_name_in_env("runpod") && machine_name_in_env("vast"));
        assert!(!machine_name_in_env("hetzner") && !machine_name_in_env("shadeform"));

        let dir = std::env::temp_dir().join(format!("arena-hostname-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let d = dir.display().to_string();
        let file = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
        let put = |f: &str, text: &str| std::fs::write(dir.join(f), text).unwrap();
        let stub = |name: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.join("bin").join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{}\n", body.replace("$D", &d))).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        // Stand-ins: `hostname` prints (with an argument: sets) the running name, $D/name;
        // `hostnamectl set-hostname X` sets it and the static one, $D/etc-hostname, and logs.
        let hostname_ok = r#"if [ $# -gt 0 ]; then echo "$1" > $D/name; else cat $D/name; fi"#;
        let hostnamectl_ok = r#"echo "$*" >> $D/calls; [ "$1" = set-hostname ] && echo "$2" > $D/name && echo "$2" > $D/etc-hostname"#;
        let cmd = hostname_command("arena9-keepsake")
            .unwrap()
            .replace("/etc/hostname", &format!("{d}/etc-hostname"))
            .replace("/etc/hosts", &format!("{d}/hosts"));
        let cloud = "# managed by cloud-init\n127.0.1.1 arena9-rainbow arena9-rainbow\n127.0.0.1 localhost\n::1 localhost ip6-localhost\n\n";
        let custom = "127.0.1.1\tcache.internal\n127.0.1.1 arena9-rainbow   arena9-rainbow cache2 # was arena9-rainbow\n10.0.0.1 arena9-rainbow\n";
        for shell in ["sh", "dash", "bash"] {
            if Command::new(shell).arg("-c").arg("true").status().is_err() {
                continue;
            }
            let run = || {
                let path = format!("{d}/bin:{}", std::env::var("PATH").unwrap_or_default());
                Command::new(shell).arg("-c").arg(&cmd).env("PATH", path).output().unwrap()
            };
            let start = |hosts: &str| {
                stub("hostname", hostname_ok);
                stub("hostnamectl", hostnamectl_ok);
                put("name", "arena9-rainbow\n");
                put("etc-hostname", "arena9-rainbow\n");
                put("hosts", hosts);
                let _ = std::fs::remove_file(dir.join("calls"));
            };
            // cloud-init's file: its line follows, the rest is byte for byte as it was.
            start(cloud);
            let out = run();
            assert!(out.status.success(), "{shell}: {}", String::from_utf8_lossy(&out.stderr));
            assert_eq!((file("name"), file("etc-hostname")), ("arena9-keepsake\n".into(), "arena9-keepsake\n".into()), "{shell}");
            assert_eq!(file("hosts"), cloud.replace("127.0.1.1 arena9-rainbow arena9-rainbow", "127.0.1.1 arena9-keepsake arena9-keepsake"), "{shell}");
            assert_eq!(file("calls"), "set-hostname arena9-keepsake\n", "{shell}");
            // Again: nothing to do, nothing done, still a success.
            std::fs::remove_file(dir.join("calls")).unwrap();
            assert!(run().status.success() && !dir.join("calls").exists(), "{shell}: idempotent");
            // Aliases, a comment and other addresses are kept; only the old names change.
            start(custom);
            assert!(run().status.success(), "{shell}");
            assert_eq!(
                file("hosts"),
                "127.0.1.1\tcache.internal\n127.0.1.1 arena9-keepsake arena9-keepsake cache2 # was arena9-rainbow\n10.0.0.1 arena9-rainbow\n",
                "{shell}"
            );
            // An indented entry is an entry (review r2): renamed, and found again on a recovery.
            start("  127.0.1.1 arena9-rainbow arena9-rainbow\n127.0.0.1 localhost\n");
            assert!(run().status.success(), "{shell}");
            assert_eq!(file("hosts"), "127.0.1.1 arena9-keepsake arena9-keepsake\n127.0.0.1 localhost\n", "{shell}");
            start("127.0.0.1 localhost\n\t127.0.1.1  arena9-rainbow\n");
            put("name", "arena9-keepsake\n");
            put("etc-hostname", "arena9-keepsake\n");
            assert!(run().status.success(), "{shell}");
            assert_eq!(file("hosts"), "127.0.0.1 localhost\n\t127.0.1.1 arena9-keepsake arena9-rainbow\n", "{shell}");
            // No 127.0.1.1 line: the file is left alone.
            start("127.0.0.1 localhost\n");
            assert!(run().status.success() && file("hosts") == "127.0.0.1 localhost\n", "{shell}");
            // hostnamectl refused and the static name can't be written: a failure — and the retry
            // once that's fixed sees the running name already set but not the static one, and
            // writes it (never a success on the running name alone).
            start(cloud);
            stub("hostnamectl", "exit 1");
            put("etc-hostname", "arena9-rainbow\n");
            std::fs::create_dir_all(dir.join("ro")).unwrap();
            let blocked = cmd.replace(&format!("{d}/etc-hostname"), &format!("{d}/ro/x/etc-hostname"));
            let path = format!("{d}/bin:{}", std::env::var("PATH").unwrap_or_default());
            let out = Command::new(shell).arg("-c").arg(&blocked).env("PATH", &path).output().unwrap();
            assert!(!out.status.success(), "{shell}: the static name wasn't written");
            assert_eq!(file("name"), "arena9-keepsake\n", "{shell}: the running one was");
            stub("hostnamectl", hostnamectl_ok);
            assert!(run().status.success(), "{shell}");
            assert_eq!((file("name"), file("etc-hostname")), ("arena9-keepsake\n".into(), "arena9-keepsake\n".into()), "{shell}");
            assert_eq!(file("calls"), "set-hostname arena9-keepsake\n", "{shell}: the retry set it");
            // A run that changed the hostname but never got to the hosts file: the old name is
            // gone from its view, so the name goes first on the line (it resolves either way).
            start(cloud);
            put("name", "arena9-keepsake\n");
            put("etc-hostname", "arena9-keepsake\n");
            assert!(run().status.success(), "{shell}");
            assert!(file("hosts").contains("\n127.0.1.1 arena9-keepsake arena9-rainbow arena9-rainbow\n"), "{shell}: {}", file("hosts"));
            // Neither way can set it: a failure.
            start(cloud);
            stub("hostnamectl", "exit 1");
            stub("hostname", "if [ $# -gt 0 ]; then exit 1; else cat $D/name; fi");
            assert!(!run().status.success(), "{shell}: not renamed is a failure");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 4 (an evals node, 2026-10-15): a bare VM must get the course repo the image pods
    /// follow — the configured `git@github.com:<ARENA_REPO_OWNER>/<ARENA_REPO_NAME>.git` (prod:
    /// styme3279/ARENA_materials) at BACKUP_REPO_PATH — never a default of the script's own.
    /// Hetzner and Shadeform pass it to the script; RunPod/Vast re-point the image's checkout.
    #[test]
    fn every_provider_follows_the_configured_repo() {
        let cfg = Config::parse(
            "ARENA_REPO_OWNER=styme3279\nARENA_REPO_NAME=ARENA_materials\nGIT_SSH_KEY_LOCAL=/k\n\
             BACKUP_REPO_PATH=/root/ARENA_materials\n",
        );
        let scfg = SetupConfig::from_config(&cfg).unwrap();
        let url = "'git@github.com:styme3279/ARENA_materials.git'";
        let script = format!("REPO_URL={url} REPO_DIR='/root/ARENA_materials' ");
        let image = format!("git remote set-url origin {url}");
        let t = SetupTimeouts::default();
        for (provider, want) in [("hetzner", &script), ("shadeform", &script), ("runpod", &image), ("vast", &image)] {
            let steps = provisioning_steps(provider, &scfg, "arena9-apple", false, "/tmp/h.sh", &t);
            let runs: Vec<&str> = steps
                .iter()
                .filter_map(|s| match s {
                    ProvisionStep::Run { cmd, .. } => Some(cmd.as_str()),
                    _ => None,
                })
                .collect();
            assert!(runs.iter().any(|c| c.contains(want.as_str())), "{provider}: {runs:?}");
        }
    }

    /// Every machine authorizes every machine's keys at setup — the shared key, the deploy key
    /// and the admin key (only the proxy box holds its private half) — on every provider: image
    /// pods in their config step, bare VMs (whose script authorizes none, and whose Hetzner create
    /// only the project's key) in a step of their own before the long script. Public halves only.
    #[test]
    fn every_provider_authorizes_every_machines_keys() {
        let keys = ["ssh-ed25519 AAAAC3NzaSHARED cohort", "ssh-ed25519 AAAAC3NzaDEPLOY deploy", "ssh-ed25519 AAAAC3NzaADMIN arena_admin"];
        let scfg = SetupConfig { authorized_pubkeys: keys.iter().map(|k| k.to_string()).collect(), ..steps_cfg() };
        let t = SetupTimeouts::default();
        for provider in ["runpod", "vast", "hetzner", "shadeform"] {
            let steps = provisioning_steps(provider, &scfg, "devtest-apple", false, "/tmp/h.sh", &t);
            let authorize = steps
                .iter()
                .position(|s| matches!(s, ProvisionStep::Run { cmd, .. } if keys.iter().all(|k| cmd.contains(&format!("grep -qxF '{k}' \"$HOME/.ssh/authorized_keys\"")))))
                .unwrap_or_else(|| panic!("{provider}: {steps:?}"));
            // After the deploy key is copied (its public half is derived from it); on a bare VM
            // before the script, so a script that fails leaves the VM reachable.
            if is_bare_vm(provider) {
                assert_eq!((authorize, steps[authorize].label()), (1, AUTHORIZE_LABEL), "{provider}");
            } else {
                assert_eq!(steps[authorize].label(), "repo + keys config", "{provider}");
            }
            let ProvisionStep::Run { cmd, .. } = &steps[authorize] else { unreachable!() };
            assert!(cmd.contains("PUB=$(ssh-keygen -y -f /root/.ssh/id_ed25519"), "{provider}: {cmd}");
        }
    }

    /// The authorize command as a machine runs it (`sh`, a temp `$HOME`, a real key from
    /// `ssh-keygen`): every machine's key once, the copied deploy key's own public half derived on
    /// the spot even though the copy left it 0644 (it's 0600 after), and a second run adds nothing.
    #[cfg(unix)]
    #[test]
    fn the_authorize_command_runs_and_adds_each_key_once() {
        use std::os::unix::fs::PermissionsExt;
        if std::process::Command::new("ssh-keygen").arg("-?").output().is_err() {
            return; // no ssh-keygen here
        }
        let home = std::env::temp_dir().join(format!("arena-authorize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let key = home.join("deploy_key");
        let made = std::process::Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", "deploy", "-f"]).arg(&key).output().unwrap();
        assert!(made.status.success(), "{made:?}");
        std::fs::remove_file(home.join("deploy_key.pub")).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let keys = ["ssh-ed25519 AAAAC3NzaSHARED cohort", "ssh-ed25519 AAAAC3NzaADMIN staff@proxy"];
        let scfg = SetupConfig {
            key_remote: key.display().to_string(),
            authorized_pubkeys: keys.iter().map(|k| k.to_string()).collect(),
            ..steps_cfg()
        };
        let run = || {
            let o = std::process::Command::new("sh").arg("-c").arg(scfg.authorized_keys_command()).env("HOME", &home).output().unwrap();
            assert!(o.status.success(), "{o:?}");
        };
        run();
        run();
        let ak = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap();
        let lines: Vec<&str> = ak.lines().collect();
        assert_eq!(lines.len(), 3, "{ak}");
        assert_eq!(lines[..2], keys);
        assert!(lines[2].starts_with("ssh-ed25519 ") && lines[2].ends_with(" deploy"), "{ak}");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&key), mode(&home.join(".ssh")), mode(&home.join(".ssh/authorized_keys"))), (0o600, 0o700, 0o600));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn setup_timeouts_resolve_flag_over_config_over_defaults() {
        let d = SetupTimeouts::default();
        assert_eq!((d.copy, d.config, d.bare_vm), (secs(60), secs(300), secs(1800)));
        let run = |cfg: Option<&str>, flag: Option<u64>| {
            SetupTimeouts::resolve(cfg, flag).map(|t| (t.copy, t.config, t.bare_vm))
        };
        // (config value, --timeout) -> (copy, config, bare_vm)
        type Row = (Option<&'static str>, Option<u64>, (Duration, Duration, Duration));
        let table: &[Row] = &[
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
        // 60 (key) + 900 (repo onto volume) + 300 (config) + 150 (boot window) + 6 (one retry pause)
        assert_eq!(pod_budget(&steps, BootRetry::default()), secs(1416));
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
        assert_eq!(calls.len(), 6);
        assert!(matches!(&calls[0], RemoteCall::Copy { local, timeout, .. } if local == "/local/key" && *timeout == Some(secs(60))));
        assert!(matches!(&calls[1], RemoteCall::Exec { cmd, timeout, .. } if cmd.contains("authorized_keys") && *timeout == Some(secs(300))));
        assert!(matches!(&calls[2], RemoteCall::Exec { cmd, timeout, .. } if *cmd == hostname_command("arena8-flutter").unwrap() && *timeout == Some(secs(300))));
        assert!(matches!(&calls[3], RemoteCall::Copy { remote, .. } if remote == "/root/hetzner_setup.sh"));
        assert!(matches!(&calls[4], RemoteCall::Exec { cmd, timeout, .. } if cmd.contains("git fetch --no-tags origin") && *timeout == Some(secs(300))));
        assert!(matches!(&calls[5], RemoteCall::Exec { cmd, timeout, .. } if cmd.ends_with("bash /root/hetzner_setup.sh") && *timeout == Some(secs(1800))));

        let fake = FakeRemote::new();
        assert_eq!(provision(&fake, &target(22), &image_steps(), BootRetry::default()).await, ProvisionOutcome::done());
        let calls = fake.calls();
        assert!(
            matches!(&calls[..], [RemoteCall::Copy { .. }, RemoteCall::Exec { cmd: reloc, timeout: t1, .. }, RemoteCall::Exec { cmd, timeout: t2, .. }]
                if reloc.contains("arena-aside") && *t1 == Some(secs(900)) && cmd.contains("git remote set-url") && *t2 == Some(secs(300))),
            "{calls:?}"
        );
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
        fake.script("10.0.0.1:22", [FakeReply::exit(255, denied), FakeReply::ok(), FakeReply::ok(), FakeReply::ok()]);
        let start = tokio::time::Instant::now();
        let out = provision_after_key_repair(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::done());
        assert_eq!(start.elapsed(), secs(40));
        assert_eq!(fake.calls().len(), 4, "refused copy, then copy + relocation + config");
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
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::ok(), FakeReply::exit(128, "fatal: couldn't find remote ref")]);
        let out = provision_after_key_repair(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert!(matches!(&out, ProvisionOutcome::Failed { step: "repo + keys config", .. }), "{out:?}");
        assert_eq!(fake.calls().len(), 3);
    }

    /// The relocation reports a repo it couldn't put on the volume as an `arena-warning:` line:
    /// the pod is set up, with the warning — never a failed setup, never silent. So does a
    /// required step (the config step's wait for a relocation still running).
    #[tokio::test(start_paused = true)]
    async fn a_step_that_worked_can_still_warn() {
        let fake = FakeRemote::new();
        let said = "arena-volume: nothing\narena-warning: repo not moved onto the /workspace volume: in use (pid 42) - re-run setup when it is idle\r\n";
        let waited = "arena-warning: another setup is still moving the repo onto the volume - the update ran anyway\n";
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::stdout(said), FakeReply::stdout(waited)]);
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(
            out,
            ProvisionOutcome::warned(vec![
                "repo onto volume: repo not moved onto the /workspace volume: in use (pid 42) - re-run setup when it is idle".into(),
                "repo + keys config: another setup is still moving the repo onto the volume - the update ran anyway".into(),
            ])
        );
        // A failed required step's stdout isn't mined for warnings: the failure is the report.
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::ok(), FakeReply::exit(1, "boom").with_stdout("arena-warning: x\n")]);
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert!(matches!(&out, ProvisionOutcome::Failed { detail, .. } if detail == "boom"), "{out:?}");
    }

    /// Live (hz F4): the bare-VM script names what built the env, but on success only the ✓
    /// line reaches the operator — which said `✓` alone. Now the script's `arena-installer:` line
    /// is on it, beside any warning, in one short suffix. Only a step that worked is read (a
    /// failure is its own report); the last line said wins; a runaway one is clipped.
    #[tokio::test(start_paused = true)]
    async fn the_installer_a_bare_vm_setup_names_is_on_its_line() {
        let steps = provisioning_steps("hetzner", &vscode_cfg(), "devtest-hz1", false, "/tmp/h.sh", &SetupTimeouts::default());
        let script = "### 4/6 ARENA repo + Python env\ninstaller: /root/r/install_uv.sh (the repo's own), torch backend cpu\n\
                      arena-installer: install_uv.sh\r\n### done — Python 3.13.16, torch 2.14.1+cpu, installer: install_uv.sh\n";
        let fake = FakeRemote::new();
        // Copy the key, authorize the keys, the hostname, copy the script, the repo update, the
        // script, the warm-up.
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::stdout(script), FakeReply::ok()]);
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::Done { warnings: vec![], installer: Some("install_uv.sh".into()), image: None });
        assert_eq!(progress_line(1, 1, "devtest-hz1", &out), "[1/1] ✓ devtest-hz1 (installer: install_uv.sh)");
        // With a warning (the warm-up failed): both.
        let fake = FakeRemote::new();
        let again = "arena-installer: install_uv.sh\narena-installer: none (env already present)\n";
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::stdout(again), FakeReply::exit(3, "warm-up incomplete: x")]);
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(
            progress_line(2, 2, "devtest-hz1", &out),
            "[2/2] ✓ devtest-hz1 (installer: none (env already present); warning: vscode warm-up: exit 3: warm-up incomplete: x)"
        );
        // A script that failed: its line isn't read.
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::exit(1, "FAILED: no torch").with_stdout(script)]);
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert!(!out.is_done() && out.installer().is_none(), "{out:?}");
        // A VM from an arena image: which image, on the line too (once, from its first setup).
        let fake = FakeRemote::new();
        let from_image = format!("{script}arena-image: devtest-image-261029-1542, built 2026-10-29 (2 h ago), commit 3eed962b9bd8; requirements unchanged, env kept\n");
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::stdout(&from_image), FakeReply::ok()]);
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out.image(), Some("devtest-image-261029-1542, built 2026-10-29 (2 h ago), commit 3eed962b9bd8; requirements unchanged, env kept"));
        assert_eq!(
            progress_line(1, 1, "devtest-hz1", &out),
            "[1/1] ✓ devtest-hz1 (installer: install_uv.sh; image: devtest-image-261029-1542, built 2026-10-29 (2 h ago), commit 3eed962b9bd8; requirements unchanged, env kept)"
        );
        // Nothing said (an image-based setup): the line as before.
        assert_eq!(step_installer("### done\narena-installer:   \n"), None);
        assert_eq!(progress_line(1, 1, "a", &ProvisionOutcome::warned(vec!["w".into()])), "[1/1] ✓ a (warning: w)");
        let runaway = step_installer(&format!("arena-installer: {}\n", "x".repeat(500))).unwrap();
        assert!(runaway.chars().count() <= INSTALLER_MAX_CHARS && runaway.contains(" … "), "{runaway}");
    }

    /// A long warning keeps its instruction: the live relocation warning (both copies exist,
    /// with the uncommitted files listed) ran past the old 300-char cut, which dropped exactly
    /// its last words — what to do. Now it's whole up to [`WARNING_MAX_CHARS`], and a runaway
    /// one loses its middle, never its end.
    #[tokio::test(start_paused = true)]
    async fn a_long_warning_keeps_its_instruction_at_the_end() {
        let files: Vec<String> = (0..12).map(|i| format!("M chapter1_transformer_interp/exercises/part{i}/solutions.py")).collect();
        let long = format!(
            "both /root/ARENA_materials (uncommitted changes: {}) and the volume copy /workspace/ARENA_materials exist - \
             neither touched: work in /root/ARENA_materials is on the container disk. Copy what's needed from \
             /root/ARENA_materials into /workspace/ARENA_materials, move /root/ARENA_materials away, then re-run setup",
            files[..3].join(", ")
        );
        assert!(long.chars().count() > 300 && long.chars().count() < WARNING_MAX_CHARS, "{}", long.len());
        let runaway = format!("both copies exist (uncommitted changes: {}) - move it away, then re-run setup", files.join(", ").repeat(3));
        assert!(runaway.chars().count() > WARNING_MAX_CHARS);
        let fake = FakeRemote::new();
        let said = format!("arena-warning: {long}\narena-warning: {runaway}\n");
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::stdout(&said), FakeReply::ok()]);
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        let w = out.warnings();
        assert_eq!(w[0], format!("repo onto volume: {long}"), "whole");
        assert!(w[1].ends_with("- move it away, then re-run setup") && w[1].contains(" … "), "{}", w[1]);
        assert!(w[1].starts_with("repo onto volume: both copies exist (uncommitted changes: M chapter1"), "{}", w[1]);
        assert!(w[1].chars().count() <= WARNING_MAX_CHARS + "repo onto volume: ".len());
    }

    /// The relocation is best-effort: failing or running out of its own budget (a huge repo,
    /// a slow volume) is a warning, and the config step still runs — with its own budget.
    #[tokio::test(start_paused = true)]
    async fn a_failed_or_slow_relocation_never_fails_setup() {
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::hang(), FakeReply::ok()]);
        let start = tokio::time::Instant::now();
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::warned(vec!["repo onto volume: timed out after 900s".into()]));
        assert_eq!(start.elapsed(), secs(900));
        let calls = fake.calls();
        assert!(matches!(&calls[2], RemoteCall::Exec { cmd, timeout, .. } if cmd.contains("git remote set-url") && *timeout == Some(secs(300))));
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::exit(2, "sh: 1: find: not found")]);
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::warned(vec!["repo onto volume: exit 2: sh: 1: find: not found".into()]));
        assert_eq!(fake.calls().len(), 3, "the config step still ran");
    }

    #[tokio::test(start_paused = true)]
    async fn failing_command_is_reported_not_retried() {
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::ok(), FakeReply::exit(128, "fatal: couldn't find remote ref")]);
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert!(matches!(&out, ProvisionOutcome::Failed { step: "repo + keys config", code: Some(128), .. }), "{out:?}");
        assert_eq!(fake.calls().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_step_times_out_naming_the_step() {
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::ok(), FakeReply::hang()]);
        let start = tokio::time::Instant::now();
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::TimedOut { step: "repo + keys config", after: secs(300) });
        assert_eq!(start.elapsed(), secs(300));
        assert_eq!(fake.calls().len(), 3, "a timeout is never retried as a connection error");
    }

    #[tokio::test(start_paused = true)]
    async fn connection_refused_then_success_is_retried_within_the_boot_window() {
        // (d) sshd not up yet: scp exits 255 with ssh's connect error twice, then works.
        let refused = || FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22: Connection refused");
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [refused(), FakeReply::error("ssh: connect to host 10.0.0.1 port 22: No route to host"), FakeReply::ok(), FakeReply::ok(), FakeReply::ok()]);
        let start = tokio::time::Instant::now();
        let out = provision(&fake, &target(22), &image_steps(), BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::done());
        assert_eq!(start.elapsed(), secs(12), "two 6s retry pauses");
        let kinds: Vec<&str> = fake
            .calls()
            .iter()
            .map(|c| if matches!(c, RemoteCall::Copy { .. }) { "copy" } else { "exec" })
            .collect();
        assert_eq!(kinds, ["copy", "copy", "copy", "exec", "exec"], "restarts from the first step");
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
        assert_eq!(out, ProvisionOutcome::TimedOut { step: "copy deploy key", after: secs(1416) });
        assert_eq!(start.elapsed(), secs(1416));
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
        // Memory-saving settings: every CPU VM; elsewhere only a machine under 20 GiB of RAM.
        for (provider, before, low, said) in [
            ("runpod", 3, "20", "if under 20 GiB of RAM"),
            ("vast", 3, "20", "if under 20 GiB of RAM"),
            ("hetzner", 6, "always", "always"),
            ("shadeform", 4, "20", "if under 20 GiB of RAM"),
        ] {
            let steps = provisioning_steps(provider, &vscode_cfg(), "arena8-apple", false, "/tmp/h.sh", &t);
            assert_eq!(steps.len(), before + 1, "{provider}");
            let ProvisionStep::Optional { label, cmd, summary, timeout } = steps.last().unwrap() else {
                panic!("{provider}: {steps:?}")
            };
            assert_eq!((*label, *timeout), ("vscode warm-up", secs(300)));
            assert!(cmd.contains("arena-vscode-warmup") && cmd.starts_with("ARENA_VSCODE_BUDGET=270 "), "{cmd}");
            assert!(cmd.contains(&format!(" ARENA_VSCODE_LOW_MEMORY={low} bash -c ")), "{provider}: {cmd}");
            assert!(summary.contains(&format!("memory-saving settings (Pylance indexing off, excludes; where unset) {said}")), "{summary}");
            // Hetzner's interpreter is the venv its script builds under the repo.
            assert!(cmd.contains("'/root/ARENA_3.0/.venv/bin/python'"), "{cmd}");
            assert!(summary.contains("ms-python.vscode-pylance"), "{summary}");
            // The required steps are exactly what they were without it.
            assert_eq!(steps[..before], provisioning_steps(provider, &steps_cfg(), "arena8-apple", false, "/tmp/h.sh", &t)[..]);
        }
        // --timeout / SETUP_TIMEOUT_SECS size the main step, not the warm-up.
        let t = SetupTimeouts::resolve(Some("900"), Some(45)).unwrap();
        assert_eq!((t.config, t.vscode), (secs(45), secs(300)));
        // Its budget counts toward the pod's ceiling: 60 + 900 + 300 + 300 + 150 + 6.
        let steps = provisioning_steps("runpod", &vscode_cfg(), "arena8-apple", false, "", &SetupTimeouts::default());
        assert_eq!(pod_budget(&steps, BootRetry::default()), secs(1716));
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
        let warned = |w: &str| ProvisionOutcome::warned(vec![w.to_string()]);

        // The script's own report: exit 3, its reason on the last stderr line (after ssh chatter).
        let fake = FakeRemote::new();
        fake.script(
            "10.0.0.1:22",
            [
                FakeReply::ok(),
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
        assert!(matches!(&fake.calls()[3], RemoteCall::Exec { timeout, .. } if *timeout == Some(secs(300))));

        // Hung: its own budget runs out, the pod is still set up.
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::hang()]);
        let start = tokio::time::Instant::now();
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out, warned("vscode warm-up: timed out after 300s"));
        assert_eq!(start.elapsed(), secs(300));

        // The connection dropping under it: a warning too — never a retry of the whole pod.
        let fake = FakeRemote::new();
        fake.script(
            "10.0.0.1:22",
            [FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::error("ssh: connect to host 10.0.0.1 port 22: Connection refused")],
        );
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert!(out.is_done() && out.warnings()[0].contains("Connection refused"), "{out:?}");
        assert_eq!(fake.calls().len(), 4);

        // A failure before it is the pod's failure, as ever — the warm-up never runs.
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22", [FakeReply::ok(), FakeReply::ok(), FakeReply::exit(128, "fatal: couldn't find remote ref")]);
        let out = provision(&fake, &target(22), &steps, BootRetry::default()).await;
        assert!(matches!(&out, ProvisionOutcome::Failed { step: "repo + keys config", .. }), "{out:?}");
        assert!(out.warnings().is_empty());
        assert_eq!(fake.calls().len(), 3);

        // All well: no warnings.
        let fake = FakeRemote::new();
        assert_eq!(provision(&fake, &target(22), &steps, BootRetry::default()).await, ProvisionOutcome::done());
        assert_eq!(fake.calls().len(), 4);
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
        let stuck_in_warm_up = DeafAfter { ok: 3, calls: AtomicUsize::new(0) };
        let out = provision(&stuck_in_warm_up, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::warned(vec!["vscode warm-up: timed out after 1716s".into()]));
        // Stuck before it: still the failure it always was.
        let stuck_in_config = DeafAfter { ok: 2, calls: AtomicUsize::new(0) };
        let out = provision(&stuck_in_config, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::TimedOut { step: "repo + keys config", after: secs(1716) });
        // Stuck in the best-effort relocation: the required config never ran — not "set up".
        let stuck_in_relocation = DeafAfter { ok: 1, calls: AtomicUsize::new(0) };
        let out = provision(&stuck_in_relocation, &target(22), &steps, BootRetry::default()).await;
        assert_eq!(out, ProvisionOutcome::TimedOut { step: RELOCATE_LABEL, after: secs(1716) });
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
        use crate::setup::{git_identity_command, GIT_IDENTITY_EMAIL, GIT_IDENTITY_NAME};
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
                self.run_update_with(force, None)
            }

            /// [`Self::run_update`] with `upstream` as the configured course repo.
            fn run_update_with(&self, force: bool, upstream: Option<&str>) -> Output {
                let mut scfg = steps_cfg();
                scfg.repo_path = self.pod.to_string_lossy().into_owned();
                scfg.repo_url = self.origin.to_string_lossy().into_owned();
                scfg.upstream_url = upstream.map(String::from);
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

        /// A bare VM's repo update (before its setup script): on a VM from an arena image the
        /// clean default branch fast-forwards to origin's — with the deploy key wired to
        /// github.com, which the image carries none of — and on a fresh VM (no checkout yet)
        /// it does nothing but say so. Real git.
        #[test]
        fn a_bare_vms_checkout_fast_forwards_before_its_setup_script() {
            let Some(f) = Fixture::new("bare-vm") else { return };
            f.clone_pod(&[]);
            let (main, _) = f.advance_origin();
            let key = f.root.join("deploy_key");
            std::fs::write(&key, "k").unwrap();
            std::fs::create_dir_all(f.root.join(".ssh")).unwrap();
            std::fs::write(f.root.join(".ssh/known_hosts"), "github.com ssh-ed25519 AAAA\n").unwrap();
            let mut scfg = steps_cfg();
            scfg.repo_path = f.pod.to_string_lossy().into_owned();
            scfg.repo_url = f.origin.to_string_lossy().into_owned();
            scfg.key_remote = key.to_string_lossy().into_owned();
            let run = |scfg: &crate::setup::SetupConfig| {
                let mut c = Command::new("sh");
                c.arg("-c").arg(scfg.bare_vm_repo_update_command(false));
                isolated(&mut c, &f.root);
                c.output().unwrap()
            };
            let out = run(&scfg);
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert_eq!(f.head(), ("main".to_string(), main));
            let config = std::fs::read_to_string(f.root.join(".ssh/config")).unwrap();
            assert!(config.contains(&format!("IdentityFile {}", key.display())), "{config}");
            f.assert_narrow();
            // A fresh VM: no checkout, nothing done (the script clones it), no ssh config written.
            std::fs::remove_file(f.root.join(".ssh/config")).unwrap();
            scfg.repo_path = f.root.join("absent").to_string_lossy().into_owned();
            let out = run(&scfg);
            assert!(out.status.success());
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "no checkout yet: the setup script clones it");
            assert!(!f.root.join(".ssh/config").exists());
        }

        /// The human (2026-10-19): with `origin` the cohort's copy, the public course repo as
        /// `upstream` too. Real git: added where there's none, a participant's own left as it is,
        /// none without one configured — and never fetched by setup (only origin's branch is).
        #[test]
        fn the_course_repo_becomes_upstream_only_where_there_is_none() {
            let Some(f) = Fixture::new("upstream") else { return };
            f.clone_pod(&[]);
            // A reachable course repo with branches (a local copy of origin): a fetch of it would
            // leave refs/remotes/upstream/* behind (review fb2 R7).
            let course = f.root.join("course.git");
            f.git(&f.root, &["clone", "-q", "--bare", "origin.git", "course.git"]);
            let up = course.to_string_lossy().into_owned();
            f.run_update_with(false, Some(&up));
            assert_eq!(f.git(&f.pod, &["remote", "get-url", "upstream"]), up);
            assert_eq!(f.git(&f.pod, &["remote", "get-url", "origin"]), f.origin.to_string_lossy());
            assert_eq!(f.git(&f.pod, &["config", "checkout.defaultRemote"]), "origin", "a plain checkout takes origin's");
            let refs = f.git(&f.pod, &["for-each-ref", "--format=%(refname)"]);
            assert!(!refs.contains("refs/remotes/upstream"), "setup fetched upstream:\n{refs}");
            // A participant's own upstream (a fork, say) stays.
            let theirs = "https://github.com/someone/fork.git";
            f.git(&f.pod, &["remote", "set-url", "upstream", theirs]);
            f.git(&f.pod, &["config", "checkout.defaultRemote", "upstream"]);
            f.run_update_with(false, Some(&up));
            assert_eq!(f.git(&f.pod, &["remote", "get-url", "upstream"]), theirs);
            assert_eq!(f.git(&f.pod, &["config", "checkout.defaultRemote"]), "upstream", "theirs too");
            // None configured (`ARENA_UPSTREAM_REPO=none`): none added.
            f.git(&f.pod, &["remote", "remove", "upstream"]);
            f.run_update_with(true, None);
            assert_eq!(f.git(&f.pod, &["remote"]), "origin");
        }

        /// Live 7/9: a fresh pod had no git identity until its first backup on a day branch, so a
        /// participant's first commit or merge stopped at "Please tell me who you are". The
        /// config step's [`git_identity_command`] gives the backup's where git finds none — under
        /// sh, bash and zsh (pods' login shell), with the step's `set -e` — and the backup's own
        /// identity lines, run after it, change nothing (no flip-flop); a global one the
        /// participant sets later takes over at the next setup, and the backup leaves it so.
        #[test]
        fn the_checkout_gets_the_backups_identity_only_where_git_finds_none() {
            let Some(f) = Fixture::new("identity") else { return };
            f.clone_pod(&[]);
            let home = f.root.join("home");
            std::fs::create_dir_all(&home).unwrap();
            let system = f.root.join("gitconfig-system");
            std::fs::write(&system, "").unwrap();
            // As on a pod: no identity in the environment, the global config in $HOME.
            let env = |c: &mut Command| {
                c.env("HOME", &home)
                    .env("GIT_CONFIG_SYSTEM", &system)
                    .env_remove("GIT_CONFIG_GLOBAL")
                    .env_remove("GIT_CONFIG_NOSYSTEM")
                    .env_remove("GIT_DIR")
                    .env_remove("GIT_WORK_TREE");
                for v in ["GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL", "EMAIL"] {
                    c.env_remove(v);
                }
            };
            let git = |args: &[&str]| -> (bool, String) {
                let mut c = Command::new("git");
                c.arg("-C").arg(&f.pod).args(args);
                env(&mut c);
                let out = c.output().unwrap();
                (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout).trim(), String::from_utf8_lossy(&out.stderr).trim()))
            };
            let pod = f.pod.to_string_lossy().into_owned();
            let identity = |shell: &str| {
                let mut c = Command::new(shell);
                c.arg("-c").arg(format!("set -e; {}; echo done", git_identity_command(&pod)));
                env(&mut c);
                let out = c.output().unwrap();
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                assert!(out.status.success() && stdout.trim() == "done", "{shell}: {stdout}{}", String::from_utf8_lossy(&out.stderr));
            };
            // The backup's identity lines, as backup.sh runs them in the checkout.
            let backup_lines: String = include_str!("backup.sh").lines().filter(|l| l.starts_with("git config user.")).collect::<Vec<_>>().join("\n");
            assert_eq!(
                backup_lines,
                format!(
                    "git config user.name >/dev/null 2>&1 || git config user.name '{GIT_IDENTITY_NAME}'\n\
                     git config user.email >/dev/null 2>&1 || git config user.email '{GIT_IDENTITY_EMAIL}'"
                ),
                "the backup's identity is the one setup gives"
            );
            let backup = || {
                let mut c = Command::new("sh");
                c.arg("-c").arg(format!("set -e; cd '{pod}'; {backup_lines}"));
                env(&mut c);
                assert!(c.status().unwrap().success());
            };
            let local = |key: &str| git(&["config", "--local", key]).1;
            let author = || git(&["log", "-1", "--format=%an <%ae>"]).1;
            let ours = format!("{GIT_IDENTITY_NAME} <{GIT_IDENTITY_EMAIL}>");
            let shells: Vec<&str> = ["sh", "bash", "zsh"]
                .into_iter()
                .filter(|s| Command::new(s).args(["-c", "true"]).status().is_ok_and(|st| st.success()))
                .collect();
            for shell in shells {
                // None anywhere: the commit fails; after setup it's the backup's identity.
                let _ = std::fs::remove_file(home.join(".gitconfig"));
                let _ = git(&["config", "--local", "--unset", "user.name"]);
                let _ = git(&["config", "--local", "--unset", "user.email"]);
                let (ok, why) = git(&["commit", "-q", "--allow-empty", "-m", "before"]);
                assert!(!ok && why.contains("Please tell me who you are"), "{shell}: {why}");
                identity(shell);
                assert_eq!((local("user.name"), local("user.email")), (GIT_IDENTITY_NAME.into(), GIT_IDENTITY_EMAIL.into()), "{shell}");
                assert!(git(&["commit", "-q", "--allow-empty", "-m", "first"]).0, "{shell}");
                assert_eq!(author(), ours, "{shell}");
                let config = std::fs::read_to_string(f.pod.join(".git/config")).unwrap();
                backup();
                identity(shell);
                assert_eq!(std::fs::read_to_string(f.pod.join(".git/config")).unwrap(), config, "{shell}: no flip-flop");
                // Their global identity, set since: setup hands over; the backup leaves it so.
                std::fs::write(home.join(".gitconfig"), "[user]\n\tname = Ada\n\temail = ada@example.com\n").unwrap();
                identity(shell);
                backup();
                identity(shell);
                assert_eq!((local("user.name"), local("user.email")), (String::new(), String::new()), "{shell}");
                assert!(git(&["commit", "-q", "--allow-empty", "-m", "theirs"]).0);
                assert_eq!(author(), "Ada <ada@example.com>", "{shell}");
                // Their repo-local one, and any other value: never touched.
                assert!(git(&["config", "--local", "user.name", "Bea"]).0);
                identity(shell);
                assert_eq!(local("user.name"), "Bea", "{shell}");
                let _ = git(&["config", "--local", "--unset", "user.name"]);
            }
            // No checkout (a non-arena image): nothing, no failure.
            let mut c = Command::new("sh");
            c.arg("-c").arg(format!("set -e; {}; echo done", git_identity_command(&f.root.join("absent").to_string_lossy())));
            env(&mut c);
            assert_eq!(String::from_utf8_lossy(&c.output().unwrap().stdout).trim(), "done");
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

        /// Review finding: setup re-runs by itself after a reset (`restart`, `reimage`, `pods
        /// start`) on the repo the volume kept, and on the default branch it `reset --hard` —
        /// reverting the participant's edits and dropping their commits. Their work is kept
        /// now, with a warning; a clean checkout still moves up.
        #[test]
        fn on_default_branch_never_discards_the_participants_work() {
            let Some(f) = Fixture::new("default-work") else { return };
            f.clone_pod(&[]);
            let (main, _) = f.advance_origin();
            // A tracked edit, uncommitted: kept, not updated, and said.
            std::fs::write(f.pod.join("a"), "my edit").unwrap();
            let before = f.head();
            let out = f.run_update(false);
            assert_eq!(std::fs::read_to_string(f.pod.join("a")).unwrap(), "my edit");
            assert_eq!(f.head(), before);
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(said.contains("arena-warning: the repo on main has its own changes or commits"), "{said}");
            // A local commit on main: kept too.
            f.git(&f.pod, &["commit", "-q", "-am", "mine"]);
            let mine = f.rev("HEAD");
            f.run_update(false);
            assert_eq!(f.rev("HEAD"), mine, "a commit origin lacks is never dropped");
            assert_eq!(f.rev("origin/main"), main, "the fetch still happened");
            // A staged-only change counts as work as well.
            f.git(&f.pod, &["reset", "-q", "--hard", "origin/main"]);
            std::fs::write(f.pod.join("a"), "staged").unwrap();
            f.git(&f.pod, &["add", "a"]);
            f.run_update(false);
            assert_eq!(f.git(&f.pod, &["diff", "--cached", "--name-only"]), "a");
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
            let Some(f) = Fixture::new("volume") else { return };
            f.clone_pod(&[]);
            let (ws, bin) = (f.root.join("workspace"), f.root.join("bin"));
            std::fs::create_dir_all(&ws).unwrap();
            std::fs::create_dir_all(&bin).unwrap();
            let stub = bin.join("mountpoint");
            crate::remote::write_test_script(&stub, format!("#!/bin/sh\n[ \"$2\" = '{}' ]\n", ws.display())).unwrap();
            let site = crate::volume::RelocationSite {
                volume: ws.display().to_string(),
                lock: f.root.join("lock").display().to_string(),
                container_mark: f.root.join("dockerenv").display().to_string(),
                copy_secs: 60,
                lock_wait_secs: 1,
            };
            let reloc = crate::volume::relocation_script(&f.pod.display().to_string(), &site).unwrap();
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
