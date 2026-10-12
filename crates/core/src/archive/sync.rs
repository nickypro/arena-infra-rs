//! `archive sync`'s pure half: which snapshot labels to push and when one is final ("sealed"),
//! which pod dirs may leave the control box, the exact rsync argv, and what a failed push means.
//!
//! The snap tier mirrors the control box's dated layout and its hard-linking, pod by pod as the
//! pulls do it (`pull::snapshot_link_dests`): `<dir>/<wNdM>/<pod>/` goes to `snap/<wNdM>/<pod>/`
//! with `--link-dest` to that pod's earlier labels on the archive and to the week's fleet
//! baseline (`<dir>/baseline/w<N>/fleet/`, pushed to `snap/baseline/`). So an unchanged file shares
//! one inode across days, and the image's files — the same on every pod — across pods too. Every
//! earlier label that is on the archive counts as a link source, complete or cut short (rsync
//! links only identical files), so one failed push never turns the following days into full
//! copies; and a label pushed without the links it should have had is never sealed, so a later
//! push re-links it ([`sealed`]). And like the pull's snapshot tier each push is **two passes**
//! ([`SnapPass`]): the files under the size cap, then every `.git` uncapped — a repo's big pack is
//! over the cap, and a capped copy would be a `.git` whose refs point at objects that never
//! arrived (`pull::PullConfig::passes`).
//!
//! What may never be in a push ([`FORBIDDEN_LONG`], test-enforced on every argv): anything that
//! deletes (`--delete*`, `--remove-*` — rrsync `-no-del` refuses them too), reads (`--sender`),
//! follows links on either side (`-L`, `-K`, `-k`, `--copy-links`), keeps a per-directory holding
//! area in the pushed tree (`--partial-dir`, `--delay-updates`: a participant's symlink of that
//! name would make the archive's rsync empty another pod's directory — review finding
//! ARCH-SEC-1), or **rewrites a file in place** (`--inplace`, `--append*`): rrsync allows those,
//! and in place on the archive means on an inode that past days' snapshots share — a silent
//! change to history. A normal push writes a changed file anew (temp + rename) and never touches
//! the old inode. `-a` is out because the receiver is unprivileged (`-D`/`-o`/`-g` mean nothing
//! there); `--chmod` is fixed so every push gives every file the same mode — which is also what
//! lets `--link-dest` match and link. On the archive itself rrsync runs with `-munge`: every
//! symlink a participant planted is stored defanged (`/rsyncd-munged/…`), never followed.

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

/// Whether `label` is final on the archive — never pushed again unless asked (`--label`): its
/// last full push (every pod through) started ≥ [`GRACE_SECS`] after its day ended, linked every
/// pod to what it should have ([`super::state::LabelRecord::linked`]), and carried every allowed
/// pod the control box holds for it (`local_pods` — a pod re-added to the list, or one whose push
/// failed, keeps the label open). A label whose day hasn't ended (clock skew included) and a
/// custom label never are.
pub fn sealed(label: &str, state: &SnapState, start_days: i64, local_pods: &[String]) -> bool {
    match (label_end(label, start_days), state.labels.get(label)) {
        (Some(end), Some(rec)) => {
            rec.ok_finished > 0 && rec.ok_started >= end + GRACE_SECS && rec.linked && local_pods.iter().all(|p| rec.pods_ok.contains(p))
        }
        _ => false,
    }
}

/// One snapshot label on the control box: its name and the allowed pod dirs it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalLabel {
    pub label: String,
    pub pods: Vec<String>,
}

/// How many older unsealed labels one run pushes besides the newest (a backlog continues on the
/// next runs): each label is up to `ARCHIVE_SNAP_TIMEOUT_SECS`, so a run stays bounded (≈ an hour
/// at the defaults) and the 15-minute cron gets its turn again.
pub const MAX_BACKLOG_PER_RUN: usize = 3;

/// What one snap run pushes, in order — and what it leaves for later runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapPlan {
    pub labels: Vec<String>,
    /// Older unsealed labels beyond [`MAX_BACKLOG_PER_RUN`]: the next runs take them.
    pub deferred: Vec<String>,
}

/// The labels to push: the **newest** local label first when it isn't final (the newest work is
/// protected soonest, even behind a backlog), then the older unsealed ones oldest first — at most
/// [`MAX_BACKLOG_PER_RUN`] of them — then each `forced` one (`--label`, pushed even if sealed or
/// deferred). Covers the first run (a backlog, worked off over a few runs as a link chain), the
/// steady state (today, and yesterday until it seals) and a backfill after an outage. Pushing the
/// newest before its predecessors costs nothing for good: it isn't sealed until it was pushed
/// with them in place, and that later push re-links its identical files.
pub fn plan_snap(local: &[LocalLabel], forced: &[String], state: &SnapState, start_days: i64) -> SnapPlan {
    let mut sorted: Vec<&LocalLabel> = local.iter().filter(|l| parse_label(&l.label).is_some()).collect();
    sorted.sort_by_key(|l| parse_label(&l.label));
    let unsealed: Vec<String> =
        sorted.iter().filter(|l| !sealed(&l.label, state, start_days, &l.pods)).map(|l| l.label.clone()).collect();
    let mut plan = SnapPlan::default();
    let newest = sorted.last().map(|l| l.label.clone());
    let mut backlog: Vec<String> = unsealed.clone();
    if let Some(n) = newest.filter(|n| unsealed.contains(n)) {
        backlog.retain(|l| *l != n);
        plan.labels.push(n);
    }
    for (i, l) in backlog.into_iter().enumerate() {
        if i < MAX_BACKLOG_PER_RUN {
            plan.labels.push(l);
        } else {
            plan.deferred.push(l);
        }
    }
    for f in sort_labels(forced.iter().map(String::as_str)).into_iter().chain(forced.iter().filter(|l| parse_label(l).is_none()).cloned()) {
        plan.deferred.retain(|d| *d != f);
        if !plan.labels.contains(&f) {
            plan.labels.push(f);
        }
    }
    plan
}

/// The most earlier labels one pod push links to (rsync takes up to 20 `--link-dest`s, and checks
/// each for every file).
pub const MAX_LINK_LABELS: usize = 6;

/// The earlier labels `pod`'s push of `label` hard-links to, newest first: every `wNdM` label
/// before it that has anything of this pod on the archive — complete or cut short — read at
/// execution time, so labels pushed earlier in the same run count. None for a custom label.
pub fn link_labels_for(label: &str, pod: &str, state: &SnapState) -> Vec<String> {
    let Some(key) = parse_label(label) else { return Vec::new() };
    let mut v: Vec<(&String, (u32, u32))> = state
        .labels
        .iter()
        .filter(|(_, r)| r.pods_tried.contains(pod))
        .filter_map(|(l, _)| Some((l, parse_label(l)?)))
        .filter(|(_, k)| *k < key)
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.into_iter().take(MAX_LINK_LABELS).map(|(l, _)| l.clone()).collect()
}

/// A fleet baseline on the control box: its week and its directory's mtime (a `pods baseline
/// --replace` makes a new one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalBaseline {
    pub week: u32,
    pub mtime: u64,
}

/// The baseline a label links to, as the pulls choose it: the newest of its week or before.
pub fn baseline_for(label: &str, baselines: &[LocalBaseline]) -> Option<LocalBaseline> {
    let (week, _) = parse_label(label)?;
    baselines.iter().filter(|b| b.week <= week).max_by_key(|b| b.week).copied()
}

/// Whether the archive holds that baseline as it is now.
pub fn baseline_pushed(b: LocalBaseline, state: &SnapState) -> bool {
    state.baselines.get(&format!("w{}", b.week)) == Some(&b.mtime)
}

