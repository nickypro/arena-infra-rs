//! Per-pod runtime metrics for the dashboard: GPU utilization (via `nvidia-smi` over
//! SSH) and an optional, operator-defined progress signal.
//!
//! "Notebook progress" has no universal source, so it's a **configurable remote
//! command** (`PROGRESS_CMD`): whatever short string it prints on stdout (e.g. a
//! percentage, an epoch count, the last line of a log) is shown verbatim. If unset,
//! the progress column is simply blank — we don't guess.
//!
//! All of this is read-only: it only ever *reads* `nvidia-smi` and runs the operator's
//! progress command. Parsing is pure and unit-tested; the SSH calls are thin.

use crate::error::Result;
use crate::ssh::{self, SshTarget};

/// `nvidia-smi` invocation that emits one CSV row per GPU, no header/units, in the
/// field order [`parse_nvidia_smi`] expects.
pub const NVIDIA_SMI_QUERY: &str = "nvidia-smi \
     --query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu \
     --format=csv,noheader,nounits";

/// One GPU's stats. Fields are optional so a partial/odd row degrades gracefully.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuStat {
    pub util_pct: Option<u32>,
    pub mem_used_mb: Option<u32>,
    pub mem_total_mb: Option<u32>,
    pub temp_c: Option<u32>,
}

/// Parse `nvidia-smi --format=csv,noheader,nounits` output into one stat per GPU.
pub fn parse_nvidia_smi(stdout: &str) -> Vec<GpuStat> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            let at = |i: usize| f.get(i).and_then(|s| s.parse::<u32>().ok());
            GpuStat {
                util_pct: at(0),
                mem_used_mb: at(1),
                mem_total_mb: at(2),
                temp_c: at(3),
            }
        })
        .collect()
}

/// Aggregated metrics for one pod, as shown in a dashboard row.
#[derive(Debug, Clone, Default)]
pub struct PodMetrics {
    pub gpus: Vec<GpuStat>,
    pub progress: Option<String>,
    /// Set when the GPU fetch failed (pod down, no nvidia-smi, etc.).
    pub error: Option<String>,
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

    /// Summed memory (used, total) in MB across GPUs, if reported.
    pub fn mem_summary(&self) -> Option<(u32, u32)> {
        let used: u32 = self.gpus.iter().filter_map(|g| g.mem_used_mb).sum();
        let total: u32 = self.gpus.iter().filter_map(|g| g.mem_total_mb).sum();
        if total == 0 {
            None
        } else {
            Some((used, total))
        }
    }

    /// Max temperature across GPUs, if reported.
    pub fn max_temp(&self) -> Option<u32> {
        self.gpus.iter().filter_map(|g| g.temp_c).max()
    }
}

/// Fetch a pod's metrics over SSH: `nvidia-smi`, then the optional progress command.
/// Never fails the caller — a fetch error is recorded in `PodMetrics::error` so one
/// down pod doesn't sink a whole-fleet sweep.
pub async fn fetch(target: &SshTarget, progress_cmd: Option<&str>) -> PodMetrics {
    let mut m = PodMetrics::default();
    match ssh::run(target, NVIDIA_SMI_QUERY).await {
        Ok(out) if out.success => m.gpus = parse_nvidia_smi(&out.stdout),
        Ok(out) => {
            m.error = Some(format!("nvidia-smi exit {:?}: {}", out.code, out.stderr.trim()))
        }
        Err(e) => m.error = Some(e.to_string()),
    }
    if let Some(pc) = progress_cmd.filter(|s| !s.is_empty()) {
        if let Ok(out) = ssh::run(target, pc).await {
            if out.success {
                let s = out.stdout.trim();
                if !s.is_empty() {
                    // Take the last non-empty line, so a noisy command still yields a
                    // single tidy value.
                    m.progress = Some(s.lines().last().unwrap_or(s).trim().to_string());
                }
            }
        }
    }
    m
}

/// The `Result` re-export keeps the public signature consistent with the rest of the
/// crate even though `fetch` itself is infallible by design.
pub type FetchResult = Result<PodMetrics>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_gpu_csv() {
        let out = "37, 1024, 16376, 52\n8, 512, 16376, 49\n";
        let gpus = parse_nvidia_smi(out);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].util_pct, Some(37));
        assert_eq!(gpus[0].mem_used_mb, Some(1024));
        assert_eq!(gpus[0].mem_total_mb, Some(16376));
        assert_eq!(gpus[0].temp_c, Some(52));
        assert_eq!(gpus[1].util_pct, Some(8));
    }

    #[test]
    fn ignores_blank_lines_and_bad_fields() {
        let out = "\n50, x, 2048, \n";
        let gpus = parse_nvidia_smi(out);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].util_pct, Some(50));
        assert_eq!(gpus[0].mem_used_mb, None); // "x" doesn't parse
        assert_eq!(gpus[0].mem_total_mb, Some(2048));
        assert_eq!(gpus[0].temp_c, None); // missing
    }

    #[test]
    fn aggregates_across_gpus() {
        let m = PodMetrics {
            gpus: parse_nvidia_smi("40, 1000, 16000, 50\n60, 2000, 16000, 70\n"),
            ..Default::default()
        };
        assert_eq!(m.mean_util(), Some(50));
        assert_eq!(m.mem_summary(), Some((3000, 32000)));
        assert_eq!(m.max_temp(), Some(70));
    }

    #[test]
    fn empty_metrics_aggregate_to_none() {
        let m = PodMetrics::default();
        assert_eq!(m.mean_util(), None);
        assert_eq!(m.mem_summary(), None);
        assert_eq!(m.max_temp(), None);
    }
}
