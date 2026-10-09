//! Pulling pod files to a local backup (legacy `backup.sh`).
//!
//! The git autocommit (`pods backup`) saves the *repo*; this saves the rest of a pod's
//! home directory to the operator's machine via rsync. It's a safety net for files that
//! never made it into git before a pod is torn down.
//!
//! Mirrors the legacy flags: archive + compress, prune empty dirs, and a per-file size cap so a stray multi-GB checkpoint doesn't blow up the
//! backup. Dotfile *directories* (caches, `.git`, …), `site-packages/`, and common
//! reconstructable/huge non-dot dirs (`hf_cache/`, `__pycache__/`, `venv/`,
//! `node_modules/`) are excluded — they're either reconstructable or huge. The rsync arg
//! vector is built purely here so it's unit-tested; the process spawn lives in the caller.
//!
//! The dated snapshot tier is **deduplicated with hard links** (`--link-dest`): unchanged files
//! are linked to this pod's previous snapshot and to the week's **fleet baseline** — the
//! image's own files, captured once from a fresh pod (`pods baseline`, see
//! [`prune_baseline`]) — instead of copied again (live: every day's snapshot was a full ~1.8G
//! per pod, almost all of it image content). Each snapshot stays a complete, restorable tree.
//! And its `.git` is complete: the size cap applies to the files, never to `.git` (a second,
//! uncapped pass — [`PullConfig::passes`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::setup::MAX_STEP_TIMEOUT_SECS;
use crate::ssh::SshTarget;

/// Options for a pull, defaulting to the legacy `backup.sh` behavior.
#[derive(Debug, Clone)]
pub struct PullConfig {
    /// Only transfer files **smaller** than this (rsync `--max-size`), e.g. `Some("50M")`.
    /// `None` = no upper cap. The snapshot tier sets this; the big-file mirror leaves it `None`.
    pub max_size: Option<String>,
    /// Only transfer files **at least** this large (rsync `--min-size`), e.g. `Some("50M")`.
    /// `None` = no lower bound. (Currently unused — the tiers split by `max_size` only.)
    pub min_size: Option<String>,
    /// Paths/globs to **keep** even if a later exclude would drop them (rsync
    /// `--include`, emitted first so it wins). Defaults keep `.git` so backups carry git
    /// history/branch state, while still excluding other dotfile dirs.
    pub includes: Vec<String>,
    /// Paths/globs to exclude (rsync `--exclude`).
    pub excludes: Vec<String>,
    /// Remote path to pull from, relative to the SSH login dir. Empty = the home dir.
    pub remote_path: String,
    /// rsync `--timeout`: give up after this many seconds with **no I/O at all**. Without
    /// it, a pod that stops answering mid-transfer (a wedged host, a dropped NAT mapping)
    /// leaves rsync waiting forever — and the */15 backup cron stacking up behind it. It
    /// only fires on silence: a slow transfer keeps moving bytes, and rsync sends
    /// keep-alives while either side is busy checksumming a big file. `None` = no limit.
    /// The wall-clock bound on a whole transfer is the caller's (`BACKUP_TIMEOUT_SECS`).
    pub io_timeout_secs: Option<u64>,
    /// Complete earlier trees whose identical files are hard-linked instead of copied (rsync
    /// `--link-dest`, absolute paths, tried in order; "identical" = same size, mtime and
    /// permissions): this pod's previous snapshot and the fleet baseline
    /// ([`snapshot_link_dests`]). They must be on the destination's filesystem (they are: all
    /// under the backup dir). Empty = no dedup.
    pub link_dest: Vec<String>,
    /// Only the `.git` dirs, nothing else — the size-capped tier's second pass
    /// ([`PullConfig::passes`]).
    pub git_only: bool,
}

/// The default [`PullConfig::io_timeout_secs`]: 5 minutes without a byte means the
/// connection is dead, not slow.
pub const RSYNC_IO_TIMEOUT_SECS: u64 = 300;

impl Default for PullConfig {
    fn default() -> Self {
        Self {
            max_size: Some("50M".to_string()),
            min_size: None,
            // Keep `.git` (at any depth) so the backup is a usable git repo, and
            // `.claude/` so Claude Code session transcripts (`.claude/projects/**/*.jsonl`,
            // the token-usage record) are archived — both would otherwise be dropped by
            // the `**/.*/` dotfile-dir exclude below. Includes are emitted first so they win.
            includes: vec![
                "**/.git/".to_string(),
                "**/.git/**".to_string(),
                "**/.claude/".to_string(),
                "**/.claude/**".to_string(),
            ],
            // …but still drop reconstructable/huge dirs. `**/.*/` covers dotfile dirs
            // (`.cache`, `.venv`, …); the rest catch non-dot caches and envs that aren't
            // (notably `hf_cache/` — multi-GB model/dataset blobs that filled the backup
            // volume) so they never get archived.
            excludes: vec![
                "**/.*/".to_string(),        // dotfile dirs (.cache, .venv, .pytest_cache, …)
                ".cache/".to_string(),       // explicit: caches (HF/pip/etc) at any depth
                "site-packages/".to_string(), // installed python packages
                "__pycache__/".to_string(),  // python bytecode cache
                "hf_cache/".to_string(),     // HuggingFace HF_HOME cache (models/datasets)
                "huggingface/".to_string(),  // HuggingFace cache (alt HF_HOME layout)
                "venv/".to_string(),         // non-dot python virtualenvs
                "node_modules/".to_string(), // npm deps
                // HuggingFace *hub* cache when HF_HOME/HF_HUB_CACHE points at a non-dot
                // dir (e.g. `rlvr_run/hf/hub/`), so it escaped the dotdir/.cache excludes
                // above. `models--*/` and `datasets--*/` are the hub's content-addressed
                // blob store — reconstructable public downloads (e.g. a ~15GB DeepSeek-R1
                // base re-pulled onto every pod). Was ~337GB / 40% of the uncapped backup.
                "models--*/".to_string(),    // HF hub model cache dirs (blobs/snapshots/refs)
                "datasets--*/".to_string(),  // HF hub dataset cache dirs
                // Scratch dir for sweeps/RLVR runs — deliberately NOT backed up (these
                // outputs are large + reproducible/wandb-logged). zebra's orchestrator is
                // told to do all its run work here.
                "TEMP_FOLDER_FOR_SWEEPS/".to_string(),
                // Where setup moved the image's checkout aside when the repo went onto the
                // `/workspace` volume (`crate::volume`): a second full copy of the repo, from the
                // image — the work is in the volume copy, which the pull captures.
                "*.arena-aside-*/".to_string(),
            ],
            remote_path: String::new(),
            io_timeout_secs: Some(RSYNC_IO_TIMEOUT_SECS),
            link_dest: Vec::new(),
            git_only: false,
        }
    }
}