/// Whether `pod`'s push of `label` links to everything it should (what lets the label seal): the
/// pod's previous local label is complete on the archive for this pod, and the label's baseline
/// (if the control box has one) is on the archive.
pub fn chain_ok(label: &str, pod: &str, local: &[LocalLabel], state: &SnapState, baseline: Option<LocalBaseline>) -> bool {
    let Some(key) = parse_label(label) else { return false };
    let prev = local
        .iter()
        .filter(|l| l.pods.iter().any(|p| p == pod))
        .filter_map(|l| Some((parse_label(&l.label)?, &l.label)))
        .filter(|(k, _)| *k < key)
        .max_by_key(|(k, _)| *k)
        .map(|(_, l)| l);
    let prev_ok = prev.is_none_or(|p| state.labels.get(p).is_some_and(|r| r.pods_ok.contains(pod)));
    prev_ok && baseline.is_none_or(|b| baseline_pushed(b, state))
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
/// `.git/config` matches every repo's, `**` crosses directories). They come first in every push's
/// rules, so nothing after them can bring one back, and `ARCHIVE_EXTRA_EXCLUDES` only adds to
/// them. Why: `copy-keys` writes per-pod OpenRouter keys and the broadcast HF/Claude tokens into
/// `~/.bashrc`/`~/.zshrc`; the pull keeps dot-*files* (at any depth), `.git/` and `.claude/`, and
/// non-dot directories (an `HF_HOME` holds `token`); a remote URL or a git `insteadOf` can embed a
/// token. The control box keeps all of these — the archive is readable by the whole cohort. (The
/// pull already leaves every other dot-*directory* — `.config/`, `.cache/`, `.aws/` … — behind;
/// they are listed here too, so a pull's filters changing can't leak them.)
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
    ".mysql_history",
    ".lesshst",
    ".viminfo",
    ".wget-hsts",
    // credential files
    ".netrc",
    "_netrc",
    ".git-credentials",
    ".gitconfig",
    ".pypirc",
    ".npmrc",
    ".yarnrc",
    ".pgpass",
    ".my.cnf",
    ".s3cfg",
    ".boto",
    ".rclone.conf",
    ".vault-token",
    ".dockercfg",
    ".env",
    ".env.*",
    ".envrc",
    ".claude*",
    "kaggle.json",
    "id_rsa",
    "id_ecdsa",
    "id_ecdsa_sk",
    "id_ed25519",
    "id_ed25519_sk",
    "id_dsa",
    "*.pem",
    "*.key",
    // a Hugging Face login: `$HF_HOME/token` (and its token list) wherever HF_HOME points
    "stored_tokens",
    "huggingface/token",
    "hf_home/token",
    "hf_cache/token",
    "hf/token",
    // credential directories
    ".ssh/",
    ".gnupg/",
    ".aws/",
    ".azure/",
    ".docker/",
    ".kaggle/",
    ".config/",
    ".cache/",
    ".huggingface/",
    ".kube/",
    ".wandb/",
    // git configs hold remote URLs (which can embed tokens) and credential helpers — git
    // treats a missing config as all defaults; also the nested clones `pods backup` folded aside
    ".git/config",
    ".git/config.worktree",
    ".git/modules/**/config",
    ".git/modules/**/config.worktree",
    ".git/worktrees/**/config.worktree",
    ".git.arena-nested/config",
    ".git.arena-nested/config.worktree",
    // rsync's own holding areas and temp dirs — never shipped, whatever they are (a participant's
    // symlink named like one is exactly ARCH-SEC-1's attack)
    ".rsync-tmp",
    ".rsync-partial",
    ".~tmp~",
];

/// Flags no push may ever carry (S1): prefixes for long options, letters for short clusters.
/// (`--del` covers every `--delete*` and rsync's own `--del` alias; `--partial` covers
/// `--partial-dir`.)
pub const FORBIDDEN_LONG: &[&str] = &[
    "--del",
    "--remove",
    "--inplace",
    "--append",
    "--backup",
    "--sender",
    "--copy-links",
    "--archive",
    "--partial",
    "--delay-updates",
    "--keep-dirlinks",
    "--temp-dir",
];
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
    /// Snap (pods and the baseline) only.
    pub pass: SnapPass,
    /// What it is, for messages: `w1d3 arena9-apple`, `baseline`, `big`.
    pub what: String,
    /// The local source directory (a trailing `/` is added).
    pub src: String,
    /// Where it goes, under rrsync's root: `snap/w1d3/arena9-apple/`, `snap/baseline/`, `big/`.
    pub dest: String,
    /// Absolute link sources under rrsync's root (`/snap/w1d2/arena9-apple/`), tried in order.
    pub link_dests: Vec<String>,
    /// The anchored rules after the secrets that pick the top level (`--include=/<pod>/`… then
    /// `--exclude=/*`); empty for a pod's push, whose whole tree is its own.
    pub top_rules: Vec<String>,
    /// Create the destination's missing parents (`--mkpath`): a pod's push into a new label.
    pub mkpath: bool,
}

/// `pod`'s push of `label` (`<dir>/<label>/<pod>/` → `snap/<label>/<pod>/`), linked to its earlier
/// labels and its baseline.
pub fn pod_job(dir: &Path, label: &str, pod: &str, pass: SnapPass, link_labels: &[String], baseline: Option<u32>) -> PushJob {
    let mut link_dests: Vec<String> = link_labels.iter().map(|l| format!("/snap/{l}/{pod}/")).collect();
    if let Some(w) = baseline {
        link_dests.push(format!("/snap/{}/w{w}/fleet/", crate::pull::BASELINE_DIR));
    }
    PushJob {
        tier: Tier::Snap,
        pass,
        what: format!("{label} {pod}"),
        src: dir.join(label).join(pod).display().to_string(),
        dest: format!("snap/{label}/{pod}/"),
        link_dests,
        top_rules: Vec::new(),
        mkpath: true,
    }
}

/// The fleet baselines' push (`<dir>/baseline/` → `snap/baseline/`): only `w<N>/fleet/` of each
/// local week, nothing else that dir may hold (an old baseline moved aside).
pub fn baseline_job(dir: &Path, weeks: &[u32], pass: SnapPass) -> PushJob {
    let mut top_rules = Vec::new();
    for w in weeks {
        top_rules.push(format!("--include=/w{w}/"));
        top_rules.push(format!("--include=/w{w}/fleet/"));
        top_rules.push(format!("--exclude=/w{w}/*"));
    }
    top_rules.push("--exclude=/*".into());
    PushJob {
        tier: Tier::Snap,
        pass,
        what: "baseline".into(),
        src: dir.join(crate::pull::BASELINE_DIR).display().to_string(),
        dest: format!("snap/{}/", crate::pull::BASELINE_DIR),
        link_dests: Vec::new(),
        top_rules,
        mkpath: false,
    }
}

/// The big tier's push (`<dir>/big/` → `big/`), only the allowed pods present.
pub fn big_job(dir: &Path, pods: &[String]) -> PushJob {
    let mut top_rules: Vec<String> = pods.iter().map(|p| format!("--include=/{p}/")).collect();
    top_rules.push("--exclude=/*".into());
    PushJob {
        tier: Tier::Big,
        pass: SnapPass::Files,
        what: "big".into(),
        src: dir.join(crate::restore::BIG).display().to_string(),
        dest: "big/".into(),
        link_dests: Vec::new(),
        top_rules,
        mkpath: false,
    }
}

