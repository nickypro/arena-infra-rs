//! Commit/backup flow: snapshot each pod's ARENA working tree to git.
//!
//! The remote command this builds runs *inside* a pod over SSH: stage everything,
//! and — only if there's something to commit — commit it and push to a **per-machine
//! branch** (`backup/<machine>`). Pushing to a dedicated branch per machine is the
//! safe default: participants' work is backed up without ever racing each other on
//! `main`. The repo path, branch prefix, and push key are all config-driven.
//!
//! This module only *renders* the command (pure, unit-tested). Running it — and thus
//! mutating remote git state — is gated behind the CLI's `--apply`.

use crate::config::Config;

/// Config for the backup flow, from `BACKUP_*` / repo keys in `config.env`.
#[derive(Debug, Clone)]
pub struct BackupConfig {
    /// Absolute path to the ARENA checkout on the pod.
    pub repo_path: String,
    /// Machine-name prefix / ARENA iteration label (e.g. "arena8").
    pub prefix: String,
    /// Iteration week (the start date is week 0) and day-within-week (1-based).
    pub week: u32,
    pub day: u32,
    /// SSH key on the pod to authenticate the `git push` (`GIT_SSH_KEY_REMOTE`).
    pub git_ssh_key: Option<String>,
}

impl BackupConfig {
    /// Build from config plus the already-computed iteration `week`/`day`.
    pub fn from_config(cfg: &Config, week: u32, day: u32) -> Self {
        // Default the repo path to /root/<ARENA_REPO_NAME>, matching the legacy layout.
        let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
            let name = cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_3.0");
            format!("/root/{name}")
        });
        Self {
            repo_path,
            prefix: cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string(),
            week,
            day,
            git_ssh_key: cfg.get("GIT_SSH_KEY_REMOTE").map(String::from),
        }
    }

    /// The autocommit branch a machine backs up to, following the ARENA convention:
    /// `autocommit-{prefix}-w{week}d{day}-{machine}` (machine = name minus the
    /// `{prefix}-` part), e.g. `autocommit-arena8-w0d1-apple`.
    pub fn branch_for(&self, machine_name: &str) -> String {
        let short = machine_name
            .strip_prefix(&format!("{}-", self.prefix))
            .unwrap_or(machine_name);
        format!("autocommit-{}-w{}d{}-{}", self.prefix, self.week, self.day, short)
    }
}

/// Render the remote shell command that backs up one machine's working tree.
///
/// It stages all changes and, if any are staged, commits and pushes to the machine's
/// backup branch; if the tree is clean it prints `NO_CHANGES` and exits 0 so a no-op
/// backup is distinguishable from a failure. `set -e` aborts on the first real error.
pub fn backup_command(cfg: &BackupConfig, machine_name: &str, commit_msg: &str) -> String {
    let branch = cfg.branch_for(machine_name);
    let mut parts: Vec<String> = vec![
        "set -e".into(),
        format!("cd {}", shell_quote(&cfg.repo_path)),
    ];
    if let Some(key) = &cfg.git_ssh_key {
        parts.push(format!(
            "export GIT_SSH_COMMAND={}",
            shell_quote(&format!(
                "ssh -i {key} -o StrictHostKeyChecking=accept-new -o BatchMode=yes"
            ))
        ));
    }
    parts.push("git add -A".into());
    // Nothing staged -> clean tree -> report and stop, not an error.
    parts.push("if git diff --cached --quiet; then echo NO_CHANGES; exit 0; fi".into());
    parts.push(format!("git commit -m {}", shell_quote(commit_msg)));
    parts.push(format!("git push origin HEAD:refs/heads/{}", shell_quote(&branch)));
    parts.join("; ")
}

/// Wrap a value in single quotes for safe inclusion in a `sh -c` string, escaping any
/// embedded single quotes the POSIX way (`'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BackupConfig {
        BackupConfig {
            repo_path: "/root/ARENA_3.0".into(),
            prefix: "arena8".into(),
            week: 0,
            day: 1,
            git_ssh_key: Some("/root/.ssh/id_ed25519".into()),
        }
    }

    #[test]
    fn renders_safe_per_machine_backup() {
        let c = backup_command(&cfg(), "arena8-apple", "arena backup");
        assert!(c.contains("cd '/root/ARENA_3.0'"));
        assert!(c.contains("git add -A"));
        // Clean-tree guard so a no-op isn't treated as failure.
        assert!(c.contains("echo NO_CHANGES"));
        assert!(c.contains("git commit -m 'arena backup'"));
        // Autocommit branch, never main.
        assert!(c.contains("HEAD:refs/heads/'autocommit-arena8-w0d1-apple'"));
        assert!(!c.contains("refs/heads/main"));
        // Uses the configured push key.
        assert!(c.contains("GIT_SSH_COMMAND='ssh -i /root/.ssh/id_ed25519"));
    }

    #[test]
    fn omits_git_ssh_command_without_key() {
        let mut c = cfg();
        c.git_ssh_key = None;
        assert!(!backup_command(&c, "arena8-apple", "m").contains("GIT_SSH_COMMAND"));
    }

    #[test]
    fn escapes_single_quotes_in_message() {
        let c = backup_command(&cfg(), "arena8-apple", "it's a backup");
        assert!(c.contains(r"'it'\''s a backup'"));
    }

    #[test]
    fn branch_follows_autocommit_convention_with_short_name() {
        let mut c = cfg();
        c.week = 1;
        c.day = 2;
        // prefix is stripped to the short machine name
        assert_eq!(c.branch_for("arena8-luna"), "autocommit-arena8-w1d2-luna");
        // a name without the prefix is used as-is
        assert_eq!(c.branch_for("luna"), "autocommit-arena8-w1d2-luna");
    }
}