impl PullConfig {
    /// Drop the `.git` includes (so `.git` is excluded by `**/.*/` like other dotdirs).
    pub fn without_git(mut self) -> Self {
        self.includes.retain(|i| !i.contains(".git"));
        self
    }

    /// The rsyncs that make this tier, run one after the other into the same destination. A
    /// size-capped tier that keeps `.git` is two: everything but `.git` under the cap, then
    /// only the `.git` dirs with no cap — the cap used to drop a repo's big pack, leaving a
    /// `.git` whose refs point at objects that never arrived (live: a 232M pack missing, `git
    /// fsck` failing). With `--link-dest` the second pass costs little on disk: a pack doesn't
    /// change, so each snapshot hard-links the previous one's. Everything else is one pass.
    pub fn passes(&self) -> Vec<PullConfig> {
        let keeps_git = self.includes.iter().any(|i| i.contains(".git"));
        if self.git_only || self.max_size.is_none() || !keeps_git {
            return vec![self.clone()];
        }
        let git = PullConfig {
            max_size: None,
            min_size: None,
            includes: self.includes.iter().filter(|i| i.contains(".git")).cloned().collect(),
            git_only: true,
            ..self.clone()
        };
        vec![self.clone().without_git(), git]
    }

    /// Pod-to-pod **replication** profile (the `pods replace` copy), as opposed to the
    /// operator-local backup tiers above. Two differences from the backup default:
    ///
    /// - **`.claude/` is excluded, not archived.** The backup tiers deliberately *keep*
    ///   `.claude/` to archive Claude Code transcripts onto the operator's own machine, but
    ///   replicating Claude Code session/credential state from one pod onto another is a ToS
    ///   concern — so the `.claude` include is dropped (the `**/.*/` dotdir exclude then
    ///   drops it) and an explicit `.claude/` exclude is added for clarity/safety.
    /// - **no size cap** — a replacement should carry the participant's full working tree,
    ///   not just sub-50MB files.
    ///
    /// `.cache` and the HuggingFace model/dataset caches are already excluded by the shared
    /// defaults, and `.git` is still kept so the new pod inherits branch/commit state.
    pub fn replication() -> Self {
        let mut s = Self { max_size: None, ..Self::default() };
        s.includes.retain(|i| !i.contains(".claude"));
        // Exclude the whole `.claude` family — both the `.claude/` dir AND the `.claude.json`
        // credential *file* in the home root. The default `**/.*/` only drops dot-*dirs*, so
        // `.claude.json` (a file) would otherwise be replicated, leaking Claude Code creds.
        s.excludes.push("**/.claude*".to_string());
        s.excludes.push(".claude*".to_string());
        // Belt-and-suspenders: never replicate SSH material between pods (already covered by
        // the dotdir exclude, but make the intent explicit — a leaked authorized_keys/key is
        // both a security and a lockout risk).
        s.excludes.push("**/.ssh/".to_string());
        s.excludes.push(".ssh/".to_string());
        // Don't copy the shell rc files: `copy-keys`/`setup` write the per-pod API keys into
        // `~/.bashrc` and `~/.zshrc` (as `export OPENAI_API_KEY=…` etc.), so replicating them
        // would carry one pod's keys onto another. The new pod gets its OWN keys re-distributed
        // at setup, and its base rc files from the image — so just skip these (plus shell
        // history, which can hold secrets typed on the command line).
        for f in [".bashrc", ".zshrc", ".zshrc.pre-oh-my-zsh", ".bash_history", ".zsh_history"] {
            s.excludes.push(f.to_string());
            s.excludes.push(format!("**/{f}"));
        }
        s
    }

    /// Snapshot tier: only files **smaller** than `threshold`, copied into the dated `wNdM`
    /// folder (`local_dest`). This is the historical default, named explicitly for symmetry.
    pub fn small_tier(threshold: impl Into<String>) -> Self {
        Self { max_size: Some(threshold.into()), min_size: None, ..Self::default() }
    }

    /// Big tier: the **complete** home (no size filter) accumulated into a single dateless
    /// folder (`big_dest`). It never deletes — every file ever backed up is kept, even after
    /// it's gone from the pod — so it's a pure "save all the files" copy, not a `--delete`
    /// mirror of current state. The snapshot tier (`small_tier`) layers dated history of the
    /// sub-threshold files on top. Both share the default excludes (HF cache, venvs, …).
    pub fn big_tier() -> Self {
        Self { max_size: None, min_size: None, ..Self::default() }
    }
}

/// The default wall-clock budget for one pod's backup rsync (`BACKUP_TIMEOUT_SECS`): 2 hours.
/// A first big-tier pull of a full home over a slow uplink takes tens of minutes, so this
/// only trips on a transfer that is stuck while still trickling bytes — a silent one is
/// already stopped by the I/O timeout ([`RSYNC_IO_TIMEOUT_SECS`]). Either way the pod is
/// reported as a named failure and the rest of the fleet's backup carries on.
pub const DEFAULT_BACKUP_TIMEOUT_SECS: u64 = 2 * 3600;

/// The per-pod budget for a backup rsync, from the `BACKUP_TIMEOUT_SECS` config value:
/// unset/empty = [`DEFAULT_BACKUP_TIMEOUT_SECS`]; otherwise whole seconds,
/// 1..=[`MAX_STEP_TIMEOUT_SECS`] (a day). Anything else is a config error naming the value
/// — a typo must not quietly become "no limit" (an unbounded backup is what stacks the
/// cron) or the default.
pub fn backup_timeout(config_value: Option<&str>) -> Result<Duration> {
    let Some(raw) = config_value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Duration::from_secs(DEFAULT_BACKUP_TIMEOUT_SECS));
    };
    raw.parse::<u64>()
        .ok()
        .filter(|n| (1..=MAX_STEP_TIMEOUT_SECS).contains(n))
        .map(Duration::from_secs)
        .ok_or_else(|| {
            Error::Config(format!(
                "BACKUP_TIMEOUT_SECS must be a whole number of seconds, 1..={MAX_STEP_TIMEOUT_SECS} (got `{raw}`)"
            ))
        })
}

/// The local destination for a pod's backup: `<base>/<label>/<pod-name>/`. The trailing
/// slash matters to rsync (copy *into* this dir).
pub fn local_dest(base: &str, label: &str, pod_name: &str) -> String {
    format!("{}/{label}/{pod_name}/", base.trim_end_matches('/'))
}

/// The single running destination for a pod's big files: `<base>/big/<pod-name>/`. Unlike
/// `local_dest` there's no `wNdM` label — every backup updates this one folder in place, so a
/// multi-GB checkpoint is stored once instead of re-copied into each day's snapshot.
pub fn big_dest(base: &str, pod_name: &str) -> String {
    format!("{}/big/{pod_name}/", base.trim_end_matches('/'))
}

