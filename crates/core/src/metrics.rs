//! Per-pod runtime probe for the dashboard: GPU stats (via `nvidia-smi`), git branch,
//! a small setup-health check (`~/.name`, the deploy key, the git origin), and an
//! optional operator-defined progress signal — all gathered in **one** SSH round-trip
//! so a faster refresh cadence doesn't mean more connections per pod.
//!
//! "Notebook progress" has no universal source, so it's a **configurable remote
//! command** (`PROGRESS_CMD`): whatever short string it prints on stdout (e.g. a
//! percentage, an epoch count, the last line of a log) is shown verbatim. If unset,
//! the progress column is simply blank — we don't guess.
//!
//! All of this is read-only: it only ever *reads* `nvidia-smi`, git state, a couple of
//! file existence checks, and runs the operator's progress command. Parsing is pure and
//! unit-tested; the SSH call is thin.

use crate::ssh::{self, SshTarget};

/// Separates the `nvidia-smi` block from the key=value health block in the probe's
/// combined stdout.
pub const SENTINEL: &str = "@@ARENA_PROBE@@";

/// One GPU's stats. Fields are optional so a partial/odd row degrades gracefully.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuStat {
    pub name: Option<String>,
    pub util_pct: Option<u32>,
    pub mem_used_mb: Option<u32>,
    pub mem_total_mb: Option<u32>,
    pub temp_c: Option<u32>,
}

/// The `nvidia-smi` query: one CSV row per GPU, name first, then the numeric fields in
/// the order [`parse_nvidia_smi`] expects.
pub const NVIDIA_SMI_QUERY: &str = "nvidia-smi \
     --query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu \
     --format=csv,noheader,nounits";

/// What to gather in the probe beyond GPU stats. Empty/None fields are skipped, so a
/// bare metrics-only probe is still possible.
#[derive(Debug, Clone, Default)]
pub struct ProbeOpts {
    /// Operator progress command (`PROGRESS_CMD`); its last stdout line is shown.
    pub progress_cmd: Option<String>,
    /// The ARENA checkout path on the pod — enables branch + git-origin reporting.
    pub repo_path: Option<String>,
    /// The deploy key path on the pod (`GIT_SSH_KEY_REMOTE`) — enables the key check.
    pub key_remote: Option<String>,
}

/// Parse `nvidia-smi --format=csv,noheader,nounits` output into one stat per GPU.
pub fn parse_nvidia_smi(stdout: &str) -> Vec<GpuStat> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            let num = |i: usize| f.get(i).and_then(|s| s.parse::<u32>().ok());
            GpuStat {
                name: f.first().map(|s| normalize_gpu_name(s)).filter(|s| !s.is_empty()),
                util_pct: num(1),
                mem_used_mb: num(2),
                mem_total_mb: num(3),
                temp_c: num(4),
            }
        })
        .collect()
}

/// Trim vendor noise from an `nvidia-smi` GPU name, preserving model case:
/// "NVIDIA RTX A4000" -> "RTX A4000", "NVIDIA GeForce RTX 4090" -> "RTX 4090".
pub fn normalize_gpu_name(raw: &str) -> String {
    let mut s = raw.trim();
    for prefix in ["NVIDIA ", "GeForce ", "Tesla "] {
        while let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim_start();
        }
    }
    s.to_string()
}

/// Aggregated metrics + status for one pod, as shown in a dashboard row.
#[derive(Debug, Clone, Default)]
pub struct PodMetrics {
    pub gpus: Vec<GpuStat>,
    pub progress: Option<String>,
    /// Set when the SSH probe failed (pod down, unreachable, key unreadable, etc.).
    pub error: Option<String>,
    /// Current git branch of the ARENA checkout, if reported.
    pub branch: Option<String>,
    /// The `origin` remote URL, if reported (used to check it points at GitHub).
    pub origin: Option<String>,
    /// Whether `~/.name` exists (None = not probed / unreachable).
    pub has_name: Option<bool>,
    /// Whether the deploy key exists on the pod (None = not probed / unreachable).
    pub has_key: Option<bool>,
    /// Root-filesystem usage (used, total) in MB, if reported.
    pub disk_used_mb: Option<u32>,
    pub disk_total_mb: Option<u32>,
}

