//! `archive sync`'s pure half: which snapshot labels to push and when one is final ("sealed"),
//! which pod dirs may leave the control box, the exact rsync argv, and what a failed push means.
//!
//! The snap tier mirrors the control box's dated layout and its hard-linking: each label
//! `<dir>/<wNdM>/` goes to `snap/<wNdM>/` with `--link-dest=/snap/<previous pushed label>/`, so an
//! unchanged file shares one inode across days on the archive too, exactly as the pulls link
//! each snapshot to the previous one. And like the pull's snapshot tier it is **two passes**
//! ([`SnapPass`]): the files under the size cap, then every `.git` uncapped — a repo's big pack
//! is over the cap, and a capped copy would be a `.git` whose refs point at objects that never
//! arrived (the pull's own lesson, `pull::PullConfig::passes`).
//!
//! What may never be in a push ([`FORBIDDEN`], test-enforced on every argv): anything that
//! deletes (`--delete*`, `--remove-*` — rrsync `-no-del` refuses them too), reads (`--sender`),
//! follows links on either side (`-L`, `-K`, `-k`, `--copy-links`), or **rewrites a file in
//! place** (`--inplace`, `--append*`): rrsync allows those, and in place on the archive means
//! on an inode that past days' snapshots share — a silent change to history. A normal push writes
//! a changed file anew (temp + rename) and never touches the old inode. `-a` is out because the
//! receiver is unprivileged (`-D`/`-o`/`-g` mean nothing there); `--chmod` is fixed so every push
//! gives every file the same mode — which is also what lets `--link-dest` match and link.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::state::SnapState;
use super::{ArchiveSettings, Tier};
use crate::restore::parse_label;

/// A label is final once a push that STARTED this long after its UTC day ended has succeeded: a
/// last pull running past midnight is still captured (spec §7.2).
pub const GRACE_SECS: u64 = 7200;

/// rsync's I/O-silence cut-off, as for every other rsync arena runs.
pub const IO_TIMEOUT_SECS: u64 = crate::pull::RSYNC_IO_TIMEOUT_SECS;

/// The control box's dirs that are never a snapshot label, whatever their name.
const NOT_LABELS: &[&str] = &[crate::restore::BIG, crate::pull::BASELINE_DIR, crate::pull::TEMP_DIR];

/// The `wNdM` labels among `names`, oldest first (by week, then day — `w10d1` after `w9d5`).
pub fn sort_labels<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut v: Vec<((u32, u32), String)> = names.into_iter().filter_map(|n| Some((parse_label(n)?, n.to_string()))).collect();
    v.sort();
    v.dedup();
    v.into_iter().map(|(_, n)| n).collect()
}