/// Build the `rsync` argv (everything after the program name) to pull `target`'s home
/// (or `pc.remote_path`) into `local_dest`. Uses the target's transport options via
/// `-e ssh …`; rsync appends `user@host` to the remote spec itself.
pub fn rsync_args(target: &SshTarget, pc: &PullConfig, local_dest: &str) -> Vec<String> {
    let mut a = rsync_flags(pc);
    // Transport: the ssh command without user@host (rsync supplies that).
    a.push("-e".into());
    a.push(target.rsh_command());
    // Source `user@host:<path>` — empty path means the login (home) dir.
    a.push(format!("{}@{}:{}", target.user, target.host, pc.remote_path));
    a.push(local_dest.to_string());
    a
}

/// Build the `rsync` argv to **push** the contents of a local directory up to `target`'s
/// home (or `pc.remote_path`). This is the second leg of the via-local pod-to-pod copy that
/// backs `pods replace`: `pull` a pod's home into a control-side staging dir, then push that
/// dir up to the freshly-built replacement. Same flags/excludes as `rsync_args`; only the
/// direction flips (local source dir → `user@host:`).
pub fn push_rsync_args(target: &SshTarget, pc: &PullConfig, local_src: &str) -> Vec<String> {
    let mut a = rsync_flags(pc);
    // CRITICAL: don't preserve owner/group. `-a` would otherwise apply the *staging dir's*
    // owner (the control user, e.g. uid 1000) to the destination's top directory (the `.`
    // entry) — chowning the pod's `/root` to a non-root uid, after which sshd StrictModes
    // rejects every key ("Permission denied") and the pod is locked out. Receiving as root,
    // dropping `-o`/`-g` leaves the home root-owned and files owned by root.
    a.push("--no-owner".into());
    a.push("--no-group".into());
    a.push("-e".into());
    a.push(target.rsh_command());
    // Local source dir first (a trailing slash means "contents of"), then the remote dest.
    a.push(local_src.to_string());
    a.push(format!("{}@{}:{}", target.user, target.host, pc.remote_path));
    a
}

/// A shell command to run **on the source pod** that rsyncs its home directly up to a
/// destination pod's raw `ip:port` — the fast path for `pods replace` (no round-trip
/// through the control machine). It authenticates with `remote_key`, a private key present
/// on the source pod whose public half the destination authorizes: the shared deploy key
/// both pods receive at `setup`. The destination's staging name (`<name>-new`) isn't in the
/// pod-side ssh config, so we target the endpoint directly. Everything is shell-quoted so
/// the glob excludes (`**/.*/` etc.) survive the source pod's shell intact; `$HOME` is left
/// unquoted on purpose so the remote shell expands it.
pub fn pod_to_pod_command(
    dest_ip: &str,
    dest_port: u16,
    dest_user: &str,
    remote_key: &str,
    pc: &PullConfig,
) -> String {
    pod_to_pod_command_into(dest_ip, dest_port, dest_user, remote_key, pc, "")
}

/// [`pod_to_pod_command`] into `dest_path` on the destination (relative to its home; empty =
/// the home) — e.g. the repo's tree into `ARENA_materials/`, which the destination resolves
/// through a symlink there (its repo on the `/workspace` volume) instead of replacing the link.
pub fn pod_to_pod_command_into(
    dest_ip: &str,
    dest_port: u16,
    dest_user: &str,
    remote_key: &str,
    pc: &PullConfig,
    dest_path: &str,
) -> String {
    let mut parts = vec!["rsync".to_string()];
    for a in rsync_flags(pc) {
        parts.push(shell_quote(&a));
    }
    // Keep the destination home root-owned regardless of source file ownership (see
    // `push_rsync_args` — the `.`-entry chown of `/root` is what locks sshd out).
    parts.push("--no-owner".into());
    parts.push("--no-group".into());
    let ssh = format!(
        "ssh -p {dest_port} -o BatchMode=yes -o StrictHostKeyChecking=no \
         -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -i {remote_key}"
    );
    parts.push("-e".into());
    parts.push(shell_quote(&ssh));
    let src = if pc.remote_path.is_empty() {
        "$HOME/".to_string()
    } else {
        format!("$HOME/{}", pc.remote_path)
    };
    parts.push(src);
    parts.push(if dest_path.is_empty() {
        format!("{dest_user}@{dest_ip}:")
    } else {
        shell_quote(&format!("{dest_user}@{dest_ip}:{dest_path}"))
    });
    parts.join(" ")
}

/// Single-quote a value for safe inclusion in a remote `sh -c` string (POSIX `'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The shared `rsync` flags + size filters + include/exclude rules (everything except the
/// `-e` transport and the source/dest operands), so pull and push stay in lockstep.
fn rsync_flags(pc: &PullConfig) -> Vec<String> {
    // No `--human-readable`: with it rsync's `--stats` print sizes as `1.84G`, which the old
    // digits-only parse read as 184 (live: "(6754 files, 184)" for a 1.8G pull). The default
    // stats are plain (digit-grouped) byte counts; [`parse_rsync_stats`] reads every format.
    let mut a = vec![
        "-avz".to_string(),
        "--info=progress2".into(),
        "--stats".into(), // emit a summary we parse to report files/bytes per pod
        "--prune-empty-dirs".into(),
    ];
    if let Some(t) = pc.io_timeout_secs {
        a.push(format!("--timeout={t}"));
    }
    // `--max-size` keeps the snapshot tier small (the big tier leaves it `None` to take
    // everything). `--min-size` is currently unused but honored if a caller sets it.
    if let Some(m) = &pc.max_size {
        a.push(format!("--max-size={m}"));
    }
    if let Some(m) = &pc.min_size {
        a.push(format!("--min-size={m}"));
    }
    // `--prune-empty-dirs` must not prune inside a kept `.git`: a repo whose refs are all packed
    // has an empty `.git/refs/heads/`, and git doesn't take a `.git` without `refs/` for a
    // repository at all ("not a git repository") — the copy would be no repo.
    if pc.includes.iter().any(|i| i.contains(".git")) {
        a.push("--filter=protect **/.git/**/".into());
    }
    // Includes first (they win over a later exclude), then excludes.
    for inc in &pc.includes {
        a.push("--include".into());
        a.push(inc.clone());
    }
    for ex in &pc.excludes {
        a.push("--exclude".into());
        a.push(ex.clone());
    }
    // The `.git` pass: whatever the rules above haven't decided — every other directory (to
    // reach the `.git`s inside it; `--prune-empty-dirs` drops the ones that hold none) and every
    // other file (dropped) — so it carries exactly the `.git` dirs the tier keeps, the dirs it
    // excludes (caches, venvs) never entered.
    if pc.git_only {
        a.extend(["--include", "*/", "--exclude", "*"].map(String::from));
    }
    for d in &pc.link_dest {
        a.push(format!("--link-dest={d}"));
    }
    a
}

/// The directory under the backup dir that holds the fleet baselines (`<dir>/baseline/`); never
/// a snapshot label.
pub const BASELINE_DIR: &str = "baseline";