impl PodMetrics {
    /// Mean GPU utilization across this pod's GPUs, if any reported.
    pub fn mean_util(&self) -> Option<u32> {
        let vals: Vec<u32> = self.gpus.iter().filter_map(|g| g.util_pct).collect();
        if vals.is_empty() {
            None
        } else {
            Some(vals.iter().sum::<u32>() / vals.len() as u32)
        }
    }

    /// Summed memory (used, total) in MB across GPUs, counting only GPUs that report
    /// *both* values — so a half-reported GPU can't skew the ratio (e.g. used > total).
    pub fn mem_summary(&self) -> Option<(u32, u32)> {
        let mut used = 0u32;
        let mut total = 0u32;
        let mut any = false;
        for g in &self.gpus {
            if let (Some(u), Some(t)) = (g.mem_used_mb, g.mem_total_mb) {
                used += u;
                total += t;
                any = true;
            }
        }
        if any {
            Some((used, total))
        } else {
            None
        }
    }

    /// Max temperature across GPUs, if reported.
    pub fn max_temp(&self) -> Option<u32> {
        self.gpus.iter().filter_map(|g| g.temp_c).max()
    }

    /// Root-filesystem (used, total) in MB, if both reported.
    pub fn disk_summary(&self) -> Option<(u32, u32)> {
        match (self.disk_used_mb, self.disk_total_mb) {
            (Some(u), Some(t)) => Some((u, t)),
            _ => None,
        }
    }

    /// A compact GPU descriptor like "RTX A4000" or "2×RTX A4000", from the live
    /// `nvidia-smi` readout (the provider list API often doesn't report GPU type).
    pub fn gpu_summary(&self) -> Option<String> {
        if self.gpus.is_empty() {
            return None;
        }
        let n = self.gpus.len();
        let name = self
            .gpus
            .iter()
            .find_map(|g| g.name.clone())
            .unwrap_or_else(|| "GPU".to_string());
        Some(if n > 1 { format!("{n}×{name}") } else { name })
    }
}

/// Build the single combined remote command for a probe.
fn remote_command(opts: &ProbeOpts) -> String {
    let mut s = format!("{NVIDIA_SMI_QUERY} 2>/dev/null; echo '{SENTINEL}'; ");
    if let Some(repo) = &opts.repo_path {
        let q = shell_quote(repo);
        s.push_str(&format!(
            "echo \"branch=$(cd {q} 2>/dev/null && git rev-parse --abbrev-ref HEAD 2>/dev/null)\"; "
        ));
        s.push_str(&format!(
            "echo \"origin=$(cd {q} 2>/dev/null && git config --get remote.origin.url 2>/dev/null)\"; "
        ));
    }
    s.push_str("([ -e \"$HOME/.name\" ] && echo name=1 || echo name=0); ");
    if let Some(key) = &opts.key_remote {
        let q = shell_quote(key);
        s.push_str(&format!("([ -e {q} ] && echo key=1 || echo key=0); "));
    }
    // Root-filesystem usage in 1K-blocks: "used total" (portable df -k + awk).
    s.push_str("echo \"disk=$(df -k / 2>/dev/null | awk 'NR>1{print $3\" \"$2; exit}')\"; ");
    if let Some(pc) = opts.progress_cmd.as_deref().filter(|s| !s.is_empty()) {
        // Take the last line so a chatty command still yields one tidy value.
        s.push_str(&format!("echo \"progress=$({pc} 2>/dev/null | tail -n1)\"; "));
    }
    s
}