/// The directory names in `dir` (not following symlinks; hidden ones skipped).
pub fn subdirs(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') && e.file_type()?.is_dir() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// The `wNdM` snapshot labels in the backup dir, oldest first.
pub fn local_labels(dir: &Path) -> std::io::Result<Vec<String>> {
    let names = subdirs(dir)?;
    Ok(sort_labels(names.iter().map(String::as_str)))
}

/// The backup dir's custom labels (`pods pull --label mylabel`): never pushed on their own
/// (`sync --label` does), listed by `status`.
pub fn custom_labels(dir: &Path) -> Vec<String> {
    subdirs(dir).unwrap_or_default().into_iter().filter(|n| parse_label(n).is_none() && !NOT_LABELS.contains(&n.as_str())).collect()
}

/// Whether `label` can be pushed as `--label`: one plain path component, not one of the backup
/// dir's own folders.
pub fn valid_custom_label(label: &str) -> bool {
    !label.is_empty()
        && label.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        && !label.starts_with('.')
        && !NOT_LABELS.contains(&label)
}

/// The day a label stands for, as days since the epoch: the start date is `w0d1` (the clock
/// `resolve_week_day` uses). `None` for a custom label.
pub fn label_day(label: &str, start_days: i64) -> Option<i64> {
    let (w, d) = parse_label(label)?;
    (d >= 1).then(|| start_days + 7 * i64::from(w) + i64::from(d) - 1)
}

/// When the label's UTC day ends (UNIX s).
pub fn label_end(label: &str, start_days: i64) -> Option<u64> {
    u64::try_from((label_day(label, start_days)? + 1) * 86_400).ok()
}

/// Whether `label` is final on the archive: its last OK push started ≥ [`GRACE_SECS`] after its
/// day ended. A label whose day hasn't ended (clock skew included) and a custom label never are.
pub fn sealed(label: &str, state: &SnapState, start_days: i64) -> bool {
    match (label_end(label, start_days), state.labels.get(label)) {
        (Some(end), Some(rec)) => rec.ok_started >= end + GRACE_SECS,
        _ => false,
    }
}

/// The labels to push, in order: every unsealed local `wNdM` label (oldest first), then each
/// `forced` one (`--label`, pushed even if sealed). One rule covers the first run (the whole
/// history, a link-dest chain), the steady state (today, and yesterday until it seals) and a
/// backfill after an outage.
pub fn plan_snap(local: &[String], forced: &[String], state: &SnapState, start_days: i64) -> Vec<String> {
    let unsealed = local.iter().map(String::as_str).filter(|l| !sealed(l, state, start_days));
    let mut out = sort_labels(unsealed.chain(forced.iter().map(String::as_str)));
    for l in forced.iter().filter(|l| parse_label(l).is_none()) {
        if !out.contains(l) {
            out.push(l.clone());
        }
    }
    out
}

/// What `label`'s push hard-links to: the newest `wNdM` label before it that has pushed OK —
/// read at execution time, so a label pushed earlier in the same run counts, and a failed one is
/// skipped (the next links to the last good one). None for the first label or a custom one.
pub fn link_dest_for(label: &str, state: &SnapState) -> Option<String> {
    let key = parse_label(label)?;
    state
        .labels
        .iter()
        .filter(|(l, r)| r.ok_finished > 0 && parse_label(l).is_some_and(|k| k < key))
        .max_by_key(|(l, _)| parse_label(l))
        .map(|(l, _)| l.clone())
}

/// The pod dirs that may leave the control box (S11): for every cohort entry of
/// `MACHINE_NAME_LIST` (never an `@` staff box), its name and its `-old`/`-new` twins (`replace`
/// / `migrate`). An unscoped `pods backup` pulls every listed pod — staff homes included — into
/// the backup dir, so the push takes only these.
pub fn allowed_pod_dirs(prefix: &str, machine_names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in machine_names.iter().map(|e| e.trim()).filter(|e| !e.is_empty() && !crate::naming::is_absolute(e)) {
        let base = crate::naming::qualify(prefix, e);
        for d in [base.clone(), format!("{base}-old"), format!("{base}-new")] {
            if plain_name(&d) && !out.contains(&d) {
                out.push(d);
            }
        }
    }
    out
}

/// `[A-Za-z0-9._-]+`, not starting with a dot: safe as an anchored rsync rule and a path.
fn plain_name(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// Split a tree's pod dirs into the allowed ones (pushed) and the rest (kept on the control box
/// only — listed, never pushed).
pub fn split_pod_dirs(present: &[String], allowed: &[String]) -> (Vec<String>, Vec<String>) {
    present.iter().cloned().partition(|d| allowed.contains(d))
}

/// Never leave the control box, at any depth (patterns without an inner `/` match anywhere;
/// `.git/config` matches every repo's). They come first in every push's rules, so nothing after
/// them can bring one back, and `ARCHIVE_EXTRA_EXCLUDES` only adds to them. Why: `copy-keys`
/// writes per-pod OpenRouter keys and the broadcast HF/Claude tokens into `~/.bashrc`/`~/.zshrc`;
/// the pull keeps dot-*files* and `.claude/`; a remote URL can embed a token. The control box
/// keeps all of these — the archive is readable by the whole cohort.
pub const SECRET_EXCLUDES: &[&str] = &[
    // shell rc files (API keys and tokens are exported here)
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".zshrc.pre-oh-my-zsh",
    // history (secrets typed on a command line)
    ".bash_history",
    ".zsh_history",
    ".python_history",
    ".node_repl_history",
    ".psql_history",
    ".sqlite_history",
    ".lesshst",
    ".viminfo",
    ".wget-hsts",
    // credentials
    ".netrc",
    ".git-credentials",
    ".pypirc",
    ".npmrc",
    ".env",
    ".env.*",
    ".envrc",
    ".claude*",
    ".ssh/",
    ".gnupg/",
    ".aws/",
    ".docker/",
    ".kaggle/",
    ".config/",
    ".huggingface/",
    "id_rsa",
    "id_ecdsa",
    "id_ed25519",
    "id_dsa",
    // remote URLs can embed tokens (git treats a missing config as all defaults); also the
    // config of the nested clones `pods backup` folded aside (`.git.arena-nested`)
    ".git/config",
    ".git.arena-nested/config",
    // the pulls' temp files (never inside a pushed tree anyway)
    ".rsync-tmp/",
];

/// Flags no push may ever carry (S1): prefixes for long options, letters for short clusters.
/// (`--del` covers every `--delete*` and rsync's own `--del` alias.)
pub const FORBIDDEN_LONG: &[&str] = &["--del", "--remove", "--inplace", "--append", "--backup", "--sender", "--copy-links", "--archive"];
pub const FORBIDDEN_SHORT: &[char] = &['a', 'K', 'L', 'k'];

/// Whether an argv carries a forbidden flag (any position). Filter rules are written
/// `--exclude=PAT`, so a pattern can never be read as a flag.
pub fn forbidden_flag(args: &[String]) -> Option<&String> {
    args.iter().find(|a| {
        if let Some(long) = a.strip_prefix("--") {
            FORBIDDEN_LONG.iter().any(|f| long.starts_with(&f[2..]))
        } else if let Some(short) = a.strip_prefix('-') {
            !short.is_empty() && a.as_str() != "-e" && short.chars().any(|c| FORBIDDEN_SHORT.contains(&c))
        } else {
            false
        }
    })
}

/// Which of the snap tier's two passes (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapPass {
    /// Everything under the size cap.
    Files,
    /// Only `.git` contents, uncapped.
    Git,
}

/// How a push reaches the archive: rsync's `-e` (the pinned ssh, push key only) and
/// `arena-push@<ip>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTransport {
    pub rsh: String,
    pub dest: String,
}

/// One rsync push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushJob {
    pub tier: Tier,
    /// Snap only.
    pub pass: SnapPass,
    /// The snap label (`None` for big).
    pub label: Option<String>,
    /// The label it hard-links to ([`link_dest_for`]).
    pub link_dest: Option<String>,
    /// The allowed pod dirs present in the source ([`split_pod_dirs`]).
    pub pods: Vec<String>,
    /// The local source: `<abs dir>/<label>` or `<abs dir>/big` (a trailing `/` is added).
    pub src: String,
}

/// The rsync argv (after the program name) for `job` — pure, so every invariant is a table test.
///
/// ```text
/// snap: -rlpt -z --chmod=u=rwX,go=rX [--max-size=M] --timeout=300 [--bwlimit=B] --stats
///       [--link-dest=/snap/<prev>/] --exclude=<secret>… --exclude=<extra>…
///       --include=/<pod>/… --exclude=/* [git pass: --include=*/ --include=**/.git/** --exclude=*]
///       -e <pinned ssh> <src>/ arena-push@<ip>:snap/<L>/
/// big:  -rlpt --chmod=u=rwX,go=rX --partial-dir=.rsync-partial --timeout=300 [--bwlimit=B] --stats
///       --exclude=<secret>… --exclude=<extra>… --include=/<pod>/… --exclude=/*
///       -e <pinned ssh> <src>/ arena-push@<ip>:big/
/// ```
///
/// - `--link-dest` is absolute: rrsync refuses `..` and re-roots `/snap/…` under its root.
/// - Filter order matters (first match wins): secrets first, then the anchored pod includes,
///   then `/*` (any other top-level dir — a staff box, a stranger — goes no further).
/// - `-z` and the cap on snap only; `--partial-dir` on big only (a budget-killed multi-GB file
///   resumes next run).
pub fn push_args(job: &PushJob, transport: &PushTransport, settings: &ArchiveSettings) -> Vec<String> {
    let mut a: Vec<String> = vec!["-rlpt".into()];
    if job.tier == Tier::Snap {
        a.push("-z".into());
    }
    a.push("--chmod=u=rwX,go=rX".into());
    match (job.tier, job.pass) {
        (Tier::Snap, SnapPass::Files) => a.push(format!("--max-size={}", settings.snap_max_size)),
        (Tier::Snap, SnapPass::Git) => {}
        (Tier::Big, _) => a.push("--partial-dir=.rsync-partial".into()),
    }
    a.push(format!("--timeout={IO_TIMEOUT_SECS}"));
    if let Some(b) = &settings.bwlimit {
        a.push(format!("--bwlimit={b}"));
    }
    a.push("--stats".into());
    if job.tier == Tier::Snap {
        if let Some(prev) = &job.link_dest {
            a.push(format!("--link-dest=/snap/{prev}/"));
        }
    }
    for s in SECRET_EXCLUDES.iter().copied().chain(settings.extra_excludes.iter().map(String::as_str)) {
        a.push(format!("--exclude={s}"));
    }
    for pod in &job.pods {
        a.push(format!("--include=/{pod}/"));
    }
    a.push("--exclude=/*".into());
    if job.tier == Tier::Snap && job.pass == SnapPass::Git {
        a.extend(["--include=*/", "--include=**/.git/**", "--exclude=*"].map(String::from));
    }
    a.push("-e".into());
    a.push(transport.rsh.clone());
    a.push(format!("{}/", job.src.trim_end_matches('/')));
    a.push(match (job.tier, &job.label) {
        (Tier::Snap, Some(l)) => format!("{}:snap/{l}/", transport.dest),
        (Tier::Snap, None) => format!("{}:snap/", transport.dest),
        (Tier::Big, _) => format!("{}:big/", transport.dest),
    });
    a
}