/// A week's fleet baseline: `<base>/baseline/w<week>/fleet/` — the image's own files from one
/// freshly set-up pod, for every pod's snapshots of that week (and later, until a newer one)
/// to hard-link to ([`snapshot_link_dests`]).
pub fn baseline_dir(base: &Path, week: u32) -> PathBuf {
    base.join(BASELINE_DIR).join(format!("w{week}")).join("fleet")
}

/// The newest non-empty fleet baseline of `week` or before (any week when `None`).
pub fn newest_baseline(base: &Path, week: Option<u32>) -> Option<PathBuf> {
    let mut weeks: Vec<u32> = std::fs::read_dir(base.join(BASELINE_DIR))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str()?.strip_prefix('w')?.parse::<u32>().ok())
        .filter(|w| week.map_or(true, |max| *w <= max))
        .collect();
    weeks.sort_unstable();
    weeks.into_iter().rev().map(|w| baseline_dir(base, w)).find(|d| non_empty_dir(d))
}

fn non_empty_dir(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) && std::fs::read_dir(dir).is_ok_and(|mut e| e.next().is_some())
}

/// What a new snapshot `<base>/<label>/<pod>/` hard-links its unchanged files to, as absolute
/// paths (rsync takes a relative `--link-dest` relative to the destination — absolute leaves no
/// doubt), most likely match first: this pod's newest other `wNdM` snapshot (day-to-day: the
/// files setup touched differ per pod, so only the pod's own history has them), then the
/// newest fleet baseline of the label's week or before (the image's files, the same on every
/// pod — what a new or replaced pod's first snapshot shares). Only directories that exist; none
/// at all when `base` doesn't exist yet.
pub fn snapshot_link_dests(base: &Path, label: &str, pod: &str) -> Vec<String> {
    let Ok(base) = std::fs::canonicalize(base) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(prev) = crate::restore::snapshot_labels(&base, pod).into_iter().find(|l| l != label) {
        out.push(base.join(prev).join(pod));
    }
    if let Some(b) = newest_baseline(&base, crate::restore::parse_label(label).map(|(w, _)| w)) {
        out.push(b);
    }
    out.into_iter().map(|p| p.display().to_string()).collect()
}

/// What a backup tree takes on disk ([`disk_use`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskUse {
    /// Regular files in it, and their bytes (what `du` of this tree alone says).
    pub files: u64,
    pub bytes: u64,
    /// The bytes of the files no other tree shares (link count 1): what keeping this tree
    /// costs on top of the others — `du` over all the snapshots counts a hard-linked file once,
    /// so this is the tree's share of that.
    pub own_bytes: u64,
}

/// [`DiskUse`] of the tree at `path` (symlinks aren't followed or counted).
pub fn disk_use(path: &Path) -> std::io::Result<DiskUse> {
    let mut d = DiskUse::default();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let m = entry.metadata()?;
            if m.is_dir() {
                stack.push(entry.path());
            } else if m.is_file() {
                d.files += 1;
                d.bytes += m.len();
                if links(&m) <= 1 {
                    d.own_bytes += m.len();
                }
            }
        }
    }
    Ok(d)
}

#[cfg(unix)]
fn links(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.nlink()
}

#[cfg(not(unix))]
fn links(_: &std::fs::Metadata) -> u64 {
    1
}

/// What [`prune_baseline`] kept and removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneStats {
    pub kept_files: u64,
    pub kept_bytes: u64,
    pub removed_files: u64,
    pub removed_bytes: u64,
}

/// Reduce a baseline freshly pulled from one pod to **the image's own files**: remove every
/// file and symlink last modified at or after `since` (UNIX seconds: the pod's container
/// creation), then the directories that leaves empty.
///
/// Why: rsync's `--link-dest` takes a file as "the same" when its size, mtime and permissions
/// match — it never compares contents. Within one pod's history that's safe. Across pods it is
/// only safe for files nobody wrote after the image was built: the image's files carry the
/// image's mtimes on every pod, while a file written at setup or later (`~/.name`, the API keys
/// in `.bashrc`, a script's output written on several pods in the same second) can match
/// another pod's in size and mtime with different contents — and a snapshot would then hold
/// the wrong pod's file. So the baseline keeps only what predates the container.
pub fn prune_baseline(dir: &Path, since: u64) -> std::io::Result<PruneStats> {
    let mut s = PruneStats::default();
    prune_dir(dir, since, &mut s)?;
    Ok(s)
}

/// [`prune_baseline`] below `dir`; whether `dir` is now empty.
fn prune_dir(dir: &Path, since: u64, s: &mut PruneStats) -> std::io::Result<bool> {
    let mut empty = true;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let m = entry.metadata()?; // not following symlinks
        if m.is_dir() {
            if prune_dir(&entry.path(), since, s)? {
                std::fs::remove_dir(entry.path())?;
            } else {
                empty = false;
            }
            continue;
        }
        let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
        let size = if m.is_file() { m.len() } else { 0 };
        if mtime.map_or(true, |t| t >= since) {
            std::fs::remove_file(entry.path())?;
            s.removed_files += 1;
            s.removed_bytes += size;
        } else {
            s.kept_files += 1;
            s.kept_bytes += size;
            empty = false;
        }
    }
    Ok(empty)
}

/// What an rsync `--stats` summary says moved (see [`parse_rsync_stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RsyncStats {
    /// Regular files sent, new or changed (`Number of regular files transferred`).
    pub files_sent: u64,
    /// Their size, in bytes (`Total transferred file size`; the files' size, not the wire's).
    pub bytes_sent: u64,
    /// Everything the transfer covered, sent or not (`Number of files`); `None` = not said.
    pub files: Option<u64>,
    /// Its size in bytes (`Total file size`).
    pub bytes: Option<u64>,
}

impl RsyncStats {
    /// `12 files, 3.4M` — what was sent (the old summary's shape).
    pub fn sent(&self) -> String {
        format!("{} files, {}", self.files_sent, human_bytes(self.bytes_sent))
    }

    /// Two passes into one destination as one ([`PullConfig::passes`]).
    pub fn add(self, o: RsyncStats) -> RsyncStats {
        let sum = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        RsyncStats {
            files_sent: self.files_sent + o.files_sent,
            bytes_sent: self.bytes_sent + o.bytes_sent,
            files: sum(self.files, o.files),
            bytes: sum(self.bytes, o.bytes),
        }
    }

    /// `12 files, 3.4M sent of 6754, 1.8G` — what moved, and what the tree it made holds (with
    /// `--link-dest` the rest was hard-linked, not sent).
    pub fn summary(&self) -> String {
        match (self.files, self.bytes) {
            (Some(n), Some(b)) => format!("{} sent of {n}, {}", self.sent(), human_bytes(b)),
            _ => self.sent(),
        }
    }
}

