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

/// Sentinels the backup command prints (followed by the branch) so the caller can tell a
/// push apart from a no-op or a deliberately-skipped protected branch.
pub const BACKUP_PUSHED: &str = "PUSHED";
pub const BACKUP_NO_CHANGES: &str = "NO_CHANGES";
pub const BACKUP_SKIPPED: &str = "SKIP";

/// Render the remote shell command that commits + pushes one machine's working tree **on
/// whatever branch it is currently on** — it never switches or creates a branch, so a
/// participant's bespoke branch is respected (matching legacy `sync_git.sh`, not
/// `init_branches.sh`). To stage work on a dedicated autocommit branch, put the pod there
/// first with `init-branches`/`set-branch`.
///
/// It refuses to push `main`/`master` (or a detached `HEAD`), printing `SKIP <branch>`
/// and exiting 0 — so an automated backup never lands commits on the protected branch.
/// Flow: resolve the current branch; skip if protected; ensure a git identity; stage; if
/// the tree is clean print `NO_CHANGES <branch>` and exit 0; otherwise commit and
/// `push -u origin <branch>`, then print `PUSHED <branch>`. `set -e` aborts on error.
pub fn backup_command(repo_path: &str, git_ssh_key: Option<&str>, commit_msg: &str) -> String {
    let mut parts: Vec<String> =
        vec!["set -e".into(), format!("cd {}", shell_quote(repo_path))];
    if let Some(key) = git_ssh_key {
        parts.push(format!(
            "export GIT_SSH_COMMAND={}",
            shell_quote(&format!(
                "ssh -i {key} -o StrictHostKeyChecking=accept-new -o BatchMode=yes"
            ))
        ));
    }
    // Stay on the current branch; never push a protected/detached one.
    parts.push("B=$(git rev-parse --abbrev-ref HEAD)".into());
    parts.push(format!(
        "if [ \"$B\" = main ] || [ \"$B\" = master ] || [ \"$B\" = HEAD ]; then echo \"{BACKUP_SKIPPED} $B\"; exit 0; fi"
    ));
    // A committer identity, in case the pod has none configured.
    parts.push("git config user.name >/dev/null 2>&1 || git config user.name 'Arena Autocommit'".into());
    parts.push(
        "git config user.email >/dev/null 2>&1 || git config user.email 'autocommit@arena.education'"
            .into(),
    );
    parts.push("git add -A".into());
    parts.push(format!(
        "if git diff --cached --quiet; then echo \"{BACKUP_NO_CHANGES} $B\"; exit 0; fi"
    ));
    parts.push(format!("git commit -m {}", shell_quote(commit_msg)));
    parts.push("git push -u origin \"$B\"".into());
    parts.push(format!("echo \"{BACKUP_PUSHED} $B\""));
    parts.join("; ")
}

/// Classify a backup command's stdout: `(sentinel, branch)`. Returns the matched
/// `BACKUP_*` sentinel and the branch it reported, or `None` if neither appeared
/// (treat that as a failure at the call site).
pub fn parse_backup_output(stdout: &str) -> Option<(&'static str, String)> {
    for line in stdout.lines() {
        let line = line.trim();
        for sentinel in [BACKUP_PUSHED, BACKUP_NO_CHANGES, BACKUP_SKIPPED] {
            if let Some(rest) = line.strip_prefix(sentinel) {
                return Some((sentinel, rest.trim().to_string()));
            }
        }
    }
    None
}

/// Render the remote command that *creates* (or switches to) a machine's autocommit
/// branch for the iteration and pushes it upstream — **without committing any work**
/// (legacy `init_branches.sh`). Run at the start of a day so the branch exists (with an
/// upstream) before any `backup`; later backups then just commit onto it. `set -e`
/// aborts on the first real error.
pub fn init_branch_command(cfg: &BackupConfig, machine_name: &str) -> String {
    let branch = cfg.branch_for(machine_name);
    let bq = shell_quote(&branch);
    let mut parts: Vec<String> =
        vec!["set -e".into(), format!("cd {}", shell_quote(&cfg.repo_path))];
    if let Some(key) = &cfg.git_ssh_key {
        parts.push(format!(
            "export GIT_SSH_COMMAND={}",
            shell_quote(&format!(
                "ssh -i {key} -o StrictHostKeyChecking=accept-new -o BatchMode=yes"
            ))
        ));
    }
    parts.push("git fetch --all --prune".into());
    // Create the branch from the current HEAD, or switch to it if it already exists.
    parts.push(format!("git checkout -b {bq} 2>/dev/null || git checkout {bq}"));
    parts.push(format!("git push -u origin {bq}"));
    parts.join("; ")
}