/// The rsync argv (after the program name) for `job` — pure, so every invariant is a table test.
///
/// ```text
/// snap pod:  -rlpt -z --chmod=u=rwX,go=rX [--max-size=M] --timeout=300 [--bwlimit=B] --stats --mkpath
///            [--link-dest=/snap/<earlier>/<pod>/]… [--link-dest=/snap/baseline/w<N>/fleet/]
///            --exclude=<secret>… --exclude=<extra>… [git pass: --include=*/ --include=**/.git/** --exclude=*]
///            -e <pinned ssh> <dir>/<L>/<pod>/ arena-push@<ip>:snap/<L>/<pod>/
/// baseline:  the same flags, --include=/w<N>/ --include=/w<N>/fleet/ --exclude=/w<N>/* … --exclude=/*,
///            <dir>/baseline/ → snap/baseline/
/// big:       -rlpt --chmod=u=rwX,go=rX --timeout=300 [--bwlimit=B] --stats --exclude=<secret>…
///            --include=/<pod>/… --exclude=/*  <dir>/big/ → big/
/// ```
///
/// - `--link-dest`s are absolute: rrsync refuses `..` and re-roots `/snap/…` under its root.
/// - Filter order matters (first match wins): secrets first, then the anchored top-level rules
///   (any other top-level dir — a staff box, a stranger — goes no further).
/// - `-z` and the cap on snap only. No partial transfers anywhere (no holding area in a
///   participant-shaped tree, ARCH-SEC-1): a big file cut off by the budget starts over next run.
pub fn push_args(job: &PushJob, transport: &PushTransport, settings: &ArchiveSettings) -> Vec<String> {
    let mut a: Vec<String> = vec!["-rlpt".into()];
    if job.tier == Tier::Snap {
        a.push("-z".into());
    }
    a.push("--chmod=u=rwX,go=rX".into());
    if job.tier == Tier::Snap && job.pass == SnapPass::Files {
        a.push(format!("--max-size={}", settings.snap_max_size));
    }
    a.push(format!("--timeout={IO_TIMEOUT_SECS}"));
    if let Some(b) = &settings.bwlimit {
        a.push(format!("--bwlimit={b}"));
    }
    a.push("--stats".into());
    if job.mkpath {
        a.push("--mkpath".into());
    }
    for d in &job.link_dests {
        a.push(format!("--link-dest={d}"));
    }
    for s in SECRET_EXCLUDES.iter().copied().chain(settings.extra_excludes.iter().map(String::as_str)) {
        a.push(format!("--exclude={s}"));
    }
    a.extend(job.top_rules.iter().cloned());
    if job.tier == Tier::Snap && job.pass == SnapPass::Git {
        a.extend(["--include=*/", "--include=**/.git/**", "--exclude=*"].map(String::from));
    }
    a.push("-e".into());
    a.push(transport.rsh.clone());
    a.push(format!("{}/", job.src.trim_end_matches('/')));
    a.push(format!("{}:{}", transport.dest, job.dest));
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

/// Every pod dir under the backup dir's labels and `big/` that is NOT allowed, split in two:
/// named like this cohort's pods (`<prefix>-…`, not a staff `@` name) but not on the list — a pod
/// that left before the archive existed, or a misspelt list — and the rest (staff boxes, other
/// cohorts'). `sync` and `status` list both as "kept on the control box only", apart.
pub fn control_box_only(dir: &Path, labels: &[String], allowed: &[String], prefix: &str, machine_names: &[String]) -> (Vec<String>, Vec<String>) {
    let staff: Vec<String> =
        machine_names.iter().filter(|e| crate::naming::is_absolute(e)).map(|e| crate::naming::qualify(prefix, e)).collect();
    let mut cohort: BTreeSet<String> = BTreeSet::new();
    let mut other: BTreeSet<String> = BTreeSet::new();
    for tree in labels.iter().map(String::as_str).chain([crate::restore::BIG]) {
        for d in subdirs(&dir.join(tree)).unwrap_or_default() {
            if allowed.contains(&d) {
                continue;
            }
            if d.starts_with(&format!("{prefix}-")) && !staff.contains(&d) {
                cohort.insert(d);
            } else {
                other.insert(d);
            }
        }
    }
    (cohort.into_iter().collect(), other.into_iter().collect())
}

/// The local fleet baselines (`<dir>/baseline/w<N>/fleet/`, non-empty), oldest week first.
pub fn local_baselines(dir: &Path) -> Vec<LocalBaseline> {
    let base = dir.join(crate::pull::BASELINE_DIR);
    let mut out: Vec<LocalBaseline> = subdirs(&base)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|w| {
            let week: u32 = w.strip_prefix('w')?.parse().ok()?;
            let fleet = base.join(&w).join("fleet");
            let non_empty = std::fs::read_dir(&fleet).ok()?.next().is_some();
            let mtime = std::fs::metadata(&fleet).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
            non_empty.then_some(LocalBaseline { week, mtime })
        })
        .collect();
    out.sort_by_key(|b| b.week);
    out
}

/// The control box's snapshot labels with their allowed pods.
pub fn local_snapshot(dir: &Path, allowed: &[String]) -> std::io::Result<Vec<LocalLabel>> {
    let mut out = Vec::new();
    for label in local_labels(dir)? {
        let present = subdirs(&dir.join(&label))?;
        let (pods, _) = split_pod_dirs(&present, allowed);
        out.push(LocalLabel { label, pods });
    }
    Ok(out)
}

/// Bytes on disk of these trees counted once (hard links shared between them count once) — what
/// pushing them to an empty archive costs at least.
pub fn unique_bytes(trees: &[std::path::PathBuf]) -> u64 {
    use std::collections::HashSet;
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut total = 0u64;
    let mut stack: Vec<std::path::PathBuf> = trees.iter().filter(|t| t.is_dir()).cloned().collect();
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else if m.is_file() {
                #[cfg(unix)]
                let key = {
                    use std::os::unix::fs::MetadataExt;
                    (m.dev(), m.ino())
                };
                #[cfg(not(unix))]
                let key = (0, total);
                if seen.insert(key) {
                    total += m.len();
                }
            }
        }
    }
    total
}

