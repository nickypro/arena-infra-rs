//! Commit/backup flow: snapshot each pod's ARENA working tree to git.
//!
//! The remote command this builds runs *inside* a pod over SSH: stage everything,
//! and — only if there's something to commit — commit it and push to a **per-machine
//! branch** (`backup/<machine>`). Pushing to a dedicated branch per machine is the
//! safe default: participants' work is backed up without ever racing each other on
//! `main`. The repo path, branch prefix, and push key are all config-driven.
//!
//! Two things never go into those commits, because each once stopped participants' work from
//! reaching GitHub: a file over [`GIT_MAX_FILE_BYTES`] (GitHub refuses the push — then every
//! push after it), and a nested repo as a gitlink (a commit id without its files). The image's
//! nested clones are adopted instead — the course repo tracks their files where they are, their
//! own `.git` untouched — once; other nested repos are kept out and reported. The script is
//! [`BACKUP_SCRIPT`].
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

/// Where the ARENA checkout — the participants' work — lives on a pod: config
/// `BACKUP_REPO_PATH`, else `/root/<ARENA_REPO_NAME>` (default `ARENA_materials`), matching
/// the legacy layout. Note the default is on the container disk, *not* a `/workspace`
/// volume — which is why a restart/stop gate can't take "the pod has a volume" to mean
/// "the work survives".
pub fn repo_path(cfg: &Config) -> String {
    cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        let name = cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials");
        format!("/root/{name}")
    })
}

