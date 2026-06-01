//! Pod provisioning: make a freshly-created pod ready to commit/back up.
//!
//! Mirrors the manual steps the legacy tooling does after a pod comes up:
//!   1. copy the ARENA git deploy key onto the pod (done by the caller via scp),
//!   2. `chmod 600` it and write the machine name to `~/.name`,
//!   3. point the repo's `origin` at the GitHub SSH URL and check out the branch.
//!
//! This module renders the on-pod shell command (pure, unit-tested). The key copy
//! (scp) and execution live in the caller, gated behind `--apply`.

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
    /// Branch to check out.
    pub branch: String,
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
        })
    }

    /// The shell command to run on the pod *after* the key has been copied: lock down
    /// the key, record the machine name in `~/.name`, and point the repo at GitHub on
    /// the right branch. `set -e` aborts on the first failure.
    pub fn remote_command(&self, machine_name: &str) -> String {
        let q = shell_quote;
        [
            "set -e".to_string(),
            format!("chmod 600 {}", q(&self.key_remote)),
            format!("echo {} > \"$HOME/.name\"", q(machine_name)),
            format!("cd {}", q(&self.repo_path)),
            format!(
                "export GIT_SSH_COMMAND={}",
                q(&format!(
                    "ssh -i {} -o StrictHostKeyChecking=accept-new -o BatchMode=yes",
                    self.key_remote
                ))
            ),
            format!("git remote set-url origin {}", q(&self.repo_url)),
            "git fetch origin".to_string(),
            format!("git checkout {}", q(&self.branch)),
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
        }
    }

    #[test]
    fn renders_provisioning_steps_in_order() {
        let c = cfg().remote_command("arena8-apple");
        assert!(c.contains("chmod 600 '/root/.ssh/id_ed25519'"));
        assert!(c.contains("echo 'arena8-apple' > \"$HOME/.name\""));
        assert!(c.contains("cd '/root/ARENA_3.0'"));
        assert!(c.contains("git remote set-url origin 'git@github.com:styme3279/ARENA_3.0.git'"));
        assert!(c.contains("git checkout 'main'"));
        // key lockdown happens before the git work
        assert!(c.find("chmod 600").unwrap() < c.find("git checkout").unwrap());
    }

    #[test]
    fn quotes_machine_name_safely() {
        // a hostile name can't break out of the echo
        let c = cfg().remote_command("a'; rm -rf /; echo '");
        assert!(c.contains(r"echo 'a'\''; rm -rf /; echo '\'''"));
    }
}
