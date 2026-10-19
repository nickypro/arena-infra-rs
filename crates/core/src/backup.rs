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
//! Before any of that, a checkout whose `origin` isn't the configured repo ([`configured_repo`])
//! is refused: nothing committed, nothing pushed, `WRONG_ORIGIN` said
//! ([`BACKUP_WRONG_ORIGIN_EXIT`]). `init-branches`' command checks the same. And a branch with an
//! operation in progress (a participant's conflicted `git pull upstream main`, …) is left as it is
//! — `BUSY`, [`BACKUP_BUSY_EXIT`] — since committing would end it, conflict markers and all; the
//! gentle switches refuse to end one too.
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
    /// The repo a pod's `origin` must be before anything is pushed to it ([`configured_repo`]);
    /// `None` = no check.
    pub origin: Option<String>,
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

/// The repo every pod's checkout must have as `origin` before arena pushes anything to it, as
/// `owner/name`: config `ARENA_REPO_OWNER` and `ARENA_REPO_NAME`, the repo `pods setup` points
/// `origin` at (`git@github.com:{owner}/{name}.git`, [`crate::setup::SetupConfig::from_config`]).
///
/// `None` (no check) when either is unset or empty, and when either holds a `/` or whitespace,
/// which no GitHub owner or repo name has: the backup script's `WRONG_ORIGIN` line reads the
/// configured repo as its first word.
pub fn configured_repo(cfg: &Config) -> Option<String> {
    let part = |key: &str| cfg.get(key).map(str::trim).filter(|v| !v.is_empty() && !v.contains('/') && !v.contains(char::is_whitespace));
    Some(format!("{}/{}", part("ARENA_REPO_OWNER")?, part("ARENA_REPO_NAME")?))
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
            origin: configured_repo(cfg),
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

/// The exit status of a backup (or `init-branches`) on a pod whose `origin` isn't the configured
/// repo ([`configured_repo`]): `WRONG_ORIGIN`, checked before anything else, so nothing is
/// committed or pushed. Live (2026-10-17): a pod nobody ran `pods setup` on had the upstream
/// course repo as `origin` and no deploy key, so it was never backed up to git, and nothing said
/// so.
pub const BACKUP_WRONG_ORIGIN_EXIT: i32 = 5;

/// The exit status of a backup that found an operation in progress on the branch ([`BUSY_OP_SH`]:
/// a merge, a cherry-pick, a revert, or conflicts a `git stash pop` left): `BUSY <branch> <what>`,
/// nothing staged, committed or pushed. Committing then would stage the conflict markers and end
/// the operation — a participant's conflicted `git pull upstream main` became a merge commit,
/// markers and all, pushed, and `git merge --abort` no longer worked (review R3). A branch switch
/// that would end one refuses with it too ([`checkout_command`], [`init_branch_command`]).
pub const BACKUP_BUSY_EXIT: i32 = 6;

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

/// `busy_op <git dir>`, run from the checkout's top: what is in progress there that a commit or a
/// branch switch would end or break — the first of `rebase` (`rebase-merge/`, or `rebase-apply/`
/// without `applying`), `am` (`rebase-apply/applying`: `git am`, which keeps HEAD attached),
/// `merge`, `cherry-pick` or `revert` (its `*_HEAD` file), `sequence` (`sequencer/` alone: a
/// multi-commit cherry-pick or revert paused between its commits, e.g. after one was resolved and
/// committed by hand) — git's own tests for them (`wt-status.c`), in the order git checks them —
/// followed by ` unmerged` while the index still has unmerged paths (conflicts not yet resolved
/// and added), else `conflicts` (unmerged paths and none of those: a `git stash pop` that
/// conflicted) — or nothing. Every path is under `<git dir>`, the worktree's own (`git rev-parse
/// --absolute-git-dir`: in a linked worktree, `.git/worktrees/<name>`, where git keeps these). The
/// `unmerged` word lets the words name conflicts only when there are some, and give the step git
/// wants first (review F4: a merge whose conflicts are all staged, a clean `git merge --no-commit`
/// or `git revert -n` has none; with some, `git commit` is refused until they're added). Review
/// pr4: `git am` with a patch that didn't apply has no unmerged path and no `*_HEAD`, so the
/// backup committed and pushed the participant's half-applied patch mid-`am`. A rebase detaches
/// HEAD, so on a branch the backup commits only a rebase that kept HEAD attached meets this; on
/// a detached HEAD it's said before the `SKIP` (`UNCOMMITTED`). Shared by the backup script
/// (`BUSY`, [`BACKUP_BUSY_EXIT`]) and the switches that refuse to end one ([`checkout_command`]'s
/// gentle switch, [`init_branch_command`]).
pub const BUSY_OP_SH: &str = "busy_op() { if [ -n \"$(git ls-files -u | head -n 1)\" ]; then busy_u=' unmerged'; else busy_u=; fi; \
     if [ -d \"$1/rebase-merge\" ]; then echo \"rebase$busy_u\"; \
     elif [ -d \"$1/rebase-apply\" ]; then if [ -e \"$1/rebase-apply/applying\" ]; then echo \"am$busy_u\"; else echo \"rebase$busy_u\"; fi; \
     elif [ -e \"$1/MERGE_HEAD\" ]; then echo \"merge$busy_u\"; elif [ -e \"$1/CHERRY_PICK_HEAD\" ]; then echo \"cherry-pick$busy_u\"; \
     elif [ -e \"$1/REVERT_HEAD\" ]; then echo \"revert$busy_u\"; elif [ -d \"$1/sequencer\" ]; then echo \"sequence$busy_u\"; \
     elif [ -n \"$busy_u\" ]; then echo conflicts; fi; }";

/// Every word [`BUSY_OP_SH`] prints first: the operations, then `conflicts` (a `git stash pop`'s).
pub const BUSY_WORDS: [&str; 7] = ["merge", "cherry-pick", "revert", "am", "rebase", "sequence", "conflicts"];

/// Every line [`BUSY_OP_SH`] prints, as `(what, unmerged)`: each operation with its conflicts
/// resolved or not, and `conflicts` (unmerged by definition).
const BUSY_LINES: [(&str, bool); 13] = [
    ("merge", false),
    ("merge", true),
    ("cherry-pick", false),
    ("cherry-pick", true),
    ("revert", false),
    ("revert", true),
    ("am", false),
    ("am", true),
    ("rebase", false),
    ("rebase", true),
    ("sequence", false),
    ("sequence", true),
    ("conflicts", true),
];

/// What is in progress on `branch` (`what`: [`BUSY_OP_SH`]'s word), in words: "a merge is in
/// progress on b", "a `git am` is in progress on b", "b has unresolved conflicts".
pub fn busy_situation(what: &str, branch: &str) -> String {
    match what {
        "merge" | "cherry-pick" | "revert" | "rebase" => format!("a {what} is in progress on {branch}"),
        "am" => format!("a `git am` is in progress on {branch}"),
        "sequence" => format!("a cherry-pick or revert sequence is in progress on {branch}"),
        "conflicts" => format!("{branch} has unresolved conflicts"),
        _ => format!("a git operation ({what}) is in progress on {branch}"),
    }
}

/// How to end an operation in progress either way — since only the participant knows which they
/// want — with the step git asks for first: while paths are `unmerged`, `git commit` (or
/// `--continue`) is refused until they're resolved and added (review F4), so that comes first.
/// A merge: "finish it (`git commit`) or abort it (`git merge --abort`)", or with unmerged paths
/// "resolve the conflicts and `git add` them, then `git commit` — or abort it (`git merge
/// --abort`)"; `git am`, a rebase: their own `--continue`/`--abort`. A sequence paused between its
/// commits may be a cherry-pick's or a revert's (only `--continue` with the right one works), so
/// both are named. A `git stash pop`'s conflicts have nothing to commit or continue: resolve and
/// add them, or undo them.
pub fn busy_advice(what: &str, unmerged: bool) -> String {
    let (finish, abort) = match what {
        "merge" => ("`git commit`", "`git merge --abort`"),
        "cherry-pick" => ("`git cherry-pick --continue`", "`git cherry-pick --abort`"),
        "revert" => ("`git revert --continue`", "`git revert --abort`"),
        "am" => ("`git am --continue`", "`git am --abort`"),
        "rebase" => ("`git rebase --continue`", "`git rebase --abort`"),
        "sequence" => ("`git cherry-pick --continue`, or `git revert --continue` for a revert", "`git cherry-pick --abort`, or `git revert --abort`"),
        "conflicts" => return "resolve them and `git add` them, or undo them (`git reset --merge`)".to_string(),
        _ => return "finish it or abort it".to_string(),
    };
    if unmerged {
        format!("resolve the conflicts and `git add` them, then {finish} — or abort it ({abort})")
    } else {
        format!("finish it ({finish}) or abort it ({abort})")
    }
}

/// [`busy_situation`]'s operation as the object of a sentence: `the merge`, `the \`git am\``,
/// `the sequence`, `the unresolved conflicts`.
pub fn busy_noun(what: &str) -> String {
    match what {
        "conflicts" => "the unresolved conflicts".to_string(),
        "am" => "the `git am`".to_string(),
        _ => format!("the {what}"),
    }
}

/// Why a branch switch must not happen mid-operation, as the parenthesis of a refusal: a merge,
/// cherry-pick or revert would be ended (its `*_HEAD` dropped); `git am`, a rebase or a paused
/// sequence would be carried, half-done, onto the other branch (their state stays: a `--continue`
/// there applies the rest onto it). `None` for a `git stash pop`'s conflicts: nothing to end.
pub fn busy_switch_why(what: &str) -> Option<String> {
    match what {
        "conflicts" => None,
        "am" | "rebase" | "sequence" => Some(format!("a switch now would carry {}, half-done, onto the other branch", busy_noun(what))),
        _ => Some(format!("a switch now would end {}", busy_noun(what))),
    }
}

/// What a backup's commit would do now, mid-operation: commit the operation's state — its
/// conflicts named only when there are unmerged paths (a merge with its conflicts staged, a clean
/// `git revert -n`, has no conflict markers to commit: review F4) — and end it; for `git am`, a
/// rebase or a sequence, commit what is there in the middle of it.
fn busy_would(what: &str, unmerged: bool) -> String {
    let conflicts = if unmerged { ", unresolved conflicts and all," } else { "" };
    match what {
        "conflicts" => "a backup now would commit the conflicted files as they are".to_string(),
        "am" | "rebase" | "sequence" => format!("a backup now would commit what is there{conflicts} in the middle of {}", busy_noun(what)),
        _ => format!("a backup now would commit the {what}'s state{conflicts} and end {}", busy_noun(what)),
    }
}

/// A branch switch's first step (`who` names it on stderr: `arena set-branch`), from the checkout's
/// top: while an operation is in progress ([`BUSY_OP_SH`]) it refuses — what is in progress and how
/// to end it ([`busy_situation`], [`busy_advice`]) on stderr, exit [`BACKUP_BUSY_EXIT`] — before
/// anything is touched. Checked with real git, mid-merge: `git checkout -b` (init-branches)
/// succeeds and drops `MERGE_HEAD`, leaving the conflicted index on the new branch with no merge to
/// abort; a gentle `git checkout` refuses unmerged paths but silently ends a merge whose conflicts
/// are resolved and staged, carrying them onto the other branch. Mid-`git am` (a patch that didn't
/// apply) a gentle checkout goes through and takes the `am` along. `--hard` doesn't run this: it
/// discards uncommitted changes to tracked files by design, the operation with them.
fn busy_refusal_sh(who: &str) -> String {
    let cases: String = BUSY_LINES
        .iter()
        .map(|&(what, unmerged)| {
            let why = busy_switch_why(what).map_or_else(String::new, |w| format!(" ({w})"));
            let msg = format!(
                "{who}: {} — not switched{why}; {} on the pod, then switch again",
                busy_situation(what, "@BRANCH@"),
                busy_advice(what, unmerged)
            );
            // The branch as the shell knows it, outside the quotes: the words never are code.
            let shell = msg.split("@BRANCH@").map(shell_quote).collect::<Vec<_>>().join("\"$BUSY_B\"");
            let printed = if unmerged && what != "conflicts" { format!("{what} unmerged") } else { what.to_string() };
            format!("{}) printf '%s\\n' {shell} >&2;; ", shell_quote(&printed))
        })
        .collect();
    format!(
        "cd \"$(git rev-parse --show-toplevel)\"; {BUSY_OP_SH}; BUSY_W=$(busy_op \"$(git rev-parse --absolute-git-dir)\"); \
         if [ -n \"$BUSY_W\" ]; then BUSY_B=$(git rev-parse --abbrev-ref HEAD); case $BUSY_W in {cases}esac; exit {BACKUP_BUSY_EXIT}; fi"
    )
}

/// The redaction's shell functions (`redact.sh`), with [`secret_regex`] and [`redact_sed`]
/// rendered in — shared by the backup script and `set-branch`'s command.
pub fn redact_sh() -> String {
    include_str!("redact.sh").replace("@SECRET_RE@", &secret_regex()).replace("@REDACT_SED@", &redact_sed())
}

/// The origin check's shell functions (`origin.sh`), shared by the two commands that push to a
/// pod's `origin`: the backup script and `init-branches`' command. `origin_check <owner/name>`
/// ends the run with `WRONG_ORIGIN` ([`BACKUP_WRONG_ORIGIN_EXIT`]) unless every URL `origin`
/// fetches from or pushes to is that GitHub repo, however it is spelled (scp-style, `ssh://`,
/// `https://` with or without credentials, `.git`, a trailing slash, any case). The URL it prints
/// has any credentials taken out.
pub fn origin_sh() -> &'static str {
    include_str!("origin.sh")
}

/// The lines of `backup.sh` that [`backup_script`] replaces with [`redact_sh`],
/// [`NO_BYPASS_URL_SH`], [`origin_sh`] and [`BUSY_OP_SH`].
const REDACT_SH_MARK: &str = "# @ARENA_REDACT_SH@";
const NO_BYPASS_URL_MARK: &str = "# @ARENA_NO_BYPASS_URL@";
const ORIGIN_SH_MARK: &str = "# @ARENA_ORIGIN_SH@";
const BUSY_OP_MARK: &str = "# @ARENA_BUSY_OP_SH@";

