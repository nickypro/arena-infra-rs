//! Commit/backup flow: snapshot each pod's ARENA working tree to git.
//!
//! The remote command this builds runs *inside* a pod over SSH: stage everything,
//! and — only if there's something to commit — commit it and push to a **per-machine
//! branch** (`backup/<machine>`). Pushing to a dedicated branch per machine is the
//! safe default: participants' work is backed up without ever racing each other on
//! `main`. The repo path, branch prefix, and push key are all config-driven.
//!
//! Three things never go into those commits, because each once stopped participants' work from
//! reaching GitHub: a file over [`GIT_MAX_FILE_BYTES`] (GitHub refuses the push — then every
//! push after it), a nested repo as a gitlink (a commit id without its files), and a pasted key
//! ([`SECRET_PATTERNS`]: GitHub's push protection refuses the push — then every push after it).
//! The image's nested clones are adopted instead — the course repo tracks their files where they
//! are, their own `.git` untouched — once; other nested repos are kept out and reported. A file
//! holding a key is committed with the key redacted, the file on disk untouched; and unpushed
//! commits holding one (an older backup's, or the participant's own) are rewritten redacted before
//! the push, which goes out as a fast-forward. The script is [`backup_script`].
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

/// The exit status of a backup whose push GitHub's push protection still refuses after the
/// redaction and the heal: a key [`SECRET_PATTERNS`] doesn't know (`PUSH_BLOCKED <reason>`).
pub const BACKUP_BLOCKED_EXIT: i32 = 4;

/// One kind of key the backup never lets into git ([`SECRET_PATTERNS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretPattern {
    /// What it is (as GitHub's push protection names it, near enough).
    pub name: &'static str,
    /// What must come right before it: [`SECRET_LEAD`], or a variant of it.
    pub lead: &'static str,
    /// The key itself, as a GNU extended regex matched on bytes (`sed -E`/`grep -E`, `LC_ALL=C`).
    pub re: &'static str,
    /// What it becomes in git: plain text that no pattern matches, so redacting twice changes
    /// nothing (the next backup's redacted blob is the one HEAD has: no new commit).
    pub replacement: &'static str,
}

/// What must come right before a key: the start of a line, a character that can't be part of a
/// word (so `task-or-v1-…` is no OpenRouter key), or a JSON escape (`\n`, `\t`, `\r`) — a
/// notebook's source and outputs are JSON strings, where a key starting a line follows `\n`.
pub const SECRET_LEAD: &str = r"(^|[^A-Za-z0-9_]|\\[nrt])";

/// [`SECRET_LEAD`], or — before a prefix nothing else starts with (`sk-or-v1-`, `sk-ant-`,
/// `github_pat_`, `gh?_`) — the end of an ANSI colour code (a notebook's coloured output:
/// `\u001b[32m` + key) or a URL-escaped quote (`%22`, `%27`). Never a letter: `task-or-v1-…`
/// still isn't a key.
pub const SECRET_LEAD_DISTINCT: &str = r"(^|[^A-Za-z0-9_]|\\[nrt]|\[[0-9;]*m|%2[27])";

/// [`SECRET_LEAD`] without `+` and `/` — the only non-word characters inside base64 — for a
/// prefix that turns up in base64 by chance (`AIza` + 35 more in a notebook's inline image).
pub const SECRET_LEAD_NO_BASE64: &str = r"(^|[^A-Za-z0-9_+/]|\\[nrt])";

/// The keys the backup redacts before committing and out of unpushed commits: high-confidence
/// formats only — a fixed prefix and a long body of a fixed alphabet — so ordinary code
/// (`sklearn`, `hf_hub_download`, a UUID, a notebook's base64 image) never matches. What we hand
/// out first (OpenRouter keys, `HF_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`), then what GitHub's push
/// protection refuses that a participant is likely to paste. A miss isn't silent: GitHub refuses
/// the push, the backup reports `PUSH_BLOCKED` with GitHub's name for the secret, and the pod's
/// files are copied by rsync instead — add the pattern here. One list: the backup script,
/// `set-branch` and the table tests all render it from here ([`secret_regex`], [`redact_sed`]).
///
/// Formats (GitHub's secret-scanning pattern list, the providers' own docs):
/// - OpenRouter: `sk-or-v1-` + 64 lowercase hex — the keys `pods up` mints; what GitHub refused
///   as "OpenRouter API Key" (prod, 2026-10-15).
/// - Anthropic: `sk-ant-api03-` (API key), `sk-ant-admin01-` (Admin API key) — 93 of
///   `[A-Za-z0-9_-]` then `AA`; `sk-ant-oat01-` the Claude Code OAuth token, same shape, which
///   setup broadcasts (`--cc-token`). Matched from 80 body characters on.
/// - OpenAI: project/service-account/admin keys `sk-proj-`/`sk-svcacct-`/`sk-admin-` (long:
///   40+ here, ~150 in practice) and any `sk-…T3BlbkFJ…` (`T3BlbkFJ` is base64 for "OpenAI";
///   the legacy `sk-` + 20 + `T3BlbkFJ` + 20). Never a bare `sk-…`: far too broad.
/// - Hugging Face user access token: `hf_` + exactly 34 letters.
/// - GitHub: `ghp_` (classic PAT), `gho_` (OAuth), `ghu_`/`ghs_` (app user/server), `ghr_`
///   (refresh) + 36+ alphanumerics; fine-grained `github_pat_` + 82+ of `[A-Za-z0-9_]`.
/// - Google API key (Gemini, Maps…): `AIza` + 35 of `[0-9A-Za-z_-]`; never after `+` or `/`
///   ([`SECRET_LEAD_NO_BASE64`]).
/// - Groq: `gsk_` + exactly 52 alphanumerics.
/// - AWS access key id: `AKIA` + exactly 16 of `[0-9A-Z]`.
///
/// Still missed, by design (each would make `task-or-v1-…`-style words match): two keys pasted
/// back to back with nothing between them (the second one), and a key glued to a following `_`
/// or letter where the format ends in `\b`.
pub const SECRET_PATTERNS: &[SecretPattern] = &[
    SecretPattern {
        name: "OpenRouter API key",
        lead: SECRET_LEAD_DISTINCT,
        re: r"sk-or-v1-[0-9a-f]{64}\b",
        replacement: "REDACTED-OPENROUTER-KEY",
    },
    SecretPattern {
        name: "Anthropic API key",
        lead: SECRET_LEAD_DISTINCT,
        re: r"sk-ant-(api03|admin01|oat01)-[A-Za-z0-9_-]{80,}",
        replacement: "REDACTED-ANTHROPIC-KEY",
    },
    SecretPattern {
        name: "OpenAI API key",
        lead: SECRET_LEAD,
        re: r"sk-(proj|svcacct|admin)-[A-Za-z0-9_-]{40,}",
        replacement: "REDACTED-OPENAI-KEY",
    },
    SecretPattern {
        name: "OpenAI API key",
        lead: SECRET_LEAD,
        re: r"sk-[A-Za-z0-9_-]{10,}T3BlbkFJ[A-Za-z0-9_-]{10,}",
        replacement: "REDACTED-OPENAI-KEY",
    },
    SecretPattern { name: "Hugging Face token", lead: SECRET_LEAD, re: r"hf_[A-Za-z]{34}\b", replacement: "REDACTED-HUGGINGFACE-TOKEN" },
    SecretPattern { name: "GitHub token", lead: SECRET_LEAD_DISTINCT, re: r"gh[pousr]_[A-Za-z0-9]{36,}", replacement: "REDACTED-GITHUB-TOKEN" },
    SecretPattern {
        name: "GitHub fine-grained token",
        lead: SECRET_LEAD_DISTINCT,
        re: r"github_pat_[A-Za-z0-9_]{82,}",
        replacement: "REDACTED-GITHUB-TOKEN",
    },
    SecretPattern {
        name: "Google API key",
        lead: SECRET_LEAD_NO_BASE64,
        re: r"AIza[0-9A-Za-z_-]{35}",
        replacement: "REDACTED-GOOGLE-API-KEY",
    },
    SecretPattern { name: "Groq API key", lead: SECRET_LEAD, re: r"gsk_[A-Za-z0-9]{52}\b", replacement: "REDACTED-GROQ-KEY" },
    SecretPattern { name: "AWS access key id", lead: SECRET_LEAD, re: r"AKIA[0-9A-Z]{16}\b", replacement: "REDACTED-AWS-ACCESS-KEY-ID" },
];

/// Any one key of [`SECRET_PATTERNS`], with its lead: what the backup looks for (and counts) in
/// a blob before rewriting it.
pub fn secret_regex() -> String {
    SECRET_PATTERNS.iter().map(|p| format!("{}{}", p.lead, p.re)).collect::<Vec<_>>().join("|")
}

/// [`SECRET_PATTERNS`] as a `sed -E` program: each key → its replacement, what came before it
/// kept. Every other byte of the blob is left exactly as it was.
pub fn redact_sed() -> String {
    // `#` as the delimiter: no pattern, lead or replacement has one (a lead has `/`).
    SECRET_PATTERNS.iter().map(|p| format!("s#{}{}#\\1{}#g", p.lead, p.re, p.replacement)).collect::<Vec<_>>().join("\n")
}

