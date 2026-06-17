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

/// Separates the trailing `nvidia-smi` compute-process block from the rest of the probe
/// output. Process names can contain `=`/`,`, so they get their own CSV section rather
/// than going through the key=value parser.
pub const PROC_SENTINEL: &str = "@@ARENA_PROC@@";

/// One GPU's stats. Fields are optional so a partial/odd row degrades gracefully.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuStat {
    pub name: Option<String>,
    pub util_pct: Option<u32>,
    pub mem_used_mb: Option<u32>,
    pub mem_total_mb: Option<u32>,
    pub temp_c: Option<u32>,
}

/// One GPU compute process from `nvidia-smi --query-compute-apps`: pid, VRAM it holds,
/// and the (possibly path-y) process name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuProc {
    pub pid: u32,
    pub mem_mb: Option<u32>,
    pub name: String,
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
    /// Whether any LLM API key (OpenRouter / Anthropic / OpenAI) is exported in the pod's
    /// shell rc files — i.e. `copy-keys` has run. None = not probed / unreachable.
    pub has_api_key: Option<bool>,
    /// Whether a Hugging Face token (`HF_TOKEN`) is exported in the pod's shell rc files.
    pub has_hf_token: Option<bool>,
    /// Whether a Claude Code token (`CLAUDE_CODE_OAUTH_TOKEN`) is exported in the rc files.
    pub has_cc_token: Option<bool>,
    /// Root-filesystem usage (used, total) in MB, if reported.
    pub disk_used_mb: Option<u32>,
    pub disk_total_mb: Option<u32>,
    /// Host CPU utilization % over a short window (`/proc/stat`), if reported.
    pub cpu_pct: Option<u32>,
    /// Host RAM (used, total) in MB (`/proc/meminfo`), if reported.
    pub host_mem_used_mb: Option<u32>,
    pub host_mem_total_mb: Option<u32>,
    /// Unix timestamp (committer date) of the ARENA checkout's last commit, if reported.
    /// In this workflow a commit *is* a backup (`pods backup` commits + pushes), so this
    /// is "when the pod was last backed up".
    pub last_commit: Option<i64>,
    /// Number of uncommitted entries in the working tree (`git status --porcelain`), if
    /// reported — i.e. work done since the last backup. `Some(0)` means a clean tree.
    pub dirty_files: Option<u32>,
    /// Commits the local branch is **ahead** of its upstream (`origin/<branch>`) — i.e.
    /// committed but not pushed (a blocked/failed push, e.g. GitHub secret-scanning,
    /// shows up here). `None` = no upstream / not probed / unreachable.
    pub ahead: Option<u32>,
    /// Commits the local branch is **behind** its upstream (origin has newer commits).
    pub behind: Option<u32>,
    /// The last few ARENA_3.0 commits, newest first, each preformatted as
    /// "<short-hash>  <relative-date>  <subject>" for a one-line-per-commit display.
    pub recent_commits: Vec<String>,
    /// GPU compute processes (`nvidia-smi --query-compute-apps`), so the detail pane can
    /// show what's actually holding the GPUs.
    pub gpu_procs: Vec<GpuProc>,
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

    /// Host RAM (used, total) in MB, if both reported.
    pub fn host_mem_summary(&self) -> Option<(u32, u32)> {
        match (self.host_mem_used_mb, self.host_mem_total_mb) {
            (Some(u), Some(t)) => Some((u, t)),
            _ => None,
        }
    }

    /// Per-GPU VRAM in GB (from the first GPU that reports a total), rounded — for the
    /// dashboard's GPU column, e.g. "16G" / "80G".
    pub fn vram_gb(&self) -> Option<u32> {
        self.gpus
            .iter()
            .find_map(|g| g.mem_total_mb)
            .map(|mb| (mb as f64 / 1024.0).round() as u32)
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
        // Always prefix the count ("1×…"/"2×…") so the column reads consistently.
        Some(format!("{n}×{name}"))
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
        // Last commit time (= last backup, since `pods backup` commits + pushes) and how
        // many entries are uncommitted (work since that backup; 0 = clean tree).
        s.push_str(&format!(
            "echo \"committed=$(cd {q} 2>/dev/null && git log -1 --format=%ct 2>/dev/null)\"; "
        ));
        s.push_str(&format!(
            "echo \"dirty=$(cd {q} 2>/dev/null && git status --porcelain 2>/dev/null | wc -l)\"; "
        ));
        // Divergence vs the upstream: "<behind>\t<ahead>" (empty when there's no
        // upstream). ahead>0 means committed-but-unpushed — e.g. a push GitHub blocked.
        s.push_str(&format!(
            "echo \"sync=$(cd {q} 2>/dev/null && git rev-list --left-right --count '@{{u}}...HEAD' 2>/dev/null)\"; "
        ));
        // The last few commits, newest first — a compact backup/work history. One
        // `clog=` line per commit; the subject is free text (may contain '=') but it's
        // the whole value after the first '=', so it survives the key=value parse.
        s.push_str(&format!(
            "(cd {q} 2>/dev/null && git log -n 4 --format='clog=%h  %cr  %s' 2>/dev/null); "
        ));
    }
    s.push_str("([ -e \"$HOME/.name\" ] && echo name=1 || echo name=0); ");
    // Any LLM API key exported in the shell rc files (i.e. `copy-keys` has run)?
    s.push_str(
        "(grep -qsE '^[[:space:]]*export (OPENROUTER_API_KEY|ANTHROPIC_API_KEY|OPENAI_API_KEY)=' \
         \"$HOME/.bashrc\" \"$HOME/.zshrc\" && echo apikey=1 || echo apikey=0); ",
    );
    // Broadcast tokens present in the shell rc files: Hugging Face and Claude Code.
    s.push_str(
        "(grep -qsE '^[[:space:]]*export (HF_TOKEN|HUGGING_FACE_HUB_TOKEN)=' \
         \"$HOME/.bashrc\" \"$HOME/.zshrc\" && echo hf=1 || echo hf=0); ",
    );
    s.push_str(
        "(grep -qsE '^[[:space:]]*export CLAUDE_CODE_OAUTH_TOKEN=' \
         \"$HOME/.bashrc\" \"$HOME/.zshrc\" && echo cc=1 || echo cc=0); ",
    );
    if let Some(key) = &opts.key_remote {
        let q = shell_quote(key);
        s.push_str(&format!("([ -e {q} ] && echo key=1 || echo key=0); "));
    }
    // Root-filesystem usage in 1K-blocks: "used total" (portable df -k + awk).
    s.push_str("echo \"disk=$(df -k / 2>/dev/null | awk 'NR>1{print $3\" \"$2; exit}')\"; ");
    // Host CPU% over a short /proc/stat window, and host RAM (used/total KB) from
    // /proc/meminfo. Cheap, and independent of the GPU.
    s.push_str(
        r#"echo "cpu=$({ grep '^cpu ' /proc/stat; sleep 0.25; grep '^cpu ' /proc/stat; } 2>/dev/null | awk 'NR==1{i1=$5;t1=0;for(i=2;i<=8;i++)t1+=$i}NR==2{i2=$5;t2=0;for(i=2;i<=8;i++)t2+=$i;dt=t2-t1;di=i2-i1;if(dt>0)printf "%.0f",(1-di/dt)*100}')"; "#,
    );
    // Memory used/total (KB). Prefer the *container's* cgroup limit (v2 memory.max /
    // v1 memory.limit_in_bytes) — `/proc/meminfo` isn't namespaced, so it reports the
    // whole host (e.g. 503G) rather than the pod's ~46G limit. Fall back to host meminfo
    // when there's no cgroup limit ("max", or a v1 sentinel ≥ host RAM).
    s.push_str(
        r#"echo "hostmem=$(mt=$(awk '/^MemTotal:/{print $2}' /proc/meminfo 2>/dev/null); ma=$(awk '/^MemAvailable:/{print $2}' /proc/meminfo 2>/dev/null); hu=$(( ${mt:-0} - ${ma:-0} )); ht=${mt:-0}; if [ -s /sys/fs/cgroup/memory.max ]; then lm=$(cat /sys/fs/cgroup/memory.max); cu=$(cat /sys/fs/cgroup/memory.current 2>/dev/null); if [ x$lm != xmax ]; then hu=$(( ${cu:-0} / 1024 )); ht=$(( lm / 1024 )); fi; elif [ -r /sys/fs/cgroup/memory/memory.limit_in_bytes ]; then lm=$(cat /sys/fs/cgroup/memory/memory.limit_in_bytes); cu=$(cat /sys/fs/cgroup/memory/memory.usage_in_bytes 2>/dev/null); if [ ${lm:-0} -lt $(( ${mt:-0} * 1024 )) ]; then hu=$(( ${cu:-0} / 1024 )); ht=$(( lm / 1024 )); fi; fi; echo $hu $ht)"; "#,
    );
    if let Some(pc) = opts.progress_cmd.as_deref().filter(|s| !s.is_empty()) {
        // Take the last line so a chatty command still yields one tidy value.
        s.push_str(&format!("echo \"progress=$({pc} 2>/dev/null | tail -n1)\"; "));
    }
    // GPU compute processes, last and in their own sentinel-delimited CSV block (process
    // names can contain '='/','). `used_memory` before `process_name` so the name is the
    // free-text tail. No-GPU / no-process pods just emit nothing here.
    s.push_str(&format!(
        "echo '{PROC_SENTINEL}'; nvidia-smi --query-compute-apps=pid,used_memory,process_name --format=csv,noheader,nounits 2>/dev/null; "
    ));
    // The probe is a `;`-chain of independent reads, so its overall exit code is just the
    // LAST command's. That last command is nvidia-smi, which doesn't exist on a CPU/Hetzner
    // pod → exit 127 → `fetch` would mark the whole probe as failed and blank EVERY column
    // (branch, disk, cpu, git) while the GPU cell shows "err". End on a guaranteed success
    // so any reachable pod parses (missing fields stay None); a genuinely unreachable pod
    // still fails earlier at the SSH layer (exit 255), which is the error we actually want.
    s.push_str("true");
    s
}