/// Why a push failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PushFailure {
    /// The archive didn't answer (down, network, DNS).
    Unreachable,
    /// Its host key isn't the pinned one (or none is pinned).
    HostKey,
    /// The push key was refused.
    AuthDenied,
    /// The volume is full — bytes or inodes.
    NoSpace,
    /// The volume isn't mounted: the push hit the root-owned immutable mountpoint.
    NotMounted,
    /// rrsync refused the argv — a contract bug in arena.
    Refused,
    /// The budget ran out, or rsync's I/O timeout.
    Timeout,
    Other,
}

impl std::fmt::Display for PushFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Classify a finished push (§7.5): `Ok(note)` for success (exit 0; exit 24 — files vanished
/// mid-push, a pull was writing — is OK with a note), else the failure class. The most specific
/// signal wins: rrsync's refusal, the host key, the key, space, the mountpoint, then timeouts and
/// reachability; exit 255 alone means ssh never got through.
pub fn classify_push(code: Option<i32>, stderr: &str, timed_out: bool) -> Result<Option<String>, PushFailure> {
    if timed_out {
        return Err(PushFailure::Timeout);
    }
    match code {
        Some(0) => return Ok(None),
        Some(24) => return Ok(Some("some files vanished while being sent (a pull was writing them) — the next push takes them".into())),
        _ => {}
    }
    let has = |n: &str| stderr.contains(n);
    if has("rrsync error:") {
        return Err(PushFailure::Refused);
    }
    if has("Host key verification failed")
        || has("HOST IDENTIFICATION HAS CHANGED")
        || (has("No ") && has("host key is known"))
        || has("Host key for ") && has("has changed")
    {
        return Err(PushFailure::HostKey);
    }
    if has("Permission denied (publickey") {
        return Err(PushFailure::AuthDenied);
    }
    if has("No space left on device") || has("Disk quota exceeded") {
        return Err(PushFailure::NoSpace);
    }
    if not_mounted(stderr) {
        return Err(PushFailure::NotMounted);
    }
    if code == Some(30) {
        return Err(PushFailure::Timeout);
    }
    let unreachable = [
        "Connection refused",
        "Connection timed out",
        "No route to host",
        "Network is unreachable",
        "Could not resolve hostname",
        "Connection reset by peer",
        "kex_exchange_identification",
        "Connection closed by",
    ];
    if code == Some(255) || unreachable.iter().any(|n| has(n)) {
        return Err(PushFailure::Unreachable);
    }
    Err(PushFailure::Other)
}

/// A permission error creating/writing a TOP-level `snap/<x>` or `big/<x>` path: the unmounted,
/// root-owned, immutable mountpoint (a mounted volume's root belongs to the push user).
fn not_mounted(stderr: &str) -> bool {
    stderr.lines().filter(|l| l.contains("Permission denied") || l.contains("Operation not permitted")).any(|l| {
        l.split('"').skip(1).step_by(2).any(|path| {
            let rel = path.strip_prefix(super::PUSH_ROOT).map(|p| p.trim_start_matches('/')).unwrap_or(path);
            let parts: Vec<&str> = rel.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
            matches!(parts.first(), Some(&"snap") | Some(&"big")) && parts.len() <= 2
        })
    })
}

/// What to do about a failure, for the push's `✗` line.
pub fn failure_hint(f: PushFailure, tier: Tier, current_gb: u32) -> String {
    match f {
        PushFailure::NoSpace => {
            let more = if current_gb == 0 { "<N>".to_string() } else { (current_gb.saturating_mul(3) / 2).div_ceil(10).saturating_mul(10).max(current_gb + 10).to_string() };
            format!("arena archive resize {} {more} (bytes or inodes: `arena archive status`)", tier.gb_flag())
        }
        PushFailure::NotMounted => "the volume isn't mounted — `arena archive status`, then `arena archive up` re-mounts it".into(),
        PushFailure::HostKey => {
            "the archive's host key isn't the pinned one — `arena archive status`; re-pin only with `arena archive up \
             --accept-new-host-key` if you know why it changed"
                .into()
        }
        PushFailure::AuthDenied => "the push key was refused — `arena archive up` re-installs the key files".into(),
        PushFailure::Unreachable => "the archive didn't answer — `arena archive status` (the label stays unsealed and backfills)".into(),
        PushFailure::Refused => "rrsync refused the push: a bug in arena's push arguments — please report it".into(),
        PushFailure::Timeout => format!(
            "ran out of its budget (ARCHIVE_{}_TIMEOUT_SECS) or rsync's I/O timeout — the next run continues",
            tier.as_str().to_uppercase()
        ),
        PushFailure::Other => String::new(),
    }
}

/// Every pod dir under the backup dir's labels and `big/` that is NOT allowed — what `sync` and
/// `status` list as "kept on the control box only".
pub fn control_box_only(dir: &Path, labels: &[String], allowed: &[String]) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for tree in labels.iter().map(String::as_str).chain([crate::restore::BIG]) {
        for d in subdirs(&dir.join(tree)).unwrap_or_default() {
            if !allowed.contains(&d) {
                out.insert(d);
            }
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::state::LabelRecord;
    use crate::config::Config;

    fn settings(extra: &str) -> ArchiveSettings {
        ArchiveSettings::from_config(&Config::parse(extra)).unwrap()
    }
    fn transport() -> PushTransport {
        PushTransport { rsh: "ssh -p 22 -i /st/push_ed25519 -o IdentitiesOnly=yes".into(), dest: "arena-push@192.0.2.1".into() }
    }
    fn job(tier: Tier, pass: SnapPass, link: Option<&str>) -> PushJob {
        PushJob {
            tier,
            pass,
            label: (tier == Tier::Snap).then(|| "w1d3".to_string()),
            link_dest: link.map(String::from),
            pods: vec!["arena9-apple".into(), "arena9-bloom-new".into()],
            src: if tier == Tier::Snap { "/b/w1d3".into() } else { "/b/big".into() },
        }
    }
    fn rec(started: u64) -> LabelRecord {
        LabelRecord { ok_started: started, ok_finished: started + 60, files: 1, bytes: 1 }
    }
    const START: i64 = 20_000; // w0d1 = day 20000 (2024-10-04)
    fn day_start(label: &str) -> u64 {
        (label_day(label, START).unwrap() as u64) * 86_400
    }

    /// S1: no push argv, in any combination, carries a forbidden flag.
    #[test]
    fn no_push_ever_carries_a_deleting_reading_link_following_or_in_place_flag() {
        let mut n = 0;
        for tier in Tier::ALL {
            for pass in [SnapPass::Files, SnapPass::Git] {
                for link in [None, Some("w1d2")] {
                    for cfg in ["", "ARCHIVE_BWLIMIT=0", "ARCHIVE_EXTRA_EXCLUDES=*.ckpt,secret/", "ARCHIVE_EXTRA_EXCLUDES=x --delete,y -a"] {
                        let a = push_args(&job(tier, pass, link), &transport(), &settings(cfg));
                        assert_eq!(forbidden_flag(&a), None, "{tier} {pass:?} {link:?} {cfg}: {a:?}");
                        n += 1;
                    }
                }
            }
        }
        assert_eq!(n, 32);
        // The checker itself catches each one.
        for bad in ["--delete", "--del", "--delete-after", "--remove-source-files", "--inplace", "--append-verify", "--backup", "--sender", "--copy-links", "-a", "-avz", "-K", "-L", "-rlptk", "--archive"] {
            assert!(forbidden_flag(&[bad.to_string()]).is_some(), "{bad}");
        }
        for ok in ["-rlpt", "-z", "-e", "--exclude=--delete", "--stats", "--partial-dir=.rsync-partial"] {
            assert!(forbidden_flag(&[ok.to_string()]).is_none(), "{ok}");
        }
    }

    #[test]
    fn snap_and_big_argv_exactly() {
        let a = push_args(&job(Tier::Snap, SnapPass::Files, Some("w1d2")), &transport(), &settings(""));
        let head: Vec<&str> = a.iter().take(8).map(String::as_str).collect();
        assert_eq!(head, ["-rlpt", "-z", "--chmod=u=rwX,go=rX", "--max-size=50M", "--timeout=300", "--bwlimit=10M", "--stats", "--link-dest=/snap/w1d2/"]);
        let tail: Vec<&str> = a[a.len() - 7..].iter().map(String::as_str).collect();
        assert_eq!(
            tail,
            [
                "--include=/arena9-apple/",
                "--include=/arena9-bloom-new/",
                "--exclude=/*",
                "-e",
                "ssh -p 22 -i /st/push_ed25519 -o IdentitiesOnly=yes",
                "/b/w1d3/",
                "arena-push@192.0.2.1:snap/w1d3/",
            ]
        );
        let big = push_args(&job(Tier::Big, SnapPass::Files, Some("w1d2")), &transport(), &settings("ARCHIVE_BWLIMIT=0"));
        let head: Vec<&str> = big.iter().take(5).map(String::as_str).collect();
        assert_eq!(head, ["-rlpt", "--chmod=u=rwX,go=rX", "--partial-dir=.rsync-partial", "--timeout=300", "--stats"]);
        assert!(!big.iter().any(|x| x == "-z" || x.starts_with("--max-size") || x.starts_with("--link-dest") || x.starts_with("--bwlimit")), "{big:?}");
        assert_eq!(big.last().unwrap(), "arena-push@192.0.2.1:big/");
        assert_eq!(big[big.len() - 2], "/b/big/");
        // The snap .git pass: uncapped, the same link-dest, only .git contents after the pods.
        let git = push_args(&job(Tier::Snap, SnapPass::Git, Some("w1d2")), &transport(), &settings(""));
        assert!(!git.iter().any(|x| x.starts_with("--max-size")), "{git:?}");
        assert!(git.contains(&"--link-dest=/snap/w1d2/".to_string()));
        let at = |x: &str| git.iter().position(|y| y == x).unwrap_or_else(|| panic!("{x} missing"));
        assert!(at("--exclude=/*") < at("--include=*/") && at("--include=*/") < at("--include=**/.git/**") && at("--include=**/.git/**") < at("--exclude=*"));
        // No link-dest without one; no partial-dir on snap; no `..` anywhere.
        let first = push_args(&job(Tier::Snap, SnapPass::Files, None), &transport(), &settings(""));
        assert!(!first.iter().any(|x| x.starts_with("--link-dest") || x.starts_with("--partial-dir") || x.contains("..")), "{first:?}");
    }

    /// S12 + S11 + S17 in the argv: every secret is excluded before any include, extras only add,
    /// the pods are anchored includes and everything else at the top is excluded; the temp dir
    /// of the pulls is excluded too and never a source.
    #[test]
    fn secrets_first_then_the_allowlisted_pods_then_nothing_else() {
        for tier in Tier::ALL {
            let a = push_args(&job(tier, SnapPass::Files, None), &transport(), &settings("ARCHIVE_EXTRA_EXCLUDES=*.ckpt, notes/private/"));
            let pos = |x: &str| a.iter().position(|y| y == x).unwrap_or_else(|| panic!("{x} missing: {a:?}"));
            let last_secret = SECRET_EXCLUDES.iter().map(|s| pos(&format!("--exclude={s}"))).max().unwrap();
            let first_include = a.iter().position(|x| x.starts_with("--include=")).unwrap();
            assert!(last_secret < first_include);
            assert!(pos("--exclude=*.ckpt") < first_include && pos("--exclude=notes/private/") < first_include);
            assert!(pos("--include=/arena9-apple/") < pos("--exclude=/*"));
            assert_eq!(a.iter().filter(|x| x.starts_with("--include=/")).count(), 2);
            assert!(a.contains(&"--exclude=.rsync-tmp/".to_string()));
            assert!(!a.iter().any(|x| x.contains(".rsync-tmp") && !x.starts_with("--exclude=")), "{a:?}");
        }
        for must in [".bashrc", ".zshrc", ".claude*", ".ssh/", ".env", ".netrc", ".git-credentials", ".git/config", ".bash_history", ".config/", "id_ed25519"] {
            assert!(SECRET_EXCLUDES.contains(&must), "{must}");
        }
    }

    #[test]
    fn extra_excludes_cant_clear_or_reinclude() {
        for bad in ["!", "+ .bashrc", "- x", "-x"] {
            let e = ArchiveSettings::from_config(&Config::parse(&format!("ARCHIVE_EXTRA_EXCLUDES={bad}"))).unwrap_err();
            assert!(e.contains("ARCHIVE_EXTRA_EXCLUDES"), "{bad}: {e}");
        }
    }

    #[test]
    fn labels_sort_by_week_then_day_and_skip_the_backup_dirs_own_folders() {
        assert_eq!(sort_labels(["w10d1", "w9d5", "big", "w0d1", "baseline", "mylabel", "w1d7", ".rsync-tmp"]), ["w0d1", "w1d7", "w9d5", "w10d1"]);
        assert_eq!(label_day("w0d1", START), Some(START));
        assert_eq!(label_day("w1d1", START), Some(START + 7));
        assert_eq!(label_day("w2d3", START), Some(START + 16));
        assert_eq!(label_day("w0d0", START), None);
        assert_eq!(label_day("mylabel", START), None);
        assert_eq!(label_end("w0d1", START), Some((START as u64 + 1) * 86_400));
        // The same clock as the pull's label.
        let (w, d) = crate::schedule::week_day(START, START + 16);
        assert_eq!(format!("w{w}d{d}"), "w2d3");
        assert!(valid_custom_label("mylabel") && valid_custom_label("pre-reset.2"));
        for bad in ["", "big", "baseline", ".rsync-tmp", "a/b", "..", ".x", "a b"] {
            assert!(!valid_custom_label(bad), "{bad:?}");
        }
    }

    /// S18 and §14.5: sealing needs a push that started ≥ 2 h after the day ended.
    #[test]
    fn sealing_needs_a_push_started_two_hours_after_the_day() {
        let end = day_start("w0d2");
        let mut st = SnapState::default();
        for (started, want) in [(end - 60, false), (end + 7 * 60, false), (end + GRACE_SECS - 1, false), (end + GRACE_SECS, true), (end + 2 * 3600 + 300, true)] {
            st.labels.insert("w0d1".into(), rec(started));
            assert_eq!(sealed("w0d1", &st, START), want, "push started {}s after midnight", started as i64 - end as i64);
        }
        // A custom label is never sealed; a future one isn't until its day passes.
        st.labels.insert("mylabel".into(), rec(u64::MAX / 2));
        assert!(!sealed("mylabel", &st, START));
        st.labels.insert("w5d1".into(), rec(day_start("w5d1") + 3600));
        assert!(!sealed("w5d1", &st, START));
        // No record: not sealed.
        assert!(!sealed("w0d3", &st, START));
    }

    #[test]
    fn plan_snap_covers_first_run_steady_state_midnight_backfill_and_forced() {
        let local: Vec<String> = ["w0d1", "w0d2", "w0d3", "w0d4"].map(String::from).to_vec();
        let st = SnapState::default();
        // First run: everything, oldest first (a link-dest chain).
        assert_eq!(plan_snap(&local, &[], &st, START), local);
        // Steady state on w0d4 at 10:00: w0d1..w0d2 sealed, w0d3 pushed at 00:07 of w0d4 (not
        // sealed), today.
        let mut st = SnapState::default();
        st.labels.insert("w0d1".into(), rec(day_start("w0d2") + 3 * 3600));
        st.labels.insert("w0d2".into(), rec(day_start("w0d3") + 2 * 3600 + 5 * 60));
        st.labels.insert("w0d3".into(), rec(day_start("w0d4") + 7 * 60));
        assert_eq!(plan_snap(&local, &[], &st, START), ["w0d3", "w0d4"]);
        // The 02:05 push seals w0d3.
        st.labels.insert("w0d3".into(), rec(day_start("w0d4") + 2 * 3600 + 5 * 60));
        assert_eq!(plan_snap(&local, &[], &st, START), ["w0d4"]);
        // Backfill after an outage (gaps included): every unsealed one, in order.
        let st2 = {
            let mut s = SnapState::default();
            s.labels.insert("w0d1".into(), rec(day_start("w0d2") + 3 * 3600));
            s
        };
        let gappy: Vec<String> = ["w0d1", "w0d3", "w1d2", "w0d4"].map(String::from).to_vec();
        assert_eq!(plan_snap(&gappy, &[], &st2, START), ["w0d3", "w0d4", "w1d2"]);
        // Forced: a sealed label and a custom one, pushed anyway (custom last).
        assert_eq!(plan_snap(&local, &["mylabel".into(), "w0d1".into()], &st, START), ["w0d1", "w0d4", "mylabel"]);
    }

    #[test]
    fn link_dest_is_the_newest_earlier_ok_label() {
        let mut st = SnapState::default();
        assert_eq!(link_dest_for("w0d1", &st), None);
        st.labels.insert("w0d1".into(), rec(10));
        st.labels.insert("w0d3".into(), rec(30));
        st.labels.insert("w1d1".into(), rec(50));
        st.labels.insert("mylabel".into(), rec(60));
        // A failed middle label (no OK record): the next links to the last good one.
        assert_eq!(link_dest_for("w0d4", &st), Some("w0d3".into()));
        assert_eq!(link_dest_for("w0d2", &st), Some("w0d1".into()));
        assert_eq!(link_dest_for("w10d1", &st), Some("w1d1".into()));
        assert_eq!(link_dest_for("w0d1", &st), None);
        assert_eq!(link_dest_for("mylabel", &st), None);
        st.labels.insert("w0d3".into(), LabelRecord::default());
        assert_eq!(link_dest_for("w0d4", &st), Some("w0d1".into()));
    }

    /// S11 (rules): cohort names and their twins only — never `@` staff boxes.
    #[test]
    fn the_allowlist_is_the_cohort_and_its_twins() {
        let list: Vec<String> = ["apple", "@james-gpu", "bloom", "", "bad/name"].map(String::from).to_vec();
        let a = allowed_pod_dirs("arena9", &list);
        assert_eq!(a, ["arena9-apple", "arena9-apple-old", "arena9-apple-new", "arena9-bloom", "arena9-bloom-old", "arena9-bloom-new"]);
        let present: Vec<String> = ["arena9-apple", "james-gpu", "arena9-ghost", "arena8-apple", "arena9-bloom-old"].map(String::from).to_vec();
        let (push, keep) = split_pod_dirs(&present, &a);
        assert_eq!(push, ["arena9-apple", "arena9-bloom-old"]);
        assert_eq!(keep, ["james-gpu", "arena9-ghost", "arena8-apple"]);
    }

    /// §14.7: recorded stderr samples.
    #[test]
    fn push_failures_are_classified_from_what_rsync_and_ssh_say() {
        use PushFailure::*;
        let cases: Vec<(Option<i32>, &str, bool, Result<bool, PushFailure>)> = vec![
            (Some(0), "", false, Ok(false)),
            (Some(24), "file has vanished: \"/b/w1d1/x\"", false, Ok(true)),
            (Some(11), "rsync: [Receiver] write failed on \"/srv/arena/push/big/arena9-apple/ckpt.pt\": No space left on device (28)", false, Err(NoSpace)),
            (Some(23), "rsync: [Receiver] mkdir \"/srv/arena/push/snap/w1d1/arena9-apple/x\" failed: No space left on device (28)", false, Err(NoSpace)),
            (Some(23), "rsync: [Receiver] mkdir \"/srv/arena/push/snap/w1d1\" failed: Operation not permitted (1)", false, Err(NotMounted)),
            (Some(23), "rsync: [Receiver] mkdir \"/srv/arena/push/big/arena9-apple\" failed: Permission denied (13)", false, Err(NotMounted)),
            (Some(23), "rsync: [generator] failed to set times on \"/srv/arena/push/big/.\": Operation not permitted (1)", false, Err(NotMounted)),
            (Some(23), "rsync: [Receiver] open \"/srv/arena/push/big/arena9-apple/a/b.py\" failed: Permission denied (13)", false, Err(Other)),
            (Some(255), "@@@@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\nHost key verification failed.", false, Err(HostKey)),
            (Some(255), "No ED25519 host key is known for 192.0.2.1 and you have requested strict checking.\nHost key verification failed.", false, Err(HostKey)),
            (Some(255), "arena-push@192.0.2.1: Permission denied (publickey).", false, Err(AuthDenied)),
            (Some(255), "ssh: connect to host 192.0.2.1 port 22: Connection refused", false, Err(Unreachable)),
            (Some(255), "ssh: connect to host 192.0.2.1 port 22: Connection timed out", false, Err(Unreachable)),
            (Some(255), "", false, Err(Unreachable)),
            (Some(1), "/usr/bin/rrsync error: do not use .. in --link-dest (anchor the path at the root of your restricted dir)", false, Err(Refused)),
            (Some(1), "/usr/bin/rrsync error: option --delete has been disabled on this server.", false, Err(Refused)),
            (Some(1), "/usr/bin/rrsync error: reading from write-only server is not allowed", false, Err(Refused)),
            (Some(30), "rsync error: timeout in data send/receive (code 30)", false, Err(Timeout)),
            (None, "", true, Err(Timeout)),
            (Some(12), "rsync error: error in rsync protocol data stream (code 12)", false, Err(Other)),
        ];
        for (code, stderr, timed_out, want) in cases {
            let got = classify_push(code, stderr, timed_out).map(|n| n.is_some());
            assert_eq!(got, want, "{code:?} {stderr}");
        }
        assert!(failure_hint(PushFailure::NoSpace, Tier::Big, 200).contains("arena archive resize --big-gb 300"));
        assert!(failure_hint(PushFailure::NoSpace, Tier::Snap, 10).contains("--snap-gb 20"));
    }

    // ---- fixtures: the real rsync (and rrsync) on local trees --------------------------------

    fn have(program: &str, arg: &str) -> bool {
        std::process::Command::new(program).arg(arg).output().is_ok_and(|o| o.status.success() || !o.stdout.is_empty())
    }

    struct Tmp(std::path::PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("arena-archive-fixture-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=a", "-c", "user.email=a@b", "-c", "init.defaultBranch=main"])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", dir)
            .output()
            .unwrap()
    }

    /// A participant's home as the pull leaves it on the control box, full of secrets.
    fn hostile_home(home: &Path) {
        write(&home.join(".bashrc"), "export OPENROUTER_API_KEY=\"sk-or-v1-SECRET\"\n");
        write(&home.join(".zshrc"), "export HF_TOKEN=\"hf_SECRET\"\n");
        write(&home.join(".profile"), "SECRET\n");
        write(&home.join(".claude.json"), "{\"oauth\":\"SECRET\"}");
        write(&home.join(".claude/.credentials.json"), "SECRET");
        write(&home.join(".claude/projects/x/session.jsonl"), "SECRET transcript");
        write(&home.join(".ssh/id_ed25519"), "-----BEGIN OPENSSH PRIVATE KEY-----\nSECRET\n");
        write(&home.join(".env"), "API_KEY=SECRET\n");
        write(&home.join("proj/.env.local"), "API_KEY=SECRET\n");
        write(&home.join(".netrc"), "machine x password SECRET\n");
        write(&home.join(".git-credentials"), "https://u:SECRET@github.com\n");
        write(&home.join(".bash_history"), "export X=SECRET\n");
        write(&home.join(".python_history"), "SECRET\n");
        write(&home.join(".config/wandb/settings"), "api_key=SECRET\n");
        write(&home.join("deep/id_rsa"), "SECRET");
        write(&home.join("notes.txt"), "my work\n");
        // A real repo whose remote URL carries a token.
        let repo = home.join("ARENA_materials");
        write(&repo.join("chapter1/solution.py"), "print('work')\n");
        assert!(git(home, &["init", "-q", "ARENA_materials"]).status.success());
        assert!(git(&repo, &["add", "."]).status.success());
        assert!(git(&repo, &["commit", "-qm", "work"]).status.success());
        assert!(git(&repo, &["remote", "add", "origin", "https://x-access-token:ghp_SECRET@github.com/o/r.git"]).status.success());
        // A pack over the snap cap: the uncapped .git pass must still carry it.
        write(&repo.join(".git/objects/pack/pack-big.pack"), &"p".repeat(30_000));
        write(&home.join("checkpoint.bin"), &"c".repeat(30_000));
    }

    /// Run `job`'s push into a LOCAL `<root>/push`: the same argv minus `-e`, the remote spec and
    /// the absolute `/snap/` link-dest rewritten to their local paths (what rrsync does).
    fn push_local(job: &PushJob, settings: &ArchiveSettings, root: &Path) -> std::process::Output {
        let push = root.join("push");
        let mut a = push_args(job, &transport(), settings);
        let e = a.iter().position(|x| x == "-e").unwrap();
        a.drain(e..e + 2);
        for x in a.iter_mut() {
            if let Some(rest) = x.strip_prefix("--link-dest=/") {
                *x = format!("--link-dest={}/{rest}", push.display());
            }
            if let Some(rest) = x.strip_prefix("arena-push@192.0.2.1:") {
                *x = format!("{}/{rest}", push.display());
            }
        }
        std::fs::create_dir_all(push.join(job.tier.as_str())).unwrap();
        std::process::Command::new("rsync").args(&a).output().unwrap()
    }

    fn files_under(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if e.file_type().unwrap().is_dir() {
                    stack.push(p.clone());
                }
                out.push(p.strip_prefix(dir).unwrap().display().to_string());
            }
        }
        out.sort();
        out
    }

    /// S11 + S12 + the snap tier's layout, through the real rsync with the exact flag set: no
    /// secret of a hostile home arrives (the repo's `.git/config` included — and git still works
    /// without it), staff and unknown dirs stay behind, a second label hard-links what didn't
    /// change and leaves the first label's inodes alone, modes become readable, nothing is ever
    /// deleted, a file over the cap stays out of snap while a `.git` pack over it arrives.
    #[test]
    fn a_hostile_home_arrives_without_its_secrets_and_days_share_inodes() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if !have("rsync", "--version") || !have("git", "--version") {
            eprintln!("rsync/git missing — skipping");
            return;
        }
        let t = tmp("hostile");
        let src = t.0.join("backup");
        let settings = settings("BACKUP_MAX_SIZE=10K");
        hostile_home(&src.join("w0d1/arena9-apple"));
        write(&src.join("w0d1/james-gpu/staff.txt"), "staff");
        write(&src.join("w0d1/arena9-ghost/x.txt"), "ghost");
        write(&src.join("w0d1/arena9-apple/private.txt"), "mine");
        std::fs::set_permissions(src.join("w0d1/arena9-apple/private.txt"), std::fs::Permissions::from_mode(0o600)).unwrap();
        let allowed = allowed_pod_dirs("arena9", &["apple".to_string(), "@james-gpu".to_string()]);
        let present = subdirs(&src.join("w0d1")).unwrap();
        let (pods, kept) = split_pod_dirs(&present, &allowed);
        assert_eq!(kept, ["arena9-ghost", "james-gpu"]);
        let job = |label: &str, pass, link: Option<&str>| PushJob {
            tier: Tier::Snap,
            pass,
            label: Some(label.into()),
            link_dest: link.map(String::from),
            pods: pods.clone(),
            src: src.join(label).display().to_string(),
        };
        for pass in [SnapPass::Files, SnapPass::Git] {
            let o = push_local(&job("w0d1", pass, None), &settings, &t.0);
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
        let dest = t.0.join("push/snap/w0d1");
        let got = files_under(&dest);
        assert!(got.iter().all(|f| f.starts_with("arena9-apple")), "{got:#?}");
        for f in &got {
            let p = dest.join(f);
            if p.is_file() {
                let text = std::fs::read_to_string(&p).unwrap_or_default();
                assert!(!text.contains("SECRET"), "{f} leaked: {text}");
            }
        }
        for gone in [".bashrc", ".zshrc", ".profile", ".claude.json", ".claude", ".ssh", ".env", "proj/.env.local", ".netrc", ".git-credentials", ".bash_history", ".python_history", ".config", "deep/id_rsa", "ARENA_materials/.git/config", "checkpoint.bin"] {
            assert!(!dest.join("arena9-apple").join(gone).exists(), "{gone} arrived");
        }
        let apple = dest.join("arena9-apple");
        assert!(apple.join("notes.txt").exists() && apple.join("ARENA_materials/chapter1/solution.py").exists());
        assert!(apple.join("ARENA_materials/.git/objects/pack/pack-big.pack").exists(), "the .git pass carries what the cap left out");
        let log = git(&apple.join("ARENA_materials"), &["log", "--oneline"]);
        assert!(log.status.success() && String::from_utf8_lossy(&log.stdout).contains("work"), "{}", String::from_utf8_lossy(&log.stderr));
        assert_eq!(std::fs::metadata(apple.join("private.txt")).unwrap().permissions().mode() & 0o777, 0o644, "a 0600 file arrives go+r");

        // The next day: unchanged files are hard links; a changed file is new and day 1 keeps its own.
        std::fs::create_dir_all(src.join("w0d2")).unwrap();
        let cp = std::process::Command::new("cp").arg("-a").arg(src.join("w0d1/arena9-apple")).arg(src.join("w0d2/")).status().unwrap();
        assert!(cp.success());
        write(&src.join("w0d2/arena9-apple/notes.txt"), "my work, day 2\n");
        std::fs::remove_file(src.join("w0d2/arena9-apple/private.txt")).unwrap();
        for pass in [SnapPass::Files, SnapPass::Git] {
            let o = push_local(&job("w0d2", pass, Some("w0d1")), &settings, &t.0);
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
        let d2 = t.0.join("push/snap/w0d2/arena9-apple");
        let ino = |p: &Path| std::fs::metadata(p).unwrap().ino();
        let sol = "ARENA_materials/chapter1/solution.py";
        assert_eq!(ino(&apple.join(sol)), ino(&d2.join(sol)), "unchanged: one inode");
        assert!(std::fs::metadata(d2.join(sol)).unwrap().nlink() > 1);
        assert_eq!(ino(&apple.join("ARENA_materials/.git/objects/pack/pack-big.pack")), ino(&d2.join("ARENA_materials/.git/objects/pack/pack-big.pack")));
        assert_ne!(ino(&apple.join("notes.txt")), ino(&d2.join("notes.txt")));
        assert_eq!(std::fs::read_to_string(apple.join("notes.txt")).unwrap(), "my work\n", "day 1 is untouched");
        // Nothing is deleted: pushing day 1 again after its source lost a file keeps the file.
        std::fs::remove_file(src.join("w0d1/arena9-apple/notes.txt")).unwrap();
        let o = push_local(&job("w0d1", SnapPass::Files, None), &settings, &t.0);
        assert!(o.status.success());
        assert!(apple.join("notes.txt").exists(), "never deleted on the archive");
        assert!(!d2.join("private.txt").exists() && apple.join("private.txt").exists());
    }

    /// S2: the real rrsync (`-wo -no-del -no-lock`, through a shim standing in for sshd's forced
    /// command) accepts the exact snap and big argv — absolute link-dest, partial-dir, chmod,
    /// max-size, bwlimit — even two at once, and refuses a delete, a read and a `..`.
    #[test]
    fn rrsync_accepts_exactly_our_pushes_and_refuses_the_rest() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let rrsync = ["/usr/bin/rrsync", "/usr/share/doc/rsync/scripts/rrsync"].into_iter().find(|p| Path::new(p).exists());
        let (Some(rrsync), true, true) = (rrsync, have("rsync", "--version"), have("python3", "--version")) else {
            eprintln!("rsync/rrsync/python3 missing — skipping");
            return;
        };
        let t = tmp("rrsync");
        let push = t.0.join("push");
        std::fs::create_dir_all(push.join("snap")).unwrap();
        std::fs::create_dir_all(push.join("big")).unwrap();
        let shim = t.0.join("shim");
        std::fs::write(
            &shim,
            format!("#!/bin/sh
while [ $# -gt 0 ] && [ \"$1\" != rsync ]; do shift; done
SSH_ORIGINAL_COMMAND=\"$*\" exec {rrsync} -wo -no-del -no-lock {}
", push.display()),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tr = PushTransport { rsh: format!("{} -p 22 -o BatchMode=yes", shim.display()), dest: "arena-push@archive".into() };
        let src = t.0.join("backup");
        for l in ["w0d1", "w0d2"] {
            write(&src.join(format!("{l}/arena9-apple/a.py")), "same\n");
            write(&src.join(format!("{l}/arena9-apple/.git/HEAD")), "ref: refs/heads/main\n");
        }
        write(&src.join("big/arena9-apple/ckpt.pt"), "weights");
        let s = settings("");
        let run = |args: &[String]| std::process::Command::new("rsync").args(args).output().unwrap();
        let job = |label: &str, pass, link: Option<&str>| PushJob {
            tier: Tier::Snap,
            pass,
            label: Some(label.into()),
            link_dest: link.map(String::from),
            pods: vec!["arena9-apple".into()],
            src: src.join(label).display().to_string(),
        };
        for (label, link) in [("w0d1", None), ("w0d2", Some("w0d1"))] {
            for pass in [SnapPass::Files, SnapPass::Git] {
                let o = run(&push_args(&job(label, pass, link), &tr, &s));
                assert!(o.status.success(), "{label} {pass:?}: {}", String::from_utf8_lossy(&o.stderr));
            }
        }
        let ino = |p: &Path| std::fs::metadata(p).unwrap().ino();
        assert_eq!(ino(&push.join("snap/w0d1/arena9-apple/a.py")), ino(&push.join("snap/w0d2/arena9-apple/a.py")), "absolute link-dest, re-rooted");
        let big = PushJob { tier: Tier::Big, pass: SnapPass::Files, label: None, link_dest: None, pods: vec!["arena9-apple".into()], src: src.join("big").display().to_string() };
        // Two pushes at once (-no-lock): snap and big side by side.
        let (a, b) = (push_args(&job("w0d2", SnapPass::Files, Some("w0d1")), &tr, &s), push_args(&big, &tr, &s));
        let h1 = std::process::Command::new("rsync").args(&a).spawn().unwrap();
        let h2 = std::process::Command::new("rsync").args(&b).spawn().unwrap();
        assert!(h1.wait_with_output().unwrap().status.success() && h2.wait_with_output().unwrap().status.success());
        assert!(push.join("big/arena9-apple/ckpt.pt").exists());
        // Refused: a delete, a read, a `..` link-dest — each classified as rrsync's refusal.
        let mut del = a.clone();
        del.insert(0, "--delete".into());
        let mut dotdot = a.clone();
        for x in dotdot.iter_mut().filter(|x| x.starts_with("--link-dest=")) {
            *x = "--link-dest=../w0d1/".into();
        }
        let read: Vec<String> = ["-rlpt", "-e", tr.rsh.as_str(), "arena-push@archive:snap/w0d1/", t.0.join("stolen").to_str().unwrap()].map(String::from).to_vec();
        for (what, args) in [("delete", del), ("read", read), ("..", dotdot)] {
            let o = run(&args);
            assert!(!o.status.success(), "{what} was accepted");
            let err = String::from_utf8_lossy(&o.stderr);
            assert_eq!(classify_push(o.status.code(), &err, false), Err(PushFailure::Refused), "{what}: {err}");
        }
        assert!(!t.0.join("stolen").exists());
    }

    #[test]
    fn custom_and_control_box_only_dirs_are_found_on_disk() {
        let d = std::env::temp_dir().join(format!("arena-archive-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for p in ["w0d1/arena9-apple", "w0d1/james-gpu", "big/arena9-apple", "big/arena9-ghost", "baseline/w0/fleet", ".rsync-tmp", "mylabel/arena9-apple"] {
            std::fs::create_dir_all(d.join(p)).unwrap();
        }
        std::fs::write(d.join("w0d2"), "a file, not a label").unwrap();
        assert_eq!(local_labels(&d).unwrap(), ["w0d1"]);
        assert_eq!(custom_labels(&d), ["mylabel"]);
        let allowed = allowed_pod_dirs("arena9", &["apple".to_string()]);
        assert_eq!(control_box_only(&d, &["w0d1".to_string()], &allowed), ["arena9-ghost", "james-gpu"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