/// The shell function both commands that push (the backup, `init-branches`) pass git's push
/// output through: it drops every line with a URL that would *allow* a secret into the repo
/// (GitHub's push protection prints `…/secret-scanning/unblock-secret/…` beside each refused
/// secret) — never printed, never passed on to a report line or a log.
pub const NO_BYPASS_URL_SH: &str = "no_bypass_url() { LC_ALL=C grep -viE 'https?://[^ ]*(unblock|bypass|secret-scanning)' || true; }";

/// The redaction's shell functions (`redact.sh`), with [`secret_regex`] and [`redact_sed`]
/// rendered in — shared by the backup script and `set-branch`'s command.
pub fn redact_sh() -> String {
    include_str!("redact.sh").replace("@SECRET_RE@", &secret_regex()).replace("@REDACT_SED@", &redact_sed())
}

/// The lines of `backup.sh` that [`backup_script`] replaces with [`redact_sh`] and
/// [`NO_BYPASS_URL_SH`].
const REDACT_SH_MARK: &str = "# @ARENA_REDACT_SH@";
const NO_BYPASS_URL_MARK: &str = "# @ARENA_NO_BYPASS_URL@";

/// The backup script ([`backup_command`]): `backup.sh` with [`redact_sh`] and
/// [`NO_BYPASS_URL_SH`] rendered in. POSIX `sh`, so it runs the same whatever the pod's login
/// shell; its header documents every line it prints.
pub fn backup_script() -> String {
    include_str!("backup.sh").replacen(REDACT_SH_MARK, redact_sh().trim_end(), 1).replacen(NO_BYPASS_URL_MARK, NO_BYPASS_URL_SH, 1)
}

/// Render the remote shell command that commits + pushes one machine's working tree **on
/// whatever branch it is currently on** — it never switches or creates a branch, so a
/// participant's bespoke branch is respected (matching legacy `sync_git.sh`, not
/// `init_branches.sh`). To stage work on a dedicated autocommit branch, put the pod there
/// first with `init-branches`/`set-branch`. It runs [`backup_script`] under `sh` with the
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
    format!("sh -c {} arena-backup {}", shell_quote(&backup_script()), script_args(repo_path, git_ssh_key, commit_msg, max_file_bytes, push_limit_bytes))
}

/// [`backup_script`]'s arguments, shell-quoted.
fn script_args(repo_path: &str, git_ssh_key: Option<&str>, commit_msg: &str, max_file_bytes: u64, push_limit_bytes: u64) -> String {
    format!(
        "{} {} {max_file_bytes} {push_limit_bytes} {}",
        shell_quote(repo_path),
        shell_quote(commit_msg),
        shell_quote(git_ssh_key.unwrap_or("")),
    )
}

/// The file a dry run's per-pod line reads [`backup_script`] from ([`backup_dry_run_line`]);
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

/// The here-doc that writes [`backup_script`] to [`BACKUP_SCRIPT_FILE`] — pasteable, printed once
/// by a dry run for its per-pod lines ([`backup_dry_run_line`]).
pub fn backup_script_heredoc() -> String {
    format!("cat > {BACKUP_SCRIPT_FILE} <<'{HEREDOC_END}'\n{}\n{HEREDOC_END}", backup_script().trim_end())
}

/// [`backup_script_heredoc`]'s delimiter: a line the script never has.
const HEREDOC_END: &str = "ARENA_BACKUP_SH_END";

/// Classify a backup command's stdout: `(sentinel, branch)`. Returns the matched
/// `BACKUP_*` sentinel and the branch it reported, or `None` if neither appeared
/// (treat that as a failure at the call site).
pub fn parse_backup_output(stdout: &str) -> Option<(&'static str, String)> {
    parse_backup_report(stdout).outcome
}

/// Everything a backup run said ([`backup_script`]'s lines).
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
    /// The branch the run backed up (`None`: a protected one, or it never got that far).
    pub branch: Option<String>,
    /// Files committed with their keys redacted: `(keys, path)` — the files on disk keep them.
    pub redacted: Vec<(u64, String)>,
    /// Unpushed commits that held a key, rewritten redacted and pushed: `(commits rewritten, old
    /// tip, new tip)` — the new tip is on GitHub, the old one in the branch's reflog.
    pub history: Option<(u64, String, String)>,
    /// Why that rewrite stopped short (the branch moved meanwhile; the next run ends it).
    pub heal_aborted: Option<String>,
    /// GitHub's push protection still refuses the push: its reason (secret type, path:line).
    pub push_blocked: Option<String>,
    /// The push itself failed otherwise (the network, a non-fast-forward…): `PUSH_FAILED`.
    pub push_failed: bool,
}

/// Parse a backup's stdout ([`backup_script`]'s header lists the lines). Unknown lines (git's
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
            "BRANCH" if !rest.trim().is_empty() => r.branch = Some(rest.trim().to_string()),
            "REDACTED" => r.redacted.extend(sized(rest)),
            "REDACTED_HISTORY" => {
                let f: Vec<&str> = rest.split_whitespace().collect();
                if let [n, old, new] = f[..] {
                    r.history = n.parse().ok().map(|n| (n, old.to_string(), new.to_string()));
                }
            }
            "PUSH_FAILED" => r.push_failed = true,
            "HEAL_ABORTED" => r.heal_aborted = Some(rest.trim().to_string()),
            "PUSH_BLOCKED" => {
                let why = rest.trim();
                r.push_blocked = Some(if why.is_empty() { "refused by GitHub push protection".into() } else { why.to_string() });
            }
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

