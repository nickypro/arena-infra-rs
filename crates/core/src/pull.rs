//! Pulling pod files to a local backup (legacy `backup.sh`).
//!
//! The git autocommit (`pods backup`) saves the *repo*; this saves the rest of a pod's
//! home directory to the operator's machine via rsync. It's a safety net for files that
//! never made it into git before a pod is torn down.
//!
//! Mirrors the legacy flags: archive + compress, human-readable progress, prune empty
//! dirs, and a per-file size cap so a stray multi-GB checkpoint doesn't blow up the
//! backup. Dotfile *directories* (caches, `.git`, …), `site-packages/`, and common
//! reconstructable/huge non-dot dirs (`hf_cache/`, `__pycache__/`, `venv/`,
//! `node_modules/`) are excluded — they're either reconstructable or huge. The rsync arg
//! vector is built purely here so it's unit-tested; the process spawn lives in the caller.

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
}

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
            ],
            remote_path: String::new(),
        }
    }
}

impl PullConfig {
    /// Drop the `.git` includes (so `.git` is excluded by `**/.*/` like other dotdirs).
    pub fn without_git(mut self) -> Self {
        self.includes.retain(|i| !i.contains(".git"));
        self
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
    parts.push(format!("{dest_user}@{dest_ip}:"));
    parts.join(" ")
}

/// Single-quote a value for safe inclusion in a remote `sh -c` string (POSIX `'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The shared `rsync` flags + size filters + include/exclude rules (everything except the
/// `-e` transport and the source/dest operands), so pull and push stay in lockstep.
fn rsync_flags(pc: &PullConfig) -> Vec<String> {
    let mut a = vec![
        "-avz".to_string(),
        "--human-readable".into(),
        "--info=progress2".into(),
        "--stats".into(), // emit a summary we parse to report files/bytes per pod
        "--prune-empty-dirs".into(),
    ];
    // `--max-size` keeps the snapshot tier small (the big tier leaves it `None` to take
    // everything). `--min-size` is currently unused but honored if a caller sets it.
    if let Some(m) = &pc.max_size {
        a.push(format!("--max-size={m}"));
    }
    if let Some(m) = &pc.min_size {
        a.push(format!("--min-size={m}"));
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
    a
}

/// Parse rsync `--stats` output into `(files_transferred, human-readable bytes)` so a
/// pull reports what actually moved (not just "done"). `None` if the stats aren't found.
pub fn parse_rsync_stats(stdout: &str) -> Option<(u64, String)> {
    let mut files = None;
    let mut bytes = None;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Number of regular files transferred:") {
            files = rest.trim().replace(',', "").parse::<u64>().ok();
        } else if let Some(rest) = line.strip_prefix("Total transferred file size:") {
            // e.g. "Total transferred file size: 1,234,567 bytes"
            let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
            bytes = digits.parse::<u64>().ok();
        }
    }
    files.map(|f| (f, human_bytes(bytes.unwrap_or(0))))
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

/// Human-readable `rsync` command for dry-run output (the `-e` value is quoted since it
/// contains spaces).
pub fn display_rsync(target: &SshTarget, pc: &PullConfig, local_dest: &str) -> String {
    let args = rsync_args(target, pc, local_dest);
    let mut out = String::from("rsync");
    for arg in args {
        if arg.contains(' ') {
            out.push_str(&format!(" '{}'", arg));
        } else {
            out.push_str(&format!(" {arg}"));
        }
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
    fn parses_rsync_stats() {
        let out = "sent 18 bytes  received 102000000 bytes\n\
                   Number of files: 1,200\n\
                   Number of regular files transferred: 922\n\
                   Total transferred file size: 107,000,000 bytes\n";
        let (files, size) = parse_rsync_stats(out).unwrap();
        assert_eq!(files, 922);
        assert_eq!(size, "102.0M");
        assert_eq!(parse_rsync_stats("nothing here"), None);
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

    #[test]
    fn display_quotes_the_transport() {
        let s = display_rsync(&target(), &PullConfig::default(), "./out/");
        assert!(s.starts_with("rsync -avz"));
        assert!(s.contains("'ssh -p 22001"));
    }
}
