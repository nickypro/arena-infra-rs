//! Pod provisioning: make a freshly-created pod ready to commit/back up.
//!
//! Faithful to the legacy `setup_em.sh`. After the caller copies the git deploy key
//! (scp), the on-pod script:
//!   1. `chmod 600` the key,
//!   2. add a `github.com` block to `~/.ssh/config` pointing at it (idempotent),
//!   3. add the deploy key's *public* half to `~/.ssh/authorized_keys` (idempotent),
//!      so anyone holding that key can also SSH into the pod — derived on the pod via
//!      `ssh-keygen -y` from the key we just copied, so no extra file is shipped,
//!   4. point the ARENA repo's `origin` at the GitHub SSH URL, fetch, and update
//!      (default: stay on the current branch, pull / reset-if-on-main; `--force`:
//!      check out the default branch and `reset --hard`), then update submodules,
//!   5. write `~/.name` as `export MACHINE_NAME='<short>'`.
//!
//! Renders the on-pod shell command (pure, unit-tested); scp + execution are in the
//! caller, gated behind `--apply`.

use crate::config::Config;
use crate::error::{Error, Result};

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
        })
    }

    /// The short machine name (the part after `{prefix}-`), e.g. arena8-apple -> apple.
    pub fn short_name<'a>(&self, machine_name: &'a str) -> &'a str {
        machine_name
            .strip_prefix(&format!("{}-", self.prefix))
            .unwrap_or(machine_name)
    }

    /// The on-pod provisioning command, run *after* the key has been scp'd. `force`
    /// mirrors `setup_em.sh --force` (hard-reset onto the default branch).
    pub fn remote_command(&self, machine_name: &str, force: bool) -> String {
        let q = shell_quote;
        let key = &self.key_remote;
        let branch = &self.branch;

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

        // Branch update: force => checkout default + hard reset; else stay put.
        let git_update = if force {
            format!(
                "git checkout {b} && git reset --hard origin/{b}",
                b = q(branch)
            )
        } else {
            // Stay on the current branch. On the default branch, hard-reset to origin;
            // otherwise (e.g. an autocommit-wNdM branch with no upstream) only fast-
            // forward if it actually tracks a remote — never fail the whole setup.
            format!(
                "CUR=$(git rev-parse --abbrev-ref HEAD); \
                 if [ \"$CUR\" = {b} ]; then git reset --hard origin/{b}; \
                 else (git rev-parse '@{{u}}' >/dev/null 2>&1 && git pull --ff-only) || true; fi",
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
        let repo_block = format!(
            "if [ -d {repo}/.git ]; then cd {repo}; git remote set-url origin {url}; \
             git fetch origin; {git_update}; git submodule update --init --recursive; \
             else echo {skip} >&2; fi",
            repo = q(&self.repo_path),
            url = q(&self.repo_url),
            skip = q(&format!(
                "arena setup: {} not present — skipping repo update (non-arena image)",
                self.repo_path
            )),
        );

        let mut steps = vec![
            "set -e".to_string(),
            format!("chmod 600 {}", q(key)),
            ssh_config,
            authorized_keys,
            repo_block,
            format!(
                "echo {} > \"$HOME/.name\"",
                q(&format!("export MACHINE_NAME='{}'", self.short_name(machine_name)))
            ),
        ];
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
}

/// Single-quote for safe inclusion in a `sh -c` string (POSIX `'\''` escaping).
fn shell_quote(s: &str) -> String {
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
        }
    }

    #[test]
    fn short_name_strips_prefix() {
        assert_eq!(cfg().short_name("arena8-apple"), "apple");
        assert_eq!(cfg().short_name("apple"), "apple");
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
        assert!(c.contains("git fetch origin"));
        assert!(c.contains("git submodule update --init --recursive"));
        // .name uses the EXPORT form with the SHORT name
        assert!(c.contains(r#"echo 'export MACHINE_NAME='\''apple'\''' > "$HOME/.name""#));
        // non-force stays on current branch
        assert!(c.contains("git rev-parse --abbrev-ref HEAD"));
        assert!(!c.contains("git checkout 'main'"));
    }

    #[test]
    fn force_hard_resets_to_default_branch() {
        let c = cfg().remote_command("arena8-apple", true);
        assert!(c.contains("git checkout 'main' && git reset --hard origin/'main'"));
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
}
