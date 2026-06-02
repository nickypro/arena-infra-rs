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
            key_local: key_local.to_string(),
            key_remote: cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519").to_string(),
            repo_path,
            repo_url: format!("git@github.com:{owner}/{name}.git"),
            branch: cfg.get("DEFAULT_BRANCH").unwrap_or("main").to_string(),
            prefix: cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string(),
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

        // Add the deploy key's public half to the authorized_keys allow-list, so the
        // same key can SSH into the pod. We derive the public key on the pod from the
        // private key we just copied (`ssh-keygen -y`), append only if not already
        // present, and tolerate a missing key without aborting `set -e`.
        let authorized_keys = format!(
            "PUB=$(ssh-keygen -y -f {key} 2>/dev/null) && touch \"$HOME/.ssh/authorized_keys\" && \
             chmod 600 \"$HOME/.ssh/authorized_keys\" && \
             (grep -qxF \"$PUB\" \"$HOME/.ssh/authorized_keys\" || echo \"$PUB\" >> \"$HOME/.ssh/authorized_keys\")"
        );

        // Branch update: force => checkout default + hard reset; else stay put.
        let git_update = if force {
            format!(
                "git checkout {b} && git reset --hard origin/{b}",
                b = q(branch)
            )
        } else {
            format!(
                "CUR=$(git rev-parse --abbrev-ref HEAD); \
                 if [ \"$CUR\" = {b} ]; then git reset --hard origin/{b}; else git pull; fi",
                b = q(branch)
            )
        };

        [
            "set -e".to_string(),
            format!("chmod 600 {}", q(key)),
            ssh_config,
            authorized_keys,
            format!("cd {}", q(&self.repo_path)),
            format!("git remote set-url origin {}", q(&self.repo_url)),
            "git fetch origin".to_string(),
            git_update,
            "git submodule update --init --recursive".to_string(),
            format!(
                "echo {} > \"$HOME/.name\"",
                q(&format!("export MACHINE_NAME='{}'", self.short_name(machine_name)))
            ),
        ]
        .join("; ")
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
        // deploy key added to the authorized_keys allow-list (idempotently)
        assert!(c.contains("ssh-keygen -y -f /root/.ssh/id_ed25519"));
        assert!(c.contains(r#"grep -qxF "$PUB" "$HOME/.ssh/authorized_keys""#));
        assert!(c.contains(r#"echo "$PUB" >> "$HOME/.ssh/authorized_keys""#));
        // repo wiring
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
}