/// Parse rsync `--stats` output so a pull reports what actually moved (not just "done").
/// `None` if the stats aren't there. Reads every way rsync prints a number — plain
/// (`1234567`), digit-grouped by locale (`1,234,567` / `1.234.567`), and human-readable with
/// a unit (`1.84G`, `1,84G`: units of 1000, what one `--human-readable` gives) — so a size
/// is never mistaken for its digits.
pub fn parse_rsync_stats(stdout: &str) -> Option<RsyncStats> {
    let mut s = RsyncStats::default();
    let mut seen = false;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Number of regular files transferred:") {
            s.files_sent = rsync_number(rest)?;
            seen = true;
        } else if let Some(rest) = line.strip_prefix("Total transferred file size:") {
            s.bytes_sent = rsync_number(rest).unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("Number of files:") {
            // `6,754 (reg: 6,000, dir: 754)`: the regular files, to compare with those sent.
            s.files = rest.split_once("reg:").map_or_else(|| rsync_number(rest), |(_, reg)| rsync_number(reg.trim_end_matches(')')));
        } else if let Some(rest) = line.strip_prefix("Total file size:") {
            s.bytes = rsync_number(rest);
        }
    }
    seen.then_some(s)
}

/// The first number in an rsync stats value (`" 1,234,567 bytes"`, `" 6,754 (reg: …)"`,
/// `" 1.84G bytes"`). See [`parse_rsync_stats`] for the formats.
fn rsync_number(raw: &str) -> Option<u64> {
    let tok = raw.split_whitespace().next()?;
    let unit = |c: char| match c {
        'K' => Some(1e3),
        'M' => Some(1e6),
        'G' => Some(1e9),
        'T' => Some(1e12),
        'P' => Some(1e15),
        _ => None,
    };
    match tok.chars().last().and_then(unit) {
        Some(scale) => {
            let num = tok[..tok.len() - 1].replace(',', ".");
            let v: f64 = num.parse().ok().filter(|v: &f64| v.is_finite() && *v >= 0.0)?;
            Some((v * scale).round() as u64)
        }
        // Without a unit any `,`/`.` is a thousands separator (rsync prints no fractions here).
        None if tok.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '.') => {
            tok.chars().filter(char::is_ascii_digit).collect::<String>().parse().ok()
        }
        None => None,
    }
}

/// Bytes as a compact human string ("0", "12K", "3.4M", "1.2G").
pub fn human_bytes(b: u64) -> String {
    const U: &[(&str, u64)] = &[("G", 1 << 30), ("M", 1 << 20), ("K", 1 << 10)];
    for (suffix, scale) in U {
        if b >= *scale {
            return format!("{:.1}{suffix}", b as f64 / *scale as f64);
        }
    }
    b.to_string()
}

/// The `rsync` command for dry-run output, every argument shell-quoted where a shell would
/// change it ([`crate::ssh::shell_word`]) — the globs (`'**/.git/**'`, `'models--*/'`) and the
/// `-e` transport — so it pastes into a shell as exactly the argv that would run.
pub fn display_rsync(target: &SshTarget, pc: &PullConfig, local_dest: &str) -> String {
    display_argv("rsync", &rsync_args(target, pc, local_dest))
}