/// Parse the trailing `nvidia-smi --query-compute-apps` CSV (pid, used_memory MiB,
/// process_name) into one [`GpuProc`] per running process. Non-numeric lines (e.g.
/// "No running processes found") are skipped via the pid parse.
pub fn parse_compute_apps(stdout: &str) -> Vec<GpuProc> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            // splitn(3): name is the remainder, so a path with commas stays intact.
            let mut it = line.splitn(3, ',').map(str::trim);
            let pid = it.next()?.parse::<u32>().ok()?;
            let mem_mb = it.next().and_then(|s| s.parse::<u32>().ok());
            let name = it.next().unwrap_or("").to_string();
            Some(GpuProc { pid, mem_mb, name })
        })
        .collect()
}

/// Parse the combined probe stdout into GPU stats plus the health/branch/progress
/// fields. The two halves are split on [`SENTINEL`].
fn parse_probe(stdout: &str, m: &mut PodMetrics) {
    let (smi, rest) = stdout.split_once(SENTINEL).unwrap_or((stdout, ""));
    m.gpus = parse_nvidia_smi(smi);
    // The compute-process CSV is fenced off at the end so its free-text names don't hit
    // the key=value parser below.
    let (kv, procs) = rest.split_once(PROC_SENTINEL).unwrap_or((rest, ""));
    m.gpu_procs = parse_compute_apps(procs);
    for line in kv.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "clog" if !v.is_empty() => m.recent_commits.push(v.to_string()),
            "branch" => m.branch = (!v.is_empty()).then(|| v.to_string()),
            "origin" => m.origin = (!v.is_empty()).then(|| v.to_string()),
            "name" => m.has_name = Some(v == "1"),
            "key" => m.has_key = Some(v == "1"),
            "apikey" => m.has_api_key = Some(v == "1"),
            "hf" => m.has_hf_token = Some(v == "1"),
            "cc" => m.has_cc_token = Some(v == "1"),
            "disk" => {
                // "used_kb total_kb" -> MB
                let mut it = v.split_whitespace().filter_map(|x| x.parse::<u64>().ok());
                if let (Some(u), Some(t)) = (it.next(), it.next()) {
                    m.disk_used_mb = Some((u / 1024) as u32);
                    m.disk_total_mb = Some((t / 1024) as u32);
                }
            }
            "cpu" => m.cpu_pct = v.parse::<u32>().ok(),
            "hostmem" => {
                let mut it = v.split_whitespace().filter_map(|x| x.parse::<u64>().ok());
                if let (Some(u), Some(t)) = (it.next(), it.next()) {
                    m.host_mem_used_mb = Some((u / 1024) as u32);
                    m.host_mem_total_mb = Some((t / 1024) as u32);
                }
            }
            "committed" => m.last_commit = v.parse::<i64>().ok(),
            "dirty" => m.dirty_files = v.parse::<u32>().ok(),
            "sync" => {
                // "<behind> <ahead>" (tab/space separated); empty => no upstream.
                let mut it = v.split_whitespace().filter_map(|x| x.parse::<u32>().ok());
                if let (Some(b), Some(a)) = (it.next(), it.next()) {
                    m.behind = Some(b);
                    m.ahead = Some(a);
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
        assert_eq!(m.gpu_summary().as_deref(), Some("1×RTX A4000"));
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
    fn parses_compute_apps_and_skips_noise() {
        let out = "1234, 512, python\n5678, 2048, /usr/bin/python3\nNo running processes found\n\n";
        let procs = parse_compute_apps(out);
        assert_eq!(procs.len(), 2);
        assert_eq!(procs[0], GpuProc { pid: 1234, mem_mb: Some(512), name: "python".into() });
        assert_eq!(procs[1].pid, 5678);
        assert_eq!(procs[1].name, "/usr/bin/python3");
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
            "NVIDIA RTX A4000, 15, 1000, 16000, 45\n{SENTINEL}\nbranch=autocommit-arena8-w0d1-apple\norigin=git@github.com:styme3279/ARENA_3.0.git\ncommitted=1717412400\ndirty=3\nsync=0\t2\nclog=a1b2c3d  2 hours ago  arena backup arena8-apple\nclog=f00ba12  3 hours ago  fix: handle k=v in subject\nname=1\napikey=1\nhf=1\ncc=0\nkey=0\ndisk=12582912 104857600\ncpu=37\nhostmem=8388608 16777216\nprogress=epoch 3/10\n{PROC_SENTINEL}\n12345, 512, /opt/conda/envs/arena-env/bin/python\n67890, 1024, python\nNo running processes found\n"
        );
        let mut m = PodMetrics::default();
        parse_probe(&out, &mut m);
        // Recent commits collect in order; a subject containing '=' survives intact.
        assert_eq!(
            m.recent_commits,
            vec![
                "a1b2c3d  2 hours ago  arena backup arena8-apple".to_string(),
                "f00ba12  3 hours ago  fix: handle k=v in subject".to_string(),
            ]
        );
        // GPU processes parse from the fenced CSV; the path-y name (last field) is intact.
        assert_eq!(m.gpu_procs.len(), 2);
        assert_eq!(m.gpu_procs[0].pid, 12345);
        assert_eq!(m.gpu_procs[0].mem_mb, Some(512));
        assert_eq!(m.gpu_procs[0].name, "/opt/conda/envs/arena-env/bin/python");
        assert_eq!(m.cpu_pct, Some(37));
        assert_eq!(m.host_mem_summary(), Some((8192, 16384))); // 8/16 GiB
        assert_eq!(m.behind, Some(0));
        assert_eq!(m.ahead, Some(2)); // 2 commits unpushed
        assert_eq!(m.gpus.len(), 1);
        assert_eq!(m.gpus[0].util_pct, Some(15));
        assert_eq!(m.branch.as_deref(), Some("autocommit-arena8-w0d1-apple"));
        assert_eq!(m.origin.as_deref(), Some("git@github.com:styme3279/ARENA_3.0.git"));
        assert_eq!(m.last_commit, Some(1717412400));
        assert_eq!(m.dirty_files, Some(3));
        assert_eq!(m.has_name, Some(true));
        assert_eq!(m.has_api_key, Some(true));
        assert_eq!(m.has_hf_token, Some(true));
        assert_eq!(m.has_cc_token, Some(false));
        assert_eq!(m.has_key, Some(false));
        // 12582912 KB / 1024 = 12288 MB used; 104857600 KB / 1024 = 102400 MB total.
        assert_eq!(m.disk_summary(), Some((12288, 102400)));
        assert_eq!(m.progress.as_deref(), Some("epoch 3/10"));
    }

    #[test]
    fn no_upstream_leaves_ahead_behind_none() {
        // `sync=` with an empty value (no upstream) must not parse to (0,0).
        let out = format!("{SENTINEL}\nbranch=feature\nsync=\nname=1\n");
        let mut m = PodMetrics::default();
        parse_probe(&out, &mut m);
        assert_eq!(m.ahead, None);
        assert_eq!(m.behind, None);
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
        assert!(cmd.contains("git log -1 --format=%ct"));
        assert!(cmd.contains("git log -n 4 --format='clog=%h"));
        assert!(cmd.contains("--query-compute-apps=pid,used_memory,process_name"));
        assert!(cmd.contains("git status --porcelain"));
        assert!(cmd.contains("echo hf=1"));
        assert!(cmd.contains("echo cc=1"));
        assert!(cmd.contains("/proc/stat"));
        assert!(cmd.contains("/proc/meminfo"));
        assert!(cmd.contains("'/root/ARENA_3.0'"));
        assert!(cmd.contains("echo name=1"));
        assert!(cmd.contains("OPENROUTER_API_KEY|ANTHROPIC_API_KEY|OPENAI_API_KEY"));
        assert!(cmd.contains("'/root/.ssh/id_ed25519'"));
        assert!(cmd.contains("cat /tmp/p"));
        // Must end on a guaranteed-success command so a CPU/Hetzner pod (no nvidia-smi,
        // which would otherwise exit 127 as the chain's last command) isn't reported as a
        // wholly-failed probe — see remote_command's trailing `true`.
        assert!(cmd.trim_end().ends_with("true"));
    }
}