/// A commit id's first 10 characters, as a report shows it.
fn short_id(id: &str) -> &str {
    id.get(..10).unwrap_or(id)
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
        if !self.redacted.is_empty() {
            let keys: u64 = self.redacted.iter().map(|(n, _)| n).sum();
            let files: Vec<String> = self.redacted.iter().map(|(_, p)| p.clone()).collect();
            notes.push(format!(
                "redacted {keys} pasted key(s) in {} file(s) before committing: {} — git has them redacted, the files on \
                 disk keep the key (shown as modified)",
                files.len(),
                some_of(&files, 3)
            ));
        }
        if let Some((n, old, new)) = &self.history {
            notes.push(format!(
                "rewrote {n} unpushed commit(s) that held a pasted key — redacted, pushed as a fast-forward ({} → {}; the \
                 originals stay in the branch's reflog on the pod)",
                short_id(old),
                short_id(new)
            ));
        }
        notes
    }

    /// For a run that failed in a way the script explained — a file over GitHub's limit in the
    /// unpushed commits ([`Self::stuck_fix`]), a key GitHub refuses that the redaction doesn't
    /// know, a heal cut short — what happened and what to do; `None` for any other run (exit
    /// `code`). `repo` is the checkout on `pod`.
    pub fn failure(&self, repo: &str, pod: &str, code: Option<i32>) -> Option<String> {
        if code == Some(BACKUP_STUCK_EXIT) {
            if let Some(fix) = self.stuck_fix(repo, pod) {
                return Some(fix);
            }
        }
        if let (Some(BACKUP_BLOCKED_EXIT), Some(why)) = (code, &self.push_blocked) {
            let branch = self.branch.as_deref().unwrap_or("its branch");
            return Some(format!(
                "GitHub push protection refuses {branch}: {why} — a secret the backup doesn't redact. Its pattern belongs in \
                 arena_core::backup::SECRET_PATTERNS; until then, take it out of the unpushed commits on {pod} by hand \
                 (the file on disk may keep it)"
            ));
        }
        self.heal_aborted.as_ref().map(|why| match &self.history {
            Some((n, _, new)) => format!("pushed {n} rewritten commit(s) ({}), then {why}", short_id(new)),
            None => why.clone(),
        })
    }

    /// Whether the run reached the repo and failed at the push itself — refused, a big file in
    /// the way, a rewrite cut short, or the push failing outright: the failures where the
    /// pod's work is there but not on GitHub, so it gets the fallback copy. A run that never got
    /// that far (no repo, ssh denied, a timeout: nothing says how far it got) fails as it
    /// always did, and isn't rsynced every 15 minutes.
    pub fn failed_at_push(&self) -> bool {
        self.push_failed || self.push_blocked.is_some() || self.heal_aborted.is_some() || !self.big_stuck.is_empty()
    }

    /// [`Self::failure`] in a few words, for a line that says what else happened (the fallback
    /// copy): `None` for a failure the script didn't explain.
    pub fn failure_brief(&self, code: Option<i32>) -> Option<String> {
        if code == Some(BACKUP_STUCK_EXIT) && !self.big_stuck.is_empty() {
            return Some(format!("file(s) over {} MB in unpushed commits", GITHUB_FILE_LIMIT_BYTES / (1024 * 1024)));
        }
        if let (Some(BACKUP_BLOCKED_EXIT), Some(why)) = (code, &self.push_blocked) {
            return Some(format!("GitHub push protection: {why}"));
        }
        self.heal_aborted.as_ref().map(|_| "the branch moved while its commits were being redacted".to_string())
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
    // The new branch carries any commit of the old one GitHub refuses (the next backup redacts
    // and pushes it): the refusal goes to the operator without GitHub's allow-the-secret URL.
    parts.push(NO_BYPASS_URL_SH.into());
    parts.push("P=$(git rev-parse --absolute-git-dir)/arena-init-push.$$".into());
    parts.push(format!("if git push -u origin {bq} 2>\"$P\"; then R=0; else R=$?; fi"));
    parts.push("no_bypass_url < \"$P\" >&2; rm -f \"$P\"; exit $R".into());
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
///
/// A file the backup committed with a pasted key redacted keeps the real key on disk, so it is
/// modified for good: a gentle switch to a branch with another version of it would refuse
/// (`would be overwritten`) — every time — and `--hard` would throw the key away, which no
/// commit has. So around the switch (`unredacted.sh`): each tracked file whose *only*
/// difference from the index is keys the redaction knows is set aside in
/// `.git/arena-unredacted/` and put back to git's version; after the switch, one whose path
/// holds exactly that version again gets its real keys back (`UNREDACTED_RESTORED <path>`), the
/// rest stay set aside (`UNREDACTED_KEPT <path>`) until a switch back. Even a failed switch puts
/// them back before it exits. A file with any other uncommitted change is left to the switch, as
/// before.
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
    let switch = if hard {
        let remote_ref = shell_quote(&format!("origin/{branch}"));
        // Force-switch to the branch reset to origin (discards local changes/commits on it).
        format!("git checkout -f -B {} {remote_ref} && git reset --hard {remote_ref}", shell_quote(branch))
    } else {
        format!("git checkout {} && git pull --ff-only", shell_quote(branch))
    };
    let mut lines = vec![parts.join("; ")];
    // The keys set aside around the switch (from the repo's top: git's paths are relative to it).
    lines.push(format!("REDACT_SED={}", shell_quote(&redact_sed())));
    lines.extend(
        include_str!("unredacted.sh").lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).map(String::from),
    );
    lines.push("cd \"$(git rev-parse --show-toplevel)\"; G=$(git rev-parse --absolute-git-dir); unredacted_aside \"$G/arena-unredacted\"".into());
    lines.push(format!("if {switch}; then SWITCHED=1; else SWITCHED=; fi"));
    // The adopted clones (listed by the backup in `$G/arena-nested`) the switch removed: back
    // from the old HEAD, staged.
    lines.push(
        "if [ -n \"$SWITCHED\" ] && [ -n \"$OLD\" ] && [ -s \"$G/arena-nested\" ]; then \
         while IFS= read -r d; do if [ -n \"$d\" ] && git cat-file -e \"$OLD:$d\" 2>/dev/null && \
         ! git cat-file -e \"HEAD:$d\" 2>/dev/null; then git checkout -q \"$OLD\" -- \":(literal)$d\" && \
         echo \"RESTORED $d\"; fi; done < \"$G/arena-nested\"; fi"
            .into(),
    );
    lines.push("unredacted_restore \"$G/arena-unredacted\"; [ -n \"$SWITCHED\" ]".into());
    lines.join("\n")
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
        let s = &backup_script();
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
        assert!(s.contains(r#"set -- -u origin "$B""#) && s.contains(r#"git push "$@""#) && s.contains("PUSHED $B"));
        assert!(s.contains("Arena Autocommit"));
        assert!(s.contains(r#"export GIT_SSH_COMMAND="ssh -i $KEY"#));
        assert_eq!(GIT_MAX_FILE_BYTES, 99_614_720, "95 MiB: headroom under GitHub's 100 MiB");
        assert_eq!(GITHUB_FILE_LIMIT_BYTES, 104_857_600, "GitHub's GH001 limit");
    }

    #[test]
    fn omits_git_ssh_command_without_key() {
        // An empty key argument: the script exports no GIT_SSH_COMMAND for it.
        assert!(backup_command("/root/ARENA_3.0", None, "m").ends_with(" 'm' 99614720 104857600 ''"));
        assert!(backup_script().contains(r#"if [ -n "$KEY" ]; then"#));
    }

    #[test]
    fn the_script_is_posix_sh() {
        for shell in ["sh", "bash"] {
            let o = std::process::Command::new(shell).arg("-n").arg("-c").arg(backup_script()).output().expect(shell);
            assert!(o.status.success(), "{shell} -n: {}", String::from_utf8_lossy(&o.stderr));
        }
    }

    /// `n` characters of `alphabet`, deterministic per `seed` — the body of a test key, built at
    /// run time: a key-shaped literal in this file would itself be refused by GitHub's push
    /// protection (this repo is public).
    fn body(alphabet: &str, n: usize, seed: usize) -> String {
        let a: Vec<char> = alphabet.chars().collect();
        (0..n).map(|i| a[(i * 7 + seed * 13 + i / 3) % a.len()]).collect()
    }
    const HEX: &str = "0123456789abcdef";
    const ALNUM: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    const LETTERS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    const URLSAFE: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";

    /// The set-aside copies of `name` under `.git/arena-unredacted` (one folder per switch).
    fn aside_copies(aside: &std::path::Path, name: &str) -> Vec<Vec<u8>> {
        let Ok(runs) = std::fs::read_dir(aside) else { return Vec::new() };
        runs.flatten().filter(|e| e.path().is_dir()).filter_map(|e| std::fs::read(e.path().join(name)).ok()).collect()
    }

    /// An OpenRouter-shaped key (`sk-or-v1-` + 64 hex), distinct per `seed`.
    fn openrouter_key(seed: usize) -> String {
        format!("{}{}", "sk-or-v1-", body(HEX, 64, seed))
    }

    /// `text` through the program a pod runs ([`redact_sed`] under GNU sed, as bytes), and how
    /// many keys [`secret_regex`] (`grep -o`) counts in it.
    #[cfg(unix)]
    fn redact(text: &str) -> (String, usize) {
        use std::io::Write;
        let run = |cmd: &str, args: &[&str]| {
            let mut c = std::process::Command::new(cmd)
                .args(args)
                .env("LC_ALL", "C")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect(cmd);
            c.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
            String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap()
        };
        (run("sed", &["-E", &redact_sed()]), run("grep", &["-aoE", "--", &secret_regex()]).lines().count())
    }

    /// The patterns, through the very sed/grep programs the pods run: every kind of key (in the
    /// places a participant pastes one — code, `.env` lines, a notebook's JSON after a `\n`
    /// escape) becomes its REDACTED text with everything around it byte-identical; look-alikes
    /// (`sklearn`, `task-or-v1-…`, short or long bodies, UUIDs, `hf_hub_download`, a bare
    /// `sk-…`, a notebook's base64 image) stay exactly as they are.
    #[cfg(unix)]
    #[test]
    fn secret_patterns_redact_every_kind_of_key_and_no_lookalike() {
        let or = openrouter_key(1);
        let or2 = openrouter_key(2);
        let ant = |kind: &str| format!("{}{kind}-{}AA", "sk-ant-", body(URLSAFE, 93, 3));
        let gh = |p: &str| format!("{p}_{}", body(ALNUM, 36, 4));
        let hf = format!("{}{}", "hf_", body(LETTERS, 34, 5));
        let aws = format!("{}{}", "AKIA", body("ABCDEFGHIJKLMNOPQRSTUVWXYZ0234567", 16, 6));
        let google = format!("{}{}", "AIza", body(URLSAFE, 35, 14));
        let groq = format!("{}{}", "gsk_", body(ALNUM, 52, 15));
        // (input line, what git gets, keys counted)
        let keys: Vec<(String, String, usize)> = vec![
            (format!("export OPENROUTER_API_KEY={or}"), "export OPENROUTER_API_KEY=REDACTED-OPENROUTER-KEY".into(), 1),
            (or.clone(), "REDACTED-OPENROUTER-KEY".into(), 1),
            (format!("client = OpenAI(api_key=\"{or}\", base_url=u)"), "client = OpenAI(api_key=\"REDACTED-OPENROUTER-KEY\", base_url=u)".into(), 1),
            (format!("    \"key = '{or}'\\n\","), "    \"key = 'REDACTED-OPENROUTER-KEY'\\n\",".into(), 1),
            (format!("   \"printed\\n{or}\\n\""), "   \"printed\\nREDACTED-OPENROUTER-KEY\\n\"".into(), 1),
            (format!("a={or} b={or2};"), "a=REDACTED-OPENROUTER-KEY b=REDACTED-OPENROUTER-KEY;".into(), 2),
            (format!("\t-H 'Authorization: Bearer {or}'"), "\t-H 'Authorization: Bearer REDACTED-OPENROUTER-KEY'".into(), 1),
            (format!("k = \"{}\"", ant("api03")), "k = \"REDACTED-ANTHROPIC-KEY\"".into(), 1),
            (format!("k = \"{}\"", ant("admin01")), "k = \"REDACTED-ANTHROPIC-KEY\"".into(), 1),
            (format!("CLAUDE_CODE_OAUTH_TOKEN={}", ant("oat01")), "CLAUDE_CODE_OAUTH_TOKEN=REDACTED-ANTHROPIC-KEY".into(), 1),
            (format!("k='{}{}'", "sk-proj-", body(URLSAFE, 156, 7)), "k='REDACTED-OPENAI-KEY'".into(), 1),
            (format!("k='{}{}'", "sk-svcacct-", body(URLSAFE, 120, 8)), "k='REDACTED-OPENAI-KEY'".into(), 1),
            (format!("k='{}{}'", "sk-admin-", body(ALNUM, 60, 9)), "k='REDACTED-OPENAI-KEY'".into(), 1),
            (format!("k='sk-{}T3BlbkFJ{}'", body(ALNUM, 20, 10), body(ALNUM, 20, 11)), "k='REDACTED-OPENAI-KEY'".into(), 1),
            (format!("login(token=\"{hf}\")"), "login(token=\"REDACTED-HUGGINGFACE-TOKEN\")".into(), 1),
            (format!("HF_TOKEN={hf}"), "HF_TOKEN=REDACTED-HUGGINGFACE-TOKEN".into(), 1),
            (format!("{} {} {}", gh("ghp"), gh("gho"), gh("ghu")), "REDACTED-GITHUB-TOKEN REDACTED-GITHUB-TOKEN REDACTED-GITHUB-TOKEN".into(), 3),
            (format!("{},{}", gh("ghs"), gh("ghr")), "REDACTED-GITHUB-TOKEN,REDACTED-GITHUB-TOKEN".into(), 2),
            (format!("t={}{}_{}", "github_pat_", body(ALNUM, 22, 12), body(ALNUM, 59, 13)), "t=REDACTED-GITHUB-TOKEN".into(), 1),
            (format!("aws_access_key_id = {aws}"), "aws_access_key_id = REDACTED-AWS-ACCESS-KEY-ID".into(), 1),
            (format!("{or} {hf} {aws}"), "REDACTED-OPENROUTER-KEY REDACTED-HUGGINGFACE-TOKEN REDACTED-AWS-ACCESS-KEY-ID".into(), 3),
            // A notebook's coloured output (the ANSI code JSON-escaped, or raw) and a URL-escaped
            // quote, before a prefix nothing else has (review).
            (format!("    \"\\u001b[32m{or}\\u001b[0m\\n\","), "    \"\\u001b[32mREDACTED-OPENROUTER-KEY\\u001b[0m\\n\",".into(), 1),
            (format!("\u{1b}[1;32m{}\u{1b}[0m", gh("ghp")), "\u{1b}[1;32mREDACTED-GITHUB-TOKEN\u{1b}[0m".into(), 1),
            (format!("GET /v1?key=%22{or}%22"), "GET /v1?key=%22REDACTED-OPENROUTER-KEY%22".into(), 1),
            (format!("genai.configure(api_key=\"{google}\")"), "genai.configure(api_key=\"REDACTED-GOOGLE-API-KEY\")".into(), 1),
            (format!("GROQ_API_KEY={groq}"), "GROQ_API_KEY=REDACTED-GROQ-KEY".into(), 1),
        ];
        for (input, want, n) in &keys {
            let (got, counted) = redact(&format!("{input}\n"));
            assert_eq!(got, format!("{want}\n"), "{want}");
            assert_eq!(counted, *n, "{want}");
        }
        // A notebook's inline image (base64 of arbitrary bytes, one long line) and every
        // look-alike: byte-identical, nothing counted.
        let image: Vec<u8> = (0..30_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let lookalikes: Vec<String> = vec![
            "from sklearn.linear_model import LogisticRegression".into(),
            "pip install scikit-learn sk-learn".into(),
            format!("task-or-v1-{}", body(HEX, 64, 1)),
            format!("{}{}", "sk-or-v1-", body(HEX, 63, 1)),
            format!("{}{}", "sk-or-v1-", body(HEX, 65, 1)),
            format!("{}{}", "sk-or-v1-", body(HEX, 64, 1).to_uppercase()),
            "sk-or-v1-... (paste your key here)".into(),
            "sk-or-v1-123e4567-e89b-12d3-a456-426614174000 123e4567-e89b-12d3-a456-426614174000".into(),
            "from huggingface_hub import hf_hub_download, hf_hub_url; os.environ['HF_HUB_ENABLE_HF_TRANSFER']".into(),
            format!("{}{}", "hf_", body(LETTERS, 33, 5)),
            format!("{}{}", "hf_", body(LETTERS, 35, 5)),
            format!("{}{}9", "hf_", body(LETTERS, 34, 5)),
            format!("x{}{}", "hf_", body(LETTERS, 34, 5)),
            "sk-ant-api03-xxxx sk-ant-... sk-proj-abc123 sk-admin-panel sk-svcacct-".into(),
            format!("sk-{}", body(ALNUM, 48, 3)),
            format!("{}{}", "ghp_", body(ALNUM, 35, 4)),
            format!("{}{}", "github_pat_", body(ALNUM, 40, 4)),
            format!("{}{}", "AKIA", body("ABCDEFGHIJKLMNOPQRSTUVWXYZ", 15, 6)),
            format!("{}{}", "AKIA", body("ABCDEFGHIJKLMNOPQRSTUVWXYZ", 17, 6)),
            format!("{}{}", "AKIA", body("abcdefghijklmnopqrstuvwxyz", 16, 6)),
            format!("X{}{}", "AKIA", body("ABCDEFGHIJKLMNOPQRSTUVWXYZ", 16, 6)),
            format!("commit {} sk-{}", body(HEX, 40, 2), body(HEX, 32, 3)),
            // The extra leads are for unmistakable prefixes only: still no `task-or-v1-…`.
            format!("\u{1b}[32mtask-or-v1-{}", body(HEX, 64, 1)),
            format!("%22task-or-v1-{}", body(HEX, 64, 1)),
            // `AIza` + 35 inside base64 (after `/` or `+`), or one short; Groq one short or glued.
            format!("iVBORw0KGgo/{}{}", "AIza", body(ALNUM, 35, 14)),
            format!("iVBORw0KGgo+{}{}", "AIza", body(ALNUM, 35, 14)),
            format!("k = '{}{}'", "AIza", body(ALNUM, 34, 14)),
            format!("{}{}", "gsk_", body(ALNUM, 51, 15)),
            format!("x{}{}", "gsk_", body(ALNUM, 52, 15)),
            format!("      \"image/png\": \"{}\"", crate::base64::encode(&image)),
        ];
        for line in &lookalikes {
            let (got, counted) = redact(&format!("{line}\n"));
            assert_eq!((got.trim_end_matches('\n'), counted), (line.as_str(), 0), "{}", &line[..line.len().min(80)]);
        }
        // Every replacement is plain text no pattern matches (so a second pass changes nothing),
        // and safe in the sed program and the single-quoted shell words it's rendered into.
        for p in SECRET_PATTERNS {
            assert!(p.replacement.chars().all(|c| c.is_ascii_uppercase() || c == '-'), "{}", p.replacement);
            assert_eq!(redact(&format!("{}\n", p.replacement)), (format!("{}\n", p.replacement), 0));
            for part in [p.lead, p.re] {
                assert!(!part.contains('#') && !part.contains('\''), "{part}");
            }
        }
        assert!(!secret_regex().contains('\'') && !redact_sed().contains('\''));
    }

    /// The patterns are rendered into what runs on the pods — from the one list, nothing left to
    /// fill in — and the script that carries them is still POSIX sh.
    #[test]
    fn the_patterns_are_rendered_into_the_script_from_the_one_list() {
        let script = backup_script();
        assert!(!script.contains("@SECRET_RE@") && !script.contains("@REDACT_SED@") && !script.contains(REDACT_SH_MARK));
        assert!(script.contains(&format!("SECRET_RE='{}'", secret_regex())));
        assert!(script.contains(&format!("REDACT_SED='{}'", redact_sed())));
        assert!(script.contains("redact_blob()") && script.contains("redact_index()"));
        assert_eq!(redact_sed().lines().count(), SECRET_PATTERNS.len());
        for p in SECRET_PATTERNS {
            assert!(secret_regex().contains(&format!("{}{}", p.lead, p.re)));
            assert!(redact_sed().contains(&format!("s#{}{}#\\1{}#g", p.lead, p.re, p.replacement)));
        }
        let set_branch = checkout_command("/r", "main", None, false);
        assert!(set_branch.contains(&format!("REDACT_SED='{}'", redact_sed())));
        assert!(set_branch.contains("unredacted_aside()") && set_branch.contains("unredacted_restore()"));
        for shell in ["sh", "bash"] {
            for cmd in [checkout_command("/r", "main", None, false), checkout_command("/r", "main", Some("/k"), true)] {
                let o = std::process::Command::new(shell).arg("-n").arg("-c").arg(&cmd).output().expect(shell);
                assert!(o.status.success(), "{shell} -n: {}", String::from_utf8_lossy(&o.stderr));
            }
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

        /// Make origin refuse a push the way GitHub's push protection does (GH013): any new text
        /// blob holding a key [`secret_regex`] knows ("OpenRouter API Key"), or one it doesn't
        /// (`DEVTEST-UNKNOWN-` + 20 digits, "Devtest Unknown Token"), named with its path:line —
        /// and a URL that would let the secret in, which must never be passed on. Binary blobs
        /// are skipped, as GitHub's scanning skips them. `accepted` runs (in origin) after a push
        /// passes the scan; the file size hook is gone.
        fn push_protect(&self, accepted: &str) {
            use std::os::unix::fs::PermissionsExt;
            let hook = self.origin.join("hooks/pre-receive");
            let script = r#"#!/bin/sh
KNOWN='@KNOWN@'
UNKNOWN='DEVTEST-UNKNOWN-[0-9]{20}'
while read old new ref; do
  [ "$new" = 0000000000000000000000000000000000000000 ] && continue
  git rev-list --objects "$new" --not --all | while read sha path; do
    [ "$(git cat-file -t "$sha")" = blob ] || continue
    [ "$(git cat-file blob "$sha" | head -c 8000 | tr -d '\000' | wc -c)" -eq "$(git cat-file blob "$sha" | head -c 8000 | wc -c)" ] || continue
    if git cat-file blob "$sha" | LC_ALL=C grep -aqE "$KNOWN"; then ty='OpenRouter API Key'
    elif git cat-file blob "$sha" | LC_ALL=C grep -aqE "$UNKNOWN"; then ty='Devtest Unknown Token'
    else continue; fi
    line=$(git cat-file blob "$sha" | LC_ALL=C grep -anE "$KNOWN|$UNKNOWN" | head -n 1 | cut -d: -f1)
    {
      echo "error: GH013: Repository rule violations found for $ref."
      echo ""
      echo "- GITHUB PUSH PROTECTION"
      echo "  ————————————————————————————————————————"
      echo "    Resolve the following violations before pushing again"
      echo ""
      echo "    - Push cannot contain secrets"
      echo ""
      echo "      —— $ty ——————————————————————————————"
      echo "       locations:"
      echo "         - commit: $new"
      echo "           path: $path:$line"
      echo ""
      echo "       (?) To push, remove secret from commit(s) or follow this URL to allow the secret."
      echo "       https://github.com/devtest/repo/security/secret-scanning/unblock-secret/2DEVTESTunblock"
    } >&2
    exit 1
  done || exit 1
done
@ACCEPTED@
"#;
            std::fs::write(&hook, script.replace("@KNOWN@", &secret_regex()).replace("@ACCEPTED@", accepted)).unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// Whether any object in origin — reachable or not — holds `needle`.
        fn origin_holds(&self, needle: &str) -> bool {
            let o = std::process::Command::new("git")
                .args(["--git-dir", self.origin.to_str().unwrap(), "cat-file", "--batch-all-objects", "--batch"])
                .output()
                .unwrap();
            o.stdout.windows(needle.len()).any(|w| w == needle.as_bytes())
        }

        /// One backup run, raw: (exit code, stdout, stderr).
        fn backup_raw(&self) -> (Option<i32>, String, String) {
            let cmd = backup_command_limited(self.work.to_str().unwrap(), None, "arena backup", self.skip, self.limit);
            let o = std::process::Command::new("bash").arg("-c").arg(cmd).output().expect("bash");
            (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
        }

        /// `rel`'s bytes in the work tree.
        fn read(&self, rel: &str) -> Vec<u8> {
            std::fs::read(self.work.join(rel)).unwrap()
        }

        /// `git log` of `rev` as `subject|author|author date|committer|committer date` lines.
        fn history(&self, dir: &std::path::Path, rev: &str) -> Vec<String> {
            self.git(dir, &["log", "--format=%s|%an <%ae>|%ad|%cn <%ce>|%cd", "--date=raw", rev]).lines().map(String::from).collect()
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

    /// Live (prod, 2026-10-15): an OpenRouter key pasted into an exercise was committed, and
    /// GitHub refused every push of the branch for ~20 hours. Now it's committed redacted — the
    /// index only — and pushed through a push-protected origin; the file on disk is byte-identical
    /// across three runs, the second and third commit nothing, and origin never sees the key.
    /// A notebook (JSON: a key in a source line and after a `\n` in an output) the same.
    #[cfg(unix)]
    #[test]
    fn a_pasted_key_is_committed_redacted_and_the_file_on_disk_never_changes() {
        let s = GitSandbox::new("redact", 1_000_000);
        s.push_protect("");
        let key = openrouter_key(1);
        let hf = format!("{}{}", "hf_", body(LETTERS, 34, 5));
        let exercise = format!("import os\nKEY = \"{key}\"\nprint(len(KEY))\n");
        let nb = format!(
            "{{\n \"cells\": [{{\"cell_type\": \"code\", \"source\": [\"import os\\n\", \"os.environ['OPENROUTER_API_KEY'] = '{key}'\\n\", \
             \"login('{hf}')\"], \"outputs\": [{{\"text\": [\"key:\\n{key}\\n\"]}}]}}]\n}}\n"
        );
        std::fs::create_dir_all(s.work.join("part4")).unwrap();
        std::fs::write(s.work.join("part4/exercises.py"), &exercise).unwrap();
        std::fs::write(s.work.join("part4/solutions.ipynb"), &nb).unwrap();
        s.put("part4/notes.md", 40);
        let before = s.git(&s.work, &["rev-parse", "HEAD"]);
        for run in 1..=3 {
            let (code, r, err) = s.backup();
            let want = if run == 1 { BACKUP_PUSHED } else { BACKUP_NO_CHANGES };
            assert_eq!((code, r.outcome.clone()), (Some(0), Some((want, "autocommit-x".into()))), "run {run}: {err}");
            let mut redacted = r.redacted.clone();
            redacted.sort();
            assert_eq!(redacted, [(1, "part4/exercises.py".to_string()), (3, "part4/solutions.ipynb".to_string())], "run {run}");
            assert!(!err.contains(&key) && !err.contains(&hf), "run {run}: a key in stderr");
            assert_eq!(s.read("part4/exercises.py"), exercise.as_bytes(), "run {run}: the file on disk changed");
            assert_eq!(s.read("part4/solutions.ipynb"), nb.as_bytes(), "run {run}: the notebook on disk changed");
            assert!(r.notes().iter().any(|n| n.starts_with("redacted 4 pasted key(s) in 2 file(s) before committing")), "{:?}", r.notes());
        }
        assert_eq!(s.git(&s.work, &["rev-list", "--count", &format!("{before}..HEAD")]), "1", "one commit, then none");
        let show = |path: &str| s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "show", &format!("autocommit-x:{path}")]);
        assert_eq!(show("part4/exercises.py"), "import os\nKEY = \"REDACTED-OPENROUTER-KEY\"\nprint(len(KEY))");
        assert_eq!(show("part4/solutions.ipynb"), nb.replace(&key, "REDACTED-OPENROUTER-KEY").replace(&hf, "REDACTED-HUGGINGFACE-TOKEN").trim_end());
        assert!(s.origin_tree("autocommit-x").iter().any(|(_, p)| p == "part4/notes.md"));
        assert!(!s.origin_holds(&key) && !s.origin_holds(&hf), "a key reached origin");
        // `git status`: just the two files whose real keys are on disk, modified.
        assert_eq!(s.git(&s.work, &["status", "--porcelain"]), "M part4/exercises.py\n M part4/solutions.ipynb");
    }

    /// Only text files are rewritten: a binary file (a NUL in its first 8000 bytes) holding a
    /// key's bytes and a symlink whose target is key-shaped are committed exactly as they are
    /// (origin here scans nothing — GitHub skips binaries; the symlink is just a shape test).
    #[cfg(unix)]
    #[test]
    fn binary_files_and_symlinks_are_never_rewritten() {
        let s = GitSandbox::new("redact-skip", 1_000_000);
        let key = openrouter_key(3);
        let mut bin = vec![0u8, 1, 2];
        bin.extend_from_slice(format!(" {key} ").as_bytes());
        std::fs::write(s.work.join("weights.bin"), &bin).unwrap();
        std::os::unix::fs::symlink(&key, s.work.join("link")).unwrap();
        std::fs::write(s.work.join("a.py"), format!("k='{key}'\n")).unwrap();
        let (code, r, err) = s.backup();
        assert_eq!((code, r.redacted.clone()), (Some(0), vec![(1, "a.py".to_string())]), "{err}");
        let blob = |path: &str| {
            let o = std::process::Command::new("git")
                .args(["--git-dir", s.origin.to_str().unwrap(), "cat-file", "blob", &format!("autocommit-x:{path}")])
                .output()
                .unwrap();
            o.stdout
        };
        assert_eq!(blob("weights.bin"), bin, "a binary file is committed byte for byte");
        assert_eq!(blob("link"), key.as_bytes(), "a symlink is committed as it is");
        assert!(s.origin_tree("autocommit-x").contains(&("120000".to_string(), "link".to_string())));
        assert_eq!(blob("a.py"), b"k='REDACTED-OPENROUTER-KEY'\n");
    }

    /// The live case: keys committed by backups from before the redaction, 3 unpushed commits,
    /// every push refused (GH013). The heal rewrites exactly those commits — same count,
    /// messages, authors, committers and dates, the key redacted — and pushes them as a
    /// fast-forward: origin's history before them is untouched. The file on disk keeps its key;
    /// the index matches the new tip, so `git status` shows that file alone, as the redaction
    /// leaves it. The next run has nothing to do.
    #[cfg(unix)]
    #[test]
    fn a_key_in_older_unpushed_commits_is_healed_by_a_fast_forward_and_the_files_are_untouched() {
        let s = GitSandbox::new("heal", 1_000_000);
        s.push_protect("");
        let key = openrouter_key(4);
        let pushed = s.git(&s.work, &["rev-parse", "HEAD"]);
        let remote_log = || s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "log", "--format=%H", "autocommit-x"]);
        let remote_before = remote_log();
        let exercise = format!("KEY = \"{key}\"\n");
        std::fs::write(s.work.join("exercises.py"), &exercise).unwrap();
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "arena backup devtest-x (old 1)"]);
        s.put("notes.md", 30);
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "arena backup devtest-x (old 2)"]);
        std::fs::write(s.work.join("exercises.py"), format!("{exercise}x = 1\n")).unwrap();
        s.git(&s.work, &["commit", "-q", "-a", "-m", "arena backup devtest-x (old 3)"]);
        let old_tip = s.git(&s.work, &["rev-parse", "HEAD"]);
        let old_history = s.history(&s.work, &format!("{pushed}..HEAD"));
        let on_disk = s.read("exercises.py");

        let (code, out, err) = s.backup_raw();
        let r = parse_backup_report(&out);
        assert_eq!((code, r.outcome.clone()), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{out}{err}");
        let (n, old, new) = r.history.clone().expect("rewritten");
        assert_eq!((n, old.as_str()), (3, old_tip.as_str()));
        assert_eq!(s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "rev-parse", "autocommit-x"]), new);
        assert!(r.notes().iter().any(|n| n.starts_with("rewrote 3 unpushed commit(s) that held a pasted key")), "{:?}", r.notes());
        // A fast-forward: origin's commits before the heal are all still there, in order.
        let remote_after = remote_log();
        assert!(remote_after.ends_with(&remote_before), "{remote_after}");
        assert_eq!(remote_after.lines().count(), remote_before.lines().count() + 3);
        // Same commits, redacted: count, messages, authors, committers, dates.
        assert_eq!(s.history(&s.work, &format!("{pushed}..HEAD")), old_history);
        assert_eq!(s.git(&s.work, &["rev-parse", "HEAD"]), new);
        assert_eq!(s.git(&s.work, &["show", "HEAD:exercises.py"]), "KEY = \"REDACTED-OPENROUTER-KEY\"\nx = 1");
        assert!(!s.origin_holds(&key));
        assert!(!out.contains(&key) && !err.contains(&key) && !err.contains("unblock-secret"), "{out}{err}");
        // Nothing on disk touched; the index is the new tip's, but for nothing.
        assert_eq!(s.read("exercises.py"), on_disk);
        assert_eq!(s.git(&s.work, &["status", "--porcelain"]), "M exercises.py");
        assert_eq!(s.git(&s.work, &["diff", "--cached", "--name-only"]), "");
        // The old commits stay reachable from the reflog (nothing is lost on the pod).
        assert_eq!(s.git(&s.work, &["rev-parse", "autocommit-x@{1}"]), old_tip);
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome, r.history), (Some(0), Some((BACKUP_NO_CHANGES, "autocommit-x".into())), None), "{err}");
        assert_eq!(r.redacted, [(1, "exercises.py".to_string())]);
    }

    /// The heal rewrites only what no remote has — `HEAD --not --remotes`, narrower than
    /// `origin/B..B`: a course update merged in from origin's main keeps its id, even with a
    /// key-shaped example in it (GitHub already has it), and the merge is rewritten with its
    /// parents mapped. And a branch with no upstream yet (the day's branch, never pushed) is
    /// healed the same way, and gets its upstream.
    #[cfg(unix)]
    #[test]
    fn the_heal_rewrites_only_unpushed_commits_merges_included_and_a_new_branch_gets_its_upstream() {
        let s = GitSandbox::new("heal-merge", 1_000_000);
        let key = openrouter_key(5);
        // The course's own update on main, already on GitHub — an example key in its docs.
        let example = format!("aws_access_key_id = {}{}\n", "AKIA", body("ABCDEFGHIJKLMNOPQRSTUVWXYZ", 16, 2));
        s.git(&s.work, &["checkout", "-q", "main"]);
        std::fs::write(s.work.join("course_v2.md"), &example).unwrap();
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "course update"]);
        s.git(&s.work, &["push", "-q", "origin", "main"]);
        let course = s.git(&s.work, &["rev-parse", "HEAD"]);
        s.push_protect("");
        s.git(&s.work, &["checkout", "-q", "autocommit-x"]);
        std::fs::write(s.work.join("ex.py"), format!("k = '{key}'\n")).unwrap();
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "old backup with a key"]);
        s.git(&s.work, &["merge", "-q", "--no-edit", "-m", "merge main", "main"]);
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome.clone()), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");
        assert_eq!(r.history.as_ref().map(|h| h.0), Some(2), "the key's commit and the merge");
        let parents = s.git(&s.work, &["log", "-1", "--format=%P", "HEAD"]);
        let parents: Vec<&str> = parents.split(' ').collect();
        assert_eq!(parents.len(), 2);
        assert_eq!(parents[1], course, "the merged-in course commit keeps its id");
        assert_eq!(s.git(&s.work, &["show", "HEAD:course_v2.md"]), example.trim_end(), "what GitHub has is never rewritten");
        assert_eq!(s.git(&s.work, &["show", &format!("{}:ex.py", parents[0])]), "k = 'REDACTED-OPENROUTER-KEY'");
        assert_eq!(s.git(&s.work, &["log", "-1", "--format=%s", parents[0]]), "old backup with a key");
        assert!(!s.origin_holds(&key));

        // No upstream yet. (`-f`: ex.py on disk still has its real key — modified for good.)
        s.git(&s.work, &["checkout", "-q", "-f", "-b", "autocommit-y", "main"]);
        std::fs::write(s.work.join("y.py"), format!("k = '{key}'\n")).unwrap();
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "y"]);
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome.clone(), r.history.map(|h| h.0)), (Some(0), Some((BACKUP_PUSHED, "autocommit-y".into())), Some(1)), "{err}");
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "@{u}"]), "origin/autocommit-y");
        assert_eq!(s.git(&s.work, &["rev-parse", "HEAD"]), s.git(&s.work, &["rev-parse", "origin/autocommit-y"]));
        assert!(!s.origin_holds(&key));
        let (code, r, _) = s.backup();
        assert_eq!((code, r.outcome), (Some(0), Some((BACKUP_NO_CHANGES, "autocommit-y".into()))));
    }

    /// The participant commits while the heal is being pushed: the branch moved, so the
    /// compare-and-swap refuses — their commit is kept, nothing on the pod is rewritten under
    /// it, and the run fails saying so. The next run redoes the same rewrite (same ids, so a
    /// fast-forward of what was pushed) with their commit on top.
    #[cfg(unix)]
    #[test]
    fn a_branch_that_moves_during_the_heal_is_never_overwritten_and_the_next_run_finishes() {
        let s = GitSandbox::new("heal-cas", 1_000_000);
        let work_git = s.work.join(".git");
        let marker = s.base.join("commit-during-heal");
        // Once a push passes the scan (the healed one), the participant commits on the pod.
        s.push_protect(&format!(
            "if [ -e '{m}' ]; then rm -f '{m}'; (unset GIT_DIR GIT_QUARANTINE_PATH GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES; \
             W='{w}'; c=$(git --git-dir=\"$W\" -c user.name=p -c user.email=p@p commit-tree \"$(git --git-dir=\"$W\" rev-parse HEAD^{{tree}})\" \
             -p \"$(git --git-dir=\"$W\" rev-parse HEAD)\" -m 'participant commit during the heal') && \
             git --git-dir=\"$W\" update-ref refs/heads/autocommit-x \"$c\"); fi; true",
            m = marker.display(),
            w = work_git.display()
        ));
        let key = openrouter_key(6);
        std::fs::write(s.work.join("ex.py"), format!("k = '{key}'\n")).unwrap();
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "old 1"]);
        s.put("n.md", 10);
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "old 2"]);
        std::fs::write(&marker, "").unwrap();
        let on_disk = s.read("ex.py");

        let (code, out, err) = s.backup_raw();
        let r = parse_backup_report(&out);
        assert_eq!(code, Some(1), "{out}{err}");
        assert!(r.outcome.is_none(), "{out}");
        let (_, _, pushed) = r.history.clone().expect("the rewrite was pushed");
        assert!(r.heal_aborted.as_deref().unwrap().contains("moved"), "{out}");
        let fail = r.failure("/r", "devtest-x", code).expect("explained");
        assert!(fail.starts_with("pushed 2 rewritten commit(s)") && fail.contains("moved"), "{fail}");
        assert_eq!(r.failure_brief(code).as_deref(), Some("the branch moved while its commits were being redacted"));
        // Their commit is the branch, untouched; origin has the rewrite.
        assert_eq!(s.git(&s.work, &["log", "-1", "--format=%s", "autocommit-x"]), "participant commit during the heal");
        assert_eq!(s.git(&s.work, &["log", "-1", "--format=%s", "autocommit-x^"]), "old 2");
        assert_eq!(s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "rev-parse", "autocommit-x"]), pushed);
        assert_eq!(s.read("ex.py"), on_disk);

        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome.clone()), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");
        assert_eq!(s.git(&s.work, &["rev-parse", "HEAD^"]), pushed, "the same rewrite, their commit on top");
        assert_eq!(s.git(&s.work, &["log", "-1", "--format=%s", "HEAD"]), "participant commit during the heal");
        assert_eq!(s.git(&s.work, &["rev-parse", "HEAD"]), s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "rev-parse", "autocommit-x"]));
        assert!(!s.origin_holds(&key));
        assert_eq!(s.read("ex.py"), on_disk);
    }

    /// A secret the patterns don't know: GitHub still refuses, and the run says which kind and
    /// where (exit 4), never the secret and never the URL that would let it in — and changes
    /// nothing on the pod. A known key in the same commits doesn't help it through.
    #[cfg(unix)]
    #[test]
    fn a_secret_the_patterns_miss_is_reported_by_type_and_place_never_by_value() {
        let s = GitSandbox::new("blocked", 1_000_000);
        s.push_protect("");
        let unknown = format!("DEVTEST-UNKNOWN-{}", body("0123456789", 20, 1));
        let key = openrouter_key(7);
        // Committed before the redaction: the known key gets healed out, the unknown one can't.
        std::fs::write(s.work.join("scratch.py"), format!("a = 1\nTOKEN = '{unknown}'\nK = '{key}'\n")).unwrap();
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "an old backup"]);
        let tip = s.git(&s.work, &["rev-parse", "HEAD"]);
        let pushed = s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "rev-parse", "autocommit-x"]);
        for run in 1..=2 {
            let (code, out, err) = s.backup_raw();
            let r = parse_backup_report(&out);
            assert_eq!(code, Some(BACKUP_BLOCKED_EXIT), "run {run}: {out}{err}");
            assert_eq!(r.push_blocked.as_deref(), Some("Devtest Unknown Token at scratch.py:2"), "run {run}: {out}");
            assert_eq!(r.branch.as_deref(), Some("autocommit-x"));
            assert!(r.outcome.is_none() && r.history.is_none());
            for text in [&out, &err] {
                assert!(!text.contains(&unknown) && !text.contains(&key), "run {run}: a secret printed: {text}");
                assert!(!text.contains("unblock-secret") && !text.contains("2DEVTESTunblock"), "run {run}: the bypass URL: {text}");
            }
            let fail = r.failure("/root/ARENA_materials", "devtest-x", code).unwrap();
            assert!(fail.starts_with("GitHub push protection refuses autocommit-x: Devtest Unknown Token at scratch.py:2 — "), "{fail}");
            assert!(fail.contains("SECRET_PATTERNS"), "{fail}");
            assert_eq!(r.failure_brief(code).as_deref(), Some("GitHub push protection: Devtest Unknown Token at scratch.py:2"));
        }
        // The heal's push was refused too (over the unknown one): nothing moved, here or there.
        assert_eq!(s.git(&s.work, &["rev-parse", "HEAD"]), tip);
        assert_eq!(s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "rev-parse", "autocommit-x"]), pushed);
        assert!(!s.origin_holds(&key) && !s.origin_holds(&unknown));
        // GitHub's older wording parses too, and a refusal with no type falls back to its reason.
        let older = parse_backup_report("PUSH_BLOCKED GitHub Personal Access Token at a.py:3\n");
        assert_eq!(older.push_blocked.as_deref(), Some("GitHub Personal Access Token at a.py:3"));
        assert_eq!(parse_backup_report("PUSH_BLOCKED\n").push_blocked.as_deref(), Some("refused by GitHub push protection"));
    }

    /// GitHub's refusal as git relays it, through the script's own parser: GH013 (rulesets)
    /// and GH009 (the older wording, `——[ Type ]——`), several secrets, and one with no
    /// locations — the reason is the type(s) and path(s), else the `[remote rejected]` reason;
    /// never a URL.
    #[cfg(unix)]
    #[test]
    fn githubs_refusal_is_summed_up_without_its_urls() {
        let script = backup_script();
        let start = script.find("push_protected() {").unwrap();
        let end = script.find("# --- Keys in the commits going out").unwrap();
        let funcs = &script[start..end];
        let dir = std::env::temp_dir().join(format!("arena-pushreason-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gh013 = "remote: error: GH013: Repository rule violations found for refs/heads/autocommit-x.        \n\
                     remote: \n\
                     remote: - GITHUB PUSH PROTECTION        \n\
                     remote:   —————————————————————————————————————————        \n\
                     remote:     Resolve the following violations before pushing again        \n\
                     remote: \n\
                     remote:     - Push cannot contain secrets        \n\
                     remote: \n\
                     remote:      (?) Learn how to resolve a blocked push        \n\
                     remote:      https://docs.github.com/code-security/secret-scanning/working-with-secret-scanning-and-push-protection/working-with-push-protection-from-the-command-line#resolving-a-blocked-push        \n\
                     remote: \n\
                     remote:       —— OpenRouter API Key ————————————————————————————————        \n\
                     remote:        locations:        \n\
                     remote:          - commit: 8728dbe67fa7ac5c5e5b5e2d0d1a8b3c4d5e6f7a        \n\
                     remote:            path: part4_persona_vectors/exercises.py:122        \n\
                     remote:          - commit: 9728dbe67fa7ac5c5e5b5e2d0d1a8b3c4d5e6f7a        \n\
                     remote:            path: joep.py:3        \n\
                     remote: \n\
                     remote:        (?) To push, remove secret from commit(s) or follow this URL to allow the secret.        \n\
                     remote:        https://github.com/org/repo/security/secret-scanning/unblock-secret/2nRkZp        \n\
                     remote: \n\
                     remote:       —— Hugging Face User Access Token ———————————————————        \n\
                     remote:        locations:        \n\
                     remote:          - commit: 8728dbe67fa7ac5c5e5b5e2d0d1a8b3c4d5e6f7a        \n\
                     remote:            path: part4_persona_vectors/exercises.py:122        \n\
                     remote: \n\
                     To github.com:org/repo.git\n \
                     ! [remote rejected] autocommit-x -> autocommit-x (push declined due to repository rule violations)\n\
                     error: failed to push some refs to 'github.com:org/repo.git'\n";
        let gh009 = "remote: error: GH009: Secrets detected! This push failed.\n\
                     remote:   ——[ GitHub Personal Access Token ]——————————————————————\n\
                     remote:      locations:\n\
                     remote:        - commit: abc\n\
                     remote:          path: config.py:5\n\
                     remote:      https://github.com/o/r/security/secret-scanning/unblock-secret/2xyz\n\
                     ! [remote rejected] main -> main (secret detected)\n";
        let bare = "remote: error: GH013: Repository rule violations found for refs/heads/x.\n \
                    ! [remote rejected] x -> x (push declined due to repository rule violations)\n";
        let other = "! [rejected]        x -> x (fetch first)\n";
        let cases = [
            (gh013, true, "OpenRouter API Key, Hugging Face User Access Token at part4_persona_vectors/exercises.py:122, joep.py:3"),
            (gh009, true, "GitHub Personal Access Token at config.py:5"),
            (bare, true, "push declined due to repository rule violations"),
            (other, false, "refused by GitHub push protection"),
        ];
        for (i, (text, protected, reason)) in cases.iter().enumerate() {
            let f = dir.join(format!("push{i}"));
            std::fs::write(&f, text).unwrap();
            let sh = format!(
                "{funcs}\nif push_protected \"$1\"; then echo yes; else echo no; fi; push_block_reason \"$1\"; echo ---; no_bypass_url < \"$1\""
            );
            let o = std::process::Command::new("sh").arg("-c").arg(sh).arg("sh").arg(&f).output().unwrap();
            let out = String::from_utf8(o.stdout).unwrap();
            let (head, shown) = out.split_once("---\n").unwrap();
            assert_eq!(head, format!("{}\n{reason}\n", if *protected { "yes" } else { "no" }), "case {i}");
            assert!(!shown.contains("unblock-secret") && !shown.contains("secret-scanning"), "case {i}: {shown}");
            assert_eq!(shown.lines().count(), text.lines().filter(|l| !l.contains("https://")).count(), "case {i}: only the URL lines go");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `init-branches` from a branch whose unpushed commits GitHub refuses: the new day's branch
    /// carries them (created from HEAD), so its first push is refused too — and the next backup
    /// heals the new branch (no upstream yet). Nothing of the old branch is left behind.
    #[cfg(unix)]
    #[test]
    fn a_new_days_branch_carrying_a_refused_commit_is_healed_by_the_next_backup() {
        let s = GitSandbox::new("init-heal", 1_000_000);
        s.push_protect("");
        let key = openrouter_key(8);
        std::fs::write(s.work.join("ex.py"), format!("k = '{key}'\n")).unwrap();
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "yesterday, with a key"]);
        let cfg = BackupConfig { repo_path: s.work.display().to_string(), prefix: "devtest".into(), week: 1, day: 2, git_ssh_key: None };
        let o = std::process::Command::new("bash").arg("-c").arg(init_branch_command(&cfg, "devtest-x")).output().unwrap();
        assert!(!o.status.success(), "the new branch's push carries the key");
        // Its refusal reaches the operator — without GitHub's allow-the-secret URL (review: it
        // used to pass the push's stderr through as it was).
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("GH013") && err.contains("locations:"), "{err}");
        assert!(!err.contains("unblock-secret") && !err.contains("secret-scanning") && !err.contains(&key), "{err}");
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "autocommit-devtest-w1d2-x");
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome, r.history.map(|h| h.0)), (Some(0), Some((BACKUP_PUSHED, "autocommit-devtest-w1d2-x".into())), Some(1)), "{err}");
        assert_eq!(s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "log", "-1", "--format=%s", "autocommit-devtest-w1d2-x"]), "yesterday, with a key");
        assert!(!s.origin_holds(&key));
    }

    /// `set-branch` on a pod with a key the backup redacted (the file on disk keeps the real key,
    /// so it is modified for good). Gentle to `main` used to refuse — every day — and `--hard`
    /// threw the key away (no commit has it). Now both switch, the real-key files set aside in
    /// `.git/arena-unredacted/`; switching back puts the keys back, byte for byte. Any other
    /// uncommitted change still stops a gentle switch, as before — keys put back first.
    #[cfg(unix)]
    #[test]
    fn set_branch_sets_real_keys_aside_and_brings_them_back() {
        let s = GitSandbox::new("setbranch-keys", 1_000_000);
        let key = openrouter_key(9);
        let ex = format!("work\nKEY = \"{key}\"\n");
        let joep = format!("mine = '{key}'\n");
        std::fs::write(s.work.join("README.md"), &ex).unwrap();
        std::fs::write(s.work.join("joep.py"), &joep).unwrap();
        std::fs::write(s.work.join("other.py"), "x = 1\n").unwrap();
        let (code, r, err) = s.backup();
        assert_eq!((code, r.redacted.len()), (Some(0), 2), "{err}");
        let run = |cmd: String| {
            let o = std::process::Command::new("bash").arg("-c").arg(cmd).output().unwrap();
            (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
        };
        let repo = s.work.to_str().unwrap();
        let aside = s.work.join(".git/arena-unredacted");
        for hard in [false, true] {
            let (ok, out, err) = run(checkout_command(repo, "main", None, hard));
            assert!(ok, "hard={hard}: {out}{err}");
            assert!(out.contains("UNREDACTED_KEPT README.md") && out.contains("UNREDACTED_KEPT joep.py"), "hard={hard}: {out}");
            assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
            assert_eq!(s.read("README.md").len(), 5, "main's own README");
            assert!(!s.work.join("joep.py").exists());
            assert_eq!(aside_copies(&aside, "README.md"), [ex.clone().into_bytes()]);
            assert_eq!(aside_copies(&aside, "joep.py"), [joep.clone().into_bytes()]);
            assert!(!out.contains(&key) && !err.contains(&key));
            // …and back: the keys are back on disk, the set-aside copies gone.
            let (ok, out, err) = run(checkout_command(repo, "autocommit-x", None, hard));
            assert!(ok, "hard={hard}: {out}{err}");
            assert!(out.contains("UNREDACTED_RESTORED README.md") && out.contains("UNREDACTED_RESTORED joep.py"), "{out}");
            assert_eq!((s.read("README.md"), s.read("joep.py")), (ex.clone().into_bytes(), joep.clone().into_bytes()), "hard={hard}");
            assert!(!aside.exists(), "hard={hard}");
            assert_eq!(s.git(&s.work, &["status", "--porcelain"]), "M README.md\n M joep.py");
        }
        // A gentle switch to the branch it's on keeps them where they are.
        let (ok, out, _) = run(checkout_command(repo, "autocommit-x", None, false));
        assert!(ok && out.contains("UNREDACTED_RESTORED README.md"), "{out}");
        assert_eq!(s.read("README.md"), ex.as_bytes());
        // A real uncommitted edit (to a file main doesn't have) still stops a gentle switch — and
        // the real keys are back on disk before it exits.
        std::fs::write(s.work.join("other.py"), "x = 2\n").unwrap();
        let (ok, out, err) = run(checkout_command(repo, "main", None, false));
        assert!(!ok && err.contains("would be overwritten"), "{out}{err}");
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "autocommit-x");
        assert_eq!((s.read("README.md"), s.read("joep.py")), (ex.clone().into_bytes(), joep.clone().into_bytes()));
    }

    /// Review: a key in a commit the participant made by hand ("commit all" on the file the
    /// backup keeps showing as modified) went out whenever GitHub didn't refuse it — push
    /// protection off, or a type it scans but doesn't block. Now the commits going out are
    /// scanned and rewritten first, whatever the remote does: this origin refuses nothing.
    #[cfg(unix)]
    #[test]
    fn a_key_in_a_commit_the_participant_made_never_leaves_the_pod() {
        let s = GitSandbox::new("own-commit", 1_000_000);
        let hf = format!("{}{}", "hf_", body(LETTERS, 34, 9));
        let mine = format!("from huggingface_hub import login\nlogin(\"{hf}\")\n");
        std::fs::write(s.work.join("mine.py"), &mine).unwrap();
        s.git(&s.work, &["add", "mine.py"]);
        s.git(&s.work, &["commit", "-q", "-m", "my own commit"]);
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome.clone()), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");
        assert_eq!(r.history.as_ref().map(|h| h.0), Some(1));
        assert!(!s.origin_holds(&hf), "the key reached origin");
        assert_eq!(s.git(&s.work, &["log", "-1", "--format=%s", "HEAD"]), "my own commit");
        assert_eq!(s.git(&s.work, &["show", "HEAD:mine.py"]), "from huggingface_hub import login\nlogin(\"REDACTED-HUGGINGFACE-TOKEN\")");
        assert_eq!(s.read("mine.py"), mine.as_bytes());
        assert_eq!(s.git(&s.work, &["status", "--porcelain"]), "M mine.py");
        // And a clean branch costs no rewrite.
        let (code, r, _) = s.backup();
        assert_eq!((code, r.outcome, r.history), (Some(0), Some((BACKUP_NO_CHANGES, "autocommit-x".into())), None));
    }

    /// Review: `redact_blob` failed open — a grep that couldn't run read as "no key", and a sed
    /// dying mid-blob committed a truncated "redacted" copy (the pipeline hid its status). Now
    /// either stops the run: nothing committed, nothing pushed, no key anywhere on origin.
    #[cfg(unix)]
    #[test]
    fn a_scan_or_rewrite_that_fails_stops_the_run_and_commits_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let real = |tool: &str| ["/usr/bin", "/bin"].iter().map(|d| format!("{d}/{tool}")).find(|p| std::path::Path::new(p).exists()).unwrap();
        for (tool, wrapper) in [
            // grep given the key patterns: exit 2, as a grep that can't run them would.
            ("grep", format!("#!/bin/sh\ncase \"$*\" in *sk-or-v1-*) exit 2;; esac\nexec {} \"$@\"\n", real("grep"))),
            // sed given the redaction program: the first bytes of the blob, then it dies.
            ("sed", format!("#!/bin/sh\ncase \"$*\" in *REDACTED-*) for a; do last=$a; done; head -c 10 \"$last\"; exit 1;; esac\nexec {} \"$@\"\n", real("sed"))),
        ] {
            let s = GitSandbox::new(&format!("failclosed-{tool}"), 1_000_000);
            let bin = s.base.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join(tool), wrapper).unwrap();
            std::fs::set_permissions(bin.join(tool), std::fs::Permissions::from_mode(0o755)).unwrap();
            let key = openrouter_key(11);
            std::fs::write(s.work.join("ex.py"), format!("k = '{key}'\nmore = 1\n")).unwrap();
            let head = s.git(&s.work, &["rev-parse", "HEAD"]);
            let cmd = backup_command_limited(s.work.to_str().unwrap(), None, "arena backup", s.skip, s.limit);
            let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
            let o = std::process::Command::new("bash").arg("-c").arg(cmd).env("PATH", path).output().unwrap();
            let (out, err) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            assert!(!o.status.success(), "{tool}: {out}{err}");
            assert!(!out.contains("PUSHED") && !out.contains("NO_CHANGES"), "{tool}: {out}");
            assert_eq!(s.git(&s.work, &["rev-parse", "HEAD"]), head, "{tool}: a commit was made");
            assert_eq!(s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "rev-parse", "autocommit-x"]), head, "{tool}");
            assert!(!s.origin_holds(&key), "{tool}: the key reached origin");
        }
    }

    /// Review: a path set aside again (another day's branch, another key) overwrote the earlier
    /// copy, so the first branch's key was gone. Every switch now sets aside into a folder of its
    /// own: both keys are kept, and each comes back on its own branch.
    #[cfg(unix)]
    #[test]
    fn a_path_set_aside_twice_keeps_both_keys_and_each_comes_back_on_its_own_branch() {
        let s = GitSandbox::new("aside-twice", 1_000_000);
        let run = |cmd: String| {
            let o = std::process::Command::new("bash").arg("-c").arg(cmd).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        let repo = s.work.to_str().unwrap();
        let day1 = format!("KEY = \"{}\"\n", openrouter_key(12));
        let day2 = format!("KEY = \"{}\"\n", openrouter_key(13));
        std::fs::write(s.work.join("ex.py"), &day1).unwrap();
        assert_eq!(s.backup().1.redacted.len(), 1);
        assert!(run(checkout_command(repo, "main", None, true)).contains("UNREDACTED_KEPT ex.py"));
        // Day 2: a new branch, the same file, another key (the same redacted text).
        s.git(&s.work, &["checkout", "-q", "-b", "autocommit-y"]);
        std::fs::write(s.work.join("ex.py"), &day2).unwrap();
        let (code, r, err) = s.backup();
        assert_eq!((code, r.redacted.len()), (Some(0), 1), "{err}");
        assert!(run(checkout_command(repo, "main", None, true)).contains("UNREDACTED_KEPT ex.py"));
        let aside = s.work.join(".git/arena-unredacted");
        let mut copies = aside_copies(&aside, "ex.py");
        copies.sort();
        let mut both = vec![day1.clone().into_bytes(), day2.clone().into_bytes()];
        both.sort();
        assert_eq!(copies, both, "both days' keys kept");
        // Each back on its own branch.
        assert!(run(checkout_command(repo, "autocommit-x", None, false)).contains("UNREDACTED_RESTORED ex.py"));
        assert_eq!(s.read("ex.py"), day1.as_bytes());
        assert!(run(checkout_command(repo, "autocommit-y", None, false)).contains("UNREDACTED_RESTORED ex.py"));
        assert_eq!(s.read("ex.py"), day2.as_bytes());
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
        assert!(!backup_script().lines().any(|l| l == HEREDOC_END));
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