/// Parse the combined probe stdout into GPU stats plus the health/branch/progress
/// fields. The two halves are split on [`SENTINEL`].
fn parse_probe(stdout: &str, m: &mut PodMetrics) {
    let (smi, rest) = stdout.split_once(SENTINEL).unwrap_or((stdout, ""));
    m.gpus = parse_nvidia_smi(smi);
    for line in rest.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "branch" => m.branch = (!v.is_empty()).then(|| v.to_string()),
            "origin" => m.origin = (!v.is_empty()).then(|| v.to_string()),
            "name" => m.has_name = Some(v == "1"),
            "key" => m.has_key = Some(v == "1"),
            "disk" => {
                // "used_kb total_kb" -> MB
                let mut it = v.split_whitespace().filter_map(|x| x.parse::<u64>().ok());
                if let (Some(u), Some(t)) = (it.next(), it.next()) {
                    m.disk_used_mb = Some((u / 1024) as u32);
                    m.disk_total_mb = Some((t / 1024) as u32);
                }
            }
            "progress" => m.progress = (!v.is_empty()).then(|| v.to_string()),
            _ => {}
        }
    }
}

/// Probe a pod over SSH in one round-trip. Never fails the caller — a connection error
/// is recorded in `PodMetrics::error` so one down pod doesn't sink a whole-fleet sweep.
pub async fn fetch(target: &SshTarget, opts: &ProbeOpts) -> PodMetrics {
    let mut m = PodMetrics::default();
    match ssh::run(target, &remote_command(opts)).await {
        Ok(out) if out.success => parse_probe(&out.stdout, &mut m),
        Ok(out) => m.error = Some(describe_failure(out.code, &out.stderr)),
        Err(e) => m.error = Some(e.to_string()),
    }
    m
}