/// Render the remote command that switches a pod's ARENA checkout to `branch`.
///
/// `hard == false` (default): **gentle** — fetch, checkout, fast-forward pull. A
/// diverged/dirty tree makes the `--ff-only` pull fail loudly rather than clobbering
/// work. `hard == true`: **destructive** — fetch, then force the local branch to exactly
/// match `origin/<branch>` (`checkout -f -B … origin/<branch>` + `reset --hard`),
/// **discarding any local commits/changes on it** (untracked files are left in place).
/// Used by `pods set-branch [--hard]` (e.g. reset everyone back to `main`).
pub fn checkout_command(repo_path: &str, branch: &str, git_ssh_key: Option<&str>, hard: bool) -> String {
    let mut parts: Vec<String> = vec!["set -e".into(), format!("cd {}", shell_quote(repo_path))];
    if let Some(key) = git_ssh_key {
        parts.push(format!(
            "export GIT_SSH_COMMAND={}",
            shell_quote(&format!(
                "ssh -i {key} -o StrictHostKeyChecking=accept-new -o BatchMode=yes"
            ))
        ));
    }
    parts.push("git fetch origin".into());
    if hard {
        let remote_ref = shell_quote(&format!("origin/{branch}"));
        // Force-switch to the branch reset to origin (discards local changes/commits on it).
        parts.push(format!("git checkout -f -B {} {remote_ref}", shell_quote(branch)));
        parts.push(format!("git reset --hard {remote_ref}"));
    } else {
        parts.push(format!("git checkout {}", shell_quote(branch)));
        parts.push("git pull --ff-only".into());
    }
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
    fn backup_stays_on_current_branch_and_protects_main() {
        let c = backup_command("/root/ARENA_3.0", Some("/root/.ssh/id_ed25519"), "arena backup");
        assert!(c.contains("cd '/root/ARENA_3.0'"));
        // Never switches/creates a branch — works on whatever HEAD is on.
        assert!(!c.contains("git checkout"));
        assert!(c.contains("B=$(git rev-parse --abbrev-ref HEAD)"));
        // Refuses to push main/master/detached.
        assert!(c.contains(r#"[ "$B" = main ]"#));
        assert!(c.contains(r#"[ "$B" = master ]"#));
        assert!(c.contains("SKIP $B"));
        // Stage + clean-tree guard + commit + push the *current* branch.
        assert!(c.contains("git add -A"));
        assert!(c.contains("NO_CHANGES $B"));
        assert!(c.contains("git commit -m 'arena backup'"));
        assert!(c.contains(r#"git push -u origin "$B""#));
        assert!(c.contains("PUSHED $B"));
        assert!(c.contains("Arena Autocommit"));
        assert!(c.contains("GIT_SSH_COMMAND='ssh -i /root/.ssh/id_ed25519"));
    }

    #[test]
    fn omits_git_ssh_command_without_key() {
        assert!(!backup_command("/root/ARENA_3.0", None, "m").contains("GIT_SSH_COMMAND"));
    }

    #[test]
    fn escapes_single_quotes_in_message() {
        let c = backup_command("/root/ARENA_3.0", None, "it's a backup");
        assert!(c.contains(r"'it'\''s a backup'"));
    }

    #[test]
    fn parses_backup_sentinels() {
        assert_eq!(parse_backup_output("PUSHED feature-x\n"), Some(("PUSHED", "feature-x".into())));
        assert_eq!(parse_backup_output("NO_CHANGES main"), Some(("NO_CHANGES", "main".into())));
        assert_eq!(parse_backup_output("SKIP master"), Some(("SKIP", "master".into())));
        assert_eq!(parse_backup_output("garbage"), None);
    }

    #[test]
    fn init_branch_creates_and_pushes_without_committing() {
        let mut c = cfg();
        c.week = 1;
        c.day = 4;
        let cmd = init_branch_command(&c, "arena8-apple");
        assert!(cmd.contains("git fetch --all --prune"));
        assert!(cmd.contains("git checkout -b 'autocommit-arena8-w1d4-apple'"));
        assert!(cmd.contains("git push -u origin 'autocommit-arena8-w1d4-apple'"));
        // It only creates/pushes the branch — never stages or commits work.
        assert!(!cmd.contains("git add"));
        assert!(!cmd.contains("git commit"));
    }

    #[test]
    fn checkout_is_gentle_ff_only() {
        let c = checkout_command("/root/ARENA_3.0", "main", Some("/root/.ssh/id_ed25519"), false);
        assert!(c.contains("cd '/root/ARENA_3.0'"));
        assert!(c.contains("git fetch origin"));
        assert!(c.contains("git checkout 'main'"));
        assert!(c.contains("git pull --ff-only")); // no hard reset
        assert!(!c.contains("reset --hard"));
        assert!(c.contains("GIT_SSH_COMMAND='ssh -i /root/.ssh/id_ed25519"));
    }

    #[test]
    fn checkout_hard_resets_to_origin() {
        let c = checkout_command("/root/ARENA_3.0", "main", None, true);
        assert!(c.contains("git fetch origin"));
        // Force-switch the branch to origin and hard-reset (destructive).
        assert!(c.contains("git checkout -f -B 'main' 'origin/main'"));
        assert!(c.contains("git reset --hard 'origin/main'"));
        assert!(!c.contains("--ff-only"));
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