/// `program` and `args` as one shell-ready line ([`crate::ssh::shell_word`] per argument).
pub fn display_argv(program: &str, args: &[String]) -> String {
    let mut out = String::from(program);
    for arg in args {
        out.push(' ');
        out.push_str(&crate::ssh::shell_word(arg));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SshTarget {
        SshTarget {
            user: "root".into(),
            host: "1.2.3.4".into(),
            port: 22001,
            key_paths: vec!["/root/.ssh/arena8_key".into()],
            connect_timeout_secs: 10,
        }
    }

    #[test]
    fn backup_timeout_table() {
        // (config value, budget in seconds or the error's tell)
        let cases: &[(Option<&str>, std::result::Result<u64, &str>)] = &[
            (None, Ok(DEFAULT_BACKUP_TIMEOUT_SECS)),
            (Some(""), Ok(7200)),
            (Some("  "), Ok(7200)),
            (Some("600"), Ok(600)),
            (Some(" 1 "), Ok(1)),
            (Some("86400"), Ok(86_400)),
            (Some("86401"), Err("got `86401`")),
            (Some("0"), Err("got `0`")),
            (Some("-5"), Err("got `-5`")),
            (Some("2h"), Err("got `2h`")),
            (Some("18446744073709551616"), Err("1..=86400")),
        ];
        for (raw, want) in cases {
            match (backup_timeout(*raw), want) {
                (Ok(d), Ok(secs)) => assert_eq!(d, Duration::from_secs(*secs), "{raw:?}"),
                (Err(e), Err(tell)) => {
                    let e = e.to_string();
                    assert!(e.contains("BACKUP_TIMEOUT_SECS") && e.contains(tell), "{raw:?}: {e}");
                }
                (got, _) => panic!("{raw:?}: got {got:?}, want {want:?}"),
            }
        }
    }

    #[test]
    fn parses_rsync_stats() {
        let out = "sent 18 bytes  received 102000000 bytes\n\
                   Number of files: 1,200\n\
                   Number of regular files transferred: 922\n\
                   Total transferred file size: 107,000,000 bytes\n";
        let s = parse_rsync_stats(out).unwrap();
        assert_eq!((s.files_sent, s.bytes_sent, s.files, s.bytes), (922, 107_000_000, Some(1200), None));
        assert_eq!(s.sent(), "922 files, 102.0M");
        assert_eq!(parse_rsync_stats("nothing here"), None);
    }

    /// Every way rsync prints its numbers. The live bug: `--human-readable` stats said `1.84G`
    /// and the digits-only parse reported `184` (bytes) for a 1.8G pull.
    #[test]
    fn rsync_numbers_in_every_format() {
        let cases: &[(&str, Option<u64>)] = &[
            (" 1234567 bytes", Some(1_234_567)),          // --no-human-readable / old rsync
            (" 1,234,567 bytes", Some(1_234_567)),        // the default (level 1), `,` grouping
            (" 1.234.567 bytes", Some(1_234_567)),        // level 1 in a `,`-decimal locale
            (" 1.84G bytes", Some(1_840_000_000)),        // one --human-readable (units of 1000)
            (" 1,84G bytes", Some(1_840_000_000)),        // …in a `,`-decimal locale
            (" 102.00M bytes", Some(102_000_000)),
            (" 3.50K bytes", Some(3_500)),
            (" 0 bytes", Some(0)),
            (" 6,754 (reg: 6,000, dir: 754)", Some(6_754)),
            (" ", None),
            (" lots", None),
            (" -1G", None),
        ];
        for (raw, want) in cases {
            assert_eq!(rsync_number(raw), *want, "{raw:?}");
        }
        let human = "Number of files: 6,754 (reg: 6,000, dir: 754)\n\
                     Number of regular files transferred: 6,754\n\
                     Total file size: 1.84G bytes\n\
                     Total transferred file size: 1.84G bytes\n";
        let s = parse_rsync_stats(human).unwrap();
        assert_eq!(s.sent(), "6754 files, 1.7G");
        assert_eq!(s.bytes, Some(1_840_000_000));
        // Our own flags don't ask for the unit format any more (it's still parsed).
        assert!(!rsync_args(&target(), &PullConfig::default(), "./d/").iter().any(|a| a == "--human-readable"));
    }

    #[test]
    fn dest_nests_label_and_name_with_trailing_slash() {
        assert_eq!(local_dest("./backup", "w1d3", "arena8-apple"), "./backup/w1d3/arena8-apple/");
        // base's trailing slash is normalized
        assert_eq!(local_dest("./backup/", "w1d3", "arena8-apple"), "./backup/w1d3/arena8-apple/");
    }

    #[test]
    fn builds_rsync_with_caps_excludes_and_transport() {
        let pc = PullConfig::default();
        let a = rsync_args(&target(), &pc, "./backup/w1d3/arena8-apple/");
        let joined = a.join(" ");
        assert!(joined.contains("-avz"));
        assert!(joined.contains("--max-size=50M"));
        assert!(joined.contains("--prune-empty-dirs"));
        // A transfer that goes silent gives up instead of hanging the backup forever.
        assert!(a.contains(&"--timeout=300".to_string()), "{joined}");
        // default excludes present as separate args (dotdirs, site-packages, and the
        // non-dot caches/envs that would otherwise bloat the backup)
        for ex in ["**/.*/", ".cache/", "site-packages/", "__pycache__/", "hf_cache/", "huggingface/", "venv/", "node_modules/", "models--*/", "datasets--*/"] {
            assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == ex), "missing exclude {ex}");
        }
        // .git is kept via an include that precedes the dotdir exclude
        assert!(a.windows(2).any(|w| w[0] == "--include" && w[1] == "**/.git/"));
        let inc = a.iter().position(|x| x == "--include").unwrap();
        let exc = a.iter().position(|x| x == "--exclude").unwrap();
        assert!(inc < exc, "includes must come before excludes");
        // without_git drops the .git include
        let no_git = rsync_args(&target(), &PullConfig::default().without_git(), "./d/");
        assert!(!no_git.iter().any(|x| x.contains(".git")));
        // transport carries port + key but NOT the host (rsync adds that)
        let e_idx = a.iter().position(|x| x == "-e").unwrap();
        let rsh = &a[e_idx + 1];
        assert!(rsh.contains("-p 22001"));
        assert!(rsh.contains("-i /root/.ssh/arena8_key"));
        assert!(!rsh.contains("1.2.3.4"));
        // source is user@host: (home) and dest is local, in that order at the end
        assert_eq!(a[a.len() - 2], "root@1.2.3.4:");
        assert_eq!(a[a.len() - 1], "./backup/w1d3/arena8-apple/");
    }

    #[test]
    fn big_tier_keeps_all_files_no_delete() {
        let a = rsync_args(&target(), &PullConfig::big_tier(), "./backup/big/arena8-apple/");
        let joined = a.join(" ");
        assert!(!joined.contains("--delete"), "big tier accumulates; it must never delete");
        assert!(!joined.contains("--max-size"), "big tier has no upper cap");
        assert!(!joined.contains("--min-size"), "big tier has no lower bound");
        // shares the same excludes (HF cache etc.) as the snapshot tier
        assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == "models--*/"));
        // dateless running dest
        assert_eq!(big_dest("./backup", "arena8-apple"), "./backup/big/arena8-apple/");
        // and the snapshot tier still caps with --max-size and never deletes
        let s = rsync_args(&target(), &PullConfig::small_tier("50M"), "./d/").join(" ");
        assert!(s.contains("--max-size=50M") && !s.contains("--min-size") && !s.contains("--delete"));
    }

    #[test]
    fn replication_excludes_claude_keeps_git_and_caches() {
        let pc = PullConfig::replication();
        let a = rsync_args(&target(), &pc, "root@5.6.7.8:");
        // .claude is NOT re-included (would be ToS-sensitive to replicate pod->pod)…
        assert!(!pc.includes.iter().any(|i| i.contains(".claude")));
        // …and the whole .claude* family is excluded — crucially the `.claude.json` *file*
        // (creds), which the dotdir-only `**/.*/` exclude would miss.
        assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == ".claude*"));
        assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == "**/.claude*"));
        // .ssh is explicitly excluded too (never replicate keys/authorized_keys).
        assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == ".ssh/"));
        // shell rc files carry per-pod API keys (copy-keys writes them there) — excluded.
        for f in [".bashrc", ".zshrc", ".zsh_history"] {
            assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == f), "missing exclude {f}");
        }
        // .git is still kept (branch/commit state carries to the new pod).
        assert!(a.windows(2).any(|w| w[0] == "--include" && w[1] == "**/.git/"));
        // cache / HF model caches stay excluded (already covered by defaults).
        for ex in [".cache/", "hf_cache/", "models--*/", "datasets--*/"] {
            assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == ex), "missing exclude {ex}");
        }
        // full tree — no size cap for a replacement.
        assert!(!a.iter().any(|x| x.starts_with("--max-size")));
    }

    #[test]
    fn pod_to_pod_command_quotes_globs_and_targets_endpoint() {
        let cmd = pod_to_pod_command("5.6.7.8", 22042, "root", "/root/.ssh/id_ed25519", &PullConfig::replication());
        // runs rsync, authenticates with the deploy key over the dest's raw port…
        assert!(cmd.starts_with("rsync "));
        assert!(cmd.contains("ssh -p 22042"));
        assert!(cmd.contains("-i /root/.ssh/id_ed25519"));
        // …glob excludes are single-quoted so the source shell doesn't expand them…
        assert!(cmd.contains("'**/.*/'"));
        assert!(cmd.contains("'.claude*'"));
        // …a stalled copy gives up (the source pod's rsync, too)…
        assert!(cmd.contains(" '--timeout=300' "), "{cmd}");
        // …$HOME stays unquoted (remote shell expands it), dest is endpoint:home.
        assert!(cmd.contains(" $HOME/ "));
        assert!(cmd.trim_end().ends_with("root@5.6.7.8:"));
    }

    #[test]
    fn push_rsync_flips_direction_local_to_remote() {
        let pc = PullConfig::replication();
        let a = push_rsync_args(&target(), &pc, "/tmp/stage/arena8-apple/");
        // Same flags/excludes as a pull…
        assert!(a.contains(&"-avz".to_string()));
        assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == ".claude*"));
        // …but local dir is the SOURCE and the pod is the DEST (reverse of rsync_args).
        assert_eq!(a[a.len() - 2], "/tmp/stage/arena8-apple/");
        assert_eq!(a[a.len() - 1], "root@1.2.3.4:");
        // owner/group preservation is OFF on a push, so the dest home isn't chowned to the
        // control user (which would lock sshd out).
        assert!(a.contains(&"--no-owner".to_string()) && a.contains(&"--no-group".to_string()));
        assert!(a.contains(&"--timeout=300".to_string()));
        // `None` drops the I/O timeout entirely.
        let unbounded = PullConfig { io_timeout_secs: None, ..PullConfig::replication() };
        assert!(!push_rsync_args(&target(), &unbounded, "/tmp/s/").iter().any(|x| x.starts_with("--timeout")));
        // transport still carries port + key
        let e = a.iter().position(|x| x == "-e").unwrap();
        assert!(a[e + 1].contains("-p 22001") && a[e + 1].contains("-i /root/.ssh/arena8_key"));
    }

    #[test]
    fn honors_custom_remote_path() {
        let mut pc = PullConfig::default();
        pc.remote_path = "ARENA_3.0/results".into();
        let a = rsync_args(&target(), &pc, "./out/");
        assert_eq!(a[a.len() - 2], "root@1.2.3.4:ARENA_3.0/results");
    }

    /// The snapshot tier is two passes when it keeps `.git` (the files under the cap, then every
    /// `.git` uncapped); every other tier is one. Link-dests ride along on both.
    #[test]
    fn a_capped_tier_keeping_git_is_two_passes() {
        let snap = PullConfig { link_dest: vec!["/b/w1d1/devtest-a".into()], ..PullConfig::small_tier("50M") };
        let p = snap.passes();
        assert_eq!(p.len(), 2);
        let files = rsync_args(&target(), &p[0], "./d/");
        assert!(files.contains(&"--max-size=50M".to_string()) && !files.iter().any(|a| a.contains(".git")), "{files:?}");
        assert!(files.contains(&"--include".to_string()), "the .claude includes stay: {files:?}");
        let git = rsync_args(&target(), &p[1], "./d/");
        let joined = git.join(" ");
        assert!(!joined.contains("--max-size") && !joined.contains(".claude"), "{joined}");
        assert!(joined.contains("--include **/.git/ --include **/.git/** --exclude **/.*/"), "{joined}");
        // Its last rules: every other dir (to reach the .gits in it), no other file.
        let tail: Vec<&str> = git.iter().map(String::as_str).skip_while(|a| *a != "*/").take(3).collect();
        assert_eq!(tail, ["*/", "--exclude", "*"], "{joined}");
        for args in [&files, &git] {
            assert!(args.contains(&"--link-dest=/b/w1d1/devtest-a".to_string()), "{args:?}");
        }
        // One pass: the big tier, --no-git, the replication profile, a pass of a pass.
        for one in [PullConfig::big_tier(), PullConfig::small_tier("50M").without_git(), PullConfig::replication(), p[1].clone()] {
            assert_eq!(one.passes().len(), 1);
        }
        assert!(!rsync_args(&target(), &PullConfig::big_tier(), "./d/").iter().any(|a| a.starts_with("--link-dest")));
    }

    #[test]
    fn snapshots_link_to_their_previous_one_and_the_weeks_baseline() {
        let base = std::env::temp_dir().join(format!("arena-pull-linkdests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let put = |rel: &str| {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        };
        for rel in ["w0d1/devtest-a/f", "w0d3/devtest-a/f", "w1d1/devtest-b/f", "custom/devtest-a/f", "big/devtest-a/f"] {
            put(rel);
        }
        put("baseline/w0/fleet/img");
        put("baseline/w2/fleet/img");
        std::fs::create_dir_all(base.join("baseline/w1/fleet")).unwrap(); // empty: not a baseline
        let abs = std::fs::canonicalize(&base).unwrap();
        let at = |rel: &str| abs.join(rel).display().to_string();
        // (label, pod, link-dests)
        let cases: &[(&str, &str, Vec<String>)] = &[
            ("w1d2", "devtest-a", vec![at("w0d3/devtest-a"), at("baseline/w0/fleet")]),
            ("w0d3", "devtest-a", vec![at("w0d1/devtest-a"), at("baseline/w0/fleet")]),
            ("w2d1", "devtest-a", vec![at("w0d3/devtest-a"), at("baseline/w2/fleet")]),
            ("w1d2", "devtest-new", vec![at("baseline/w0/fleet")]), // a new pod: the image's files
            ("custom2", "devtest-a", vec![at("w0d3/devtest-a"), at("baseline/w2/fleet")]),
        ];
        for (label, pod, want) in cases {
            assert_eq!(&snapshot_link_dests(&base, label, pod), want, "{label} {pod}");
        }
        assert_eq!(baseline_dir(&base, 3), base.join("baseline/w3/fleet"));
        let _ = std::fs::remove_dir_all(&base);
        assert!(snapshot_link_dests(&base, "w0d1", "devtest-a").is_empty(), "no backup dir yet");
    }

    /// Only the image's files survive into a baseline: a file (or link) written at or after the
    /// container's creation is dropped, then the directories left empty.
    #[cfg(unix)]
    #[test]
    fn a_baseline_keeps_only_what_predates_the_container() {
        let dir = std::env::temp_dir().join(format!("arena-pull-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let at = |secs: u64| std::time::UNIX_EPOCH + Duration::from_secs(secs);
        let put = |rel: &str, n: usize, mtime: u64| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![b'x'; n]).unwrap();
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(at(mtime)).unwrap();
        };
        // A container created a minute ago: the symlink made just now is newer.
        let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() - 60;
        put("arena-llm-context/model.txt", 1000, created - 86_400 * 30); // the image's
        put("ARENA_materials/chapter1/solutions.py", 200, created - 86_400); // the image's
        put("ARENA_materials/chapter1/my_answers.py", 50, created + 3600); // written since
        put(".name", 30, created); // at setup, the same second: dropped
        put("setup_only/out.log", 10, created + 5); // a whole dir written since: gone
        std::os::unix::fs::symlink("/workspace/ARENA_materials", dir.join("linked")).unwrap(); // made now
        let s = prune_baseline(&dir, created).unwrap();
        assert_eq!(s, PruneStats { kept_files: 2, kept_bytes: 1200, removed_files: 4, removed_bytes: 90 });
        assert!(dir.join("arena-llm-context/model.txt").is_file() && dir.join("ARENA_materials/chapter1/solutions.py").is_file());
        for gone in ["ARENA_materials/chapter1/my_answers.py", ".name", "setup_only", "linked"] {
            assert!(std::fs::symlink_metadata(dir.join(gone)).is_err(), "{gone}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The live problem, end to end with a real rsync: every day's snapshot was a full copy, and
    /// its `.git` lacked the pack over the size cap. Now each snapshot is still a complete tree —
    /// the same layout, its repo fsck-clean — but unchanged files are hard links: to the pod's
    /// previous snapshot, and (for a pod with no snapshot yet) to the fleet baseline captured
    /// from another pod. `disk_use` says what a snapshot really adds.
    #[cfg(unix)]
    #[test]
    fn snapshots_are_hard_linked_complete_trees_with_a_complete_git() {
        use crate::volume::fake_ssh;
        use std::os::unix::fs::MetadataExt;
        if !fake_ssh::have_rsync() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("arena-pull-dedup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let image_time = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        // A pod's home: an image file (its mtime the image's), work, and a repo whose pack is far
        // over the 2K cap.
        let home_of = |pod: &str, work: &str| {
            let h = root.join(pod);
            let img = h.join("arena-llm-context/doc.txt");
            std::fs::create_dir_all(img.parent().unwrap()).unwrap();
            std::fs::write(&img, "the image's own file\n".repeat(50)).unwrap();
            std::fs::File::options().write(true).open(&img).unwrap().set_modified(image_time).unwrap();
            std::fs::create_dir_all(h.join("work")).unwrap();
            std::fs::write(h.join("work/notes.md"), work).unwrap();
            h
        };
        let home = home_of("pod-a", "day 1\n");
        let repo = home.join("ARENA_materials");
        let git = |args: &[&str]| {
            let o = std::process::Command::new("git").current_dir(&repo).args(args).output().unwrap();
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        std::fs::create_dir_all(&repo).unwrap();
        git(&["init", "-q"]);
        let noise: Vec<u8> = (0..60_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        std::fs::write(repo.join("data.bin"), &noise).unwrap();
        git(&["add", "-A"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "data"]);
        git(&["gc", "-q"]);
        let fake = fake_ssh::install(&root);
        let base = root.join("backup");
        let pull = |pod_home: &std::path::Path, label: &str, pod: &str| {
            let dest = local_dest(&base.display().to_string(), label, pod);
            std::fs::create_dir_all(&dest).unwrap();
            let pc = PullConfig { link_dest: snapshot_link_dests(&base, label, pod), ..PullConfig::small_tier("2K") };
            for pass in pc.passes() {
                fake_ssh::rsync(&fake_ssh::swap_transport(&rsync_args(&target(), &pass, &dest), &fake), pod_home);
            }
            base.join(label).join(pod)
        };
        let ino = |p: std::path::PathBuf| std::fs::metadata(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())).ino();
        let pack = |snap: &std::path::Path| {
            std::fs::read_dir(snap.join("ARENA_materials/.git/objects/pack"))
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .find(|p| p.extension().is_some_and(|x| x == "pack"))
                .expect("the pack")
        };

        // Day 1: a complete repo — the pack over the cap included — and the cap still applies to files.
        let d1 = pull(&home, "w0d1", "devtest-a");
        assert!(std::fs::metadata(pack(&d1)).unwrap().len() > 2048);
        assert!(!d1.join("ARENA_materials/data.bin").exists(), "the files over the cap stay in big/");
        let fsck = std::process::Command::new("git").current_dir(d1.join("ARENA_materials")).args(["fsck", "--no-progress"]).output().unwrap();
        assert!(fsck.status.success(), "the snapshot's repo reads: {}", String::from_utf8_lossy(&fsck.stderr));
        assert_eq!(disk_use(&d1).unwrap().own_bytes, disk_use(&d1).unwrap().bytes, "nothing to link to yet");

        // Day 2: only the edited note is new on disk; the rest are links to day 1.
        std::fs::write(home.join("work/notes.md"), "day 2, longer\n").unwrap();
        let d2 = pull(&home, "w0d2", "devtest-a");
        assert_eq!(ino(d2.join("arena-llm-context/doc.txt")), ino(d1.join("arena-llm-context/doc.txt")));
        assert_eq!(ino(pack(&d2)), ino(pack(&d1)), "the pack is linked, not copied again");
        assert_ne!(ino(d2.join("work/notes.md")), ino(d1.join("work/notes.md")));
        assert_eq!(std::fs::read_to_string(d1.join("work/notes.md")).unwrap(), "day 1\n", "day 1 kept as it was");
        let u = disk_use(&d2).unwrap();
        assert_eq!(u.own_bytes, "day 2, longer\n".len() as u64, "{u:?}");
        assert_eq!(u.files, disk_use(&d1).unwrap().files, "a complete tree, the same layout");

        // A baseline from pod-a (container created after the image, before the work)…
        let created = image_time.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600;
        let b = pull(&home, "w0-capture", "devtest-a");
        prune_baseline(&b, created).unwrap();
        std::fs::create_dir_all(baseline_dir(&base, 0).parent().unwrap()).unwrap();
        std::fs::rename(&b, baseline_dir(&base, 0)).unwrap();
        assert!(baseline_dir(&base, 0).join("arena-llm-context/doc.txt").is_file());
        assert!(!baseline_dir(&base, 0).join("work").exists(), "pod-a's work isn't in it");
        // …and a new pod's first snapshot: the image's file is the baseline's, its work its own.
        let home_b = home_of("pod-b", "b's work\n");
        let first = pull(&home_b, "w0d2", "devtest-b");
        assert_eq!(ino(first.join("arena-llm-context/doc.txt")), ino(baseline_dir(&base, 0).join("arena-llm-context/doc.txt")));
        assert_eq!(std::fs::read_to_string(first.join("work/notes.md")).unwrap(), "b's work\n");
        assert_eq!(disk_use(&first).unwrap().own_bytes, "b's work\n".len() as u64);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn display_quotes_the_transport() {
        let s = display_rsync(&target(), &PullConfig::default(), "./out/");
        assert!(s.starts_with("rsync -avz"));
        assert!(s.contains("'ssh -p 22001"));
        assert!(s.contains("--include '**/.git/**'") && s.contains("--exclude 'models--*/'"), "{s}");
    }

    /// The dry-run line pastes into a shell as exactly the argv that runs — in a directory where
    /// the old unquoted globs (`**/.git/**`, `models--*/`, `*.arena-aside-*/`) would have
    /// expanded into local file names.
    #[cfg(unix)]
    #[test]
    fn a_displayed_rsync_pastes_back_as_its_argv() {
        let dir = std::env::temp_dir().join(format!("arena-display-rsync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["models--x", "a.arena-aside-1", "z/.git/objects", "venv"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let pc = PullConfig { remote_path: "ARENA materials/".into(), ..PullConfig::small_tier("50M") };
        let dest = "./backup/w0d4/devtest bravo/";
        let line = display_rsync(&target(), &pc, dest);
        let rest = line.strip_prefix("rsync ").unwrap();
        let o = std::process::Command::new("bash")
            .current_dir(&dir)
            .args(["-O", "globstar", "-c", &format!("printf '%s\\n' {rest}")])
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let back: Vec<String> = String::from_utf8_lossy(&o.stdout).lines().map(String::from).collect();
        assert_eq!(back, rsync_args(&target(), &pc, dest), "{line}");
    }
}