/// Single-quote for safe inclusion in a `sh -c` string (POSIX `'\''` escaping).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Turn a failed remote command into a readable, correctly-attributed error.
///
/// SSH itself exits 255 when it can't connect or authenticate — that's *not* an
/// nvidia-smi failure, so saying "nvidia-smi exit 255" is misleading. We attribute
/// 255 to SSH and, when the stderr shows the identity file couldn't be read or the
/// key was rejected, add the most common cause: the dashboard isn't running as a user
/// that can read the configured SSH key (e.g. a `/root` key while running as `dev`).
fn describe_failure(code: Option<i32>, stderr: &str) -> String {
    let stderr = stderr.trim();
    if code == Some(255) {
        let auth_problem = stderr.contains("not accessible")
            || stderr.contains("Permission denied")
            || stderr.contains("Too many authentication failures");
        let hint = if auth_problem {
            " (ssh key unreadable or rejected — run as a user that can read SHARED_SSH_KEY_PATH, e.g. root)"
        } else {
            ""
        };
        format!("ssh connect failed: {stderr}{hint}")
    } else {
        format!("probe exit {code:?}: {stderr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_255_to_ssh_with_key_hint() {
        let e = describe_failure(
            Some(255),
            "Warning: Identity file /root/.ssh/arena8_key not accessible: Permission denied.",
        );
        assert!(e.starts_with("ssh connect failed:"));
        assert!(e.contains("run as a user that can read"));
    }

    #[test]
    fn non_ssh_exit_is_not_relabeled_as_ssh() {
        let e = describe_failure(Some(1), "boom");
        assert!(e.starts_with("probe exit Some(1)"));
    }

    #[test]
    fn normalizes_gpu_names() {
        assert_eq!(normalize_gpu_name("NVIDIA RTX A4000"), "RTX A4000");
        assert_eq!(normalize_gpu_name("NVIDIA GeForce RTX 4090"), "RTX 4090");
        assert_eq!(normalize_gpu_name("Tesla V100-SXM2-16GB"), "V100-SXM2-16GB");
    }

    #[test]
    fn parses_multi_gpu_csv_with_name() {
        let out = "NVIDIA RTX A4000, 37, 1024, 16376, 52\nNVIDIA RTX A4000, 8, 512, 16376, 49\n";
        let gpus = parse_nvidia_smi(out);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].name.as_deref(), Some("RTX A4000"));
        assert_eq!(gpus[0].util_pct, Some(37));
        assert_eq!(gpus[0].mem_used_mb, Some(1024));
        assert_eq!(gpus[0].temp_c, Some(52));
    }

    #[test]
    fn gpu_summary_counts_and_names() {
        let m = PodMetrics {
            gpus: parse_nvidia_smi("NVIDIA RTX A4000, 0, 0, 16000, 30\n"),
            ..Default::default()
        };
        assert_eq!(m.gpu_summary().as_deref(), Some("RTX A4000"));
        let m2 = PodMetrics {
            gpus: parse_nvidia_smi(
                "NVIDIA RTX A4000, 0, 0, 16000, 30\nNVIDIA RTX A4000, 0, 0, 16000, 30\n",
            ),
            ..Default::default()
        };
        assert_eq!(m2.gpu_summary().as_deref(), Some("2×RTX A4000"));
        assert_eq!(PodMetrics::default().gpu_summary(), None);
    }

    #[test]
    fn aggregates_across_gpus() {
        let m = PodMetrics {
            gpus: parse_nvidia_smi("A, 40, 1000, 16000, 50\nB, 60, 2000, 16000, 70\n"),
            ..Default::default()
        };
        assert_eq!(m.mean_util(), Some(50));
        assert_eq!(m.mem_summary(), Some((3000, 32000)));
        assert_eq!(m.max_temp(), Some(70));
    }

    #[test]
    fn parses_combined_probe_output() {
        let out = format!(
            "NVIDIA RTX A4000, 15, 1000, 16000, 45\n{SENTINEL}\nbranch=autocommit-arena8-w0d1-apple\norigin=git@github.com:styme3279/ARENA_3.0.git\nname=1\nkey=0\ndisk=12582912 104857600\nprogress=epoch 3/10\n"
        );
        let mut m = PodMetrics::default();
        parse_probe(&out, &mut m);
        assert_eq!(m.gpus.len(), 1);
        assert_eq!(m.gpus[0].util_pct, Some(15));
        assert_eq!(m.branch.as_deref(), Some("autocommit-arena8-w0d1-apple"));
        assert_eq!(m.origin.as_deref(), Some("git@github.com:styme3279/ARENA_3.0.git"));
        assert_eq!(m.has_name, Some(true));
        assert_eq!(m.has_key, Some(false));
        // 12582912 KB / 1024 = 12288 MB used; 104857600 KB / 1024 = 102400 MB total.
        assert_eq!(m.disk_summary(), Some((12288, 102400)));
        assert_eq!(m.progress.as_deref(), Some("epoch 3/10"));
    }

    #[test]
    fn empty_branch_origin_become_none() {
        let out = format!("{SENTINEL}\nbranch=\norigin=\nname=0\n");
        let mut m = PodMetrics::default();
        parse_probe(&out, &mut m);
        assert_eq!(m.branch, None);
        assert_eq!(m.origin, None);
        assert_eq!(m.has_name, Some(false));
        assert_eq!(m.has_key, None); // not in output -> not probed
    }

    #[test]
    fn remote_command_includes_requested_parts() {
        let cmd = remote_command(&ProbeOpts {
            progress_cmd: Some("cat /tmp/p".into()),
            repo_path: Some("/root/ARENA_3.0".into()),
            key_remote: Some("/root/.ssh/id_ed25519".into()),
        });
        assert!(cmd.contains("nvidia-smi"));
        assert!(cmd.contains("git rev-parse --abbrev-ref HEAD"));
        assert!(cmd.contains("remote.origin.url"));
        assert!(cmd.contains("'/root/ARENA_3.0'"));
        assert!(cmd.contains("echo name=1"));
        assert!(cmd.contains("'/root/.ssh/id_ed25519'"));
        assert!(cmd.contains("cat /tmp/p"));
    }
}