impl BackupConfig {
    /// Build from config plus the already-computed iteration `week`/`day`.
    pub fn from_config(cfg: &Config, week: u32, day: u32) -> Self {
        Self {
            repo_path: repo_path(cfg),
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

/// The largest file the git backup commits. GitHub refuses a push carrying any file over
/// [`GITHUB_FILE_LIMIT_BYTES`] (`GH001 … pre-receive hook declined`) — and then every later
/// push of the branch, since the blob stays in the unpushed history even after the file is
/// deleted (live: one 110 MB checkpoint stopped a pod's git backup for good). Files above this
/// (with headroom under the limit) are left out of the commit and reported; the rsync big tier
/// keeps them.
pub const GIT_MAX_FILE_BYTES: u64 = 95 * 1024 * 1024;

/// GitHub's own per-file limit (100 MiB): a push carrying a bigger blob is refused. What the
/// backup judges a branch *stuck* by ([`BACKUP_STUCK_EXIT`]) — never [`GIT_MAX_FILE_BYTES`]: a
/// file between the two, committed by hand, is one GitHub takes, so its push must go ahead.
pub const GITHUB_FILE_LIMIT_BYTES: u64 = 100 * 1024 * 1024;

/// The exit status of a backup that found a file over GitHub's limit already in the branch's
/// unpushed commits — no push can succeed until they're rewritten ([`BackupReport::stuck_fix`]).
pub const BACKUP_STUCK_EXIT: i32 = 3;

/// The backup script ([`backup_command`]). POSIX `sh`, so it runs the same whatever the pod's
/// login shell; its header documents every line it prints.
pub const BACKUP_SCRIPT: &str = include_str!("backup.sh");

/// Render the remote shell command that commits + pushes one machine's working tree **on
/// whatever branch it is currently on** — it never switches or creates a branch, so a
/// participant's bespoke branch is respected (matching legacy `sync_git.sh`, not
/// `init_branches.sh`). To stage work on a dedicated autocommit branch, put the pod there
/// first with `init-branches`/`set-branch`. It runs [`BACKUP_SCRIPT`] under `sh` with the
/// repo, message, size limits ([`GIT_MAX_FILE_BYTES`], [`GITHUB_FILE_LIMIT_BYTES`]) and push key
/// as arguments.
///
/// It refuses to push `main`/`master` (or a detached `HEAD`), printing `SKIP <branch>`
/// and exiting 0 — so an automated backup never lands commits on the protected branch.
/// Flow: resolve the current branch; skip if protected; ensure a git identity; adopt the
/// image's nested clones into the repo once (their files tracked in place, their own `.git`
/// untouched), keep any other nested repo out of the commit (never a gitlink); stage everything but files over the limit; commit if anything is
/// staged; then push whenever the branch has **no upstream yet or is ahead of it** — not
/// only when this run committed. A push that failed last tick (network blip, GitHub outage,
/// a key briefly missing) leaves its commit local and the tree clean, so "nothing staged"
/// must not read as "nothing to do": the next tick retries the push. Only a clean tree *and*
/// `HEAD == @{u}` prints `NO_CHANGES <branch>` (exit 0); a push prints `PUSHED <branch>`. A
/// file over GitHub's limit already in the unpushed commits fails it with
/// [`BACKUP_STUCK_EXIT`] and what to reset to — before a push GitHub would refuse.
/// `set -e` aborts on any other error, so a rejected push is a failure.
pub fn backup_command(repo_path: &str, git_ssh_key: Option<&str>, commit_msg: &str) -> String {
    backup_command_limited(repo_path, git_ssh_key, commit_msg, GIT_MAX_FILE_BYTES, GITHUB_FILE_LIMIT_BYTES)
}

/// [`backup_command`] with the size limits as parameters — the largest file committed, and the
/// largest a push may carry — so tests simulate GitHub's limit with a few-byte one.
pub fn backup_command_limited(
    repo_path: &str,
    git_ssh_key: Option<&str>,
    commit_msg: &str,
    max_file_bytes: u64,
    push_limit_bytes: u64,
) -> String {
    format!("sh -c {} arena-backup {}", shell_quote(BACKUP_SCRIPT), script_args(repo_path, git_ssh_key, commit_msg, max_file_bytes, push_limit_bytes))
}

/// [`BACKUP_SCRIPT`]'s arguments, shell-quoted.
fn script_args(repo_path: &str, git_ssh_key: Option<&str>, commit_msg: &str, max_file_bytes: u64, push_limit_bytes: u64) -> String {
    format!(
        "{} {} {max_file_bytes} {push_limit_bytes} {}",
        shell_quote(repo_path),
        shell_quote(commit_msg),
        shell_quote(git_ssh_key.unwrap_or("")),
    )
}

/// The file a dry run's per-pod line reads [`BACKUP_SCRIPT`] from ([`backup_dry_run_line`]);
/// [`backup_script_heredoc`] writes it.
pub const BACKUP_SCRIPT_FILE: &str = "arena-backup.sh";

/// [`backup_command`] as the remote half of a pasteable line: the same script and arguments,
/// the script read from stdin (`sh -s`) instead of carried in the command — for a dry run's
/// per-pod line and the TUI's confirm modal, which show the arguments without the script's
/// ~150 lines.
pub fn backup_summary(repo_path: &str, git_ssh_key: Option<&str>, commit_msg: &str) -> String {
    format!("sh -s {}", script_args(repo_path, git_ssh_key, commit_msg, GIT_MAX_FILE_BYTES, GITHUB_FILE_LIMIT_BYTES))
}

/// One pod's backup as a dry run prints it: `ssh … 'sh -s <args>' < arena-backup.sh`. Pasted
/// into a shell next to the file ([`backup_script_heredoc`] writes it), it runs exactly what the
/// backup runs; without the file the local shell refuses to start it at all. (It used to show
/// the script as `"$ARENA_BACKUP_SH"`, which the pod's shell expanded to nothing: `sh -c ""`
/// did nothing and exited 0 — a paste that looked like a successful backup.)
pub fn backup_dry_run_line(target: &crate::ssh::SshTarget, repo_path: &str, git_ssh_key: Option<&str>, commit_msg: &str) -> String {
    format!("{} < {BACKUP_SCRIPT_FILE}", target.display_command(&backup_summary(repo_path, git_ssh_key, commit_msg)))
}

/// The here-doc that writes [`BACKUP_SCRIPT`] to [`BACKUP_SCRIPT_FILE`] — pasteable, printed once
/// by a dry run for its per-pod lines ([`backup_dry_run_line`]).
pub fn backup_script_heredoc() -> String {
    format!("cat > {BACKUP_SCRIPT_FILE} <<'{HEREDOC_END}'\n{}\n{HEREDOC_END}", BACKUP_SCRIPT.trim_end())
}

/// [`backup_script_heredoc`]'s delimiter: a line the script never has.
const HEREDOC_END: &str = "ARENA_BACKUP_SH_END";

/// Classify a backup command's stdout: `(sentinel, branch)`. Returns the matched
/// `BACKUP_*` sentinel and the branch it reported, or `None` if neither appeared
/// (treat that as a failure at the call site).
pub fn parse_backup_output(stdout: &str) -> Option<(&'static str, String)> {
    parse_backup_report(stdout).outcome
}

/// Everything a backup run said ([`BACKUP_SCRIPT`]'s lines).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupReport {
    /// The sentinel and the branch it named (`None`: it never got that far).
    pub outcome: Option<(&'static str, String)>,
    /// Files over the limit left out of this commit: `(bytes, path)`.
    pub big_skipped: Vec<(u64, String)>,
    /// Files over GitHub's limit already in the unpushed commits: `(bytes, path)`.
    pub big_stuck: Vec<(u64, String)>,
    /// What to `git reset` to, to rewrite those commits without them (`None`: nothing to say,
    /// or no pushed commit to go back to).
    pub stuck_base: Option<String>,
    /// Nested clones adopted this run: the repo tracks their files now (their `.git` untouched).
    pub adopted: Vec<String>,
    /// Nested repos left as they are and kept out of the commit.
    pub kept: Vec<String>,
}

/// Parse a backup's stdout ([`BACKUP_SCRIPT`]'s header lists the lines). Unknown lines (git's
/// own chatter) are ignored.
pub fn parse_backup_report(stdout: &str) -> BackupReport {
    let mut r = BackupReport::default();
    let sized = |rest: &str| {
        let (n, path) = rest.split_once(' ')?;
        Some((n.parse::<u64>().ok()?, path.to_string()))
    };
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "BIGSKIP" => r.big_skipped.extend(sized(rest)),
            "BIGSTUCK" => r.big_stuck.extend(sized(rest)),
            "BIGSTUCK_BASE" => r.stuck_base = Some(rest.trim()).filter(|b| !b.is_empty() && *b != "none").map(String::from),
            "NESTED_ADOPTED" if !rest.is_empty() => r.adopted.push(rest.to_string()),
            "NESTED_KEPT" if !rest.is_empty() => r.kept.push(rest.to_string()),
            _ if r.outcome.is_none() => {
                for sentinel in [BACKUP_PUSHED, BACKUP_NO_CHANGES, BACKUP_SKIPPED] {
                    if word == sentinel {
                        r.outcome = Some((sentinel, rest.trim().to_string()));
                    }
                }
            }
            _ => {}
        }
    }
    r
}

/// Bytes as `110.0 MB` (MiB, as GitHub reports file sizes).
fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// `a, b, c` — at most `max` of them, then `(+N more)`.
fn some_of(items: &[String], max: usize) -> String {
    let mut s = items.iter().take(max).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > max {
        s.push_str(&format!(" (+{} more)", items.len() - max));
    }
    s
}

impl BackupReport {
    /// What a run that worked has to say besides its outcome: files kept out of git, nested
    /// repos adopted or kept out. One phrase each; empty when there's nothing.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if !self.big_skipped.is_empty() {
            let files: Vec<String> = self.big_skipped.iter().map(|(n, p)| format!("{p} ({})", mb(*n))).collect();
            notes.push(format!(
                "skipped {} file(s) > {} MB: {} — GitHub refuses them; covered by the rsync big tier (`pods pull`)",
                files.len(),
                GIT_MAX_FILE_BYTES / (1024 * 1024),
                some_of(&files, 3)
            ));
        }
        if !self.adopted.is_empty() {
            notes.push(format!(
                "now tracking the files of {} nested clone(s) in the repo (their own .git left as it is): {}",
                self.adopted.len(),
                some_of(&self.adopted, 4)
            ));
        }
        if !self.kept.is_empty() {
            notes.push(format!(
                "{} nested repo(s) with their own history kept out of git (the rsync tiers have them): {}",
                self.kept.len(),
                some_of(&self.kept, 4)
            ));
        }
        notes
    }

    /// For a run that found files over GitHub's limit in the unpushed commits: why nothing can be
    /// pushed, and the exact commands that fix it — rewrite those commits without the files
    /// (they stay on disk), then back up again. `None` for any other run.
    pub fn stuck_fix(&self, repo: &str, pod: &str) -> Option<String> {
        if self.big_stuck.is_empty() {
            return None;
        }
        let files: Vec<String> = self.big_stuck.iter().map(|(n, p)| format!("{p} ({})", mb(*n))).collect();
        let why = format!(
            "{} file(s) over {} MB in unpushed commits — GitHub refuses every push of the branch until they're rewritten: {}",
            files.len(),
            GITHUB_FILE_LIMIT_BYTES / (1024 * 1024),
            some_of(&files, 3)
        );
        Some(match &self.stuck_base {
            Some(base) => format!(
                "{why}. Fix (keeps every file on disk; the unpushed commits become one, without the big file(s)): \
                 `arena pods run -t {pod} {}`, then `arena pods backup {pod} --no-pull`",
                shell_quote(&format!("git -C {} reset -q {base}", crate::ssh::shell_word(repo)))
            ),
            None => format!("{why}. No pushed commit to reset to: rewrite the branch's history on {pod} by hand"),
        })
    }
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
///
/// A nested clone the backup adopted (its files tracked by the course repo) is tracked on the
/// autocommit branches only — the course's `main` doesn't have it — so switching to such a
/// branch would delete its files from disk and break that exercise. After the switch, each
/// adopted clone the old HEAD had and the new one doesn't is put back from the old HEAD
/// (`RESTORED <path>` on stdout), **staged**: as untracked files they would block a switch
/// back to a branch that tracks them ("untracked working tree files would be overwritten"),
/// staged they ride along with it. A backup never commits on `main`, and the next
/// `init-branches` branch commits them.
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
    parts.push("OLD=$(git rev-parse -q --verify HEAD) || OLD=".into());
    if hard {
        let remote_ref = shell_quote(&format!("origin/{branch}"));
        // Force-switch to the branch reset to origin (discards local changes/commits on it).
        parts.push(format!("git checkout -f -B {} {remote_ref}", shell_quote(branch)));
        parts.push(format!("git reset --hard {remote_ref}"));
    } else {
        parts.push(format!("git checkout {}", shell_quote(branch)));
        parts.push("git pull --ff-only".into());
    }
    // The adopted clones (listed by the backup in `$G/arena-nested`) the switch removed: back
    // from the old HEAD, staged.
    parts.push(
        "G=$(git rev-parse --absolute-git-dir); if [ -n \"$OLD\" ] && [ -s \"$G/arena-nested\" ]; then \
         while IFS= read -r d; do if [ -n \"$d\" ] && git cat-file -e \"$OLD:$d\" 2>/dev/null && \
         ! git cat-file -e \"HEAD:$d\" 2>/dev/null; then git checkout -q \"$OLD\" -- \":(literal)$d\" && \
         echo \"RESTORED $d\"; fi; done < \"$G/arena-nested\"; fi"
            .into(),
    );
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
    fn repo_path_is_the_config_path_else_root_repo_name() {
        let cases = [
            ("BACKUP_REPO_PATH=/workspace/ARENA\nARENA_REPO_NAME=r\n", "/workspace/ARENA"),
            ("ARENA_REPO_NAME=r\n", "/root/r"),
            ("", "/root/ARENA_materials"),
        ];
        for (text, want) in cases {
            assert_eq!(repo_path(&Config::parse(text)), want, "{text:?}");
        }
    }

    #[test]
    fn backup_stays_on_current_branch_and_protects_main() {
        let c = backup_command("/root/ARENA_3.0", Some("/root/.ssh/id_ed25519"), "arena backup");
        // The script under sh (whatever the login shell), its arguments exact.
        assert!(c.starts_with("sh -c '"), "{c}");
        assert!(
            c.ends_with(&format!(
                " arena-backup '/root/ARENA_3.0' 'arena backup' {GIT_MAX_FILE_BYTES} {GITHUB_FILE_LIMIT_BYTES} '/root/.ssh/id_ed25519'"
            )),
            "{c}"
        );
        let s = BACKUP_SCRIPT;
        // Never switches/creates a branch — works on whatever HEAD is on.
        assert!(!s.contains("git checkout"));
        assert!(s.contains("B=$(git rev-parse --abbrev-ref HEAD)"));
        // Refuses to push main/master/detached.
        assert!(s.contains(r#"[ "$B" = main ]"#) && s.contains(r#"[ "$B" = master ]"#) && s.contains("SKIP $B"));
        // Stage + clean-tree guard + commit + push the *current* branch.
        assert!(s.contains(r#"git add -A -- "$@""#));
        assert!(s.contains("NO_CHANGES $B"));
        assert!(s.contains(r#"git commit -q -m "$MSG""#));
        // "Nothing staged" isn't "nothing to do": push whenever ahead of / without an upstream.
        assert!(s.contains("git rev-list --count '@{u}..HEAD'"));
        assert!(s.contains(r#"git push -u origin "$B""#) && s.contains("PUSHED $B"));
        assert!(s.contains("Arena Autocommit"));
        assert!(s.contains(r#"export GIT_SSH_COMMAND="ssh -i $KEY"#));
        assert_eq!(GIT_MAX_FILE_BYTES, 99_614_720, "95 MiB: headroom under GitHub's 100 MiB");
        assert_eq!(GITHUB_FILE_LIMIT_BYTES, 104_857_600, "GitHub's GH001 limit");
    }

    #[test]
    fn omits_git_ssh_command_without_key() {
        // An empty key argument: the script exports no GIT_SSH_COMMAND for it.
        assert!(backup_command("/root/ARENA_3.0", None, "m").ends_with(" 'm' 99614720 104857600 ''"));
        assert!(BACKUP_SCRIPT.contains(r#"if [ -n "$KEY" ]; then"#));
    }

    #[test]
    fn the_script_is_posix_sh() {
        for shell in ["sh", "bash"] {
            let o = std::process::Command::new(shell).arg("-n").arg("-c").arg(BACKUP_SCRIPT).output().expect(shell);
            assert!(o.status.success(), "{shell} -n: {}", String::from_utf8_lossy(&o.stderr));
        }
    }

    #[test]
    fn the_report_reads_every_line_and_says_what_to_do() {
        let out = "NESTED_ADOPTED chapter4/exercises/shutdown_avoidance\nNESTED_ADOPTED Grokking\n\
                   NESTED_KEPT projects/my repo\nBIGSKIP 115343360 exercises/devtest_ckpt.pt\n\
                   [autocommit-x 1a2b3c] arena backup\nPUSHED autocommit-x\n";
        let r = parse_backup_report(out);
        assert_eq!(r.outcome, Some((BACKUP_PUSHED, "autocommit-x".into())));
        assert_eq!(r.adopted, ["chapter4/exercises/shutdown_avoidance", "Grokking"]);
        assert_eq!(r.kept, ["projects/my repo"]);
        assert_eq!(r.big_skipped, [(115_343_360, "exercises/devtest_ckpt.pt".to_string())]);
        assert_eq!(r.stuck_fix("/root/ARENA_materials", "devtest-bravo"), None);
        assert_eq!(
            r.notes(),
            [
                "skipped 1 file(s) > 95 MB: exercises/devtest_ckpt.pt (110.0 MB) — GitHub refuses them; covered by the rsync big tier (`pods pull`)",
                "now tracking the files of 2 nested clone(s) in the repo (their own .git left as it is): \
                 chapter4/exercises/shutdown_avoidance, Grokking",
                "1 nested repo(s) with their own history kept out of git (the rsync tiers have them): projects/my repo",
            ]
        );
        // A branch stuck with a big blob: the exact fix, with the pod and the reset target.
        let stuck = parse_backup_report("BIGSTUCK 115343360 exercises/devtest_ckpt.pt\nBIGSTUCK_BASE 65ea2a3f\n");
        assert_eq!(stuck.outcome, None);
        let fix = stuck.stuck_fix("/root/ARENA_materials", "devtest-bravo").unwrap();
        assert!(fix.starts_with("1 file(s) over 100 MB in unpushed commits — GitHub refuses every push"), "{fix}");
        assert!(fix.contains("exercises/devtest_ckpt.pt (110.0 MB)"), "{fix}");
        assert!(
            fix.ends_with(
                "`arena pods run -t devtest-bravo 'git -C /root/ARENA_materials reset -q 65ea2a3f'`, then \
                 `arena pods backup devtest-bravo --no-pull`"
            ),
            "{fix}"
        );
        // A path that needs quoting stays one argument inside the quoted command.
        let spaced = stuck.stuck_fix("/root/my repo", "devtest-bravo").unwrap();
        assert!(spaced.contains("'git -C '\\''/root/my repo'\\'' reset -q 65ea2a3f'"), "{spaced}");
        let none = parse_backup_report("BIGSTUCK 9 a\nBIGSTUCK_BASE none\n");
        assert!(none.stuck_fix("/r", "p").unwrap().contains("by hand"));
        // Old sentinels still parse; a look-alike word doesn't.
        assert_eq!(parse_backup_output("SKIP main"), Some((BACKUP_SKIPPED, "main".into())));
        assert_eq!(parse_backup_output("SKIPPED x\nBIGSKIP 1 a"), None);
    }

    /// A real repo and a local bare "origin" whose pre-receive hook refuses any blob over
    /// `limit` bytes — GitHub's GH001, scaled down — for the backup command to run against.
    /// The backup keeps files over `skip` out of its commits (`limit` unless a test sets it
    /// lower, as [`GIT_MAX_FILE_BYTES`] is) and judges a branch stuck by `limit`.
    #[cfg(unix)]
    struct GitSandbox {
        base: std::path::PathBuf,
        origin: std::path::PathBuf,
        work: std::path::PathBuf,
        limit: u64,
        skip: u64,
    }

    #[cfg(unix)]
    impl GitSandbox {
        /// `work` on branch `autocommit-x`, pushed (with an upstream), from a `main` that is
        /// on origin too; `data.bin` (10 bytes) tracked.
        fn new(tag: &str, limit: u64) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let base = std::env::temp_dir().join(format!("arena-backup-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            let s = GitSandbox { origin: base.join("origin.git"), work: base.join("work"), base, limit, skip: limit };
            s.git(&s.base, &["init", "-q", "--bare", s.origin.to_str().unwrap()]);
            let hook = s.origin.join("hooks/pre-receive");
            std::fs::write(
                &hook,
                format!(
                    "#!/bin/sh\nwhile read old new ref; do\n  [ \"$new\" = 0000000000000000000000000000000000000000 ] && continue\n  \
                     git rev-list --objects \"$new\" --not --all | git cat-file --batch-check='%(objecttype) %(objectsize) %(rest)' | \
                     awk -v max={limit} '$1 == \"blob\" && $2 > max {{ print \"remote: error: File \" $3 \" is \" $2 \" bytes; this exceeds the file size limit (GH001)\"; bad = 1 }} END {{ exit bad }}' >&2 || exit 1\ndone\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            s.git(&s.base, &["init", "-q", "-b", "main", s.work.to_str().unwrap()]);
            s.git(&s.work, &["config", "user.name", "t"]);
            s.git(&s.work, &["config", "user.email", "t@t"]);
            s.put("data.bin", 10);
            s.put("README.md", 5);
            s.git(&s.work, &["add", "-A"]);
            s.git(&s.work, &["commit", "-q", "-m", "init"]);
            s.git(&s.work, &["remote", "add", "origin", s.origin.to_str().unwrap()]);
            s.git(&s.work, &["push", "-q", "-u", "origin", "main"]);
            s.git(&s.work, &["checkout", "-q", "-b", "autocommit-x"]);
            s.git(&s.work, &["push", "-q", "-u", "origin", "autocommit-x"]);
            s
        }

        fn git(&self, dir: &std::path::Path, args: &[&str]) -> String {
            let o = std::process::Command::new("git").current_dir(dir).args(args).output().expect("git");
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }

        /// A file of `n` bytes (distinct content per path) in the work tree.
        fn put(&self, rel: &str, n: usize) {
            let p = self.work.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let seed = rel.bytes().fold(7u8, |a, b| a.wrapping_mul(31).wrapping_add(b));
            std::fs::write(p, (0..n).map(|i| seed.wrapping_add(i as u8)).collect::<Vec<u8>>()).unwrap();
        }

        /// One backup run: (exit code, report, stderr).
        fn backup(&self) -> (Option<i32>, BackupReport, String) {
            let cmd = backup_command_limited(self.work.to_str().unwrap(), None, "arena backup", self.skip, self.limit);
            let o = std::process::Command::new("bash").arg("-c").arg(cmd).output().expect("bash");
            (o.status.code(), parse_backup_report(&String::from_utf8_lossy(&o.stdout)), String::from_utf8_lossy(&o.stderr).into_owned())
        }

        /// What origin's `branch` holds: `(mode, path)` of every entry.
        fn origin_tree(&self, branch: &str) -> Vec<(String, String)> {
            let out = self.git(&self.base, &["--git-dir", self.origin.to_str().unwrap(), "ls-tree", "-r", branch]);
            out.lines()
                .map(|l| {
                    let (meta, path) = l.split_once('\t').unwrap();
                    (meta.split(' ').next().unwrap().to_string(), path.to_string())
                })
                .collect()
        }

        /// An upstream repo (one commit, `u.py`) for clones the image's way.
        fn upstream(&self) -> std::path::PathBuf {
            let upstream = self.base.join("upstream");
            if !upstream.exists() {
                self.git(&self.base, &["init", "-q", "-b", "main", upstream.to_str().unwrap()]);
                std::fs::write(upstream.join("u.py"), "upstream\n").unwrap();
                self.git(&upstream, &["-c", "user.name=u", "-c", "user.email=u@u", "add", "-A"]);
                self.git(&upstream, &["-c", "user.name=u", "-c", "user.email=u@u", "commit", "-q", "-m", "u"]);
            }
            upstream
        }

        /// A clone of [`Self::upstream`] at `rel` in the work tree, untracked.
        fn clone_upstream(&self, rel: &str) -> std::path::PathBuf {
            let clone = self.work.join(rel);
            self.git(&self.base, &["clone", "-q", self.upstream().to_str().unwrap(), clone.to_str().unwrap()]);
            clone
        }

        fn origin_blob_size(&self, branch: &str, path: &str) -> u64 {
            let spec = format!("{branch}:{path}");
            self.git(&self.base, &["--git-dir", self.origin.to_str().unwrap(), "cat-file", "-s", &spec]).parse().unwrap()
        }
    }

    #[cfg(unix)]
    impl Drop for GitSandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// The live GH001 loop: a checkpoint over the limit made every push fail. Now files over
    /// it — new, or tracked and grown — stay out of the commit (on disk, reported every run)
    /// and everything else is pushed through the size-checking origin.
    #[cfg(unix)]
    #[test]
    fn files_over_the_limit_stay_out_of_git_and_the_rest_is_pushed() {
        let s = GitSandbox::new("big", 1000);
        s.put("exercises/devtest_ckpt.pt", 5000); // new, over the limit
        s.put("data.bin", 3000); // tracked, grown over it
        s.put("exercises/notes.md", 200); // new work, small
        s.put("exercises/at_the_limit.bin", 1000); // exactly the limit: fine
        s.put("exercises/staged_by_hand.pt", 4000); // `git add`ed by the participant
        s.git(&s.work, &["add", "exercises/staged_by_hand.pt"]);
        let (code, r, err) = s.backup();
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(r.outcome, Some((BACKUP_PUSHED, "autocommit-x".into())));
        let mut skipped = r.big_skipped.clone();
        skipped.sort();
        assert_eq!(
            skipped,
            [(3000, "data.bin".to_string()), (4000, "exercises/staged_by_hand.pt".to_string()), (5000, "exercises/devtest_ckpt.pt".to_string())]
        );
        let tree: Vec<String> = s.origin_tree("autocommit-x").into_iter().map(|(_, p)| p).collect();
        assert!(tree.contains(&"exercises/notes.md".to_string()) && tree.contains(&"exercises/at_the_limit.bin".to_string()), "{tree:?}");
        assert!(!tree.contains(&"exercises/devtest_ckpt.pt".to_string()) && !tree.contains(&"exercises/staged_by_hand.pt".to_string()), "{tree:?}");
        assert_eq!(s.origin_blob_size("autocommit-x", "data.bin"), 10, "the grown file's last good version stays");
        assert!(s.work.join("exercises/devtest_ckpt.pt").exists(), "never touched on disk");
        // Its blob was never even written (a 2 GB checkpoint staged every tick would fill the disk).
        let sha = s.git(&s.work, &["hash-object", "exercises/devtest_ckpt.pt"]);
        let o = std::process::Command::new("git").current_dir(&s.work).args(["cat-file", "-e", &sha]).output().unwrap();
        assert!(!o.status.success(), "the big file's blob is in the object store");

        // Next run: nothing new — and it says so, still naming what it keeps out.
        let (code, r, _) = s.backup();
        assert_eq!((code, r.outcome.clone()), (Some(0), Some((BACKUP_NO_CHANGES, "autocommit-x".into()))));
        assert_eq!(r.big_skipped.len(), 3);
    }

    /// A pod already stuck (the blob was committed before this guard): a clear failure with the
    /// fix — every run, never a silent `NO_CHANGES` — and the fix works, keeping the work.
    #[cfg(unix)]
    #[test]
    fn a_branch_stuck_with_a_big_blob_fails_with_the_fix_and_the_fix_works() {
        let s = GitSandbox::new("stuck", 1000);
        let upstream = s.git(&s.work, &["rev-parse", "HEAD"]);
        s.put("exercises/devtest_ckpt.pt", 5000);
        s.put("exercises/work.py", 100);
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "an old backup's commit"]);
        std::fs::remove_file(s.work.join("exercises/devtest_ckpt.pt")).unwrap(); // deleting it doesn't help
        s.put("exercises/more.py", 50);
        for _ in 0..2 {
            let (code, r, err) = s.backup();
            assert_eq!(code, Some(BACKUP_STUCK_EXIT), "{err}");
            assert_eq!(r.big_stuck, [(5000, "exercises/devtest_ckpt.pt".to_string())]);
            assert_eq!(r.stuck_base.as_deref(), Some(upstream.as_str()));
            assert!(err.contains("GH001") && err.contains(&format!("reset -q {upstream}")), "{err}");
            assert!(r.outcome.is_none());
        }
        // The fix it prints: reset to the upstream (the working tree is untouched), back up.
        s.git(&s.work, &["reset", "-q", &upstream]);
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");
        let tree: Vec<String> = s.origin_tree("autocommit-x").into_iter().map(|(_, p)| p).collect();
        assert!(tree.contains(&"exercises/work.py".to_string()) && tree.contains(&"exercises/more.py".to_string()), "{tree:?}");
        assert!(!tree.iter().any(|p| p.ends_with(".pt")), "{tree:?}");

        // A branch with no upstream yet: the base is the commit before its first unpushed one.
        let s = GitSandbox::new("stuck-new", 1000);
        s.git(&s.work, &["checkout", "-q", "-b", "autocommit-y", "main"]);
        let main = s.git(&s.work, &["rev-parse", "HEAD"]);
        s.put("big.bin", 4000);
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "big"]);
        let (code, r, _) = s.backup();
        assert_eq!((code, r.stuck_base.as_deref()), (Some(BACKUP_STUCK_EXIT), Some(main.as_str())));
    }

    /// The image ships clones of upstream repos inside the exercises; `git add -A` committed
    /// them as gitlinks, so the participants' work in them never reached GitHub (live). They
    /// are adopted once — the course repo tracks their files where they are, their own `.git`
    /// untouched — while a repo with history of its own (a participant's project) is kept out,
    /// never a gitlink, and reported every run.
    #[cfg(unix)]
    #[test]
    fn nested_clones_are_adopted_once_and_repos_with_their_own_history_kept_out() {
        let s = GitSandbox::new("nested", 1_000_000);
        let upstream = s.upstream();
        let clone = s.clone_upstream("exercises/shutdown_avoidance");
        // The participant's work inside it.
        std::fs::write(clone.join("devtest_participant_work.py"), "work\n").unwrap();
        std::fs::write(clone.join("u.py"), "edited\n").unwrap();
        // A participant's own repo, with its own commit.
        let mine = s.work.join("projects/my repo");
        s.git(&s.base, &["init", "-q", mine.to_str().unwrap()]);
        std::fs::write(mine.join("m.py"), "mine\n").unwrap();
        s.git(&mine, &["-c", "user.name=p", "-c", "user.email=p@p", "add", "-A"]);
        s.git(&mine, &["-c", "user.name=p", "-c", "user.email=p@p", "commit", "-q", "-m", "mine"]);

        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome.clone()), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");
        assert_eq!(r.adopted, ["exercises/shutdown_avoidance"]);
        assert_eq!(r.kept, ["projects/my repo"]);
        let tree = s.origin_tree("autocommit-x");
        assert!(!tree.iter().any(|(mode, _)| mode == "160000"), "no gitlinks: {tree:?}");
        let paths: Vec<&str> = tree.iter().map(|(_, p)| p.as_str()).collect();
        assert!(paths.contains(&"exercises/shutdown_avoidance/devtest_participant_work.py"), "{paths:?}");
        assert!(paths.contains(&"exercises/shutdown_avoidance/u.py"), "{paths:?}");
        assert!(!paths.iter().any(|p| p.contains("/.git") || p.starts_with("projects/")), "{paths:?}");
        let edited = s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "show", "autocommit-x:exercises/shutdown_avoidance/u.py"]);
        assert_eq!(edited, "edited");
        // Adopted in place: the clone's own .git is where it was, untouched; the kept repo too.
        assert_eq!(s.git(&clone, &["rev-parse", "HEAD"]), s.git(&upstream, &["rev-parse", "HEAD"]));
        assert!(mine.join(".git/HEAD").is_file());
        let exclude = std::fs::read_to_string(s.work.join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("# BEGIN arena-backup\n/projects/my repo/\n# END arena-backup\n"), "{exclude}");

        // Later work in it — a new file in a new directory — goes in with the next backup.
        std::fs::create_dir_all(clone.join("results")).unwrap();
        std::fs::write(clone.join("results/run2.txt"), "r\n").unwrap();
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome.clone()), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");
        assert!(r.adopted.is_empty(), "once: {:?}", r.adopted);
        assert!(s.origin_tree("autocommit-x").iter().any(|(_, p)| p == "exercises/shutdown_avoidance/results/run2.txt"));

        // Once per checkout: a clone appearing later isn't adopted — it's kept out, and said.
        let later = s.clone_upstream("exercises/later_clone");
        let (code, r, _) = s.backup();
        assert_eq!((code, r.outcome.clone()), (Some(0), Some((BACKUP_NO_CHANGES, "autocommit-x".into()))));
        assert!(r.adopted.is_empty());
        assert_eq!(r.kept, ["exercises/later_clone", "projects/my repo"]);
        assert!(later.join(".git/HEAD").is_file());
        let exclude = std::fs::read_to_string(s.work.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("# BEGIN arena-backup").count(), 1, "{exclude}");

        // Undone as the script says — out of the index; nothing on disk changes.
        s.git(&s.work, &["rm", "-r", "-q", "--cached", "exercises/shutdown_avoidance"]);
        assert!(clone.join("devtest_participant_work.py").is_file() && clone.join(".git/HEAD").is_file());
    }

    /// Review of the first design, which folded a clone by moving its `.git` aside
    /// (`.git.arena-nested`): every rsync tier drops that dot-dir, so the participant's stash and
    /// side branches in it lived only on the pod (gone on teardown or `pods replace`), and git run
    /// inside the exercise then worked on the whole course repo — a `git reset --hard` there
    /// reverted work across the checkout. Adopted in place, neither happens.
    #[cfg(unix)]
    #[test]
    fn an_adopted_clone_keeps_its_own_git_and_git_inside_it_stays_inside_it() {
        use crate::volume::fake_ssh;
        let s = GitSandbox::new("adopt-state", 1_000_000);
        let clone = s.clone_upstream("ex/clone");
        let g = |dir: &std::path::Path, args: &[&str]| {
            let mut a = vec!["-c", "user.name=p", "-c", "user.email=p@p"];
            a.extend_from_slice(args);
            s.git(dir, &a)
        };
        // The participant's git state in it: a stash and a side-branch commit — HEAD back on main
        // (= origin/main), so it reads as the image's clone and is adopted.
        std::fs::write(clone.join("u.py"), "stashed edit\n").unwrap();
        g(&clone, &["stash", "-q"]);
        g(&clone, &["checkout", "-q", "-b", "side"]);
        std::fs::write(clone.join("side.py"), "side\n").unwrap();
        g(&clone, &["add", "side.py"]);
        g(&clone, &["commit", "-q", "-m", "side work"]);
        let side = g(&clone, &["rev-parse", "HEAD"]);
        g(&clone, &["checkout", "-q", "main"]);
        std::fs::write(clone.join("work.py"), "w\n").unwrap();
        let (code, r, err) = s.backup();
        assert_eq!((code, r.adopted.clone()), (Some(0), vec!["ex/clone".to_string()]), "{err}");
        assert!(s.origin_tree("autocommit-x").iter().any(|(_, p)| p == "ex/clone/work.py"));
        // Its git state is where it was…
        assert!(g(&clone, &["stash", "list"]).contains("stash@{0}"));
        assert_eq!(g(&clone, &["rev-parse", "side"]), side);
        // …and the rsync tiers carry it: the big tier, the snapshot's (two passes), and the
        // `pods replace` copy onto a new pod.
        if fake_ssh::have_rsync() {
            let fake = fake_ssh::install(&s.base);
            let target =
                crate::ssh::SshTarget { user: "root".into(), host: "pod".into(), port: 22, key_paths: vec![], connect_timeout_secs: 5, host_keys: Default::default() };
            for (tier, pc) in [
                ("big", crate::pull::PullConfig::big_tier()),
                ("snapshot", crate::pull::PullConfig::small_tier("50M")),
                ("replication", crate::pull::PullConfig::replication()),
            ] {
                let dest = s.base.join(format!("backup-{tier}"));
                for pass in pc.passes() {
                    let args = crate::pull::rsync_args(&target, &pass, &format!("{}/", dest.display()));
                    fake_ssh::rsync(&fake_ssh::swap_transport(&args, &fake), &s.work);
                }
                let copy = dest.join("ex/clone");
                let read = |args: &[&str]| {
                    let o = std::process::Command::new("git").current_dir(&copy).args(args).output().unwrap();
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                };
                assert!(read(&["stash", "list"]).contains("stash@{0}"), "{tier}: the stash is backed up");
                assert_eq!(read(&["rev-parse", "side"]), side, "{tier}: the side branch is backed up");
            }
        }
        // git run inside the exercise works on the clone, never on the course repo: a hard reset
        // there leaves an uncommitted edit at the course repo's root alone.
        let top = g(&clone, &["rev-parse", "--show-toplevel"]);
        assert_eq!(std::fs::canonicalize(top).unwrap(), std::fs::canonicalize(&clone).unwrap());
        std::fs::write(s.work.join("README.md"), "an uncommitted course-repo edit\n").unwrap();
        g(&clone, &["reset", "-q", "--hard"]);
        assert_eq!(std::fs::read_to_string(s.work.join("README.md")).unwrap(), "an uncommitted course-repo edit\n");
    }

    /// Pods backed up before this fix carry gitlinks in their history: adopting such a clone
    /// replaces the gitlink with its files.
    #[cfg(unix)]
    #[test]
    fn an_earlier_backups_gitlink_is_replaced_by_the_files() {
        let s = GitSandbox::new("gitlink", 1_000_000);
        let clone = s.clone_upstream("Grokking");
        s.git(&s.work, &["add", "-A"]); // the old backup: a gitlink
        s.git(&s.work, &["commit", "-q", "-m", "old backup"]);
        s.git(&s.work, &["push", "-q"]);
        assert!(s.origin_tree("autocommit-x").iter().any(|(m, p)| m == "160000" && p == "Grokking"));
        std::fs::write(clone.join("work.py"), "w\n").unwrap();
        let (code, r, err) = s.backup();
        assert_eq!((code, r.adopted.clone()), (Some(0), vec!["Grokking".to_string()]), "{err}");
        let tree = s.origin_tree("autocommit-x");
        assert!(!tree.iter().any(|(m, _)| m == "160000"), "{tree:?}");
        assert!(tree.iter().any(|(_, p)| p == "Grokking/work.py") && tree.iter().any(|(_, p)| p == "Grokking/u.py"), "{tree:?}");
        assert!(clone.join(".git/HEAD").is_file());
    }

    /// `set-branch main` would delete an adopted clone's files (main doesn't track them) and break
    /// that exercise: they're put back, staged — and the switch back to the autocommit branch
    /// goes through (untracked, they'd block it). The clone's own repo never notices.
    #[cfg(unix)]
    #[test]
    fn set_branch_keeps_adopted_clones_on_disk_and_can_switch_back() {
        let s = GitSandbox::new("setbranch", 1_000_000);
        let clone = s.clone_upstream("exercises/solver");
        std::fs::write(clone.join("work.py"), "w\n").unwrap();
        let (code, r, err) = s.backup();
        assert_eq!((code, r.adopted.len()), (Some(0), 1), "{err}");
        let run = |cmd: String| {
            let o = std::process::Command::new("bash").arg("-c").arg(cmd).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        let repo = s.work.to_str().unwrap();
        for hard in [false, true] {
            let out = run(checkout_command(repo, "main", None, hard));
            assert!(out.contains("RESTORED exercises/solver"), "hard={hard}: {out}");
            assert_eq!(std::fs::read_to_string(clone.join("work.py")).unwrap(), "w\n", "hard={hard}");
            assert!(clone.join("u.py").is_file());
            assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
            // The clone's own view: its files all there, its one untracked file the work.
            assert_eq!(s.git(&clone, &["status", "--porcelain"]), "?? work.py", "hard={hard}");
            // …and back: the staged files ride along.
            let back = run(checkout_command(repo, "autocommit-x", None, false));
            assert!(!back.contains("RESTORED"), "{back}");
            assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "autocommit-x");
            assert!(s.git(&s.work, &["status", "--porcelain"]).is_empty(), "hard={hard}");
        }
    }

    /// GitHub refuses files over 100 MiB; the backup keeps files over 95 MiB out of its commits
    /// (headroom). A file between the two committed by hand is one GitHub takes: the backup
    /// pushes it — it used to call the branch stuck (judged by 95) and stop its git backup
    /// until someone rewrote valid history. Over the real limit it's still stuck.
    #[cfg(unix)]
    #[test]
    fn a_file_between_the_skip_size_and_githubs_limit_committed_by_hand_is_pushed() {
        let mut s = GitSandbox::new("between", 2000);
        s.skip = 1000;
        s.put("model_97mb.bin", 1500);
        s.git(&s.work, &["add", "model_97mb.bin"]);
        s.git(&s.work, &["commit", "-q", "-m", "by hand"]);
        s.put("exercises/new_97mb.bin", 1500); // not committed: kept out (the headroom)
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome.clone()), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");
        assert!(r.big_stuck.is_empty(), "{:?}", r.big_stuck);
        assert_eq!(s.origin_blob_size("autocommit-x", "model_97mb.bin"), 1500);
        assert_eq!(r.big_skipped, [(1500, "exercises/new_97mb.bin".to_string())]);
        // Over GitHub's limit: stuck, as before.
        s.put("huge.bin", 2500);
        s.git(&s.work, &["add", "huge.bin"]);
        s.git(&s.work, &["commit", "-q", "-m", "too big"]);
        let (code, r, err) = s.backup();
        assert_eq!(code, Some(BACKUP_STUCK_EXIT), "{err}");
        assert_eq!(r.big_stuck, [(2500, "huge.bin".to_string())]);
        assert!(err.contains("GH001"), "{err}");
    }

    /// The dry run's per-pod line pastes: next to the file its here-doc writes, it runs the
    /// backup (here through a stand-in `ssh` running the remote command locally); without the
    /// file the shell refuses to start it. The old line (`sh -c "$ARENA_BACKUP_SH" …`) expanded
    /// to `sh -c ""` on the pod: nothing done, exit 0 — a paste that looked like a backup.
    #[cfg(unix)]
    #[test]
    fn the_dry_run_line_pastes_and_fails_loudly_without_the_script() {
        let s = GitSandbox::new("paste", 1_000_000);
        s.put("exercises/notes.md", 20);
        let target = crate::ssh::SshTarget {
            user: "root".into(),
            host: "1.2.3.4".into(),
            port: 22001,
            key_paths: vec!["/root/.ssh/devtest key".into()],
            connect_timeout_secs: 10,
            host_keys: Default::default(),
        };
        let line = backup_dry_run_line(&target, s.work.to_str().unwrap(), None, "arena backup");
        // The stand-in: ssh's last argument is the remote command; stdin passes through.
        let ssh = "ssh() { for a; do last=$a; done; sh -c \"$last\"; }";
        let dir = s.base.join("paste");
        std::fs::create_dir_all(&dir).unwrap();
        let paste = |text: String| std::process::Command::new("bash").current_dir(&dir).arg("-c").arg(text).output().unwrap();
        let o = paste(format!("{ssh}\n{line}"));
        assert!(!o.status.success(), "no script file: must fail, not look like a backup");
        assert!(String::from_utf8_lossy(&o.stderr).contains("arena-backup.sh"), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(!s.origin_tree("autocommit-x").iter().any(|(_, p)| p == "exercises/notes.md"));
        let o = paste(format!("{ssh}\n{}\n{line}", backup_script_heredoc()));
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(o.status.success() && out.contains("PUSHED autocommit-x"), "{out}{}", String::from_utf8_lossy(&o.stderr));
        assert!(s.origin_tree("autocommit-x").iter().any(|(_, p)| p == "exercises/notes.md"));
        assert!(line.starts_with("ssh ") && line.ends_with(" < arena-backup.sh"), "{line}");
        assert!(line.contains(&format!("{GIT_MAX_FILE_BYTES} {GITHUB_FILE_LIMIT_BYTES}")), "{line}");
        assert!(!BACKUP_SCRIPT.lines().any(|l| l == HEREDOC_END));
    }

    #[test]
    fn escapes_single_quotes_in_message() {
        let c = backup_command("/root/ARENA_3.0", None, "it's a backup");
        assert!(c.contains(r"'it'\''s a backup'"));
    }

    /// Run the rendered command for real against a local bare "origin" (no network, no
    /// SSH): the live finding was a push that failed once and was then never retried,
    /// because the next tick saw a clean tree and reported `NO_CHANGES` with exit 0.
    #[cfg(unix)]
    #[test]
    fn a_failed_push_is_retried_on_the_next_tick_without_a_new_edit() {
        use std::process::Command;
        let base = std::env::temp_dir().join(format!("arena-backup-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let (origin, work) = (base.join("origin.git"), base.join("work"));
        let git = |dir: &std::path::Path, args: &[&str]| {
            let o = Command::new("git").current_dir(dir).args(args).output().expect("git");
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        git(&base, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&base, &["init", "-q", work.to_str().unwrap()]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["config", "user.email", "t@t"]);
        std::fs::write(work.join("a.txt"), "1").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "init"]);
        git(&work, &["checkout", "-q", "-b", "autocommit-x"]);
        git(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
        let run = || {
            let o = Command::new("bash")
                .arg("-c")
                .arg(backup_command(work.to_str().unwrap(), None, "arena backup"))
                .output()
                .expect("bash");
            (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string())
        };
        let remote_head = || {
            let o = Command::new("git")
                .args(["--git-dir", origin.to_str().unwrap(), "rev-parse", "refs/heads/autocommit-x"])
                .output()
                .unwrap();
            o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };

        // 1. No upstream yet, clean tree: the branch is still pushed (creates it upstream).
        let (ok, out) = run();
        assert!(ok && out.contains("PUSHED autocommit-x"), "{out}");
        assert_eq!(remote_head().as_deref(), Some(git(&work, &["rev-parse", "HEAD"]).as_str()));

        // 2. In sync and clean: the only case that may say NO_CHANGES.
        let (ok, out) = run();
        assert!(ok && out.contains("NO_CHANGES autocommit-x"), "{out}");

        // 3. A new edit, but the push fails (origin unreachable): a loud failure, commit kept.
        std::fs::write(work.join("b.txt"), "work").unwrap();
        git(&work, &["remote", "set-url", "origin", base.join("missing.git").to_str().unwrap()]);
        let (ok, out) = run();
        assert!(!ok && !out.contains("NO_CHANGES"), "a failed push must fail: {out}");
        let local = git(&work, &["rev-parse", "HEAD"]);
        assert_ne!(remote_head().as_deref(), Some(local.as_str()));

        // 4. Origin is back, nothing new edited: the next tick must push the stuck commit.
        git(&work, &["remote", "set-url", "origin", origin.to_str().unwrap()]);
        let (ok, out) = run();
        assert!(ok && out.contains("PUSHED autocommit-x"), "the stuck commit must be retried: {out}");
        assert_eq!(remote_head().as_deref(), Some(local.as_str()));

        let _ = std::fs::remove_dir_all(&base);
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
