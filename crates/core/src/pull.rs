//! Pulling pod files to a local backup (legacy `backup.sh`).
//!
//! The git autocommit (`pods backup`) saves the *repo*; this saves the rest of a pod's
//! home directory to the operator's machine via rsync. It's a safety net for files that
//! never made it into git before a pod is torn down.
//!
//! Mirrors the legacy flags: archive + compress, human-readable progress, prune empty
//! dirs, and a per-file size cap so a stray multi-GB checkpoint doesn't blow up the
//! backup. Dotfile *directories* (caches, `.git`, …) and `site-packages/` are excluded —
//! they're either reconstructable or huge. The rsync arg vector is built purely here so
//! it's unit-tested; the process spawn lives in the caller.

use crate::ssh::SshTarget;

/// Options for a pull, defaulting to the legacy `backup.sh` behavior.
#[derive(Debug, Clone)]
pub struct PullConfig {
    /// Skip any single file larger than this (rsync `--max-size`), e.g. `"50M"`.
    pub max_size: String,
    /// Paths/globs to exclude (rsync `--exclude`).
    pub excludes: Vec<String>,
    /// Remote path to pull from, relative to the SSH login dir. Empty = the home dir.
    pub remote_path: String,
}

impl Default for PullConfig {
    fn default() -> Self {
        Self {
            max_size: "50M".to_string(),
            // Dotfile dirs (`.cache`, `.git`, …) and python envs: big and/or rebuildable.
            excludes: vec!["**/.*/".to_string(), "site-packages/".to_string()],
            remote_path: String::new(),
        }
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
        "--prune-empty-dirs".into(),
        format!("--max-size={}", pc.max_size),
    ];
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
        // both default excludes present as separate args
        assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == "**/.*/"));
        assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == "site-packages/"));
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