/// `ARCHIVE_BWLIMIT` as bytes per second (rsync's units: KiB by default, `K`/`M`/`G` suffixes in
/// 1024s). `None` = unlimited or unreadable.
pub fn bwlimit_bytes(bwlimit: Option<&str>) -> Option<u64> {
    let b = bwlimit?;
    let (num, mul) = match b.chars().last()? {
        'K' | 'k' => (&b[..b.len() - 1], 1u64 << 10),
        'M' | 'm' => (&b[..b.len() - 1], 1 << 20),
        'G' | 'g' => (&b[..b.len() - 1], 1 << 30),
        c if c.is_ascii_digit() => (b, 1 << 10),
        _ => return None,
    };
    let v: f64 = num.parse().ok()?;
    (v > 0.0).then(|| (v * mul as f64) as u64)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::archive::state::LabelRecord;
    use crate::config::Config;

    fn settings(extra: &str) -> ArchiveSettings {
        ArchiveSettings::from_config(&Config::parse(extra)).unwrap()
    }
    fn transport() -> PushTransport {
        PushTransport { rsh: "ssh -p 22 -i /st/push_ed25519 -o IdentitiesOnly=yes".into(), dest: "arena-push@192.0.2.1".into() }
    }
    fn set(pods: &[&str]) -> BTreeSet<String> {
        pods.iter().map(|s| s.to_string()).collect()
    }
    /// A label pushed OK at `started` for `pods`, linked.
    fn rec(started: u64, pods: &[&str]) -> LabelRecord {
        LabelRecord { ok_started: started, ok_finished: started + 60, files: 1, bytes: 1, pods_ok: set(pods), pods_tried: set(pods), linked: true }
    }
    fn local(labels: &[(&str, &[&str])]) -> Vec<LocalLabel> {
        labels.iter().map(|(l, p)| LocalLabel { label: l.to_string(), pods: p.iter().map(|s| s.to_string()).collect() }).collect()
    }
    const START: i64 = 20_000; // w0d1 = day 20000 (2024-10-04)
    fn day_start(label: &str) -> u64 {
        (label_day(label, START).unwrap() as u64) * 86_400
    }
    const A: &[&str] = &["arena9-apple"];

    /// Every kind of push this module builds, for the table tests.
    fn all_jobs() -> Vec<PushJob> {
        let d = Path::new("/b");
        let mut v = Vec::new();
        for pass in [SnapPass::Files, SnapPass::Git] {
            v.push(pod_job(d, "w1d3", "arena9-apple", pass, &[], None));
            v.push(pod_job(d, "w1d3", "arena9-apple", pass, &["w1d2".into(), "w1d1".into()], Some(1)));
            v.push(baseline_job(d, &[0, 1], pass));
        }
        v.push(big_job(d, &["arena9-apple".into(), "arena9-bloom-new".into()]));
        v
    }

    /// S1 (and ARCH-SEC-1): no push argv, in any combination, carries a flag that deletes, reads,
    /// follows a link, rewrites in place or keeps a holding area in the pushed tree.
    #[test]
    fn no_push_ever_carries_a_deleting_reading_link_following_or_in_place_flag() {
        let mut n = 0;
        for job in all_jobs() {
            for cfg in ["", "ARCHIVE_BWLIMIT=0", "ARCHIVE_EXTRA_EXCLUDES=*.ckpt,secret/", "ARCHIVE_EXTRA_EXCLUDES=x --delete,y -a"] {
                let a = push_args(&job, &transport(), &settings(cfg));
                assert_eq!(forbidden_flag(&a), None, "{} {cfg}: {a:?}", job.what);
                n += 1;
            }
        }
        assert_eq!(n, 28);
        // The checker itself catches each one.
        for bad in [
            "--delete",
            "--del",
            "--delete-after",
            "--remove-source-files",
            "--inplace",
            "--append-verify",
            "--backup",
            "--sender",
            "--copy-links",
            "-a",
            "-avz",
            "-K",
            "-L",
            "-rlptk",
            "--archive",
            "--partial",
            "--partial-dir=.rsync-partial",
            "--delay-updates",
            "--keep-dirlinks",
            "--temp-dir=x",
        ] {
            assert!(forbidden_flag(&[bad.to_string()]).is_some(), "{bad}");
        }
        for ok in ["-rlpt", "-z", "-e", "--exclude=--delete", "--exclude=.rsync-partial", "--stats", "--mkpath", "--link-dest=/snap/w1d1/p/"] {
            assert!(forbidden_flag(&[ok.to_string()]).is_none(), "{ok}");
        }
    }

    #[test]
    fn pod_baseline_and_big_argv_exactly() {
        let d = Path::new("/b");
        let a = push_args(&pod_job(d, "w1d3", "arena9-apple", SnapPass::Files, &["w1d2".into(), "w1d1".into()], Some(1)), &transport(), &settings(""));
        let head: Vec<&str> = a.iter().take(11).map(String::as_str).collect();
        assert_eq!(
            head,
            [
                "-rlpt",
                "-z",
                "--chmod=u=rwX,go=rX",
                "--max-size=50M",
                "--timeout=300",
                "--bwlimit=10M",
                "--stats",
                "--mkpath",
                "--link-dest=/snap/w1d2/arena9-apple/",
                "--link-dest=/snap/w1d1/arena9-apple/",
                "--link-dest=/snap/baseline/w1/fleet/",
            ]
        );
        let tail: Vec<&str> = a[a.len() - 4..].iter().map(String::as_str).collect();
        assert_eq!(tail, ["-e", "ssh -p 22 -i /st/push_ed25519 -o IdentitiesOnly=yes", "/b/w1d3/arena9-apple/", "arena-push@192.0.2.1:snap/w1d3/arena9-apple/"]);
        assert!(!a.iter().any(|x| x == "--exclude=/*"), "a pod's push takes its whole (secret-filtered) tree");
        // The .git pass: uncapped, the same links, only .git contents.
        let git = push_args(&pod_job(d, "w1d3", "arena9-apple", SnapPass::Git, &["w1d2".into()], None), &transport(), &settings(""));
        assert!(!git.iter().any(|x| x.starts_with("--max-size")), "{git:?}");
        assert!(git.contains(&"--link-dest=/snap/w1d2/arena9-apple/".to_string()));
        let at = |x: &str| git.iter().position(|y| y == x).unwrap_or_else(|| panic!("{x} missing"));
        assert!(at("--exclude=.git/config") < at("--include=*/") && at("--include=*/") < at("--include=**/.git/**") && at("--include=**/.git/**") < at("--exclude=*"));
        // The baselines: only w<N>/fleet of the local weeks.
        let b = push_args(&baseline_job(d, &[0, 2], SnapPass::Files), &transport(), &settings(""));
        let rules: Vec<&str> = b.iter().filter(|x| x.starts_with("--include=/") || x.starts_with("--exclude=/")).map(String::as_str).collect();
        assert_eq!(rules, ["--include=/w0/", "--include=/w0/fleet/", "--exclude=/w0/*", "--include=/w2/", "--include=/w2/fleet/", "--exclude=/w2/*", "--exclude=/*"]);
        assert_eq!(&b[b.len() - 2..], ["/b/baseline/", "arena-push@192.0.2.1:snap/baseline/"]);
        // Big: no -z, no cap, no links, no holding area; only the allowed pods.
        let big = push_args(&big_job(d, &["arena9-apple".into()]), &transport(), &settings("ARCHIVE_BWLIMIT=0"));
        let head: Vec<&str> = big.iter().take(4).map(String::as_str).collect();
        assert_eq!(head, ["-rlpt", "--chmod=u=rwX,go=rX", "--timeout=300", "--stats"]);
        assert!(!big.iter().any(|x| x == "-z" || x.starts_with("--max-size") || x.starts_with("--link-dest") || x.starts_with("--partial") || x.starts_with("--bwlimit") || x == "--mkpath"), "{big:?}");
        assert_eq!(&big[big.len() - 6..big.len() - 4], ["--include=/arena9-apple/", "--exclude=/*"].map(String::from));
        assert_eq!(&big[big.len() - 2..], ["/b/big/", "arena-push@192.0.2.1:big/"]);
        // No `..` anywhere.
        for j in all_jobs() {
            assert!(!push_args(&j, &transport(), &settings("")).iter().any(|x| x.contains("..")), "{}", j.what);
        }
    }

    /// S12 + S11 + S17 in the argv: every secret is excluded before any include, extras only add,
    /// the top level is only the allowed pods; the pulls' temp dir and rsync's holding areas too.
    #[test]
    fn secrets_first_then_the_allowlisted_pods_then_nothing_else() {
        for job in all_jobs() {
            let a = push_args(&job, &transport(), &settings("ARCHIVE_EXTRA_EXCLUDES=*.ckpt, notes/private/"));
            let pos = |x: &str| a.iter().position(|y| y == x).unwrap_or_else(|| panic!("{x} missing: {a:?}"));
            let last_secret = SECRET_EXCLUDES.iter().map(|s| pos(&format!("--exclude={s}"))).max().unwrap();
            let first_rule = a.iter().position(|x| x.starts_with("--include=") || x == "--exclude=/*" || x == "--exclude=*").unwrap_or(usize::MAX);
            assert!(last_secret < first_rule && pos("--exclude=notes/private/") < first_rule, "{}", job.what);
            for x in ["--exclude=.rsync-tmp", "--exclude=.rsync-partial", "--exclude=.~tmp~"] {
                assert!(a.contains(&x.to_string()), "{x}");
            }
        }
        let big = push_args(&big_job(Path::new("/b"), &["arena9-apple".into()]), &transport(), &settings(""));
        assert!(big.iter().position(|x| x == "--include=/arena9-apple/") < big.iter().position(|x| x == "--exclude=/*"));
        for must in [".bashrc", ".zshrc", ".claude*", ".ssh/", ".env", ".netrc", ".git-credentials", ".gitconfig", ".git/config", ".git/config.worktree", ".git/modules/**/config", ".bash_history", ".config/", "id_ed25519", ".pgpass"] {
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

    /// S18 and §14.5: sealing needs a full push that started ≥ 2 h after the day ended, linked
    /// to everything it should have (AD1), with every allowed pod the control box holds (AD5).
    #[test]
    fn sealing_needs_a_linked_complete_push_started_two_hours_after_the_day() {
        let end = day_start("w0d2");
        let mut st = SnapState::default();
        for (started, want) in [(end - 60, false), (end + 7 * 60, false), (end + GRACE_SECS - 1, false), (end + GRACE_SECS, true), (end + 2 * 3600 + 300, true)] {
            st.labels.insert("w0d1".into(), rec(started, A));
            assert_eq!(sealed("w0d1", &st, START, &["arena9-apple".into()]), want, "push started {}s after midnight", started as i64 - end as i64);
        }
        let late = end + 3 * 3600;
        // Pushed without its links (a failed predecessor, a backlog's out-of-order push): open.
        st.labels.insert("w0d1".into(), LabelRecord { linked: false, ..rec(late, A) });
        assert!(!sealed("w0d1", &st, START, &["arena9-apple".into()]));
        // A pod the control box holds that wasn't pushed (re-added to the list, or its push failed): open.
        st.labels.insert("w0d1".into(), rec(late, A));
        assert!(!sealed("w0d1", &st, START, &["arena9-apple".into(), "arena9-bloom".into()]));
        // Only partly pushed (no full push yet): open.
        st.labels.insert("w0d1".into(), LabelRecord { ok_started: 0, ok_finished: 0, ..rec(late, A) });
        assert!(!sealed("w0d1", &st, START, &["arena9-apple".into()]));
        // A custom label never; a future one not until its day passes; no record never.
        st.labels.insert("mylabel".into(), rec(u64::MAX / 2, A));
        assert!(!sealed("mylabel", &st, START, &[]));
        st.labels.insert("w5d1".into(), rec(day_start("w5d1") + 3600, A));
        assert!(!sealed("w5d1", &st, START, &[]));
        assert!(!sealed("w0d3", &st, START, &[]));
    }

    /// The run's order (AD1): the newest label first, then the backlog oldest first, capped; the
    /// steady state, the midnight case, forced labels.
    #[test]
    fn plan_snap_pushes_the_newest_first_and_caps_the_backlog() {
        let days = ["w0d1", "w0d2", "w0d3", "w0d4", "w0d5", "w0d6"];
        let loc = local(&days.iter().map(|d| (*d, A)).collect::<Vec<_>>());
        let st = SnapState::default();
        let p = plan_snap(&loc, &[], &st, START);
        assert_eq!(p.labels, ["w0d6", "w0d1", "w0d2", "w0d3"]);
        assert_eq!(p.deferred, ["w0d4", "w0d5"]);
        // Steady state on w0d4 at 10:00: w0d1..w0d2 final, w0d3 pushed at 00:07 (open), today.
        let loc4 = local(&days[..4].iter().map(|d| (*d, A)).collect::<Vec<_>>());
        let mut st = SnapState::default();
        st.labels.insert("w0d1".into(), rec(day_start("w0d2") + 3 * 3600, A));
        st.labels.insert("w0d2".into(), rec(day_start("w0d3") + 2 * 3600 + 5 * 60, A));
        st.labels.insert("w0d3".into(), rec(day_start("w0d4") + 7 * 60, A));
        assert_eq!(plan_snap(&loc4, &[], &st, START).labels, ["w0d4", "w0d3"]);
        // The 02:05 push seals w0d3.
        st.labels.insert("w0d3".into(), rec(day_start("w0d4") + 2 * 3600 + 5 * 60, A));
        assert_eq!(plan_snap(&loc4, &[], &st, START), SnapPlan { labels: vec!["w0d4".into()], deferred: vec![] });
        // Forced: a final label and a custom one, pushed anyway (custom last); a forced deferred one is taken.
        let p = plan_snap(&loc4, &["mylabel".into(), "w0d1".into()], &st, START);
        assert_eq!(p.labels, ["w0d4", "w0d1", "mylabel"]);
        let p = plan_snap(&loc, &["w0d5".into()], &SnapState::default(), START);
        assert_eq!((p.labels.last().map(String::as_str), p.deferred.clone()), (Some("w0d5"), vec!["w0d4".to_string()]));
    }

    /// AD1: a pod links to every earlier label it has on the archive — complete or cut short —
    /// newest first, capped; never another pod's; none for a custom label.
    #[test]
    fn a_pod_links_to_its_earlier_labels_partial_ones_included() {
        let mut st = SnapState::default();
        assert!(link_labels_for("w0d1", "arena9-apple", &st).is_empty());
        st.labels.insert("w0d1".into(), rec(10, A));
        // w0d2 timed out half-way: on the archive, not complete.
        st.labels.insert("w0d2".into(), LabelRecord { ok_started: 0, ok_finished: 0, pods_ok: set(&[]), pods_tried: set(A), ..Default::default() });
        st.labels.insert("w0d3".into(), rec(30, &["arena9-bloom"]));
        assert_eq!(link_labels_for("w0d4", "arena9-apple", &st), ["w0d2", "w0d1"]);
        assert_eq!(link_labels_for("w0d4", "arena9-bloom", &st), ["w0d3"]);
        assert_eq!(link_labels_for("w0d2", "arena9-apple", &st), ["w0d1"]);
        assert!(link_labels_for("mylabel", "arena9-apple", &st).is_empty());
        for d in 1..=9 {
            st.labels.insert(format!("w1d{d}"), rec(100 + d, A));
        }
        let l = link_labels_for("w2d1", "arena9-apple", &st);
        assert_eq!(l.len(), MAX_LINK_LABELS);
        assert_eq!(l[0], "w1d9");
    }

    /// What lets a label seal: the pod's previous LOCAL label complete on the archive for it, and
    /// the week's baseline there (AD1, AD8).
    #[test]
    fn a_push_is_linked_only_against_a_complete_predecessor_and_the_baseline() {
        let loc = local(&[("w0d1", A), ("w0d2", &[]), ("w0d3", A)]);
        let mut st = SnapState::default();
        assert!(chain_ok("w0d1", "arena9-apple", &loc, &st, None), "the first label has nothing to link to");
        assert!(!chain_ok("w0d3", "arena9-apple", &loc, &st, None), "w0d1 (its previous) isn't on the archive");
        st.labels.insert("w0d1".into(), LabelRecord { pods_tried: set(A), ..Default::default() });
        assert!(!chain_ok("w0d3", "arena9-apple", &loc, &st, None), "only cut short");
        st.labels.insert("w0d1".into(), rec(10, A));
        assert!(chain_ok("w0d3", "arena9-apple", &loc, &st, None), "w0d2 doesn't hold the pod: w0d1 is its previous");
        let b = LocalBaseline { week: 0, mtime: 77 };
        assert!(!chain_ok("w0d3", "arena9-apple", &loc, &st, Some(b)), "the baseline isn't pushed");
        st.baselines.insert("w0".into(), 76);
        assert!(!chain_ok("w0d3", "arena9-apple", &loc, &st, Some(b)), "an older baseline of that week");
        st.baselines.insert("w0".into(), 77);
        assert!(chain_ok("w0d3", "arena9-apple", &loc, &st, Some(b)));
        let bs = [LocalBaseline { week: 0, mtime: 1 }, LocalBaseline { week: 2, mtime: 2 }];
        assert_eq!(baseline_for("w1d3", &bs).map(|b| b.week), Some(0));
        assert_eq!(baseline_for("w2d1", &bs).map(|b| b.week), Some(2));
        assert_eq!(baseline_for("mylabel", &bs), None);
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

    #[test]
    fn bandwidth_and_unique_sizes() {
        assert_eq!(bwlimit_bytes(Some("10M")), Some(10 << 20));
        assert_eq!(bwlimit_bytes(Some("500K")), Some(500 << 10));
        assert_eq!(bwlimit_bytes(Some("100")), Some(100 << 10));
        assert_eq!(bwlimit_bytes(Some("1.5G")), Some(3 << 29));
        assert_eq!(bwlimit_bytes(None), None);
        let t = tmp("unique");
        write(&t.0.join("a/x"), &"x".repeat(100));
        std::fs::create_dir_all(t.0.join("b")).unwrap();
        std::fs::hard_link(t.0.join("a/x"), t.0.join("b/x")).unwrap();
        write(&t.0.join("b/y"), &"y".repeat(10));
        assert_eq!(unique_bytes(&[t.0.join("a"), t.0.join("b")]), 110, "a hard link counts once");
    }

    // ---- fixtures: the real rsync (and rrsync) on local trees --------------------------------

    fn have(program: &str, arg: &str) -> bool {
        std::process::Command::new(program).arg(arg).output().is_ok_and(|o| o.status.success() || !o.stdout.is_empty())
    }

    struct Tmp(PathBuf);
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

    /// (path under the home, arrives on the archive?) — the secret files a participant's home can
    /// hold once pulled (dot-files at any depth, `.git/`, `.claude/`, non-dot dirs), and harmless
    /// lookalikes that must still arrive.
    const SECRET_TABLE: &[(&str, bool)] = &[
        (".bashrc", false),
        (".zshrc", false),
        (".profile", false),
        (".bash_history", false),
        (".python_history", false),
        (".netrc", false),
        (".git-credentials", false),
        (".gitconfig", false),
        (".pgpass", false),
        (".my.cnf", false),
        (".s3cfg", false),
        (".boto", false),
        (".rclone.conf", false),
        (".pypirc", false),
        (".npmrc", false),
        (".vault-token", false),
        (".env", false),
        ("proj/.env.local", false),
        (".envrc", false),
        (".claude.json", false),
        (".claude/.credentials.json", false),
        (".claude/projects/x/session.jsonl", false),
        (".ssh/id_ed25519", false),
        (".config/gh/hosts.yml", false),
        (".config/wandb/settings", false),
        (".cache/huggingface/token", false),
        (".huggingface/token", false),
        (".aws/credentials", false),
        (".docker/config.json", false),
        (".kaggle/kaggle.json", false),
        ("proj/kaggle.json", false),
        ("hf/token", false),
        ("work/huggingface/token", false),
        ("hf_home/stored_tokens", false),
        ("deep/id_rsa", false),
        ("keys/server.pem", false),
        ("keys/api.key", false),
        ("ARENA_materials/.git/config", false),
        ("ARENA_materials/.git/config.worktree", false),
        ("ARENA_materials/.git/modules/sub/config", false),
        ("ARENA_materials/.git/modules/a/b/config", false),
        ("ARENA_materials/.git.arena-nested/config", false),
        ("proj/.rsync-partial/x", false),
        ("proj/.rsync-tmp/x", false),
        // lookalikes that are work, not secrets
        ("notes.txt", true),
        ("notes/config", true),
        ("gitconfig_notes.md", true),
        ("tokenizer.json", true),
        ("hf/model.json", true),
        ("env/settings.py", true),
        ("my_env.py", true),
        (".gitignore", true),
        ("ARENA_materials/.git/HEAD", true),
        ("ARENA_materials/.git/modules/sub/HEAD", true),
        ("ARENA_materials/chapter1/config.py", true),
        ("docs/token_usage.md", true),
    ];

    /// Run `job`'s push into a LOCAL `<root>/push`: the same argv minus `-e`, the remote spec and
    /// the absolute link-dests rewritten to their local paths (what rrsync does).
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

    /// ARCH-SEC-2 / S12, the filter table through the real rsync (a pod push, then big): no
    /// secret arrives, every lookalike does.
    #[test]
    fn the_secret_filter_drops_every_credential_file_and_keeps_the_work() {
        if !have("rsync", "--version") {
            return;
        }
        let t = tmp("secrets");
        let home = t.0.join("backup/w0d1/arena9-apple");
        for (path, _) in SECRET_TABLE {
            write(&home.join(path), "SECRET-or-work\n");
        }
        let s = settings("");
        for pass in [SnapPass::Files, SnapPass::Git] {
            let o = push_local(&pod_job(&t.0.join("backup"), "w0d1", "arena9-apple", pass, &[], None), &s, &t.0);
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
        let cp = std::process::Command::new("cp").arg("-a").arg(t.0.join("backup/w0d1")).arg(t.0.join("backup/big")).status().unwrap();
        assert!(cp.success());
        let o = push_local(&big_job(&t.0.join("backup"), &["arena9-apple".into()]), &s, &t.0);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        for tree in ["push/snap/w0d1/arena9-apple", "push/big/arena9-apple"] {
            for (path, arrives) in SECRET_TABLE {
                assert_eq!(t.0.join(tree).join(path).exists(), *arrives, "{tree}/{path}");
            }
        }
    }

    /// S11 + S12 + the snap tier's layout, through the real rsync with the exact flag set: no
    /// secret of a hostile home arrives (the repo's `.git/config` included — and git still works
    /// without it), a second label hard-links what didn't change and leaves the first label's
    /// inodes alone, modes become readable, nothing is ever deleted, a file over the cap stays out
    /// of snap while a `.git` pack over it arrives.
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
        let home = src.join("w0d1/arena9-apple");
        write(&home.join(".bashrc"), "export OPENROUTER_API_KEY=\"sk-or-v1-SECRET\"\n");
        write(&home.join(".gitconfig"), "[url \"https://ghp_SECRET@github.com/\"]\n\tinsteadOf = https://github.com/\n");
        write(&home.join("notes.txt"), "my work\n");
        write(&home.join("private.txt"), "mine");
        std::fs::set_permissions(home.join("private.txt"), std::fs::Permissions::from_mode(0o600)).unwrap();
        let repo = home.join("ARENA_materials");
        write(&repo.join("chapter1/solution.py"), "print('work')\n");
        assert!(git(&home, &["init", "-q", "ARENA_materials"]).status.success());
        assert!(git(&repo, &["add", "."]).status.success());
        assert!(git(&repo, &["commit", "-qm", "work"]).status.success());
        assert!(git(&repo, &["remote", "add", "origin", "https://x-access-token:ghp_SECRET@github.com/o/r.git"]).status.success());
        write(&repo.join(".git/objects/pack/pack-big.pack"), &"p".repeat(30_000));
        write(&home.join("checkpoint.bin"), &"c".repeat(30_000));
        let job = |label: &str, pass, links: &[String]| pod_job(&src, label, "arena9-apple", pass, links, None);
        for pass in [SnapPass::Files, SnapPass::Git] {
            let o = push_local(&job("w0d1", pass, &[]), &settings, &t.0);
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
        let apple = t.0.join("push/snap/w0d1/arena9-apple");
        for f in files_under(&apple) {
            let text = std::fs::read_to_string(apple.join(&f)).unwrap_or_default();
            assert!(!text.contains("SECRET"), "{f} leaked: {text}");
        }
        for gone in [".bashrc", ".gitconfig", "ARENA_materials/.git/config", "checkpoint.bin"] {
            assert!(!apple.join(gone).exists(), "{gone} arrived");
        }
        assert!(apple.join("ARENA_materials/.git/objects/pack/pack-big.pack").exists(), "the .git pass carries what the cap left out");
        let log = git(&apple.join("ARENA_materials"), &["log", "--oneline"]);
        assert!(log.status.success() && String::from_utf8_lossy(&log.stdout).contains("work"), "{}", String::from_utf8_lossy(&log.stderr));
        assert_eq!(std::fs::metadata(apple.join("private.txt")).unwrap().permissions().mode() & 0o777, 0o644, "a 0600 file arrives go+r");

        // The next day: unchanged files are hard links; a changed file is new and day 1 keeps its own.
        std::fs::create_dir_all(src.join("w0d2")).unwrap();
        assert!(std::process::Command::new("cp").arg("-a").arg(&home).arg(src.join("w0d2/")).status().unwrap().success());
        write(&src.join("w0d2/arena9-apple/notes.txt"), "my work, day 2\n");
        std::fs::remove_file(src.join("w0d2/arena9-apple/private.txt")).unwrap();
        for pass in [SnapPass::Files, SnapPass::Git] {
            let o = push_local(&job("w0d2", pass, &["w0d1".into()]), &settings, &t.0);
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
        let d2 = t.0.join("push/snap/w0d2/arena9-apple");
        let ino = |p: &Path| std::fs::metadata(p).unwrap().ino();
        let sol = "ARENA_materials/chapter1/solution.py";
        assert_eq!(ino(&apple.join(sol)), ino(&d2.join(sol)), "unchanged: one inode");
        assert_eq!(ino(&apple.join("ARENA_materials/.git/objects/pack/pack-big.pack")), ino(&d2.join("ARENA_materials/.git/objects/pack/pack-big.pack")));
        assert_ne!(ino(&apple.join("notes.txt")), ino(&d2.join("notes.txt")));
        assert_eq!(std::fs::read_to_string(apple.join("notes.txt")).unwrap(), "my work\n", "day 1 is untouched");
        // Nothing is deleted: pushing day 1 again after its source lost a file keeps the file.
        std::fs::remove_file(home.join("notes.txt")).unwrap();
        assert!(push_local(&job("w0d1", SnapPass::Files, &[]), &settings, &t.0).status.success());
        assert!(apple.join("notes.txt").exists(), "never deleted on the archive");
        assert!(!d2.join("private.txt").exists() && apple.join("private.txt").exists());
    }

    /// The push key's forced command (render_push_key_line), rooted at `push` — what sshd runs for
    /// every push, here through a shim standing in for sshd.
    fn forced(rrsync: &str, push: &Path, munge: bool) -> String {
        let line = crate::archive::render::render_push_key_line(rrsync, "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPushPushPushPushPushPushPushPushPushPus1 x").unwrap();
        let cmd = line.split("command=\"").nth(1).unwrap().split('"').next().unwrap().replace(crate::archive::PUSH_ROOT, &push.display().to_string());
        if munge {
            cmd
        } else {
            cmd.replace(" -munge", "")
        }
    }

    /// A transport whose ssh is a shim that runs `forced_cmd` as sshd would.
    fn shim_transport(dir: &Path, name: &str, forced_cmd: &str) -> PushTransport {
        use std::os::unix::fs::PermissionsExt;
        let shim = dir.join(name);
        std::fs::write(
            &shim,
            format!("#!/bin/sh\nwhile [ $# -gt 0 ] && [ \"$1\" != rsync ]; do shift; done\nSSH_ORIGINAL_COMMAND=\"$*\" exec {forced_cmd}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        PushTransport { rsh: format!("{} -p 22 -o BatchMode=yes", shim.display()), dest: "arena-push@archive".into() }
    }

    fn rrsync_path() -> Option<&'static str> {
        let p = ["/usr/bin/rrsync", "/usr/share/doc/rsync/scripts/rrsync"].into_iter().find(|p| Path::new(p).exists())?;
        (have("rsync", "--version") && have("python3", "--version")).then_some(p)
    }

    /// S2: the real rrsync behind the real forced command (`-munge -wo -no-del -no-lock`) accepts
    /// the exact pod (several absolute link-dests, one of them missing, `--mkpath`), baseline and
    /// big argv — two at once — and refuses a delete, a read and a `..`.
    #[test]
    fn rrsync_accepts_exactly_our_pushes_and_refuses_the_rest() {
        use std::os::unix::fs::MetadataExt;
        let Some(rrsync) = rrsync_path() else { return };
        let t = tmp("rrsync");
        let push = t.0.join("push");
        std::fs::create_dir_all(push.join("snap")).unwrap();
        std::fs::create_dir_all(push.join("big")).unwrap();
        let tr = shim_transport(&t.0, "shim", &forced(rrsync, &push, true));
        let src = t.0.join("backup");
        for l in ["w0d1", "w0d2"] {
            write(&src.join(format!("{l}/arena9-apple/a.py")), "same\n");
            write(&src.join(format!("{l}/arena9-apple/.git/HEAD")), "ref: refs/heads/main\n");
        }
        write(&src.join("baseline/w0/fleet/image.py"), "image\n");
        write(&src.join("big/arena9-apple/ckpt.pt"), "weights");
        let s = settings("");
        let run = |args: &[String]| std::process::Command::new("rsync").args(args).output().unwrap();
        for pass in [SnapPass::Files, SnapPass::Git] {
            let o = run(&push_args(&baseline_job(&src, &[0], pass), &tr, &s));
            assert!(o.status.success(), "baseline {pass:?}: {}", String::from_utf8_lossy(&o.stderr));
        }
        for (label, links) in [("w0d1", vec![]), ("w0d2", vec!["w0d1".to_string(), "w0d0".to_string()])] {
            for pass in [SnapPass::Files, SnapPass::Git] {
                let o = run(&push_args(&pod_job(&src, label, "arena9-apple", pass, &links, Some(0)), &tr, &s));
                assert!(o.status.success(), "{label} {pass:?}: {}", String::from_utf8_lossy(&o.stderr));
            }
        }
        let ino = |p: &Path| std::fs::metadata(p).unwrap().ino();
        assert_eq!(ino(&push.join("snap/w0d1/arena9-apple/a.py")), ino(&push.join("snap/w0d2/arena9-apple/a.py")), "absolute link-dest, re-rooted");
        assert!(push.join("snap/baseline/w0/fleet/image.py").exists());
        // Two pushes at once (-no-lock): snap and big side by side.
        let a = push_args(&pod_job(&src, "w0d2", "arena9-apple", SnapPass::Files, &["w0d1".into()], None), &tr, &s);
        let b = push_args(&big_job(&src, &["arena9-apple".into()]), &tr, &s);
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

    /// The victim's archived data and an attacker's tree full of planted symlinks. rsync looks for
    /// (and afterwards deletes) a "partial" copy of each file it handles under the holding dir's
    /// name — so the attacker names files like the victim's, beside a link of that name.
    fn plant_attack(t: &Path, push: &Path) -> PathBuf {
        let src = t.join("backup");
        write(&push.join("snap/w0d1/arena9-victim/work.py"), "victim's day 1\n");
        write(&push.join("big/arena9-victim/ckpt.pt"), "victim's weights\n");
        let evil = src.join("big/arena9-evil");
        write(&evil.join("proj/work.py"), "attacker");
        write(&evil.join("ckpt.pt"), "attacker");
        write(&evil.join("deep/a/ckpt.pt"), "attacker");
        std::os::unix::fs::symlink(push.join("snap/w0d1/arena9-victim"), evil.join("proj/.rsync-partial")).unwrap();
        std::os::unix::fs::symlink("../arena9-victim", evil.join(".rsync-partial")).unwrap();
        std::os::unix::fs::symlink("../../../arena9-victim", evil.join("deep/a/.rsync-partial")).unwrap();
        std::os::unix::fs::symlink("../../w0d1/arena9-victim", evil.join("escape")).unwrap();
        let snap = src.join("w0d2/arena9-evil");
        write(&snap.join("work.py"), "attacker");
        std::os::unix::fs::symlink(push.join("snap/w0d1/arena9-victim"), snap.join(".rsync-partial")).unwrap();
        std::os::unix::fs::symlink("../../w0d1/arena9-victim", snap.join("victim")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", snap.join("passwd")).unwrap();
        src
    }

    fn victim_intact(push: &Path) -> bool {
        std::fs::read_to_string(push.join("snap/w0d1/arena9-victim/work.py")).ok().as_deref() == Some("victim's day 1\n")
            && std::fs::read_to_string(push.join("big/arena9-victim/ckpt.pt")).ok().as_deref() == Some("victim's weights\n")
    }

    /// ARCH-SEC-1 / ARCH-SEC-3, on the real rrsync: a participant's symlinks named like rsync's
    /// holding area (absolute, `..`-relative, at any depth) and others pointing at another pod's
    /// archived data can't touch it — pushed twice, in both tiers. The test first reproduces the
    /// attack with the old argv (`--partial-dir=.rsync-partial`, no exclude) and no `-munge`, then
    /// shows each defence on its own: today's argv without `-munge`, and the OLD argv with it.
    #[test]
    fn planted_symlinks_never_touch_another_pods_archive() {
        let Some(rrsync) = rrsync_path() else { return };
        let s = settings("");
        let jobs = |src: &Path| {
            vec![
                big_job(src, &["arena9-evil".into()]),
                pod_job(src, "w0d2", "arena9-evil", SnapPass::Files, &[], None),
                pod_job(src, "w0d2", "arena9-evil", SnapPass::Git, &[], None),
            ]
        };
        // The old big argv: a per-directory holding area, and the name not excluded.
        let old = |args: Vec<String>| -> Vec<String> {
            let mut a: Vec<String> = args.into_iter().filter(|x| x != "--exclude=.rsync-partial").collect();
            a.insert(2, "--partial-dir=.rsync-partial".into());
            a
        };
        let scenario = |tag: &str, munge: bool, old_argv: bool| -> (bool, String) {
            let t = tmp(tag);
            let push = t.0.join("push");
            std::fs::create_dir_all(push.join("snap")).unwrap();
            let src = plant_attack(&t.0, &push);
            let tr = shim_transport(&t.0, "shim", &forced(rrsync, &push, munge));
            let mut log = String::new();
            for _ in 0..2 {
                for j in jobs(&src) {
                    let mut a = push_args(&j, &tr, &s);
                    if old_argv && j.tier == Tier::Big {
                        a = old(a);
                    }
                    let o = std::process::Command::new("rsync").args(&a).output().unwrap();
                    log.push_str(&String::from_utf8_lossy(&o.stderr));
                }
            }
            (victim_intact(&push), log)
        };
        // The attack is real: the old argv on an un-munged archive damages the victim.
        let (intact, log) = scenario("sec1-old", false, true);
        assert!(!intact, "the reviewer's scenario no longer reproduces — the fixture proves nothing: {log}");
        // Today: the argv alone (no holding area, the names excluded) …
        let (intact, log) = scenario("sec1-argv", false, false);
        assert!(intact, "today's argv: {log}");
        // … and -munge alone (even the old argv).
        let (intact, log) = scenario("sec1-munge", true, true);
        assert!(intact, "-munge: {log}");
        // Both, as shipped; the other links arrive defanged.
        let t = tmp("sec1-shipped");
        let push = t.0.join("push");
        std::fs::create_dir_all(push.join("snap")).unwrap();
        let src = plant_attack(&t.0, &push);
        let tr = shim_transport(&t.0, "shim", &forced(rrsync, &push, true));
        for j in jobs(&src) {
            let o = std::process::Command::new("rsync").args(push_args(&j, &tr, &s)).output().unwrap();
            assert!(o.status.success(), "{}: {}", j.what, String::from_utf8_lossy(&o.stderr));
        }
        assert!(victim_intact(&push));
        for (link, was) in [("snap/w0d2/arena9-evil/victim", "../../w0d1/arena9-victim"), ("snap/w0d2/arena9-evil/passwd", "/etc/passwd"), ("big/arena9-evil/escape", "../../w0d1/arena9-victim")] {
            let target = std::fs::read_link(push.join(link)).unwrap();
            assert_eq!(target, Path::new(&format!("/rsyncd-munged/{was}")), "{link}");
        }
        for name in ["big/arena9-evil/.rsync-partial", "big/arena9-evil/proj/.rsync-partial", "snap/w0d2/arena9-evil/.rsync-partial"] {
            assert!(std::fs::symlink_metadata(push.join(name)).is_err(), "{name} was shipped");
        }
    }

    /// AD1 on the real rsync: a label first pushed without its links (out of order, or after a
    /// failed predecessor) is a full copy; the later linked push replaces its identical files by
    /// hard links — the space comes back, which is why an unlinked push is never sealed.
    #[test]
    fn a_later_linked_push_relinks_a_full_copy() {
        use std::os::unix::fs::MetadataExt;
        if !have("rsync", "--version") {
            return;
        }
        let t = tmp("relink");
        let src = t.0.join("backup");
        write(&src.join("w0d1/arena9-apple/a.py"), "same\n");
        std::fs::create_dir_all(src.join("w0d2")).unwrap();
        assert!(std::process::Command::new("cp").arg("-a").arg(src.join("w0d1/arena9-apple")).arg(src.join("w0d2/")).status().unwrap().success());
        let s = settings("");
        assert!(push_local(&pod_job(&src, "w0d2", "arena9-apple", SnapPass::Files, &[], None), &s, &t.0).status.success());
        assert!(push_local(&pod_job(&src, "w0d1", "arena9-apple", SnapPass::Files, &[], None), &s, &t.0).status.success());
        let ino = |l: &str| std::fs::metadata(t.0.join(format!("push/snap/{l}/arena9-apple/a.py"))).unwrap().ino();
        assert_ne!(ino("w0d1"), ino("w0d2"), "two full copies");
        assert!(push_local(&pod_job(&src, "w0d2", "arena9-apple", SnapPass::Files, &["w0d1".into()], None), &s, &t.0).status.success());
        assert_eq!(ino("w0d1"), ino("w0d2"), "re-linked");
    }

    /// AD8 on the real rsync: with the fleet baseline pushed, two pods' identical image files
    /// share one inode on the archive (the control box's own dedup, mirrored).
    #[test]
    fn the_baseline_dedups_across_pods() {
        use std::os::unix::fs::MetadataExt;
        if !have("rsync", "--version") {
            return;
        }
        let t = tmp("baseline");
        let src = t.0.join("backup");
        write(&src.join("baseline/w0/fleet/ARENA_materials/big_notebook.ipynb"), &"n".repeat(5000));
        for pod in ["arena9-apple", "arena9-bloom"] {
            let to = src.join(format!("w0d1/{pod}/ARENA_materials"));
            std::fs::create_dir_all(&to).unwrap();
            assert!(std::process::Command::new("cp").arg("-p").arg(src.join("baseline/w0/fleet/ARENA_materials/big_notebook.ipynb")).arg(&to).status().unwrap().success());
            write(&src.join(format!("w0d1/{pod}/mine.py")), pod);
        }
        let s = settings("");
        for pass in [SnapPass::Files, SnapPass::Git] {
            assert!(push_local(&baseline_job(&src, &[0], pass), &s, &t.0).status.success());
        }
        for pod in ["arena9-apple", "arena9-bloom"] {
            let o = push_local(&pod_job(&src, "w0d1", pod, SnapPass::Files, &[], Some(0)), &s, &t.0);
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
        let ino = |p: &str| std::fs::metadata(t.0.join("push/snap").join(p)).unwrap().ino();
        let nb = "ARENA_materials/big_notebook.ipynb";
        assert_eq!(ino(&format!("w0d1/arena9-apple/{nb}")), ino(&format!("baseline/w0/fleet/{nb}")));
        assert_eq!(ino(&format!("w0d1/arena9-apple/{nb}")), ino(&format!("w0d1/arena9-bloom/{nb}")));
        assert_ne!(ino("w0d1/arena9-apple/mine.py"), ino("w0d1/arena9-bloom/mine.py"));
    }

    #[test]
    fn local_labels_baselines_and_control_box_only_dirs_are_found_on_disk() {
        let t = tmp("disk");
        let d = &t.0;
        for p in ["w0d1/arena9-apple", "w0d1/james-gpu", "w0d1/arena9-gone", "big/arena9-apple", "big/arena9-ghost", "big/arena8-x", "baseline/w0/fleet/x", "baseline/w1/fleet", ".rsync-tmp", "mylabel/arena9-apple"] {
            std::fs::create_dir_all(d.join(p)).unwrap();
        }
        std::fs::write(d.join("baseline/w0/fleet/x/f"), "x").unwrap();
        std::fs::write(d.join("w0d2"), "a file, not a label").unwrap();
        assert_eq!(local_labels(d).unwrap(), ["w0d1"]);
        assert_eq!(custom_labels(d), ["mylabel"]);
        let list = vec!["apple".to_string(), "@james-gpu".to_string()];
        let allowed = allowed_pod_dirs("arena9", &list);
        let snap = local_snapshot(d, &allowed).unwrap();
        assert_eq!(snap, local(&[("w0d1", A)]));
        let (cohort, other) = control_box_only(d, &["w0d1".to_string()], &allowed, "arena9", &list);
        assert_eq!(cohort, ["arena9-ghost", "arena9-gone"]);
        assert_eq!(other, ["arena8-x", "james-gpu"]);
        let b = local_baselines(d);
        assert_eq!(b.iter().map(|b| b.week).collect::<Vec<_>>(), [0], "an empty baseline doesn't count");
    }
}
