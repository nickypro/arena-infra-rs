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
    /// Skip any single file larger than this (rsync `--max-size`), e.g. `"50M"`.
    pub max_size: String,
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
            max_size: "50M".to_string(),
            // Keep `.git` (at any depth) so the backup is a usable git repo…
            includes: vec!["**/.git/".to_string(), "**/.git/**".to_string()],
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
}

/// The local destination for a pod's backup: `<base>/<label>/<pod-name>/`. The trailing
/// slash matters to rsync (copy *into* this dir).
pub fn local_dest(base: &str, label: &str, pod_name: &str) -> String {
    format!("{}/{label}/{pod_name}/", base.trim_end_matches('/'))
}

/// Build the `rsync` argv (everything after the program name) to pull `target`'s home
/// (or `pc.remote_path`) into `local_dest`. Uses the target's transport options via
/// `-e ssh …`; rsync appends `user@host` to the remote spec itself.
pub fn rsync_args(target: &SshTarget, pc: &PullConfig, local_dest: &str) -> Vec<String> {
    let mut a = vec![
        "-avz".to_string(),
        "--human-readable".into(),
        "--info=progress2".into(),
        "--stats".into(), // emit a summary we parse to report files/bytes per pod
        "--prune-empty-dirs".into(),
        format!("--max-size={}", pc.max_size),
    ];
    // Includes first (they win over a later exclude), then excludes.
    for inc in &pc.includes {
        a.push("--include".into());
        a.push(inc.clone());
    }
    for ex in &pc.excludes {
        a.push("--exclude".into());
        a.push(ex.clone());
    }
    // Transport: the ssh command without user@host (rsync supplies that).
    a.push("-e".into());
    a.push(target.rsh_command());
    // Source `user@host:<path>` — empty path means the login (home) dir.
    a.push(format!("{}@{}:{}", target.user, target.host, pc.remote_path));
    a.push(local_dest.to_string());
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
        for ex in ["**/.*/", ".cache/", "site-packages/", "__pycache__/", "hf_cache/", "huggingface/", "venv/", "node_modules/"] {
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