/// The backup script ([`backup_command`]): `backup.sh` with [`redact_sh`], [`NO_BYPASS_URL_SH`],
/// [`origin_sh`] and [`BUSY_OP_SH`] rendered in. POSIX `sh`, so it runs the same whatever the
/// pod's login shell; its header documents every line it prints.
pub fn backup_script() -> String {
    include_str!("backup.sh")
        .replacen(ORIGIN_SH_MARK, origin_sh().trim_end(), 1)
        .replacen(REDACT_SH_MARK, redact_sh().trim_end(), 1)
        .replacen(NO_BYPASS_URL_MARK, NO_BYPASS_URL_SH, 1)
        .replacen(BUSY_OP_MARK, BUSY_OP_SH, 1)
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
/// staged; then push whenever **origin has no such branch yet or the branch is ahead of
/// origin's** — not only when this run committed. A push that failed last tick (network blip,
/// GitHub outage, a key briefly missing) leaves its commit local and the tree clean, so "nothing
/// staged" must not read as "nothing to do": the next tick retries the push. Only a clean tree
/// *and* `HEAD == refs/remotes/origin/<branch>` prints `NO_CHANGES <branch>` (exit 0); a push
/// prints `PUSHED <branch>`. "Already pushed" always means on origin — the branch's configured
/// upstream and the course repo setup adds as `upstream` don't count (review ai-review claude
/// #6): a commit only `upstream` has is scanned for keys and redacted like any other. A
/// file over GitHub's limit already in the unpushed commits fails it with
/// [`BACKUP_STUCK_EXIT`] and what to reset to — before a push GitHub would refuse.
/// `set -e` aborts on any other error, so a rejected push is a failure.
///
/// `origin` is the configured repo ([`configured_repo`]). Before anything else, the main/master
/// SKIP included, a checkout whose `origin` isn't that repo ends the run with `WRONG_ORIGIN` and
/// [`BACKUP_WRONG_ORIGIN_EXIT`]: nothing committed, nothing pushed. `None`: no check.
///
/// On a branch it backs up, an operation in progress — a conflicted merge, cherry-pick or revert,
/// or a `git stash pop`'s conflicts ([`BUSY_OP_SH`]) — ends the run before anything is staged:
/// `BUSY <branch> <what>`, [`BACKUP_BUSY_EXIT`], nothing committed or pushed, so the participant
/// can still finish or abort it ([`BackupReport::failure`] says how). It is checked again right
/// before the staging and right before the commit, so one started during the scans is caught
/// too (review ai-review codex #1). On one it skips, the work
/// there no backup saves — that, changes to tracked files, commits no remote has, or untracked
/// files — is only said (`UNCOMMITTED`, [`BackupReport::uncommitted`]) before the `SKIP`,
/// read-only: the run still skips it, exit 0, and `set-branch --hard` copies the pod before
/// resetting it.
pub fn backup_command(repo_path: &str, git_ssh_key: Option<&str>, commit_msg: &str, origin: Option<&str>) -> String {
    backup_command_limited(repo_path, git_ssh_key, commit_msg, GIT_MAX_FILE_BYTES, GITHUB_FILE_LIMIT_BYTES, origin)
}

/// [`backup_command`] with the size limits as parameters — the largest file committed, and the
/// largest a push may carry — so tests simulate GitHub's limit with a few-byte one.
pub fn backup_command_limited(
    repo_path: &str,
    git_ssh_key: Option<&str>,
    commit_msg: &str,
    max_file_bytes: u64,
    push_limit_bytes: u64,
    origin: Option<&str>,
) -> String {
    format!(
        "sh -c {} arena-backup {}",
        shell_quote(&backup_script()),
        script_args(repo_path, git_ssh_key, commit_msg, max_file_bytes, push_limit_bytes, origin)
    )
}

/// [`backup_script`]'s arguments, shell-quoted.
fn script_args(repo_path: &str, git_ssh_key: Option<&str>, commit_msg: &str, max_file_bytes: u64, push_limit_bytes: u64, origin: Option<&str>) -> String {
    format!(
        "{} {} {max_file_bytes} {push_limit_bytes} {} {}",
        shell_quote(repo_path),
        shell_quote(commit_msg),
        shell_quote(git_ssh_key.unwrap_or("")),
        shell_quote(origin.unwrap_or("")),
    )
}

/// The file a dry run's per-pod line reads [`backup_script`] from ([`backup_dry_run_line`]);
/// [`backup_script_heredoc`] writes it.
pub const BACKUP_SCRIPT_FILE: &str = "arena-backup.sh";

/// [`backup_command`] as the remote half of a pasteable line: the same script and arguments,
/// the script read from stdin (`sh -s`) instead of carried in the command — for a dry run's
/// per-pod line and the TUI's confirm modal, which show the arguments without the script's
/// ~150 lines.
pub fn backup_summary(repo_path: &str, git_ssh_key: Option<&str>, commit_msg: &str, origin: Option<&str>) -> String {
    format!("sh -s {}", script_args(repo_path, git_ssh_key, commit_msg, GIT_MAX_FILE_BYTES, GITHUB_FILE_LIMIT_BYTES, origin))
}

/// One pod's backup as a dry run prints it: `ssh … 'sh -s <args>' < arena-backup.sh`. Pasted
/// into a shell next to the file ([`backup_script_heredoc`] writes it), it runs exactly what the
/// backup runs; without the file the local shell refuses to start it at all. (It used to show
/// the script as `"$ARENA_BACKUP_SH"`, which the pod's shell expanded to nothing: `sh -c ""`
/// did nothing and exited 0 — a paste that looked like a successful backup.)
pub fn backup_dry_run_line(target: &crate::ssh::SshTarget, repo_path: &str, git_ssh_key: Option<&str>, commit_msg: &str, origin: Option<&str>) -> String {
    format!("{} < {BACKUP_SCRIPT_FILE}", target.display_command(&backup_summary(repo_path, git_ssh_key, commit_msg, origin)))
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
    /// `origin` isn't the configured repo, so nothing was committed or pushed (`WRONG_ORIGIN`):
    /// `(the configured owner/name, origin's URL with any credentials taken out)`. The URL is
    /// empty when there is no `origin` at all.
    pub wrong_origin: Option<(String, String)>,
    /// What is in progress on the branch (`BUSY <branch> <what>`: `merge`, `cherry-pick`, `revert`
    /// or `conflicts`), so nothing was staged, committed or pushed.
    pub busy: Option<String>,
    /// The index has unmerged paths — conflicts not yet resolved and added (`BUSY`'s or
    /// `UNCOMMITTED`'s `unmerged` word; always so for `conflicts`).
    pub unmerged: bool,
    /// On a branch no backup commits (a `SKIP`), the work there that a `--hard` reset could
    /// discard (`UNCOMMITTED <branch> <what>`), the first of: an operation in progress
    /// ([`BUSY_OP_SH`]'s word), `changes` to tracked files, `commits` no remote has, `untracked`
    /// files (not ignored, nested repos aside). `set-branch --hard` copies such a pod first, `.git`
    /// with it.
    pub uncommitted: Option<String>,
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
            "WRONG_ORIGIN" if !rest.trim().is_empty() => {
                let (want, url) = rest.split_once(' ').unwrap_or((rest, ""));
                r.wrong_origin = Some((want.trim().to_string(), url.trim().to_string()));
            }
            "HEAL_ABORTED" => r.heal_aborted = Some(rest.trim().to_string()),
            "BUSY" => {
                if let [branch, what, ref more @ ..] = rest.split_whitespace().collect::<Vec<_>>()[..] {
                    r.busy = Some(what.to_string());
                    r.unmerged = what == "conflicts" || more.contains(&"unmerged");
                    r.branch.get_or_insert_with(|| branch.to_string());
                }
            }
            "UNCOMMITTED" => {
                if let [_, what, ref more @ ..] = rest.split_whitespace().collect::<Vec<_>>()[..] {
                    r.uncommitted = Some(what.to_string());
                    r.unmerged = what == "conflicts" || more.contains(&"unmerged");
                }
            }
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
    /// know, a heal cut short, an operation in progress ([`Self::in_progress`]) — what happened
    /// and what to do; `None` for any other run (exit `code`). `repo` is the checkout on `pod`.
    pub fn failure(&self, repo: &str, pod: &str, code: Option<i32>) -> Option<String> {
        if let Some(why) = self.wrong_origin_why(code) {
            return Some(format!("{why} — not backed up to git; run `arena pods setup {pod}`"));
        }
        if let Some(what) = self.in_progress(code) {
            let situation = busy_situation(what, self.branch.as_deref().unwrap_or("its branch"));
            let (would, advice) = (busy_would(what, self.unmerged), busy_advice(what, self.unmerged));
            return Some(format!("{situation} — not committed ({would}); {advice} on the pod"));
        }
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
    /// pod's work is there but not on GitHub, so it gets the fallback copy. An operation in
    /// progress (`BUSY`) counts too: the run reached the repo and stopped before the commit, so
    /// GitHub has the work only up to the last backup, every run until the participant ends it —
    /// the copy is all that keeps the rest. A run that never got that far (no repo, ssh denied, a
    /// timeout: nothing says how far it got) fails as it always did, and isn't rsynced every 15
    /// minutes. A wrong `origin` is not one of these
    /// either (nothing was tried): what to do with it is the caller's ([`Self::wrong_origin`]).
    pub fn failed_at_push(&self) -> bool {
        self.push_failed || self.push_blocked.is_some() || self.heal_aborted.is_some() || !self.big_stuck.is_empty() || self.busy.is_some()
    }

    /// [`Self::failure`] in a few words, for a line that says what else happened (the fallback
    /// copy): `None` for a failure the script didn't explain.
    pub fn failure_brief(&self, code: Option<i32>) -> Option<String> {
        if let Some(why) = self.wrong_origin_why(code) {
            return Some(why);
        }
        if let Some(what) = self.in_progress(code) {
            let situation = busy_situation(what, self.branch.as_deref().unwrap_or("its branch"));
            return Some(format!("{situation} — not committed; {} on the pod", busy_advice(what, self.unmerged)));
        }
        if code == Some(BACKUP_STUCK_EXIT) && !self.big_stuck.is_empty() {
            return Some(format!("file(s) over {} MB in unpushed commits", GITHUB_FILE_LIMIT_BYTES / (1024 * 1024)));
        }
        if let (Some(BACKUP_BLOCKED_EXIT), Some(why)) = (code, &self.push_blocked) {
            return Some(format!("GitHub push protection: {why}"));
        }
        self.heal_aborted.as_ref().map(|_| "the branch moved while its commits were being redacted".to_string())
    }

    /// For a run that stopped because `origin` isn't the configured repo (`WRONG_ORIGIN`, exit
    /// [`BACKUP_WRONG_ORIGIN_EXIT`]): `origin is <url>, not the configured <owner>/<name>`, or that
    /// there is no `origin` at all. `None` for any other run.
    pub fn wrong_origin_why(&self, code: Option<i32>) -> Option<String> {
        let (want, url) = self.wrong_origin.as_ref().filter(|_| code == Some(BACKUP_WRONG_ORIGIN_EXIT))?;
        Some(if url.is_empty() {
            format!("it has no `origin` remote (the configured repo is {want})")
        } else {
            format!("origin is {url}, not the configured {want}")
        })
    }

    /// For a run that stopped because an operation is in progress on the branch (`BUSY`, exit
    /// [`BACKUP_BUSY_EXIT`]): what it is (`merge`, `cherry-pick`, `revert`, `conflicts`). `None` for
    /// any other run.
    pub fn in_progress(&self, code: Option<i32>) -> Option<&str> {
        self.busy.as_deref().filter(|_| code == Some(BACKUP_BUSY_EXIT))
    }

    /// For a run that skipped a protected branch (`SKIP`) with work there no backup saves
    /// ([`Self::uncommitted`]): that work in words, and why only a copy can keep it — "main has
    /// uncommitted changes to tracked files — no backup commits on main", "a merge is in progress
    /// on main — no backup commits on main", "main has commits no remote has — no backup pushes
    /// main", "main has untracked files — no backup commits on main". `None` for any other run.
    pub fn uncommitted_why(&self) -> Option<String> {
        let what = self.uncommitted.as_deref()?;
        let (_, branch) = self.outcome.as_ref().filter(|(sentinel, _)| *sentinel == BACKUP_SKIPPED)?;
        let on = if branch == "HEAD" { "a detached HEAD" } else { branch.as_str() };
        Some(format!("{} — {}", uncommitted_situation(what, on), uncommitted_reason(what, on)))
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

/// The work on a branch no backup commits (`on`: `main`, `a detached HEAD`) that an `UNCOMMITTED`
/// line names (`what`: [`BUSY_OP_SH`]'s word, `changes`, `commits` or `untracked`), in words: "main
/// has uncommitted changes to tracked files", "main has commits no remote has", "main has
/// untracked files", "a merge is in progress on main".
pub fn uncommitted_situation(what: &str, on: &str) -> String {
    match what {
        "changes" => format!("{on} has uncommitted changes to tracked files"),
        "commits" => format!("{on} has commits no remote has"),
        "untracked" => format!("{on} has untracked files"),
        _ => busy_situation(what, on),
    }
}

/// Why no backup keeps that work (`what` as for [`uncommitted_situation`]): "no backup commits on
/// main" — or, for commits already made, "no backup pushes main".
pub fn uncommitted_reason(what: &str, on: &str) -> String {
    match what {
        "commits" => format!("no backup pushes {on}"),
        _ => format!("no backup commits on {on}"),
    }
}

/// Render the remote command that *creates* (or switches to) a machine's autocommit
/// branch for the iteration and pushes it upstream — **without committing any work**
/// (legacy `init_branches.sh`). Run at the start of a day so the branch exists (with an
/// upstream) before any `backup`; later backups then just commit onto it. `set -e`
/// aborts on the first real error. A checkout whose `origin` isn't the configured repo
/// (`cfg.origin`) is refused before anything else, as the backup refuses it (`WRONG_ORIGIN`,
/// [`BACKUP_WRONG_ORIGIN_EXIT`]): the new branch is neither made nor pushed. After the checkout,
/// the real keys a switch set aside on the branch now checked out go back where the file holds
/// exactly their redacted version (`UNREDACTED_RESTORED <path>`) — `unredacted.sh`'s restore,
/// the step [`checkout_command`] ends with — so a day branch that already exists isn't left with
/// them set aside, unsaid (live bsec O1). A checkout with an operation in progress (a conflicted
/// merge, …: [`BUSY_OP_SH`]) is refused next, before anything is fetched or switched (exit
/// [`BACKUP_BUSY_EXIT`], what and how to end it on stderr): `git checkout -b` would drop its
/// `MERGE_HEAD` and carry the conflicted index onto the new branch, with no merge left to abort.
pub fn init_branch_command(cfg: &BackupConfig, machine_name: &str) -> String {
    let branch = cfg.branch_for(machine_name);
    let bq = shell_quote(&branch);
    let mut parts: Vec<String> =
        vec!["set -e".into(), format!("cd {}", shell_quote(&cfg.repo_path))];
    // Never a participant's new branch pushed to whatever `origin` is: the same check as the
    // backup's, before anything else (WRONG_ORIGIN, exit 5).
    if let Some(origin) = &cfg.origin {
        let functions: Vec<&str> = origin_sh().lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).collect();
        parts.push(functions.join("\n"));
        parts.push(format!("origin_check {}", shell_quote(origin)));
    }
    // Never a participant's merge (cherry-pick, revert, conflicts) ended by the switch:
    // `checkout -b` would drop its MERGE_HEAD and leave the conflicts on the new branch.
    parts.push(busy_refusal_sh("arena init-branches"));
    if let Some(key) = &cfg.git_ssh_key {
        parts.push(format!(
            "export GIT_SSH_COMMAND={}",
            shell_quote(&format!(
                "ssh -i {key} -o StrictHostKeyChecking=accept-new -o BatchMode=yes"
            ))
        ));
    }
    // Origin only — the remote this pushes to and branches from. Not `--all`: setup adds the
    // public course repo as `upstream`, and under `set -e` one unreachable remote (a typo, an
    // outage, a renamed repo) stopped every pod's day branch (review fb2 R1).
    parts.push("git fetch --prune origin".into());
    // Create the branch from the current HEAD, or switch to it if it already exists.
    parts.push(format!("git checkout -b {bq} 2>/dev/null || git checkout {bq}"));
    // Landing on a day branch that already exists (`set-branch main --hard --init-branches` run
    // twice in a day): the real keys the switch to main set aside on it go back, by the same rule
    // as set-branch's ([`checkout_command`]), and are said (`UNREDACTED_RESTORED`). On a new
    // branch nothing was set aside on it, so nothing moves: yesterday's keys wait for theirs.
    parts.push(format!("REDACT_SED={}", shell_quote(&redact_sed())));
    parts.push(unredacted_sh());
    parts.push("cd \"$(git rev-parse --show-toplevel)\"; unredacted_restore \"$(git rev-parse --absolute-git-dir)/arena-unredacted\"".into());
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
/// `hard == false` (default): **gentle** — fetch origin, checkout, then fast-forward to
/// origin's branch (`git merge --ff-only refs/remotes/origin/<branch>`). A diverged/dirty tree
/// makes the fast-forward fail loudly rather than clobbering work, and git refuses to overwrite an
/// untracked file. Origin's branch, named outright — never the local branch's configured upstream
/// (review ai-review codex #5: a `git pull --ff-only` there followed the upstream a participant's
/// own `feat` tracked, `upstream/feat`, so it reached the course repo — failing the switch when that
/// was unreachable, and merging the course's branch rather than the cohort's when it wasn't).
/// `hard == true`: **destructive** — fetch, then force the local branch to exactly match
/// `origin/<branch>` (`checkout -f -B … origin/<branch>` + `reset --hard`), **discarding any
/// local commits/changes on it** — and **overwriting any untracked file at a path
/// `origin/<branch>` tracks** (review pr4: checked with real git, a participant's untracked
/// `solutions.py` on main became origin's); other untracked
/// files stay. So `set-branch --hard` copies a pod on main with untracked files first. A `git am`,
/// rebase or paused cherry-pick/revert sequence it switches away from would outlive the reset (git
/// keeps their state across a checkout) and stop every later switch and backup as `BUSY`, and a
/// `git rebase --abort` would then take the pod back off the branch: once switched, `--hard` ends
/// them as it ends a merge (`--quit`: their state only — the stash a rebase's `--autostash` made
/// is kept in the stash list). Used by `pods set-branch [--hard]` (e.g. reset everyone back to
/// `main`).
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
/// rest stay set aside (`UNREDACTED_KEPT <path>`) until a switch back. The putting back is the
/// command's EXIT trap, set just before the set-aside (review ai-review claude #4): a failed
/// switch, a step that fails under `set -e` and a hangup, interrupt or termination (each made
/// an exit) all put them back before the command ends — never a participant left with the
/// redacted placeholder in their code. A file with any other uncommitted change is left to the
/// switch, as before.
///
/// An operation in progress (a participant's conflicted `git pull upstream main`, a cherry-pick,
/// a revert, a `git stash pop`'s conflicts: [`BUSY_OP_SH`]) is never ended by the gentle switch:
/// it refuses first — what is in progress and how to end it, on stderr, exit
/// [`BACKUP_BUSY_EXIT`] — touching nothing (the keys are set aside only after it), where `git
/// checkout` would silently drop a merge whose conflicts are resolved and staged and carry them
/// onto the other branch. `--hard` goes ahead and ends it with the other local changes it
/// discards (`set-branch` copies such a pod first and says so).
pub fn checkout_command(repo_path: &str, branch: &str, git_ssh_key: Option<&str>, hard: bool) -> String {
    let mut parts: Vec<String> = vec!["set -e".into(), format!("cd {}", shell_quote(repo_path))];
    if !hard {
        // A participant's merge (cherry-pick, revert, conflicts) is never ended by the gentle
        // switch: it refuses, saying what and how, before anything else.
        parts.push(busy_refusal_sh("arena set-branch"));
    }
    if let Some(key) = git_ssh_key {
        parts.push(format!(
            "export GIT_SSH_COMMAND={}",
            shell_quote(&format!(
                "ssh -i {key} -o StrictHostKeyChecking=accept-new -o BatchMode=yes"
            ))
        ));
    }
    // Origin only, pruned: a branch origin no longer has is no branch to switch to.
    parts.push("git fetch --prune origin".into());
    parts.push("OLD=$(git rev-parse -q --verify HEAD) || OLD=".into());
    let switch = if hard {
        let remote_ref = shell_quote(&format!("origin/{branch}"));
        // Force-switch to the branch reset to origin (discards local changes/commits on it), then
        // end any `git am`, rebase or sequence the switch left behind (their state only).
        format!(
            "git checkout -f -B {} {remote_ref} && git reset --hard {remote_ref} && {{ git rebase --quit >/dev/null 2>&1; \
             git am --quit >/dev/null 2>&1; git cherry-pick --quit >/dev/null 2>&1; true; }}",
            shell_quote(branch)
        )
    } else {
        // `checkout.defaultRemote`: with an `upstream` remote too (the course repo, whose branch
        // names the cohort's copy shares), a plain checkout of a branch both have refused as
        // "matched multiple remote tracking branches" (review fb2 R2); origin's is the one. Then
        // origin's branch, by its full name — never the branch's own upstream (codex #5).
        format!(
            "git -c checkout.defaultRemote=origin checkout {} && git merge --ff-only {}",
            shell_quote(branch),
            shell_quote(&format!("refs/remotes/origin/{branch}"))
        )
    };
    let mut lines = vec![parts.join("; ")];
    // The keys set aside around the switch (from the repo's top: git's paths are relative to it).
    lines.push(format!("REDACT_SED={}", shell_quote(&redact_sed())));
    lines.push(unredacted_sh());
    // The putting back is the EXIT trap, set before anything is set aside: whatever ends the
    // command from here on — the switch failing, `set -e`, a signal (made an exit, so the trap
    // runs) — the keys go back first (claude #4).
    lines.push(
        "cd \"$(git rev-parse --show-toplevel)\"; G=$(git rev-parse --absolute-git-dir); \
         trap 'unredacted_restore \"$G/arena-unredacted\"' EXIT; trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM; \
         unredacted_aside \"$G/arena-unredacted\""
            .into(),
    );
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
    lines.push("[ -n \"$SWITCHED\" ]".into());
    lines.join("\n")
}

/// `unredacted.sh`'s functions, comments and blank lines left out, for a command to define and
/// call (it needs `REDACT_SED` set): around set-branch's switch ([`checkout_command`]), and after
/// init-branches' ([`init_branch_command`]) — one rule for both.
fn unredacted_sh() -> String {
    include_str!("unredacted.sh").lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).collect::<Vec<_>>().join("\n")
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
            origin: None,
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
        let c = backup_command("/root/ARENA_3.0", Some("/root/.ssh/id_ed25519"), "arena backup", Some("styme3279/ARENA_materials"));
        // The script under sh (whatever the login shell), its arguments exact.
        assert!(c.starts_with("sh -c '"), "{c}");
        assert!(
            c.ends_with(&format!(
                " arena-backup '/root/ARENA_3.0' 'arena backup' {GIT_MAX_FILE_BYTES} {GITHUB_FILE_LIMIT_BYTES} '/root/.ssh/id_ed25519' \
                 'styme3279/ARENA_materials'"
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
        // "Nothing staged" isn't "nothing to do": push whenever ahead of origin's branch, or origin
        // has none — origin's, never the branch's own upstream (ai-review claude #6).
        assert!(s.contains(r#"git rev-list --count "refs/remotes/origin/$B..HEAD""#) && !s.contains("@{u}"));
        assert!(s.contains(r#"set -- -u origin "$B""#) && s.contains(r#"git push "$@""#) && s.contains("PUSHED $B"));
        assert!(s.contains("Arena Autocommit"));
        assert!(s.contains(r#"export GIT_SSH_COMMAND="ssh -i $KEY"#));
        assert_eq!(GIT_MAX_FILE_BYTES, 99_614_720, "95 MiB: headroom under GitHub's 100 MiB");
        assert_eq!(GITHUB_FILE_LIMIT_BYTES, 104_857_600, "GitHub's GH001 limit");
    }

    #[test]
    fn omits_git_ssh_command_without_key() {
        // An empty key argument: the script exports no GIT_SSH_COMMAND for it.
        assert!(backup_command("/root/ARENA_3.0", None, "m", None).ends_with(" 'm' 99614720 104857600 '' ''"));
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
        /// The configured repo the backup checks `origin` against (`None`: no check).
        want: Option<String>,
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
            let s = GitSandbox { origin: base.join("origin.git"), work: base.join("work"), base, limit, skip: limit, want: None };
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
            let (code, out, err) = self.backup_raw();
            (code, parse_backup_report(&out), err)
        }

        /// `bash -c cmd` with git kept off the network: only local paths and ssh, and ssh is
        /// [`Self::github`]'s stand-in (a run that tried GitHub over https fails at once).
        fn run(&self, cmd: &str) -> std::process::Output {
            std::process::Command::new("bash")
                .arg("-c")
                .arg(cmd)
                .env("GIT_ALLOW_PROTOCOL", "file:ssh")
                .env("GIT_SSH_COMMAND", self.base.join("fake-ssh"))
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .expect("bash")
        }

        /// A bare repo standing in for `github.com/<owner_name>` (created once), served over a
        /// stand-in `ssh` that runs git's remote command in `<base>/github` and logs each call
        /// to `<base>/ssh.log`. Its scp-style URL is `git@github.com:<owner_name>.git`.
        fn github(&self, owner_name: &str) -> std::path::PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let ssh = self.base.join("fake-ssh");
            std::fs::write(&ssh, "#!/bin/sh\necho \"$*\" >> \"$(dirname \"$0\")/ssh.log\"\nfor a; do last=$a; done\ncd \"$(dirname \"$0\")/github\" && exec sh -c \"$last\"\n").unwrap();
            std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
            let bare = self.base.join("github").join(format!("{owner_name}.git"));
            if !bare.exists() {
                std::fs::create_dir_all(&bare).unwrap();
                self.git(&self.base, &["init", "-q", "--bare", bare.to_str().unwrap()]);
            }
            bare
        }

        /// The stand-in ssh's calls so far (the log is emptied).
        fn ssh_calls(&self) -> Vec<String> {
            let log = self.base.join("ssh.log");
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let _ = std::fs::remove_file(&log);
            text.lines().map(String::from).collect()
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
            let cmd = backup_command_limited(self.work.to_str().unwrap(), None, "arena backup", self.skip, self.limit, self.want.as_deref());
            let o = self.run(&cmd);
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
        let cfg = BackupConfig { repo_path: s.work.display().to_string(), prefix: "devtest".into(), week: 1, day: 2, git_ssh_key: None, origin: None };
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
            let cmd = backup_command_limited(s.work.to_str().unwrap(), None, "arena backup", s.skip, s.limit, None);
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

    /// Review (fb2 R1, R2): setup adds the public course repo as `upstream`. The switches must not
    /// depend on it: init-branches fetched `--all` under `set -e`, so an unreachable upstream (a
    /// typo, an outage) stopped every pod's day branch; and a gentle switch to a branch both
    /// remotes have (the cohort repo is a copy of the course's) failed as "matched multiple remote
    /// tracking branches" once upstream's refs were there. Now init fetches origin only, and the
    /// switch takes origin's branch — fast-forwarding to it by name, even for a local branch that
    /// tracks upstream's (ai-review codex #5).
    #[cfg(unix)]
    #[test]
    fn the_switches_never_depend_on_an_upstream_remote() {
        let s = GitSandbox::new("upstream-switch", 1_000_000);
        let repo = s.work.to_str().unwrap();
        let up = s.base.join("upstream.git");
        s.git(&s.base, &["init", "-q", "--bare", up.to_str().unwrap()]);
        // `feat` on both: origin's with one more commit, upstream's = main.
        s.git(&s.work, &["checkout", "-q", "-b", "feat", "main"]);
        s.put("feat.txt", 3);
        s.git(&s.work, &["add", "-A"]);
        s.git(&s.work, &["commit", "-q", "-m", "feat"]);
        s.git(&s.work, &["push", "-q", "origin", "feat"]);
        s.git(&s.work, &["push", "-q", up.to_str().unwrap(), "main:feat"]);
        s.git(&s.work, &["remote", "add", "upstream", up.to_str().unwrap()]);
        s.git(&s.work, &["fetch", "-q", "upstream"]);
        s.git(&s.work, &["checkout", "-q", "autocommit-x"]);
        s.git(&s.work, &["branch", "-q", "-D", "feat"]);
        let run = |cmd: String| {
            let o = s.run(&cmd);
            (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
        };
        let (ok, out) = run(checkout_command(repo, "feat", None, false));
        assert!(ok, "{out}");
        assert_eq!(s.git(&s.work, &["rev-parse", "HEAD"]), s.git(&s.work, &["rev-parse", "origin/feat"]));
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "feat@{u}"]), "origin/feat");
        // A `feat` of the participant's own, tracking upstream's (review ai-review codex #5): the
        // switch fast-forwards it to origin's. (A `git pull --ff-only` merged from upstream's —
        // nothing new there — and left it behind origin's.)
        let own_feat = || {
            s.git(&s.work, &["checkout", "-q", "autocommit-x"]);
            s.git(&s.work, &["branch", "-q", "-f", "--track", "feat", "upstream/feat"]);
            assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "feat@{u}"]), "upstream/feat");
        };
        own_feat();
        let (ok, out) = run(checkout_command(repo, "feat", None, false));
        assert!(ok, "{out}");
        assert_eq!(s.git(&s.work, &["rev-parse", "HEAD"]), s.git(&s.work, &["rev-parse", "origin/feat"]), "origin's feat");
        // Upstream unreachable: https, which this sandbox's git refuses at once (as a typo'd or
        // down repo would fail on a pod).
        s.git(&s.work, &["remote", "set-url", "upstream", "https://github.com/ARENA-education/ARENA_material.git"]);
        let cfg = BackupConfig { repo_path: repo.to_string(), prefix: "devtest".into(), week: 1, day: 4, git_ssh_key: None, origin: None };
        let (ok, out) = run(init_branch_command(&cfg, "devtest-x"));
        assert!(ok, "{out}");
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "autocommit-devtest-w1d4-x");
        assert_eq!(s.git(&s.origin, &["rev-parse", "autocommit-devtest-w1d4-x"]), s.git(&s.work, &["rev-parse", "HEAD"]));
        // …and the gentle switch never asks upstream either: its own `feat` still lands on origin's.
        own_feat();
        let (ok, out) = run(checkout_command(repo, "feat", None, false));
        assert!(ok, "{out}");
        assert_eq!(s.git(&s.work, &["rev-parse", "HEAD"]), s.git(&s.work, &["rev-parse", "origin/feat"]), "origin's feat");
    }

    /// Live (bsec O1): `set-branch main --hard --init-branches` run twice in a day landed the pod
    /// back on its day branch with the real keys still set aside, and nothing said so — init's
    /// checkout ran no restore. Now it runs set-branch's: back, byte for byte, and said. A new
    /// day's branch (the normal morning) gets none of yesterday's: they stay set aside for theirs.
    #[cfg(unix)]
    #[test]
    fn init_branches_onto_an_existing_day_branch_puts_its_set_aside_keys_back() {
        let s = GitSandbox::new("init-unredacted", 1_000_000);
        let repo = s.work.to_str().unwrap();
        let day = |d: u32| BackupConfig { repo_path: repo.to_string(), prefix: "devtest".into(), week: 1, day: d, git_ssh_key: None, origin: None };
        let run = |cmd: String| {
            let o = s.run(&cmd);
            let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
            assert!(o.status.success(), "{out}{err}");
            (out, err)
        };
        let head = || s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let aside = s.work.join(".git/arena-unredacted");
        // The morning: today's branch, a key pasted, backed up (redacted in git, real on disk).
        run(init_branch_command(&day(2), "devtest-x"));
        assert_eq!(head(), "autocommit-devtest-w1d2-x");
        let key = openrouter_key(14);
        let readme = format!("notes\nKEY = \"{key}\"\n");
        std::fs::write(s.work.join("README.md"), &readme).unwrap();
        let (code, r, err) = s.backup();
        assert_eq!((code, r.redacted.len()), (Some(0), 1), "{err}");
        // The reset to main sets it aside…
        assert!(run(checkout_command(repo, "main", None, true)).0.contains("UNREDACTED_KEPT README.md"));
        assert_eq!(s.read("README.md").len(), 5, "main's own README");
        // …and the second init of the day lands on the branch that exists: the key comes back.
        let (out, err) = run(init_branch_command(&day(2), "devtest-x"));
        assert_eq!(head(), "autocommit-devtest-w1d2-x");
        assert_eq!(out.lines().filter(|l| l.starts_with("UNREDACTED_")).collect::<Vec<_>>(), ["UNREDACTED_RESTORED README.md"], "{out}");
        assert_eq!(s.read("README.md"), readme.as_bytes());
        assert!(!aside.exists(), "nothing left set aside");
        assert_eq!(s.git(&s.work, &["status", "--porcelain"]), "M README.md");
        assert!(!out.contains(&key) && !err.contains(&key));
        // A new day: the key stays set aside for its own branch, and nothing is said.
        assert!(run(checkout_command(repo, "main", None, true)).0.contains("UNREDACTED_KEPT README.md"));
        let (out, _) = run(init_branch_command(&day(3), "devtest-x"));
        assert_eq!(head(), "autocommit-devtest-w1d3-x");
        assert!(!out.contains("UNREDACTED_"), "{out}");
        assert_eq!(s.read("README.md").len(), 5);
        assert_eq!(aside_copies(&aside, "README.md"), [readme.clone().into_bytes()]);
        // …until a switch back to that branch.
        assert!(run(checkout_command(repo, "autocommit-devtest-w1d2-x", None, false)).0.contains("UNREDACTED_RESTORED README.md"));
        assert_eq!(s.read("README.md"), readme.as_bytes());
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
        let line = backup_dry_run_line(&target, s.work.to_str().unwrap(), None, "arena backup", None);
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
        let c = backup_command("/root/ARENA_3.0", None, "it's a backup", None);
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
                .arg(backup_command(work.to_str().unwrap(), None, "arena backup", None))
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
        assert!(cmd.contains("git fetch --prune origin") && !cmd.contains("--all"), "{cmd}");
        assert!(cmd.contains("git checkout -b 'autocommit-arena8-w1d4-apple'"));
        assert!(cmd.contains("git push -u origin 'autocommit-arena8-w1d4-apple'"));
        // It only creates/pushes the branch — never stages or commits work. (The refusal of a
        // checkout mid-merge names `git add`/`git commit` in the words it prints, never runs them.)
        let refusal = busy_refusal_sh("arena init-branches");
        assert!(cmd.contains(&refusal) && refusal.contains("printf '%s\\n' 'arena init-branches: a merge is in progress on '"));
        let run = cmd.replace(&refusal, "");
        assert!(!run.contains("git add"));
        assert!(!run.contains("git commit"));
    }

    #[test]
    fn checkout_is_gentle_ff_only() {
        let c = checkout_command("/root/ARENA_3.0", "main", Some("/root/.ssh/id_ed25519"), false);
        assert!(c.contains("cd '/root/ARENA_3.0'"));
        assert!(c.contains("git fetch --prune origin"));
        // Origin's branch by its full name, never the local branch's upstream (codex #5).
        assert!(c.contains("git -c checkout.defaultRemote=origin checkout 'main' && git merge --ff-only 'refs/remotes/origin/main'"));
        assert!(!c.contains("git pull") && !c.contains("@{u}")); // no hard reset
        assert!(!c.contains("reset --hard"));
        assert!(c.contains("GIT_SSH_COMMAND='ssh -i /root/.ssh/id_ed25519"));
    }

    #[test]
    fn checkout_hard_resets_to_origin() {
        let c = checkout_command("/root/ARENA_3.0", "main", None, true);
        assert!(c.contains("git fetch --prune origin"));
        // Force-switch the branch to origin and hard-reset (destructive).
        assert!(c.contains("git checkout -f -B 'main' 'origin/main'"));
        assert!(c.contains("git reset --hard 'origin/main'"));
        assert!(!c.contains("--ff-only"));
        // The keys' restore is the EXIT trap, set before the set-aside (claude #4).
        for cmd in [c.clone(), checkout_command("/r", "main", None, false)] {
            let trap = cmd.find("trap 'unredacted_restore \"$G/arena-unredacted\"' EXIT").expect("the trap");
            assert!(trap < cmd.find("unredacted_aside \"$G").unwrap() && cmd.contains("trap 'exit 143' TERM"));
        }
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

    /// The configured repo is `owner/name` when both are set, else no check.
    #[test]
    fn the_configured_repo_is_owner_and_name_or_no_check() {
        let cases = [
            ("ARENA_REPO_OWNER=styme3279\nARENA_REPO_NAME=ARENA_materials\n", Some("styme3279/ARENA_materials")),
            ("ARENA_REPO_OWNER= nickypro \nARENA_REPO_NAME=arena-sandbox-materials\n", Some("nickypro/arena-sandbox-materials")),
            ("ARENA_REPO_NAME=ARENA_materials\n", None),
            ("ARENA_REPO_OWNER=styme3279\n", None),
            ("ARENA_REPO_OWNER=\nARENA_REPO_NAME=r\n", None),
            ("ARENA_REPO_OWNER=a b\nARENA_REPO_NAME=r\n", None),
            ("ARENA_REPO_OWNER=o/x\nARENA_REPO_NAME=r\n", None),
            ("", None),
        ];
        for (text, want) in cases {
            let cfg = Config::parse(text);
            assert_eq!(configured_repo(&cfg).as_deref(), want, "{text:?}");
            assert_eq!(BackupConfig::from_config(&cfg, 1, 2).origin.as_deref(), want, "{text:?}");
        }
    }

    /// `origin_key` and `origin_shown` from the very script, under `sh`.
    #[cfg(unix)]
    fn origin_fn(func: &str, url: &str) -> String {
        let script = format!("{}\n{func} \"$1\"", origin_sh());
        let o = std::process::Command::new("sh").arg("-c").arg(script).arg("sh").arg(url).output().unwrap();
        assert!(o.status.success(), "{func} {url}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim_end_matches('\n').to_string()
    }

    /// Every spelling of the configured repo is it: scp-style, `ssh://` (a port, GitHub's
    /// ssh.github.com:443), `https://` (with or without credentials), `.git`, trailing slashes,
    /// any case. Anything else is not: another owner, a fork's name, another host or alias, a
    /// longer path, a local path. The URL a report prints never carries the credentials.
    #[cfg(unix)]
    #[test]
    fn every_spelling_of_the_configured_repo_matches_and_nothing_else_does() {
        let token = format!("{}{}", "ghp_", body(ALNUM, 36, 4));
        let want = origin_fn("origin_key", "git@github.com:Styme3279/ARENA_materials.git");
        assert_eq!(want, "github.com/styme3279/arena_materials");
        let with_token = format!("https://devtest-user:{token}@github.com/styme3279/ARENA_materials.git");
        let token_user = format!("https://{token}@github.com/STYME3279/arena_materials/");
        let cases: Vec<(&str, bool)> = vec![
            ("git@github.com:styme3279/ARENA_materials.git", true),
            ("git@github.com:STYME3279/arena_materials", true),
            ("github.com:styme3279/ARENA_materials.git/", true),
            ("ssh://git@github.com/styme3279/ARENA_materials.git", true),
            ("ssh://git@github.com:22/styme3279/ARENA_materials", true),
            ("ssh://git@ssh.github.com:443/styme3279/ARENA_materials.git", true),
            ("https://github.com/styme3279/ARENA_materials", true),
            ("https://github.com/styme3279/ARENA_materials.git/", true),
            ("https://WWW.GitHub.com/STYME3279/arena_MATERIALS//", true),
            ("http://github.com/styme3279/ARENA_materials", true),
            ("git://github.com/styme3279/ARENA_materials.git", true),
            (&with_token, true),
            (&token_user, true),
            ("https://github.com/ARENA-education/ARENA_materials", false),
            ("git@github.com:ARENA-education/ARENA_materials.git", false),
            ("https://github.com/styme3279/ARENA_materials-fork", false),
            ("https://github.com/styme3279/ARENA_materials/extra", false),
            ("https://gitlab.com/styme3279/ARENA_materials", false),
            ("git@github-deploy:styme3279/ARENA_materials.git", false),
            ("/srv/git/styme3279/ARENA_materials.git", false),
            ("file:///srv/github.com/styme3279/ARENA_materials", false),
            ("../ARENA_materials", false),
            ("", false),
        ];
        for (url, same) in &cases {
            assert_eq!(origin_fn("origin_key", url) == want, *same, "{url}: {}", origin_fn("origin_key", url));
        }
        // What a report may print: never the user or the token.
        let shown_cases: Vec<(String, &str)> = vec![
            (with_token.clone(), "https://github.com/styme3279/ARENA_materials.git"),
            (token_user.clone(), "https://github.com/STYME3279/arena_materials/"),
            (format!("https://devtest-user:pa/{token}@github.com/o/n"), "https://github.com/o/n"),
            (format!("https://github.com/o/n.git?access_token={token}#x"), "https://github.com/o/n.git"),
            ("git@github.com:o/n.git".into(), "github.com:o/n.git"),
            ("https://github.com/ARENA-education/ARENA_materials".into(), "https://github.com/ARENA-education/ARENA_materials"),
            ("/srv/git/n.git".into(), "/srv/git/n.git"),
        ];
        for (url, shown) in &shown_cases {
            let got = origin_fn("origin_shown", url);
            assert_eq!(got, *shown, "{url}");
            assert!(!got.contains(&token) && !got.contains("devtest-user"), "{got}");
        }
    }

    /// `WRONG_ORIGIN` read back: the configured repo, the URL, what the operator is told, and that
    /// it isn't a failure at the push. Only with its exit status: the line alone means nothing.
    #[test]
    fn a_wrong_origin_says_whose_repo_and_how_to_fix_it() {
        let r = parse_backup_report("WRONG_ORIGIN styme3279/ARENA_materials https://github.com/ARENA-education/ARENA_materials\n");
        assert_eq!(
            r.wrong_origin,
            Some(("styme3279/ARENA_materials".to_string(), "https://github.com/ARENA-education/ARENA_materials".to_string()))
        );
        assert_eq!((r.outcome.clone(), r.branch.clone(), r.failed_at_push()), (None, None, false));
        let code = Some(BACKUP_WRONG_ORIGIN_EXIT);
        assert_eq!(
            r.failure("/root/ARENA_materials", "devtest-bloom", code).unwrap(),
            "origin is https://github.com/ARENA-education/ARENA_materials, not the configured styme3279/ARENA_materials — not \
             backed up to git; run `arena pods setup devtest-bloom`"
        );
        assert_eq!(
            r.failure_brief(code).unwrap(),
            "origin is https://github.com/ARENA-education/ARENA_materials, not the configured styme3279/ARENA_materials"
        );
        assert_eq!((r.failure("/r", "p", Some(1)), r.wrong_origin_why(Some(0))), (None, None));
        let none = parse_backup_report("WRONG_ORIGIN styme3279/ARENA_materials\n");
        assert_eq!(
            none.failure("/r", "devtest-bloom", code).unwrap(),
            "it has no `origin` remote (the configured repo is styme3279/ARENA_materials) — not backed up to git; run \
             `arena pods setup devtest-bloom`"
        );
        assert_eq!(parse_backup_report("WRONG_ORIGIN \n").wrong_origin, None);
    }

    /// The live gap (2026-10-17): a pod nobody set up had the upstream course repo as `origin`
    /// and was never backed up to git, silently. Now, before anything else (the main SKIP too),
    /// a checkout whose origin isn't the configured repo is refused: `WRONG_ORIGIN`, exit 5,
    /// nothing committed, nothing pushed, nothing contacted, its credentials never printed. A
    /// push URL of its own counts too, and so does no origin at all. Every spelling of the right
    /// repo goes ahead: scp-style (any case) pushes to it; https gets past the check (and then
    /// can't push here, where git may use only local paths and ssh). No configured repo: no check.
    #[cfg(unix)]
    #[test]
    fn a_checkout_whose_origin_isnt_the_configured_repo_is_refused_before_anything_else() {
        let mut s = GitSandbox::new("origin", 1_000_000);
        let github = s.github("Styme3279/ARENA_materials");
        s.want = Some("styme3279/ARENA_materials".into());
        s.put("exercises/work.py", 30);
        let head = s.git(&s.work, &["rev-parse", "HEAD"]);
        let origin_refs = || s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "for-each-ref"]);
        let pushed_before = origin_refs();
        let untouched = |s: &GitSandbox| {
            assert_eq!(s.git(&s.work, &["rev-parse", "HEAD"]), head, "nothing committed");
            assert!(s.git(&s.work, &["status", "--porcelain"]).contains("exercises/"), "the work still uncommitted");
            assert!(s.ssh_calls().is_empty(), "nothing contacted");
        };
        // The sandbox's own origin (a local path) isn't the configured repo.
        let (code, r, err) = s.backup();
        assert_eq!(code, Some(BACKUP_WRONG_ORIGIN_EXIT), "{err}");
        assert_eq!(r.wrong_origin, Some(("styme3279/ARENA_materials".to_string(), s.origin.display().to_string())));
        assert_eq!((r.outcome, r.branch), (None, None), "before anything else");
        untouched(&s);
        assert_eq!(origin_refs(), pushed_before, "nothing pushed");
        // The upstream course repo, as an image has it, with credentials in the URL: refused, the
        // URL said without them. On main too: before the SKIP.
        let token = format!("{}{}", "ghp_", body(ALNUM, 36, 9));
        s.git(&s.work, &["remote", "set-url", "origin", &format!("https://devtest-user:{token}@github.com/ARENA-education/ARENA_materials.git")]);
        for branch in ["autocommit-x", "main"] {
            s.git(&s.work, &["checkout", "-q", branch]);
            let (code, out, err) = s.backup_raw();
            assert_eq!(code, Some(BACKUP_WRONG_ORIGIN_EXIT), "{branch}: {out}{err}");
            assert_eq!(out, "WRONG_ORIGIN styme3279/ARENA_materials https://github.com/ARENA-education/ARENA_materials.git\n", "{branch}");
            assert!(!out.contains(&token) && !err.contains(&token) && !out.contains("devtest-user") && !err.contains("devtest-user"), "{out}{err}");
            untouched(&s);
        }
        s.git(&s.work, &["checkout", "-q", "autocommit-x"]);
        // The right fetch URL, but a push URL elsewhere: refused, naming where the push would go.
        s.git(&s.work, &["remote", "set-url", "origin", "git@github.com:Styme3279/ARENA_materials.git"]);
        s.git(&s.work, &["remote", "set-url", "--push", "origin", "https://github.com/someone/ARENA_materials"]);
        let (code, r, _) = s.backup();
        assert_eq!((code, r.wrong_origin.map(|w| w.1)), (Some(BACKUP_WRONG_ORIGIN_EXIT), Some("https://github.com/someone/ARENA_materials".into())));
        untouched(&s);
        // No origin at all.
        s.git(&s.work, &["remote", "remove", "origin"]);
        let (code, r, _) = s.backup();
        assert_eq!((code, r.wrong_origin), (Some(BACKUP_WRONG_ORIGIN_EXIT), Some(("styme3279/ARENA_materials".into(), String::new()))));
        untouched(&s);
        // The configured repo over https, credentials and all: past the check, to the push.
        s.git(&s.work, &["remote", "add", "origin", &format!("https://devtest-user:{token}@github.com/STYME3279/arena_materials")]);
        let (code, r, err) = s.backup();
        assert_eq!((code, r.wrong_origin, r.push_failed), (Some(1), None, true), "{err}");
        assert!(!err.contains(&token), "{err}");
        // scp-style, another case: checked, committed, pushed (to the stand-in for GitHub).
        s.git(&s.work, &["remote", "set-url", "origin", "git@github.com:Styme3279/ARENA_materials.git"]);
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");
        assert!(s.ssh_calls().iter().any(|c| c.contains("git-receive-pack 'Styme3279/ARENA_materials.git'")));
        let on_github = s.git(&s.base, &["--git-dir", github.to_str().unwrap(), "ls-tree", "-r", "--name-only", "autocommit-x"]);
        assert!(on_github.lines().any(|p| p == "exercises/work.py"), "{on_github}");
        // A configured owner no GitHub account can have (a typo with a `#`: its key is cut to two
        // parts, so it has none) matches nothing — not even a local path, whose key is just as
        // empty: refused, loudly, rather than let through.
        s.git(&s.work, &["remote", "set-url", "origin", s.origin.to_str().unwrap()]);
        s.want = Some("styme#3279/ARENA_materials".into());
        s.put("exercises/more.py", 10);
        let (code, r, _) = s.backup();
        assert_eq!((code, r.wrong_origin.map(|w| w.1)), (Some(BACKUP_WRONG_ORIGIN_EXIT), Some(s.origin.display().to_string())));
        // No configured repo: no check (a local origin pushes, as it always did).
        s.want = None;
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome, r.wrong_origin), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into())), None), "{err}");
    }

    /// `init-branches` would push a participant's new day branch to whatever `origin` is. Now a
    /// wrong one is refused first: no branch made, nothing fetched or pushed. The configured repo
    /// gets its branch as before.
    #[cfg(unix)]
    #[test]
    fn init_branches_never_pushes_a_new_branch_to_a_wrong_origin() {
        let s = GitSandbox::new("init-origin", 1_000_000);
        let github = s.github("Styme3279/ARENA_materials");
        let mut cfg = BackupConfig {
            repo_path: s.work.display().to_string(),
            prefix: "devtest".into(),
            week: 1,
            day: 2,
            git_ssh_key: None,
            origin: Some("styme3279/ARENA_materials".into()),
        };
        s.git(&s.work, &["remote", "set-url", "origin", "https://github.com/ARENA-education/ARENA_materials"]);
        let o = s.run(&init_branch_command(&cfg, "devtest-x"));
        let out = String::from_utf8_lossy(&o.stdout);
        assert_eq!(o.status.code(), Some(BACKUP_WRONG_ORIGIN_EXIT), "{out}{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(out, "WRONG_ORIGIN styme3279/ARENA_materials https://github.com/ARENA-education/ARENA_materials\n");
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "autocommit-x", "no branch made or switched to");
        assert!(s.git(&s.work, &["branch", "--list", "autocommit-devtest-w1d2-x"]).is_empty());
        assert!(s.ssh_calls().is_empty());
        // The configured repo (scp-style, served by the stand-in): made and pushed, as before.
        s.git(&s.work, &["remote", "set-url", "origin", "git@github.com:Styme3279/ARENA_materials.git"]);
        let o = s.run(&init_branch_command(&cfg, "devtest-x"));
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "autocommit-devtest-w1d2-x");
        s.git(&s.base, &["--git-dir", github.to_str().unwrap(), "rev-parse", "--verify", "autocommit-devtest-w1d2-x"]);
        // No configured repo: the command has no check in it at all.
        cfg.origin = None;
        assert!(!init_branch_command(&cfg, "devtest-x").contains("origin_check"));
    }

    /// The check is rendered into the script before the SKIP, from the one `origin.sh`, and the
    /// script still parses under sh and bash.
    #[test]
    fn the_origin_check_comes_first_in_the_script() {
        let s = backup_script();
        assert!(!s.contains("@ARENA_ORIGIN_SH@"));
        let check = s.find("origin_check \"$ORIGIN\"").expect("the check");
        assert!(check < s.find("echo \"SKIP $B\"").unwrap() && check < s.find("git add -A").unwrap());
        assert!(s.contains(origin_sh().trim_end()));
        assert!(s.contains("ORIGIN=${6:-}"));
    }

    /// `BUSY <branch> <what> [unmerged]`: what is in progress, said with how to end it either way —
    /// the verbs of the operation it is — and a failure at the push (the pod is copied instead).
    /// Review F4: conflicts are named only when there are unmerged paths (a merge whose conflicts
    /// are staged, a clean `git revert -n`, has no markers to commit), and then the advice starts
    /// with resolving and adding them — git refuses `git commit` / `--continue` until then. Every
    /// variant's words, pinned.
    #[test]
    fn the_busy_line_says_whats_in_progress_and_how_to_end_it() {
        let code = Some(BACKUP_BUSY_EXIT);
        let r = parse_backup_report("BRANCH autocommit-x\nBUSY autocommit-x merge unmerged\n");
        assert_eq!((r.busy.as_deref(), r.unmerged, r.branch.as_deref(), r.outcome.clone()), (Some("merge"), true, Some("autocommit-x"), None));
        assert!(r.failed_at_push(), "copied instead, like a failed push");
        assert_eq!((r.in_progress(code), r.in_progress(Some(1)), r.in_progress(Some(0))), (Some("merge"), None, None));
        assert_eq!(
            r.failure_brief(code).unwrap(),
            "a merge is in progress on autocommit-x — not committed; resolve the conflicts and `git add` them, then `git commit` \
             — or abort it (`git merge --abort`) on the pod"
        );
        assert_eq!((r.failure("/r", "p", Some(1)), r.failure_brief(Some(1))), (None, None), "only with its own exit");
        // (the BUSY line, the whole failure line)
        let cases: &[(&str, &str)] = &[
            (
                "BUSY b merge unmerged",
                "a merge is in progress on b — not committed (a backup now would commit the merge's state, unresolved conflicts \
                 and all, and end the merge); resolve the conflicts and `git add` them, then `git commit` — or abort it (`git \
                 merge --abort`) on the pod",
            ),
            (
                "BUSY b merge",
                "a merge is in progress on b — not committed (a backup now would commit the merge's state and end the merge); \
                 finish it (`git commit`) or abort it (`git merge --abort`) on the pod",
            ),
            (
                "BUSY b cherry-pick unmerged",
                "a cherry-pick is in progress on b — not committed (a backup now would commit the cherry-pick's state, unresolved \
                 conflicts and all, and end the cherry-pick); resolve the conflicts and `git add` them, then `git cherry-pick \
                 --continue` — or abort it (`git cherry-pick --abort`) on the pod",
            ),
            (
                "BUSY b cherry-pick",
                "a cherry-pick is in progress on b — not committed (a backup now would commit the cherry-pick's state and end the \
                 cherry-pick); finish it (`git cherry-pick --continue`) or abort it (`git cherry-pick --abort`) on the pod",
            ),
            (
                "BUSY b revert unmerged",
                "a revert is in progress on b — not committed (a backup now would commit the revert's state, unresolved conflicts \
                 and all, and end the revert); resolve the conflicts and `git add` them, then `git revert --continue` — or abort \
                 it (`git revert --abort`) on the pod",
            ),
            (
                "BUSY b revert",
                "a revert is in progress on b — not committed (a backup now would commit the revert's state and end the revert); \
                 finish it (`git revert --continue`) or abort it (`git revert --abort`) on the pod",
            ),
            (
                "BUSY b conflicts",
                "b has unresolved conflicts — not committed (a backup now would commit the conflicted files as they are); resolve \
                 them and `git add` them, or undo them (`git reset --merge`) on the pod",
            ),
            // Review pr4: `git am`, a rebase that kept HEAD attached, a sequence paused between
            // its commits — each with its own way out (a sequence's may be a cherry-pick's or a
            // revert's).
            (
                "BUSY b am",
                "a `git am` is in progress on b — not committed (a backup now would commit what is there in the middle of the `git \
                 am`); finish it (`git am --continue`) or abort it (`git am --abort`) on the pod",
            ),
            (
                "BUSY b am unmerged",
                "a `git am` is in progress on b — not committed (a backup now would commit what is there, unresolved conflicts and \
                 all, in the middle of the `git am`); resolve the conflicts and `git add` them, then `git am --continue` — or abort \
                 it (`git am --abort`) on the pod",
            ),
            (
                "BUSY b rebase",
                "a rebase is in progress on b — not committed (a backup now would commit what is there in the middle of the \
                 rebase); finish it (`git rebase --continue`) or abort it (`git rebase --abort`) on the pod",
            ),
            (
                "BUSY b rebase unmerged",
                "a rebase is in progress on b — not committed (a backup now would commit what is there, unresolved conflicts and \
                 all, in the middle of the rebase); resolve the conflicts and `git add` them, then `git rebase --continue` — or \
                 abort it (`git rebase --abort`) on the pod",
            ),
            (
                "BUSY b sequence",
                "a cherry-pick or revert sequence is in progress on b — not committed (a backup now would commit what is there in \
                 the middle of the sequence); finish it (`git cherry-pick --continue`, or `git revert --continue` for a revert) or \
                 abort it (`git cherry-pick --abort`, or `git revert --abort`) on the pod",
            ),
            (
                "BUSY b sequence unmerged",
                "a cherry-pick or revert sequence is in progress on b — not committed (a backup now would commit what is there, \
                 unresolved conflicts and all, in the middle of the sequence); resolve the conflicts and `git add` them, then `git \
                 cherry-pick --continue`, or `git revert --continue` for a revert — or abort it (`git cherry-pick --abort`, or `git \
                 revert --abort`) on the pod",
            ),
        ];
        for (line, want) in cases {
            let r = parse_backup_report(&format!("BRANCH b\n{line}\n"));
            assert_eq!(r.failure("/r", "p", code).unwrap(), *want, "{line}");
            assert!(!r.failure("/r", "p", code).unwrap().contains("conflict markers"), "{line}");
        }
        // Every word the script prints has its own words — none falls back to the generic ones —
        // and every line it can print is a case of the switches' refusal.
        assert_eq!(cases.len(), BUSY_LINES.len());
        for what in BUSY_WORDS {
            assert!(!busy_situation(what, "b").contains("git operation (") && busy_advice(what, false) != "finish it or abort it", "{what}");
            assert!(BUSY_LINES.iter().any(|(w, _)| *w == what), "{what}");
            let printed = if what == "conflicts" { "BUSY b conflicts".to_string() } else { format!("BUSY b {what}") };
            assert!(cases.iter().any(|(l, _)| *l == printed), "{what}");
        }
        // A conflicts line is unmerged by definition; an unknown word after the operation isn't.
        assert!(parse_backup_report("BUSY b conflicts\n").unmerged);
        assert!(!parse_backup_report("BUSY b merge later\n").unmerged);
        // The branch from the BUSY line when no BRANCH came first; a line without its word is no BUSY.
        assert_eq!(parse_backup_report("BUSY feature merge\n").branch.as_deref(), Some("feature"));
        assert_eq!(parse_backup_report("BUSY feature\n").busy, None);
        assert!(!parse_backup_report("BUSY feature\n").failed_at_push());
    }

    /// [`GitSandbox`] with the course repo as `upstream` (a clone of origin's `main`, as setup adds
    /// it) holding a change to README.md, and the participant's own change to it committed on
    /// `autocommit-x` (not pushed): `git pull upstream main` conflicts. Returns the course repo.
    #[cfg(unix)]
    fn with_conflicting_upstream(s: &GitSandbox) -> std::path::PathBuf {
        let course = s.base.join("course");
        s.git(&s.base, &["clone", "-q", "-b", "main", s.origin.to_str().unwrap(), course.to_str().unwrap()]);
        std::fs::write(course.join("README.md"), "the course's version\n").unwrap();
        s.git(&course, &["-c", "user.name=u", "-c", "user.email=u@u", "commit", "-q", "-am", "upstream"]);
        s.git(&s.work, &["remote", "add", "upstream", course.to_str().unwrap()]);
        std::fs::write(s.work.join("README.md"), "the participant's version\n").unwrap();
        s.git(&s.work, &["commit", "-q", "-am", "mine"]);
        course
    }

    /// git, expected to fail (a conflict): (exit code, stdout + stderr).
    #[cfg(unix)]
    fn git_failing(dir: &std::path::Path, args: &[&str]) -> (Option<i32>, String) {
        let o = std::process::Command::new("git").current_dir(dir).args(args).env("GIT_EDITOR", "true").output().expect("git");
        (o.status.code(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
    }

    /// Review R3: a participant mid-way through a conflicted `git pull upstream main` (setup adds
    /// the `upstream` remote) had the conflict markers committed by the next backup — a merge
    /// commit, pushed — after which `git merge --abort` and VS Code's conflict view no longer
    /// worked. Now a merge, a cherry-pick or a revert in progress, or a `git stash pop`'s
    /// conflicts, stop the run before anything is staged: `BUSY`, exit 6, HEAD, the index and the
    /// tree exactly as they were, nothing pushed — and the participant's abort still brings back
    /// the tree they had. Once it's ended the next run backs up as always, and so does a merge
    /// that went through.
    #[cfg(unix)]
    #[test]
    fn an_operation_in_progress_is_never_committed_and_can_still_be_aborted() {
        let s = GitSandbox::new("busy", 1_000_000);
        let course = with_conflicting_upstream(&s);
        // Commits for the cherry-pick (a side branch) and the revert (two edits of one line) to conflict on.
        s.git(&s.work, &["checkout", "-q", "-b", "side", "main"]);
        std::fs::write(s.work.join("README.md"), "the side branch's version\n").unwrap();
        s.git(&s.work, &["commit", "-q", "-am", "side"]);
        s.git(&s.work, &["checkout", "-q", "autocommit-x"]);
        let origin_ref = || s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "rev-parse", "refs/heads/autocommit-x"]);
        // What each case commits first (nothing, or the commits its operation conflicts over).
        type Prepare = Box<dyn Fn(&GitSandbox)>;
        // (what, its commits first, the command that conflicts, the participant's way out, its `*_HEAD`)
        type Case<'a> = (&'a str, Prepare, &'a [&'a str], &'a [&'a str], Option<&'a str>);
        let edit = |s: &GitSandbox, text: &str| {
            std::fs::write(s.work.join("README.md"), format!("{text}\n")).unwrap();
            s.git(&s.work, &["commit", "-q", "-am", text]);
        };
        let cases: Vec<Case> = vec![
            ("merge", Box::new(|_| ()), &["pull", "-q", "--no-rebase", "--no-edit", "upstream", "main"], &["merge", "--abort"], Some("MERGE_HEAD")),
            ("cherry-pick", Box::new(|_| ()), &["cherry-pick", "side"], &["cherry-pick", "--abort"], Some("CHERRY_PICK_HEAD")),
            ("revert", Box::new(move |s| ["a", "b"].iter().for_each(|v| edit(s, v))), &["revert", "--no-edit", "HEAD~1"], &["revert", "--abort"], Some("REVERT_HEAD")),
            (
                "conflicts",
                Box::new(move |s| {
                    std::fs::write(s.work.join("README.md"), "stashed\n").unwrap();
                    s.git(&s.work, &["stash", "-q"]);
                    edit(s, "committed");
                }),
                &["stash", "pop"],
                &["reset", "--merge"],
                None,
            ),
        ];
        for (what, prepare, start, abort, head_file) in &cases {
            // New work beside the operation, never staged by the backup while it lasts.
            s.put(&format!("notes-{what}.md"), 20);
            prepare(&s);
            let before = (s.git(&s.work, &["rev-parse", "HEAD"]), s.read("README.md"), s.git(&s.work, &["status", "--porcelain"]));
            let (code, said) = git_failing(&s.work, start);
            assert_ne!(code, Some(0), "{what}: no conflict: {said}");
            let during = s.git(&s.work, &["status", "--porcelain"]);
            assert!(during.contains("UU README.md"), "{what}: {during}");
            let pushed = origin_ref();
            let (code, out, err) = s.backup_raw();
            assert_eq!(code, Some(BACKUP_BUSY_EXIT), "{what}: {out}{err}");
            // Conflicted, so `unmerged` after an operation's word (a stash pop's conflicts are that already).
            let printed = if *what == "conflicts" { what.to_string() } else { format!("{what} unmerged") };
            assert_eq!(out, format!("BRANCH autocommit-x\nBUSY autocommit-x {printed}\n"), "{what}: nothing else done");
            assert_eq!(s.git(&s.work, &["rev-parse", "HEAD"]), before.0, "{what}: nothing committed");
            assert_eq!(s.git(&s.work, &["status", "--porcelain"]), during, "{what}: nothing staged");
            assert_eq!(origin_ref(), pushed, "{what}: nothing pushed");
            if let Some(f) = head_file {
                assert!(s.work.join(".git").join(f).exists(), "{what}: {f} gone");
            }
            let words = parse_backup_report(&out).failure("/r", "p", code).unwrap();
            assert!(words.contains(&abort.join(" ")) && words.contains("`git add` them"), "{what}: says how to abort, and to add first: {words}");
            if *what == "merge" {
                // Why the words say to resolve and add first: git refuses the commit until then (review F4).
                let (c, said) = git_failing(&s.work, &["commit", "--no-edit"]);
                assert!(c != Some(0) && said.contains("unmerged"), "{said}");
            }
            // The participant's way out still works, and brings back what they had.
            s.git(&s.work, abort);
            let after = (s.git(&s.work, &["rev-parse", "HEAD"]), s.read("README.md"), s.git(&s.work, &["status", "--porcelain"]));
            assert_eq!(after, before, "{what}: the abort restores the tree");
            // Ended: backed up as always.
            let (code, r, err) = s.backup();
            assert_eq!((code, r.outcome, r.busy), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into())), None), "{what}: {err}");
        }
        assert_eq!(s.git(&s.work, &["stash", "list"]).lines().count(), 1, "the conflicted stash is kept");

        // Nothing unmerged, still in progress (review F4): a clean `git revert -n` (git 2.43 leaves
        // REVERT_HEAD), and a conflicted merge whose conflicts are resolved and added. BUSY without
        // `unmerged`, words that name no conflicts — and the way out they give works as said.
        s.git(&s.work, &["revert", "-n", "HEAD"]);
        assert!(s.work.join(".git/REVERT_HEAD").exists() && s.git(&s.work, &["ls-files", "-u"]).is_empty());
        let (code, out, _) = s.backup_raw();
        assert_eq!((code, out.as_str()), (Some(BACKUP_BUSY_EXIT), "BRANCH autocommit-x\nBUSY autocommit-x revert\n"));
        assert_eq!(
            parse_backup_report(&out).failure("/r", "p", code).unwrap(),
            "a revert is in progress on autocommit-x — not committed (a backup now would commit the revert's state and end the \
             revert); finish it (`git revert --continue`) or abort it (`git revert --abort`) on the pod"
        );
        s.git(&s.work, &["-c", "core.editor=true", "revert", "--continue"]);
        assert_eq!(s.backup().1.outcome, Some((BACKUP_PUSHED, "autocommit-x".into())), "finished: backed up");
        git_failing(&s.work, &["pull", "-q", "--no-rebase", "--no-edit", "upstream", "main"]);
        std::fs::write(s.work.join("README.md"), "resolved\n").unwrap();
        s.git(&s.work, &["add", "README.md"]);
        let (code, out, _) = s.backup_raw();
        assert_eq!((code, out.as_str()), (Some(BACKUP_BUSY_EXIT), "BRANCH autocommit-x\nBUSY autocommit-x merge\n"));
        assert_eq!(
            parse_backup_report(&out).failure("/r", "p", code).unwrap(),
            "a merge is in progress on autocommit-x — not committed (a backup now would commit the merge's state and end the \
             merge); finish it (`git commit`) or abort it (`git merge --abort`) on the pod"
        );
        s.git(&s.work, &["commit", "-q", "--no-edit"]);
        assert_eq!(s.backup().1.outcome, Some((BACKUP_PUSHED, "autocommit-x".into())), "finished: backed up");

        // A merge that went through (no conflict, committed) is backed up like any commit.
        s.git(&course, &["checkout", "-q", "-b", "extra", "main~1"]);
        std::fs::write(course.join("extra.py"), "x = 1\n").unwrap();
        s.git(&course, &["add", "extra.py"]);
        s.git(&course, &["-c", "user.name=u", "-c", "user.email=u@u", "commit", "-q", "-m", "extra"]);
        s.git(&s.work, &["pull", "-q", "--no-rebase", "--no-edit", "upstream", "extra"]);
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome, r.busy), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into())), None), "{err}");
        assert_eq!(origin_ref(), s.git(&s.work, &["rev-parse", "HEAD"]));
        assert_eq!(s.git(&s.work, &["rev-list", "--parents", "-n1", "HEAD"]).split(' ').count(), 3, "a merge commit, pushed");
        assert!(s.origin_tree("autocommit-x").iter().any(|(_, p)| p == "extra.py"));
    }

    /// Checked with real git, mid-merge: `git checkout -b` (init-branches) succeeded, dropped
    /// MERGE_HEAD and carried the conflicted index onto the new branch — no merge left to abort —
    /// and a gentle `git checkout` ends a merge whose conflicts are resolved and staged. Now both
    /// refuse first, touching nothing, with what is in progress and how to end it (exit 6); the
    /// merge can still be aborted afterwards. `--hard` goes ahead and ends it with the other local
    /// changes it discards, as `--hard` does (set-branch copies such a pod first and says so).
    #[cfg(unix)]
    #[test]
    fn a_gentle_switch_or_init_branches_never_ends_an_operation_in_progress() {
        let s = GitSandbox::new("busy-switch", 1_000_000);
        with_conflicting_upstream(&s);
        let repo = s.work.to_str().unwrap();
        let cfg = BackupConfig { repo_path: repo.into(), prefix: "devtest".into(), week: 1, day: 2, git_ssh_key: None, origin: None };
        let head = s.git(&s.work, &["rev-parse", "HEAD"]);
        let mine = s.read("README.md");
        git_failing(&s.work, &["pull", "-q", "--no-rebase", "--no-edit", "upstream", "main"]);
        let during = s.git(&s.work, &["status", "--porcelain"]);
        let merge = "a merge is in progress on autocommit-x — not switched (a switch now would end the merge); resolve the \
                     conflicts and `git add` them, then `git commit` — or abort it (`git merge --abort`) on the pod, then switch \
                     again\n";
        for (cmd, who) in [(checkout_command(repo, "main", None, false), "arena set-branch"), (init_branch_command(&cfg, "devtest-x"), "arena init-branches")] {
            let o = s.run(&cmd);
            let err = String::from_utf8_lossy(&o.stderr);
            assert_eq!((o.status.code(), err.as_ref()), (Some(BACKUP_BUSY_EXIT), format!("{who}: {merge}").as_str()), "{who}");
            assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "autocommit-x", "{who}: not switched");
            assert_eq!(s.git(&s.work, &["rev-parse", "HEAD"]), head, "{who}");
            assert_eq!(s.git(&s.work, &["status", "--porcelain"]), during, "{who}: nothing touched");
            assert!(s.work.join(".git/MERGE_HEAD").exists(), "{who}: the merge ended");
        }
        assert!(s.git(&s.work, &["branch", "--list", "autocommit-devtest-w1d2-x"]).is_empty(), "no day branch made");
        // Resolved and staged, still a merge: still refused (git's own checkout would carry it off).
        std::fs::write(s.work.join("README.md"), "resolved\n").unwrap();
        s.git(&s.work, &["add", "README.md"]);
        let o = s.run(&checkout_command(repo, "main", None, false));
        assert_eq!(
            (o.status.code(), String::from_utf8_lossy(&o.stderr).into_owned()),
            (
                Some(BACKUP_BUSY_EXIT),
                "arena set-branch: a merge is in progress on autocommit-x — not switched (a switch now would end the merge); \
                 finish it (`git commit`) or abort it (`git merge --abort`) on the pod, then switch again\n"
                    .to_string()
            ),
            "nothing unmerged now: the plain way to finish it"
        );
        s.git(&s.work, &["merge", "--abort"]);
        assert_eq!((s.git(&s.work, &["rev-parse", "HEAD"]), s.read("README.md")), (head.clone(), mine), "aborted as if never switched");

        // A `git stash pop`'s conflicts: refused too, with their own way out — whichever directory
        // of the checkout the command starts in (the conflict is at its top).
        s.put("exercises/keep.py", 3);
        let sub = BackupConfig { repo_path: s.work.join("exercises").display().to_string(), ..cfg.clone() };
        std::fs::write(s.work.join("README.md"), "stashed\n").unwrap();
        s.git(&s.work, &["stash", "-q"]);
        std::fs::write(s.work.join("README.md"), "committed\n").unwrap();
        s.git(&s.work, &["commit", "-q", "-am", "committed"]);
        git_failing(&s.work, &["stash", "pop"]);
        let o = s.run(&init_branch_command(&sub, "devtest-x"));
        assert_eq!(
            (o.status.code(), String::from_utf8_lossy(&o.stderr).into_owned()),
            (
                Some(BACKUP_BUSY_EXIT),
                "arena init-branches: autocommit-x has unresolved conflicts — not switched; resolve them and `git add` them, or \
                 undo them (`git reset --merge`) on the pod, then switch again\n"
                    .to_string()
            )
        );
        s.git(&s.work, &["reset", "--merge"]);

        // --hard: switched, the merge ended with the local changes it discards.
        let tip = s.git(&s.work, &["rev-parse", "HEAD"]);
        git_failing(&s.work, &["pull", "-q", "--no-rebase", "--no-edit", "upstream", "main"]);
        assert!(s.work.join(".git/MERGE_HEAD").exists());
        let o = s.run(&checkout_command(repo, "main", None, true));
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
        assert!(!s.work.join(".git/MERGE_HEAD").exists() && s.git(&s.work, &["ls-files", "-u"]).is_empty());
        assert_eq!(s.git(&s.work, &["rev-parse", "autocommit-x"]), tip, "its commits stay in the branch");
        // A clean checkout switches as before (the guard lets it through).
        let o = s.run(&checkout_command(repo, "autocommit-x", None, false));
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "autocommit-x");
    }

    /// The check is rendered into the script once, after the branch is known (the SKIP before it)
    /// and before anything is staged — the nested-clone adoption, `update-index`, `git add` — and
    /// called again right before the staging and right before the commit (codex #1).
    #[test]
    fn the_busy_check_comes_before_anything_is_staged() {
        let s = backup_script();
        assert!(!s.contains("@ARENA_BUSY_OP_SH@") && s.matches(BUSY_OP_SH).count() == 1);
        assert!(s.contains("busy_stop() {\n    W=$(busy_op \"$G\")\n    if [ -n \"$W\" ]; then echo \"BUSY $B $W\"; exit 6; fi\n}"));
        let calls: Vec<usize> = s.match_indices("\nbusy_stop\n").map(|(i, _)| i).collect();
        assert_eq!(calls.len(), 3, "first, before the staging, before the commit");
        let check = calls[0];
        assert!(s.find("echo \"SKIP $B\"").unwrap() < check && s.find("G=$(git rev-parse --absolute-git-dir)").unwrap() < check);
        let branch = s.find("echo \"BRANCH $B\"").unwrap();
        for later in ["update-index", "git add -A", "git rm -q --cached", "git commit", "git push"] {
            assert!(check < branch + s[branch..].find(later).unwrap(), "{later}");
        }
        // Right before each: nothing in between.
        assert!(s.contains("\nbusy_stop\ngit add -A -- \"$@\"\n") && calls[1] == s.find("\nbusy_stop\ngit add -A").unwrap());
        assert!(calls[2] == s.find("\nbusy_stop\nif ! git diff --cached --quiet; then git commit").unwrap());
        // The gentle switch and init-branches check it too, first; --hard doesn't.
        let gentle = checkout_command("/r", "main", None, false);
        assert!(gentle.contains(BUSY_OP_SH) && gentle.find("busy_op").unwrap() < gentle.find("git fetch").unwrap());
        assert!(!checkout_command("/r", "main", None, true).contains("busy_op"));
        let init = init_branch_command(&cfg(), "arena8-apple");
        assert!(init.find("busy_op").unwrap() < init.find("git checkout -b").unwrap());
        // On a branch it skips, the work there is only said, read-only, before the SKIP.
        let skip = &s[s.find("if [ \"$B\" = main ]").unwrap()..s.find("echo \"SKIP $B\"").unwrap()];
        assert!(skip.contains("echo \"UNCOMMITTED $B $W\"") && skip.contains("tracked_changed \"$GD\""), "{skip}");
        // Changes to tracked files: through a copy of the index, never the participant's (claude #9).
        let tracked = &s[s.find("tracked_changed() {").unwrap()..s.find("if [ \"$B\" = main ]").unwrap()];
        assert!(tracked.contains("cp \"$1/index\" \"$_ti\"") && tracked.contains("rm -f \"$_ti\" \"$_ti.lock\""), "{tracked}");
        for plumbing in ["git update-index -q --refresh", "git diff-index --quiet HEAD --"] {
            assert!(tracked.contains(&format!("GIT_INDEX_FILE=\"$_ti\" {plumbing}")), "{plumbing}");
        }
        // Commits no remote has, untracked files (review pr4): read-only too.
        assert!(skip.contains("git --no-optional-locks rev-list -n1 HEAD --not --remotes"), "{skip}");
        assert!(skip.contains("git --no-optional-locks ls-files --others --exclude-standard"), "{skip}");
        let order = ["W=changes", "W=commits", "W=untracked"].map(|w| skip.find(w).unwrap_or_else(|| panic!("{w}")));
        assert!(order.windows(2).all(|p| p[0] < p[1]), "{order:?}");
        // The --hard switch ends a `git am`, rebase or sequence once it has switched; gentle never does.
        let hard = checkout_command("/r", "main", None, true);
        let quits = hard.find("git rebase --quit").expect("quit");
        assert!(hard.find("git reset --hard").unwrap() < quits && hard.contains("git am --quit") && hard.contains("git cherry-pick --quit"));
        assert!(!checkout_command("/r", "main", None, false).contains("--quit"));
        for write in ["git add", "update-index", "git commit", "git push", "git stash", "git reset"] {
            assert!(!skip.contains(write), "{write}");
        }
    }

    /// Review fb2 F3: a pod on main — no backup commits there — with a merge in progress or
    /// changes to tracked files was reset by `set-branch --hard` with no copy: its backup said
    /// only `SKIP`. Now the run says what a reset could discard first (`UNCOMMITTED main <what>`),
    /// still exit 0 and SKIP, touching nothing — not even the index's stat cache. Review pr4 adds
    /// commits no remote has (a participant's work committed on main, its tree clean, was reset
    /// away uncopied) and untracked files (`--hard` overwrites one the new branch tracks:
    /// [`an_untracked_file_the_new_branch_tracks_is_overwritten_by_hard_so_any_is_said`]), on main
    /// or a detached HEAD. A fresh pod — main as on origin, a nested clone as the image ships
    /// them, an ignored file — says nothing, so `--hard` copies none of them.
    #[cfg(unix)]
    #[test]
    fn on_main_the_work_a_hard_reset_would_discard_is_said_before_the_skip() {
        let s = GitSandbox::new("uncommitted", 1_000_000);
        with_conflicting_upstream(&s);
        s.git(&s.work, &["checkout", "-q", "main"]);
        let index = || std::fs::read(s.work.join(".git/index")).unwrap();
        let said = |want: &str| {
            let before = (s.git(&s.work, &["rev-parse", "HEAD"]), s.git(&s.work, &["status", "--porcelain"]));
            let index_before = index();
            let (code, out, err) = s.backup_raw();
            assert_eq!(index(), index_before, "{want}: the index untouched");
            assert_eq!((code, out.as_str()), (Some(0), want), "{err}");
            assert_eq!((s.git(&s.work, &["rev-parse", "HEAD"]), s.git(&s.work, &["status", "--porcelain"])), before, "{want}");
            parse_backup_report(&out)
        };
        let r = said("SKIP main\n");
        assert_eq!((r.uncommitted.as_deref(), r.uncommitted_why()), (None, None), "clean");
        // What a fresh pod has besides: a nested clone (its own .git, listed as `dir/`), an ignored file.
        s.clone_upstream("exercises/clone");
        std::fs::write(s.work.join(".git/info/exclude"), "*.log\n").unwrap();
        std::fs::write(s.work.join("run.log"), "ignored\n").unwrap();
        assert_eq!(said("SKIP main\n").uncommitted, None, "nothing of the participant's");
        s.put("scratch.py", 4);
        let r = said("UNCOMMITTED main untracked\nSKIP main\n");
        assert_eq!(r.uncommitted_why().unwrap(), "main has untracked files — no backup commits on main");
        std::fs::write(s.work.join("README.md"), "edited on main\n").unwrap();
        let r = said("UNCOMMITTED main changes\nSKIP main\n");
        assert_eq!(r.outcome, Some((BACKUP_SKIPPED, "main".into())));
        assert_eq!(r.uncommitted_why().unwrap(), "main has uncommitted changes to tracked files — no backup commits on main");
        // Committed on main, not pushed: the tree is clean, the commit is on no remote.
        s.git(&s.work, &["commit", "-q", "-am", "committed on main"]);
        std::fs::remove_file(s.work.join("scratch.py")).unwrap();
        let r = said("UNCOMMITTED main commits\nSKIP main\n");
        assert!(!r.unmerged && r.busy.is_none() && !r.failed_at_push(), "{r:?}");
        assert_eq!(r.uncommitted_why().unwrap(), "main has commits no remote has — no backup pushes main");
        // A conflicted `git pull upstream main` on main itself.
        git_failing(&s.work, &["pull", "-q", "--no-rebase", "--no-edit", "upstream", "main"]);
        assert!(s.work.join(".git/MERGE_HEAD").exists());
        let r = said("UNCOMMITTED main merge unmerged\nSKIP main\n");
        assert!(r.unmerged && r.busy.is_none() && !r.failed_at_push(), "{r:?}");
        assert_eq!(r.uncommitted_why().unwrap(), "a merge is in progress on main — no backup commits on main");
        s.git(&s.work, &["merge", "--abort"]);
        assert_eq!(said("UNCOMMITTED main commits\nSKIP main\n").uncommitted.as_deref(), Some("commits"), "aborted: the commit is still only here");
        // Once a remote has it (init-branches pushed it on the day's branch), nothing.
        s.git(&s.work, &["push", "-q", "origin", "main:refs/heads/autocommit-y"]);
        assert_eq!(said("SKIP main\n").uncommitted, None);
        // A detached HEAD: its own commits, else nothing.
        s.git(&s.work, &["checkout", "-q", "--detach", "origin/main"]);
        assert_eq!(said("SKIP HEAD\n").uncommitted, None);
        std::fs::write(s.work.join("README.md"), "committed on a detached HEAD\n").unwrap();
        s.git(&s.work, &["commit", "-q", "-am", "detached"]);
        let r = said("UNCOMMITTED HEAD commits\nSKIP HEAD\n");
        assert_eq!(r.uncommitted_why().unwrap(), "a detached HEAD has commits no remote has — no backup pushes a detached HEAD");
        // Parsed only with its words; never on a run that didn't skip.
        assert_eq!(parse_backup_report("UNCOMMITTED main\nSKIP main\n").uncommitted, None);
        assert_eq!(parse_backup_report("UNCOMMITTED main changes\nBRANCH b\nPUSHED b\n").uncommitted_why(), None);
        assert_eq!(
            parse_backup_report("UNCOMMITTED HEAD conflicts\nSKIP HEAD\n").uncommitted_why().unwrap(),
            "a detached HEAD has unresolved conflicts — no backup commits on a detached HEAD"
        );
        assert_eq!(
            parse_backup_report("UNCOMMITTED HEAD rebase unmerged\nSKIP HEAD\n").uncommitted_why().unwrap(),
            "a rebase is in progress on a detached HEAD — no backup commits on a detached HEAD"
        );
    }

    /// Review pr4: `set-branch --hard`'s `checkout -f` and `reset --hard` overwrite an untracked
    /// file wherever the branch they reset to tracks the same path — checked here with real git:
    /// a participant's `solutions.py` on main, never committed, becomes the one the course staff
    /// pushed to origin's main since. Which path origin's branch tracks, the backup can't tell: it
    /// doesn't know the branch, and the pod's `origin/main` doesn't have the new file until the
    /// switch's own fetch (the backup here runs before it, as set-branch's does). So any untracked
    /// file on a branch no backup commits is said (`UNCOMMITTED main untracked`), and `--hard`
    /// copies the pod first. An untracked file the new branch doesn't track stays.
    #[cfg(unix)]
    #[test]
    fn an_untracked_file_the_new_branch_tracks_is_overwritten_by_hard_so_any_is_said() {
        let s = GitSandbox::new("untracked-hard", 1_000_000);
        let repo = s.work.to_str().unwrap();
        s.git(&s.work, &["checkout", "-q", "main"]);
        std::fs::write(s.work.join("solutions.py"), "the participant's own work\n").unwrap();
        std::fs::write(s.work.join("notes.md"), "notes\n").unwrap();
        // The staff push a solutions.py to the cohort's main; the pod hasn't fetched it.
        let staff = s.base.join("staff");
        s.git(&s.base, &["clone", "-q", "-b", "main", s.origin.to_str().unwrap(), staff.to_str().unwrap()]);
        std::fs::write(staff.join("solutions.py"), "the staff's solutions\n").unwrap();
        s.git(&staff, &["add", "solutions.py"]);
        s.git(&staff, &["-c", "user.name=u", "-c", "user.email=u@u", "commit", "-q", "-m", "solutions"]);
        s.git(&staff, &["push", "-q", "origin", "main"]);
        assert!(s.git(&s.work, &["ls-tree", "-r", "--name-only", "origin/main"]).lines().all(|p| p != "solutions.py"), "not fetched yet");
        let (code, out, err) = s.backup_raw();
        assert_eq!((code, out.as_str()), (Some(0), "UNCOMMITTED main untracked\nSKIP main\n"), "{err}");
        // What the copy is for: --hard makes the participant's file the staff's.
        let o = s.run(&checkout_command(repo, "main", None, true));
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(std::fs::read_to_string(s.work.join("solutions.py")).unwrap(), "the staff's solutions\n", "overwritten");
        assert_eq!(std::fs::read_to_string(s.work.join("notes.md")).unwrap(), "notes\n", "one the branch doesn't track stays");
        // The gentle switch refuses instead (git's own check), so it needs no copy.
        s.git(&s.work, &["reset", "-q", "--hard", "HEAD~1"]);
        std::fs::write(s.work.join("solutions.py"), "the participant's own work\n").unwrap();
        let o = s.run(&checkout_command(repo, "main", None, false));
        assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("would be overwritten"), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(std::fs::read_to_string(s.work.join("solutions.py")).unwrap(), "the participant's own work\n");
    }

    /// Review pr4: `git am` keeps HEAD attached, and a patch that doesn't apply leaves no unmerged
    /// path and no `*_HEAD` — only `rebase-apply/` — so the backup committed and pushed what the
    /// participant had half-applied by hand, mid-`am`; a rebase (`rebase-merge/`) and a cherry-pick
    /// sequence paused between its commits (`sequencer/` alone, one conflict resolved and committed
    /// by hand) went unseen too. Now each is `BUSY` on a branch the backup commits (`am`,
    /// `sequence`) and said before the SKIP elsewhere (`UNCOMMITTED HEAD rebase …`): nothing
    /// staged, committed or pushed; the gentle switch and init-branches refuse it, its own way out
    /// named; `--hard` switches and ends it, so the init and the backup after it go on; and the way
    /// out each names works.
    #[cfg(unix)]
    #[test]
    fn a_git_am_a_rebase_or_a_paused_sequence_is_never_committed_or_switched_away_from() {
        let s = GitSandbox::new("busy-am", 1_000_000);
        let repo = s.work.to_str().unwrap();
        let cfg = BackupConfig { repo_path: repo.into(), prefix: "devtest".into(), week: 1, day: 2, git_ssh_key: None, origin: None };
        let origin_ref = || s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "rev-parse", "refs/heads/autocommit-x"]);
        let state = || (s.git(&s.work, &["rev-parse", "HEAD"]), s.git(&s.work, &["status", "--porcelain"]));
        let refused = |cmd: &str, want: &str| {
            let before = state();
            let o = s.run(cmd);
            assert_eq!((o.status.code(), String::from_utf8_lossy(&o.stderr).into_owned()), (Some(BACKUP_BUSY_EXIT), want.to_string()));
            assert_eq!(state(), before, "{want}: nothing touched");
        };
        // A patch of README.md from a side branch, and three commits to cherry-pick (the first conflicting).
        s.git(&s.work, &["checkout", "-q", "-b", "side", "main"]);
        std::fs::write(s.work.join("README.md"), "the side branch's version\n").unwrap();
        s.git(&s.work, &["commit", "-q", "-am", "side"]);
        let patches = s.base.join("patches");
        s.git(&s.work, &["format-patch", "-q", "-1", "-o", patches.to_str().unwrap()]);
        let patch = std::fs::read_dir(&patches).unwrap().next().unwrap().unwrap().path();
        for f in ["x.py", "y.py"] {
            s.put(f, 3);
            s.git(&s.work, &["add", f]);
            s.git(&s.work, &["commit", "-q", "-m", f]);
        }
        s.git(&s.work, &["checkout", "-q", "autocommit-x"]);
        std::fs::write(s.work.join("README.md"), "the participant's version\n").unwrap();
        s.git(&s.work, &["commit", "-q", "-am", "mine"]);
        assert_eq!(s.backup().1.outcome, Some((BACKUP_PUSHED, "autocommit-x".into())));

        // `git am`: the patch doesn't apply; the participant applies it by hand, half-way.
        let (code, said) = git_failing(&s.work, &["am", patch.to_str().unwrap()]);
        assert_ne!(code, Some(0), "{said}");
        assert!(s.work.join(".git/rebase-apply/applying").exists() && s.git(&s.work, &["ls-files", "-u"]).is_empty());
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "autocommit-x", "HEAD attached");
        std::fs::write(s.work.join("README.md"), "half-applied by hand\n").unwrap();
        let (before, pushed) = (state(), origin_ref());
        let (code, out, err) = s.backup_raw();
        assert_eq!((code, out.as_str()), (Some(BACKUP_BUSY_EXIT), "BRANCH autocommit-x\nBUSY autocommit-x am\n"), "{err}");
        assert_eq!((state(), origin_ref()), (before, pushed), "nothing staged, committed or pushed");
        assert_eq!(
            parse_backup_report(&out).failure("/r", "p", code).unwrap(),
            "a `git am` is in progress on autocommit-x — not committed (a backup now would commit what is there in the middle of \
             the `git am`); finish it (`git am --continue`) or abort it (`git am --abort`) on the pod"
        );
        // git's own checkout takes the `am` along to the other branch; the switches refuse.
        let am = "a `git am` is in progress on autocommit-x — not switched (a switch now would carry the `git am`, half-done, onto \
                  the other branch); finish it (`git am --continue`) or abort it (`git am --abort`) on the pod, then switch again\n";
        refused(&checkout_command(repo, "main", None, false), &format!("arena set-branch: {am}"));
        refused(&init_branch_command(&cfg, "devtest-x"), &format!("arena init-branches: {am}"));
        assert!(s.work.join(".git/rebase-apply").exists());
        // Its way out works; the next backup goes on.
        s.git(&s.work, &["am", "--abort"]);
        assert!(!s.work.join(".git/rebase-apply").exists());
        // (`am --abort` keeps an edit made since: the backup commits it, now the `am` is over.)
        assert_eq!(s.read("README.md"), b"half-applied by hand\n");
        assert_eq!(s.backup().1.outcome, Some((BACKUP_PUSHED, "autocommit-x".into())));

        // A cherry-pick of three commits: the first conflicts (`cherry-pick unmerged`); resolved and
        // committed by hand, two still to come: only `sequencer/` is left (`sequence`).
        let (code, said) = git_failing(&s.work, &["cherry-pick", "main..side"]);
        assert_ne!(code, Some(0), "{said}");
        let (code, out, _) = s.backup_raw();
        assert_eq!((code, out.as_str()), (Some(BACKUP_BUSY_EXIT), "BRANCH autocommit-x\nBUSY autocommit-x cherry-pick unmerged\n"));
        std::fs::write(s.work.join("README.md"), "resolved\n").unwrap();
        s.git(&s.work, &["add", "README.md"]);
        s.git(&s.work, &["commit", "-q", "--no-edit"]);
        assert!(s.work.join(".git/sequencer").is_dir() && !s.work.join(".git/CHERRY_PICK_HEAD").exists());
        let (before, pushed) = (state(), origin_ref());
        let (code, out, _) = s.backup_raw();
        assert_eq!((code, out.as_str()), (Some(BACKUP_BUSY_EXIT), "BRANCH autocommit-x\nBUSY autocommit-x sequence\n"));
        assert_eq!((state(), origin_ref()), (before, pushed), "the commit made by hand isn't pushed mid-sequence either");
        refused(
            &init_branch_command(&cfg, "devtest-x"),
            "arena init-branches: a cherry-pick or revert sequence is in progress on autocommit-x — not switched (a switch now \
             would carry the sequence, half-done, onto the other branch); finish it (`git cherry-pick --continue`, or `git revert \
             --continue` for a revert) or abort it (`git cherry-pick --abort`, or `git revert --abort`) on the pod, then switch \
             again\n",
        );
        s.git(&s.work, &["-c", "core.editor=true", "cherry-pick", "--continue"]);
        assert!(!s.work.join(".git/sequencer").exists());
        assert_eq!(s.backup().1.outcome, Some((BACKUP_PUSHED, "autocommit-x".into())), "finished: backed up");
        let pushed: Vec<String> = s.origin_tree("autocommit-x").into_iter().map(|(_, p)| p).collect();
        assert!(pushed.contains(&"x.py".to_string()) && pushed.contains(&"y.py".to_string()), "{pushed:?}");

        // A rebase detaches HEAD: said before the SKIP; --hard switches and ends it, and the init goes on.
        let (code, said) = git_failing(&s.work, &["rebase", "side"]);
        assert_ne!(code, Some(0), "{said}");
        assert!(s.work.join(".git/rebase-merge").is_dir());
        let (code, out, _) = s.backup_raw();
        assert_eq!((code, out.as_str()), (Some(0), "UNCOMMITTED HEAD rebase unmerged\nSKIP HEAD\n"));
        let o = s.run(&checkout_command(repo, "main", None, true));
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
        assert!(!s.work.join(".git/rebase-merge").exists(), "ended with the switch");
        assert_eq!(s.backup_raw().1, "SKIP main\n");

        // `git am` on main: said before the SKIP; --hard ends it too, and init-branches goes on.
        std::fs::write(s.work.join("README.md"), "edited on main\n").unwrap();
        s.git(&s.work, &["commit", "-q", "-am", "main edit"]);
        git_failing(&s.work, &["am", patch.to_str().unwrap()]);
        assert_eq!(s.backup_raw().1, "UNCOMMITTED main am\nSKIP main\n");
        let o = s.run(&checkout_command(repo, "main", None, true));
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(!s.work.join(".git/rebase-apply").exists(), "ended with the switch");
        assert_eq!(s.backup_raw().1, "SKIP main\n");
        let o = s.run(&init_branch_command(&cfg, "devtest-x"));
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), cfg.branch_for("devtest-x"));
    }

    /// The real `git`'s path, for a stand-in to hand on to.
    #[cfg(unix)]
    fn real_git() -> String {
        let o = std::process::Command::new("sh").args(["-c", "command -v git"]).output().expect("sh");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// A PATH whose `git` is the real one, but runs `script` first (`sh -c`, from where it was
    /// called) — once, its output dropped — on the call whose arguments are exactly `at`: the
    /// participant's own command, at one precise moment of a run.
    #[cfg(unix)]
    fn git_acting_at(s: &GitSandbox, at: &str, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let bin = s.base.join("bin-hook");
        std::fs::create_dir_all(&bin).unwrap();
        let once = s.base.join("hook.sh");
        let _ = std::fs::remove_file(s.base.join("hook.sh.ran"));
        std::fs::write(&once, script).unwrap();
        let once = shell_quote(once.to_str().unwrap());
        std::fs::write(
            bin.join("git"),
            format!(
                "#!/bin/sh\nif [ \"$*\" = {} ] && [ -f {once} ]; then mv {once} {once}.ran; sh {once}.ran >/dev/null 2>&1; fi\nexec {} \"$@\"\n",
                shell_quote(at),
                shell_quote(&real_git())
            ),
        )
        .unwrap();
        std::fs::set_permissions(bin.join("git"), std::fs::Permissions::from_mode(0o755)).unwrap();
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())
    }

    /// `bash -c cmd` with this PATH: (exit code, stdout, stderr).
    #[cfg(unix)]
    fn run_on_path(cmd: &str, path: &str) -> (Option<i32>, String, String) {
        let o = std::process::Command::new("bash").arg("-c").arg(cmd).env("PATH", path).output().expect("bash");
        (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
    }

    /// Review ai-review claude #6: setup adds the course repo as `upstream`, and "already pushed"
    /// meant "on any remote" — so a commit only `upstream` had (its `main` merged into the day's
    /// branch) was never scanned for keys and went to origin with the course's key-shaped example
    /// in it (or was refused by push protection every tick); and a branch made from
    /// `upstream/main`, tracking it, read as in sync (`NO_CHANGES`) though origin had none of it.
    /// Now "pushed" means on origin: the course's commit goes out redacted, the file on disk and
    /// upstream's own ref untouched, and that branch is pushed.
    #[cfg(unix)]
    #[test]
    fn a_key_in_a_commit_only_upstream_has_is_redacted_before_it_reaches_origin() {
        let s = GitSandbox::new("upstream-key", 1_000_000);
        let key = openrouter_key(21);
        let course = s.base.join("course");
        s.git(&s.base, &["clone", "-q", "-b", "main", s.origin.to_str().unwrap(), course.to_str().unwrap()]);
        let example = format!("Set your key: OPENROUTER_API_KEY = \"{key}\"\n");
        std::fs::write(course.join("course_key.md"), &example).unwrap();
        s.git(&course, &["add", "course_key.md"]);
        s.git(&course, &["-c", "user.name=u", "-c", "user.email=u@u", "commit", "-q", "-m", "course update"]);
        let theirs = s.git(&course, &["rev-parse", "HEAD"]);
        s.git(&s.work, &["remote", "add", "upstream", course.to_str().unwrap()]);
        s.git(&s.work, &["fetch", "-q", "upstream"]);
        // The participant's own work, backed up; then the course's update merged in.
        std::fs::write(s.work.join("mine.py"), "mine = 1\n").unwrap();
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");
        s.git(&s.work, &["merge", "-q", "--no-edit", "--no-ff", "upstream/main"]);
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome.clone()), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");
        assert_eq!(r.history.as_ref().map(|h| h.0), Some(2), "the course's commit and the merge, rewritten");
        assert!(!s.origin_holds(&key), "the key reached origin");
        assert_eq!(
            s.git(&s.work, &["show", "origin/autocommit-x:course_key.md"]),
            "Set your key: OPENROUTER_API_KEY = \"REDACTED-OPENROUTER-KEY\""
        );
        assert_eq!(s.read("course_key.md"), example.as_bytes(), "the file on disk keeps it");
        assert_eq!(s.git(&s.work, &["rev-parse", "upstream/main"]), theirs, "upstream's own ref untouched");
        // A branch made from upstream's main, tracking it: origin has none of it, so it's pushed
        // (redacted), and from then on tracks origin's.
        s.git(&s.work, &["checkout", "-q", "-f", "-b", "mine", "upstream/main"]);
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "mine@{u}"]), "upstream/main");
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome), (Some(0), Some((BACKUP_PUSHED, "mine".into()))), "{err}");
        assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "mine@{u}"]), "origin/mine");
        assert!(!s.origin_holds(&key));
        let (code, r, _) = s.backup();
        assert_eq!((code, r.outcome), (Some(0), Some((BACKUP_NO_CHANGES, "mine".into()))));
    }

    /// Review ai-review codex #1: the operation check ran once, before the scans. A participant's
    /// `git pull upstream main` that conflicted while the backup scanned the nested repos and the
    /// files had its conflicts staged as resolved by `add -A`; a `git merge --no-commit` begun
    /// after the staging was ended by the backup's own commit (a merge commit, pushed). Now the
    /// check runs again right before each: the run stops `BUSY`, nothing staged or committed, the
    /// operation where the participant left it. (A stand-in `git` starts the participant's command
    /// at the exact moment: the BIGSKIP scan, then the key scan.)
    #[cfg(unix)]
    #[test]
    fn an_operation_begun_during_the_scans_is_caught_before_the_staging_and_the_commit() {
        let s = GitSandbox::new("busy-late", 1_000_000);
        let course = with_conflicting_upstream(&s);
        let origin_ref = || s.git(&s.base, &["--git-dir", s.origin.to_str().unwrap(), "rev-parse", "refs/heads/autocommit-x"]);
        let script = backup_command_limited(s.work.to_str().unwrap(), None, "arena backup", s.skip, s.limit, None);
        std::fs::write(s.work.join("notes.md"), "notes\n").unwrap();
        let before = (s.git(&s.work, &["rev-parse", "HEAD"]), origin_ref());
        // A conflicted pull, begun while the files are scanned: after the first check.
        let path = git_acting_at(
            &s,
            "ls-files -z -o -m --exclude-standard",
            "git -c user.name=u -c user.email=u@u pull -q --no-rebase --no-edit upstream main\n",
        );
        let (code, out, err) = run_on_path(&script, &path);
        assert!(s.base.join("hook.sh.ran").exists(), "the pull ran mid-backup");
        assert_eq!((code, out.as_str()), (Some(BACKUP_BUSY_EXIT), "BRANCH autocommit-x\nBUSY autocommit-x merge unmerged\n"), "{err}");
        assert_eq!(s.git(&s.work, &["status", "--porcelain"]), "UU README.md\n?? notes.md", "its conflict not staged, nor anything else");
        assert_eq!((s.git(&s.work, &["rev-parse", "HEAD"]), origin_ref()), before, "nothing committed or pushed");
        s.git(&s.work, &["merge", "--abort"]);
        assert_eq!(s.read("README.md"), b"the participant's version\n", "the abort works");
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");

        // A clean merge left uncommitted (`--no-commit`), begun after the staging (at the key scan):
        // the backup's commit would have made it a merge commit and ended it.
        s.git(&course, &["checkout", "-q", "-b", "extra", "main~1"]);
        std::fs::write(course.join("extra.py"), "x = 1\n").unwrap();
        s.git(&course, &["add", "extra.py"]);
        s.git(&course, &["-c", "user.name=u", "-c", "user.email=u@u", "commit", "-q", "-m", "extra"]);
        s.git(&s.work, &["fetch", "-q", "upstream"]);
        let before = (s.git(&s.work, &["rev-parse", "HEAD"]), origin_ref());
        let path = git_acting_at(&s, "diff --cached --raw --no-renames --diff-filter=d --no-abbrev", "git merge -q --no-commit --no-ff upstream/extra\n");
        let (code, out, err) = run_on_path(&script, &path);
        assert!(s.base.join("hook.sh.ran").exists(), "the merge ran mid-backup");
        assert_eq!((code, out.as_str()), (Some(BACKUP_BUSY_EXIT), "BRANCH autocommit-x\nBUSY autocommit-x merge\n"), "{err}");
        assert_eq!((s.git(&s.work, &["rev-parse", "HEAD"]), origin_ref()), before, "not committed: still the participant's merge");
        assert!(s.work.join(".git/MERGE_HEAD").exists());
        assert_eq!(s.git(&s.work, &["diff", "--cached", "--name-only"]), "extra.py");
        // The participant finishes it; the next backup pushes their merge.
        s.git(&s.work, &["commit", "-q", "--no-edit"]);
        let (code, r, err) = s.backup();
        assert_eq!((code, r.outcome), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".into()))), "{err}");
        assert_eq!(s.git(&s.work, &["rev-list", "--parents", "-n1", "origin/autocommit-x"]).split(' ').count(), 3, "their merge, pushed");
    }

    /// Review ai-review claude #9: on main the check for changes to tracked files was the porcelain
    /// diff under `--no-optional-locks`. Now it is plumbing, as the review asked, on a copy of the
    /// index — refreshed there, `diff-index` there, removed after — so the participant's index,
    /// its bytes and its mtime, is never written; a file whose stat changed but not its content is
    /// no change (the refresh sees that: without it `diff-index` reads it as changed); a real one,
    /// in the tree or staged, still is.
    #[cfg(unix)]
    #[test]
    fn the_main_check_compares_through_a_copy_of_the_index_and_never_writes_it() {
        let s = GitSandbox::new("main-index", 1_000_000);
        s.git(&s.work, &["checkout", "-q", "main"]);
        let index = s.work.join(".git/index");
        let snap = || (std::fs::read(&index).unwrap(), std::fs::metadata(&index).unwrap().modified().unwrap());
        let touch = |rel: &str| {
            let f = std::fs::File::options().write(true).open(s.work.join(rel)).unwrap();
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000)).unwrap();
        };
        let ours = || std::fs::read_dir(s.work.join(".git")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("arena-backup")).count();
        let check = |want: &str| {
            let before = snap();
            let (code, out, err) = s.backup_raw();
            assert_eq!((code, out.as_str()), (Some(0), want), "{err}");
            assert_eq!(snap(), before, "{want}: the participant's index never written");
            assert_eq!(ours(), 0, "{want}: the copy is gone");
        };
        check("SKIP main\n");
        // Touched: its stat changed, not its content — no change.
        touch("README.md");
        check("SKIP main\n");
        // Its content changed (the same size, the same old mtime): a change.
        std::fs::write(s.work.join("README.md"), b"XXXXX").unwrap();
        touch("README.md");
        check("UNCOMMITTED main changes\nSKIP main\n");
        // Staged, the tree as staged: still a change against HEAD.
        s.git(&s.work, &["add", "README.md"]);
        check("UNCOMMITTED main changes\nSKIP main\n");
        s.git(&s.work, &["reset", "-q", "--hard"]);
        check("SKIP main\n");
    }

    /// Review ai-review claude #4, to verify: could the gentle switch's refusal of a pod mid-merge
    /// exit with the participant's real keys still set aside (the redacted placeholder left in
    /// their code)? No: the refusal comes first, before anything is set aside — checked here. But a
    /// switch that ended any other way between the set-aside and the putting back did leave them
    /// set aside: the putting back was the command's last step. Now it is its EXIT trap, and a
    /// hangup, interrupt or TERM (a dropped connection, a timeout's kill) is made an exit: a switch
    /// killed mid-way puts them back too.
    #[cfg(unix)]
    #[test]
    fn a_refused_or_killed_switch_never_leaves_the_real_keys_set_aside() {
        use std::os::unix::fs::PermissionsExt;
        let s = GitSandbox::new("keys-busy", 1_000_000);
        let repo = s.work.to_str().unwrap();
        let ex = format!("KEY = \"{}\"\n", openrouter_key(22));
        std::fs::write(s.work.join("ex.py"), &ex).unwrap();
        let (code, r, err) = s.backup();
        assert_eq!((code, r.redacted.len()), (Some(0), 1), "{err}");
        let aside = s.work.join(".git/arena-unredacted");
        // A conflicted pull of README.md; ex.py stays as it is, its real key on disk.
        let course = s.base.join("course");
        s.git(&s.base, &["clone", "-q", "-b", "main", s.origin.to_str().unwrap(), course.to_str().unwrap()]);
        std::fs::write(course.join("README.md"), "the course's version\n").unwrap();
        s.git(&course, &["-c", "user.name=u", "-c", "user.email=u@u", "commit", "-q", "-am", "upstream"]);
        s.git(&s.work, &["remote", "add", "upstream", course.to_str().unwrap()]);
        std::fs::write(s.work.join("README.md"), "the participant's version\n").unwrap();
        s.git(&s.work, &["commit", "-q", "-m", "mine", "README.md"]);
        git_failing(&s.work, &["pull", "-q", "--no-rebase", "--no-edit", "upstream", "main"]);
        let o = s.run(&checkout_command(repo, "main", None, false));
        assert_eq!(o.status.code(), Some(BACKUP_BUSY_EXIT), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(s.read("ex.py"), ex.as_bytes(), "the real key still in the file");
        assert!(!aside.exists(), "nothing set aside");
        s.git(&s.work, &["merge", "--abort"]);
        assert_eq!(s.read("ex.py"), ex.as_bytes());

        // Killed as the checkout starts (after the set-aside): a `git` that TERMs the command.
        let bin = s.base.join("bin-kill");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            bin.join("git"),
            format!(
                "#!/bin/sh\ncase \"$*\" in *\" checkout main\" | \"checkout -f -B main origin/main\") kill -TERM \"$PPID\"; exit 1;; esac\nexec {} \"$@\"\n",
                shell_quote(&real_git())
            ),
        )
        .unwrap();
        std::fs::set_permissions(bin.join("git"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        for hard in [false, true] {
            let (code, out, err) = run_on_path(&checkout_command(repo, "main", None, hard), &path);
            assert_eq!(code, Some(143), "hard={hard}: {out}{err}");
            assert_eq!(s.read("ex.py"), ex.as_bytes(), "hard={hard}: the real key put back");
            assert!(!aside.exists(), "hard={hard}");
            assert_eq!(s.git(&s.work, &["rev-parse", "--abbrev-ref", "HEAD"]), "autocommit-x", "hard={hard}");
        }
    }
}
