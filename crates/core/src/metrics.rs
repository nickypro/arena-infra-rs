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
//! file existence checks, and runs the operator's progress command. Its git runs through
//! [`RO_GIT_SH`]: nothing the participant's repository configures (an fsmonitor hook, a clean
//! filter, a `gpg.program` for signed commits) is run, and nothing is written. Parsing is pure and
//! unit-tested; the SSH call is thin, goes through a [`Remote`] (so it is testable with
//! `FakeRemote`) and is bounded by [`PROBE_TIMEOUT`], so a wedged pod shows as an error
//! row instead of stalling a `pods list` or a dashboard refresh.

use crate::remote::{Remote, PROBE_TIMEOUT};
use crate::ssh::SshTarget;

/// Separates the `nvidia-smi` block from the key=value health block in the probe's
/// combined stdout.
pub const SENTINEL: &str = "@@ARENA_PROBE@@";

/// Separates the trailing `nvidia-smi` compute-process block from the rest of the probe
/// output. Process names can contain `=`/`,`, so they get their own CSV section rather
/// than going through the key=value parser.
pub const PROC_SENTINEL: &str = "@@ARENA_PROC@@";

/// POSIX sh functions for git that runs nothing the repository configures and writes nothing —
/// the dashboard's probe and `pods ask`'s (`ask/probe.sh`, `busy_op`'s `git ls-files -u`
/// included) read the participants' repos with them, as root, every few seconds:
/// - `ro_git ARGS…`: `git --no-optional-locks` (no index refresh written, no `index.lock`), with
///   `core.fsmonitor=` (no fsmonitor hook or daemon: an *empty* value is off in every git —
///   `false` would be run as a hook path by git < 2.36), `core.untrackedCache=false`,
///   `log.showSignature=false` (a signed commit would run `gpg.program`), and four `-c` words
///   per driver `ro_setup` found. `-c` reaches git's own children too (`stash list`'s `git log`).
///   With `$ro_timeout` set, under `timeout` where the pod has it — and a KILL 2 s after its TERM
///   (`-k 2`; review ai-review codex #4: a git that ignores TERM, or waits on a hung volume, would
///   otherwise hold `timeout` forever). The words are built with
///   `set --`, never as an unquoted `$var`: the dashboard's probe runs in the pod user's login
///   shell — zsh on the pods — which doesn't split one, so git got all of them as one argument,
///   refused it, and every repo with a filter driver (git-lfs puts `filter.lfs.*` in the global
///   config) read `dirty` unknown (review ai-review claude #8). sh, bash and zsh read it alike.
///   The driver list is read to its end whatever it holds: its last name's space put back when it
///   isn't there, a pass that doesn't shrink it ends the loop, an empty name skipped (review
///   ai-review claude #8: `lfs` without its space never shrank, the loop ran forever and `"$@"`
///   grew without bound, as root on a pod). `ro_drivers` and `ro_timeout` are set (empty) where the
///   functions are defined: one inherited from the login shell's environment is never used.
/// - `ro_setup` (in the repo): `$ro_drivers` (each name then a space) is every filter driver the
///   config defines (read through `ro_git`, so within `$ro_timeout` too), for `ro_git` to empty — no clean/smudge/process, not required: git hashes a
///   racily-clean file's content even in plumbing, and that would run its clean filter. A driver
///   a `.gitattributes` names but no config defines runs nothing anyway. Fails (nothing to
///   compare with) when the config can't be read or a driver's name has anything but
///   `[A-Za-z0-9._-]` in it (a space would end it in that list).
/// - `ro_changes`: `git status --porcelain`'s `XY path` lines, from plumbing that never reads a
///   file's contents unless racily clean (then without filters): `diff-index --cached` (X),
///   `diff-files` without refreshing (Y; submodules ignored — their `git status` would run their
///   own helpers) and `ls-files --others` (`?? path`, an untracked directory as `dir/`). It may
///   overcount: a file only touched (its stat changed, its content not) reads as modified until
///   git refreshes the index — fine for a count. Fails when any of them does.
pub const RO_GIT_SH: &str = r#"ro_drivers=
ro_timeout=
ro_git() {
    ro_n=$#
    ro_r=${ro_drivers:-}
    [ -z "$ro_r" ] || ro_r="${ro_r% } "
    while [ -n "$ro_r" ]; do
        ro_d=${ro_r%% *}
        ro_s=${ro_r#* }
        [ "$ro_s" != "$ro_r" ] || break
        ro_r=$ro_s
        [ -n "$ro_d" ] || continue
        set -- "$@" -c "filter.$ro_d.clean=" -c "filter.$ro_d.smudge=" -c "filter.$ro_d.process=" -c "filter.$ro_d.required=false"
    done
    while [ "$ro_n" -gt 0 ]; do
        set -- "$@" "$1"
        shift
        ro_n=$((ro_n - 1))
    done
    if [ -n "${ro_timeout:-}" ] && command -v timeout >/dev/null 2>&1; then
        set -- timeout -k 2 "$ro_timeout" git --no-optional-locks -c core.fsmonitor= -c core.untrackedCache=false -c log.showSignature=false "$@"
    else
        set -- command git --no-optional-locks -c core.fsmonitor= -c core.untrackedCache=false -c log.showSignature=false "$@"
    fi
    "$@"
}
ro_setup() {
    ro_drivers=
    ro_k=$(ro_git config --name-only --get-regexp '^filter[.]' 2>/dev/null)
    case $? in 0 | 1) ;; *) return 1 ;; esac
    if printf '%s\n' "$ro_k" | LC_ALL=C grep -q '[^A-Za-z0-9._-]'; then return 1; fi
    ro_drivers=$(printf '%s\n' "$ro_k" | sed -e '/^$/d' -e 's/^filter[.]//' -e 's/[.][^.]*$//' | sort -u | tr '\n' ' ')
}
ro_changes() {
    ro_base=HEAD
    ro_git rev-parse -q --verify HEAD >/dev/null 2>&1 || ro_base=$(ro_git hash-object -t tree --stdin </dev/null) || return 1
    ro_i=$(ro_git diff-index --cached --name-status --no-renames "$ro_base" --) || return 1
    ro_w=$(ro_git diff-files --name-status --no-renames --ignore-submodules=all) || return 1
    ro_o=$(ro_git ls-files --others --exclude-standard --directory --no-empty-directory) || return 1
    { printf '%s\n' "$ro_i" | sed -e '/^$/d' -e 's/^/I /'; printf '%s\n' "$ro_w" | sed -e '/^$/d' -e 's/^/W /'; } |
        awk '{ k = substr($0, 1, 1); r = substr($0, 3); t = index(r, "\t"); if (t < 2) next
               p = substr(r, t + 1); if (!(p in seen)) { seen[p] = 1; order[++n] = p }
               if (k == "I") { if (!(p in x)) x[p] = substr(r, 1, 1) } else if (!(p in y)) y[p] = substr(r, 1, 1) }
             END { for (i = 1; i <= n; i++) { p = order[i]; printf "%s%s %s\n", ((p in x) ? x[p] : " "), ((p in y) ? y[p] : " "), p } }'
    printf '%s\n' "$ro_o" | sed -e '/^$/d' -e 's/^/?? /'
}
"#;

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
    /// Bound each `nvidia-smi` to this many seconds where the pod has `timeout` (`pods ask`'s
    /// probe: a wedged GPU must cost only the GPU's facts, not the whole call's budget); `None`:
    /// unbounded but for the probe's own budget (the dashboard's).
    pub nvidia_smi_timeout: Option<u64>,
    /// Bound the repo's part — its `cd` into the checkout and every git of it, as one `sh` — to
    /// this many seconds where the pod has `timeout` (`pods ask`'s probe; review ai-review codex
    /// #4: a hung `/workspace` volume took the whole call's budget, and every section after it);
    /// `None`: unbounded but for the probe's own budget (the dashboard's).
    pub repo_timeout: Option<u64>,
}

/// `nvidia-smi ARGS`, within `secs` (KILLed 2 s after its TERM) where the pod has `timeout` — one
/// simple command, which any login shell reads alike — else as it is.
fn nvidia_smi(args: &str, secs: Option<u64>) -> String {
    match secs {
        None => format!("nvidia-smi {args}"),
        Some(s) => format!(
            "sh -c 'if command -v timeout >/dev/null 2>&1; then exec timeout -k 2 {s} nvidia-smi \"$@\"; else exec nvidia-smi \"$@\"; fi' nvidia-smi {args}"
        ),
    }
}

/// `script` run as one `sh -c` within `secs` where the pod has `timeout`, else as it is — a part of
/// the probe that can hang as a whole (a hung volume under every step of it). What it printed
/// before is kept. Its whole process group is KILLed 2 s after the TERM, what it started too: the
/// `sh` traps TERM (a handler, so what it runs still gets TERM's default), because `timeout` sends
/// its KILL only while the process it runs is alive — a `sh` that died on the TERM let a step that
/// ignores TERM (a git waiting on a hung volume) outlive the bound.
fn bounded_sh(script: &str, secs: u64) -> String {
    format!(
        "sh -c 'if command -v timeout >/dev/null 2>&1; then exec timeout -k 2 {secs} sh -c \"$0\"; else exec sh -c \"$0\"; fi' {}; ",
        shell_quote(&format!("trap : TERM\n{script}"))
    )
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
    /// Number of uncommitted entries in the working tree (`git status --porcelain`'s, from
    /// `ro_changes`), if reported — i.e. work done since the last backup. For the dashboard's
    /// eyes only: a file only touched counts until git refreshes the index, and a submodule's
    /// own edits never count (`--ignore-submodules=all`), so `Some(0)` is "nothing the
    /// checkout's git sees", never "nothing to lose" — no reset, replace or terminate goes by it
    /// (each reads the pod itself first).
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
    let query = NVIDIA_SMI_QUERY.strip_prefix("nvidia-smi ").unwrap_or(NVIDIA_SMI_QUERY);
    let mut s = format!("{} 2>/dev/null; echo '{SENTINEL}'; ", nvidia_smi(query, opts.nvidia_smi_timeout));
    if let Some(repo) = &opts.repo_path {
        let q = shell_quote(repo);
        let mut part = String::new();
        // Every git here is `ro_git` ([`RO_GIT_SH`]): nothing the participant's repo configures
        // runs, nothing is written (a plain `git status` would refresh and write the index
        // under `index.lock`, so a refresh could fail the participant's own `git commit`).
        part.push_str(RO_GIT_SH);
        part.push_str(&format!(
            "echo \"branch=$(cd {q} 2>/dev/null && ro_git rev-parse --abbrev-ref HEAD 2>/dev/null)\"; "
        ));
        // Its first line only: git reads `\n` in a quoted config value as a newline, and a value
        // run over lines could plant `key=value` lines of its own (review ai-review claude #2).
        part.push_str(&format!(
            "echo \"origin=$(cd {q} 2>/dev/null && ro_git config --get remote.origin.url 2>/dev/null | head -n 1)\"; "
        ));
        // Last commit time (= last backup, since `pods backup` commits + pushes) and how
        // many entries are uncommitted (work since that backup; 0 = clean tree — a file only
        // touched counts until git refreshes the index, a submodule's edits never: see
        // `ro_changes` and `PodMetrics::dirty_files`).
        part.push_str(&format!(
            "echo \"committed=$(cd {q} 2>/dev/null && ro_git log -1 --format=%ct 2>/dev/null)\"; "
        ));
        part.push_str(&format!(
            "echo \"dirty=$(cd {q} 2>/dev/null && ro_setup && ro_c=$(ro_changes 2>/dev/null) && printf '%s\\n' \"$ro_c\" | grep -c .)\"; "
        ));
        // Divergence vs the upstream: "<behind>\t<ahead>" (empty when there's no
        // upstream). ahead>0 means committed-but-unpushed — e.g. a push GitHub blocked.
        part.push_str(&format!(
            "echo \"sync=$(cd {q} 2>/dev/null && ro_git rev-list --left-right --count '@{{u}}...HEAD' 2>/dev/null)\"; "
        ));
        // The last few commits, newest first — a compact backup/work history. One
        // `clog=` line per commit; the subject is free text (may contain '=') but it's
        // the whole value after the first '=', so it survives the key=value parse.
        part.push_str(&format!(
            "(cd {q} 2>/dev/null && ro_git log -n 4 --format='clog=%h  %cr  %s' 2>/dev/null); "
        ));
        match opts.repo_timeout {
            Some(secs) => s.push_str(&bounded_sh(&part, secs)),
            None => s.push_str(&part),
        }
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
        "echo '{PROC_SENTINEL}'; {} 2>/dev/null; ",
        nvidia_smi("--query-compute-apps=pid,used_memory,process_name --format=csv,noheader,nounits", opts.nvidia_smi_timeout)
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

/// The probe's remote command for `opts` — what [`fetch_with`] runs; `pods ask` runs it inside
/// its own one-call bundle (`crate::ask::bundle`).
pub fn probe_command(opts: &ProbeOpts) -> String {
    remote_command(opts)
}

/// What [`probe_command`] printed, read into a [`PodMetrics`]: anything missing or odd stays
/// `None` (never an error — a partial reply still says what it can).
pub fn parse_probe_output(stdout: &str) -> PodMetrics {
    let mut m = PodMetrics::default();
    parse_probe(stdout, &mut m);
    m
}

/// Probe a pod over `remote` in one round-trip, within [`PROBE_TIMEOUT`]. Never fails the
/// caller — a connection error or a timeout is recorded in `PodMetrics::error` so one down
/// (or wedged) pod doesn't sink a whole-fleet sweep.
pub async fn fetch_with(remote: &dyn Remote, target: &SshTarget, opts: &ProbeOpts) -> PodMetrics {
    let mut m = PodMetrics::default();
    match remote.exec(target, &remote_command(opts), Some(PROBE_TIMEOUT)).await {
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
        format!("probe {}: {stderr}", crate::ssh::exit_label(code))
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
        assert_eq!(e, "probe exit 1: boom");
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

    fn target() -> SshTarget {
        SshTarget {
            user: "root".into(),
            host: "10.0.0.1".into(),
            port: 22001,
            key_paths: vec![],
            connect_timeout_secs: 5,
            host_keys: Default::default(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_with_parses_a_fake_nvidia_smi_reply() {
        use crate::remote::{FakeRemote, FakeReply, RemoteCall};
        let fake = FakeRemote::new();
        let reply = format!(
            "NVIDIA RTX A4000, 12, 900, 16376, 41\nNVIDIA RTX A4000, 0, 3, 16376, 38\n{SENTINEL}\nbranch=main\nname=1\n{PROC_SENTINEL}\n"
        );
        fake.script("10.0.0.1:22001", [FakeReply::stdout(&reply)]);
        let opts = ProbeOpts { repo_path: Some("/root/ARENA_materials".into()), ..Default::default() };
        let m = fetch_with(&fake, &target(), &opts).await;
        assert_eq!(m.error, None);
        assert_eq!(m.gpu_summary().as_deref(), Some("2×RTX A4000"));
        assert_eq!(m.gpus[0].util_pct, Some(12));
        assert_eq!(m.branch.as_deref(), Some("main"));
        assert_eq!(m.has_name, Some(true));
        // One round-trip: the combined probe command, under the probe budget.
        assert!(matches!(
            &fake.calls()[..],
            [RemoteCall::Exec { cmd, timeout, .. }] if *cmd == remote_command(&opts) && *timeout == Some(PROBE_TIMEOUT)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_with_reports_a_failed_or_wedged_pod_as_an_error_row() {
        use crate::remote::{FakeRemote, FakeReply};
        let fake = FakeRemote::new();
        fake.script(
            "10.0.0.1:22001",
            [FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22001: Connection refused"), FakeReply::hang()],
        );
        let m = fetch_with(&fake, &target(), &ProbeOpts::default()).await;
        assert!(m.error.as_deref().is_some_and(|e| e.starts_with("ssh connect failed:")), "{:?}", m.error);
        assert!(m.gpus.is_empty());
        // A hung probe ends at the budget (paused clock: instantly), not never.
        let start = tokio::time::Instant::now();
        let m = fetch_with(&fake, &target(), &ProbeOpts::default()).await;
        assert_eq!(start.elapsed(), PROBE_TIMEOUT);
        assert!(m.error.as_deref().is_some_and(|e| e.contains("timed out after 20s")), "{:?}", m.error);
    }

    #[test]
    fn remote_command_includes_requested_parts() {
        let cmd = remote_command(&ProbeOpts {
            progress_cmd: Some("cat /tmp/p".into()),
            repo_path: Some("/root/ARENA_3.0".into()),
            key_remote: Some("/root/.ssh/id_ed25519".into()),
            nvidia_smi_timeout: None,
            repo_timeout: None,
        });
        assert!(cmd.contains("nvidia-smi"));
        assert!(cmd.contains("git rev-parse --abbrev-ref HEAD"));
        assert!(cmd.contains("remote.origin.url"));
        assert!(cmd.contains("git log -1 --format=%ct"));
        assert!(cmd.contains("git log -n 4 --format='clog=%h"));
        assert!(cmd.contains("--query-compute-apps=pid,used_memory,process_name"));
        // Its git is the read-only kind, and never `git status` (refresh, filters, fsmonitor).
        assert!(cmd.contains(RO_GIT_SH) && cmd.contains("ro_setup && ro_c=$(ro_changes"));
        assert!(!cmd.contains("git status") && !cmd.contains("&& git "), "{cmd}");
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

    /// Review ai-review claude #8 (lane ASK7 item F): `ro_git`'s driver loop ends whatever
    /// `ro_drivers` holds — `lfs` without its trailing space never shrank, so the loop ran forever,
    /// `"$@"` growing, as root on a pod; names two spaces apart; none — and a `ro_drivers` (or a
    /// `ro_timeout`) inherited from the environment is never used: the dashboard's probe runs in
    /// the pod user's login shell. Under bash and zsh, each run inside a `timeout`, a stub git
    /// printing what it got.
    #[cfg(target_os = "linux")]
    #[test]
    fn ro_gits_driver_loop_always_ends() {
        use std::process::Command;
        let t = crate::ask::ai::tests::Tmp::new("ro-loop");
        let bin = t.0.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        crate::remote::write_test_script(bin.join("git"), "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.display());
        let shells: Vec<&str> = crate::backup::tests::login_shells().into_iter().filter(|s| *s != "fish").collect();
        assert!(shells.contains(&"bash"), "{shells:?}");
        let run = |shell: &str, script: String, env: &[(&str, &str)]| {
            let mut c = Command::new("timeout");
            c.args(["-k", "1", "5", shell, "-c", &script]).env_clear().env("PATH", &path);
            c.envs(env.iter().copied());
            let o = c.output().unwrap();
            assert!(o.status.success(), "{shell}: {script:?} {env:?} → {:?} (124/137: it spun)", o.status.code());
            String::from_utf8(o.stdout).unwrap().lines().map(String::from).collect::<Vec<_>>()
        };
        for shell in shells {
            for (drivers, want) in [("lfs", &["lfs"][..]), ("lfs ", &["lfs"]), ("a  b", &["a", "b"]), ("a b ", &["a", "b"]), ("", &[])] {
                let args = run(shell, format!("{RO_GIT_SH}ro_drivers='{drivers}'\nro_git status"), &[]);
                let filters: Vec<&str> = args.iter().filter(|a| a.starts_with("filter.")).map(String::as_str).collect();
                let expect: Vec<String> = want.iter().flat_map(|d| ["clean=", "smudge=", "process=", "required=false"].map(|k| format!("filter.{d}.{k}"))).collect();
                assert_eq!(filters, expect, "{shell} {drivers:?}");
                assert_eq!(args.last().map(String::as_str), Some("status"), "{shell} {drivers:?}");
            }
            // Inherited from the environment: neither is used (a `timeout abc` would refuse to run git).
            let args = run(shell, format!("{RO_GIT_SH}ro_git status"), &[("ro_drivers", "evil"), ("ro_timeout", "abc")]);
            assert!(args.first().is_some_and(|a| a == "--no-optional-locks") && !args.iter().any(|a| a.contains("evil")), "{shell}: {args:?}");
        }
    }

    /// Review ai-review claude #8: the dashboard's probe runs in the pod user's login shell — zsh
    /// on the pods — which doesn't split an unquoted `$var`. `ro_git`'s filter-emptying `-c` words
    /// went to git as one argument, git refused it, and in any repo with a filter driver configured
    /// (`git lfs install` puts `filter.lfs.*` in the global config) `dirty` read unknown. Under sh,
    /// bash and zsh (fish runs none of this probe: its functions are POSIX), with git-lfs's global
    /// config: `dirty` is the right count, and the filter never ran — though an unchanged lfs file
    /// is racily clean, so plain plumbing hashes it through the filter (the fixture is armed).
    #[cfg(target_os = "linux")]
    #[test]
    fn dirty_counts_right_under_each_login_shell_with_a_global_lfs_filter() {
        use std::process::Command;
        let have = |t: &str| Command::new("sh").arg("-c").arg(format!("command -v {t}")).output().is_ok_and(|o| o.status.success());
        if !["git", "touch"].iter().all(|t| have(t)) {
            eprintln!("git/touch missing — skipping");
            return;
        }
        let t = crate::ask::ai::tests::Tmp::new("lfs-dirty");
        let (home, marks) = (t.0.join("home"), t.0.join("marks"));
        let repo = home.join("ARENA_materials");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&marks).unwrap();
        // Git as the pod's: this home's global config, no system one.
        let isolated = |c: &mut Command| {
            c.env("HOME", &home).env("GIT_CONFIG_NOSYSTEM", "1").env_remove("GIT_CONFIG_GLOBAL").env_remove("XDG_CONFIG_HOME");
        };
        let git = |args: &[&str]| {
            let mut c = Command::new("git");
            c.args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"]).args(args).current_dir(&repo);
            isolated(&mut c);
            c.output().unwrap()
        };
        let ok = |args: &[&str]| {
            let o = git(args);
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        ok(&["init", "-q"]);
        std::fs::write(repo.join(".gitattributes"), "*.bin filter=lfs diff=lfs merge=lfs -text\n").unwrap();
        for f in ["model.bin", "data.bin", "train.py"] {
            std::fs::write(repo.join(f), format!("{f} v1\n")).unwrap();
        }
        ok(&["add", "."]);
        ok(&["commit", "-q", "-m", "first"]);
        std::fs::write(repo.join("train.py"), "train.py v2\n").unwrap();
        ok(&["add", "train.py"]);
        // git-lfs's global config, its helper a stand-in that says it ran.
        let lfs = t.0.join("git-lfs");
        crate::remote::write_test_script(&lfs, format!("#!/bin/sh\necho \"$*\" >> {}/lfs\n[ \"$1\" = filter-process ] && exit 1\ncat\n", marks.display()))
            .unwrap();
        let l = lfs.display();
        std::fs::write(
            home.join(".gitconfig"),
            format!("[filter \"lfs\"]\n\tclean = {l} clean -- %f\n\tsmudge = {l} smudge -- %f\n\tprocess = {l} filter-process\n\trequired = true\n"),
        )
        .unwrap();
        // Work: train.py staged, model.bin (lfs) modified, a new file — 3 entries; data.bin as
        // committed, every index entry racily clean.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(repo.join("model.bin"), "model.bin v2\n").unwrap();
        std::fs::write(repo.join("scratch.txt"), "s\n").unwrap();
        assert!(Command::new("touch").args(["-d", "@978307200"]).arg(repo.join(".git/index")).status().unwrap().success());
        let index = std::fs::read(repo.join(".git/index")).unwrap();
        let fired = || std::fs::read_to_string(marks.join("lfs")).unwrap_or_default();

        let opts = ProbeOpts { repo_path: Some(repo.display().to_string()), ..Default::default() };
        let shells: Vec<&str> = std::iter::once("sh").chain(crate::backup::tests::login_shells().into_iter().filter(|s| *s != "fish")).collect();
        for shell in shells {
            let mut c = Command::new(shell);
            c.arg("-c").arg(remote_command(&opts)).current_dir(&home);
            isolated(&mut c);
            let o = c.output().unwrap();
            let m = parse_probe_output(&String::from_utf8_lossy(&o.stdout));
            assert_eq!((m.branch.as_deref(), m.dirty_files), (Some("main"), Some(3)), "{shell}: {}", String::from_utf8_lossy(&o.stderr));
            assert_eq!(fired(), "", "{shell}: the probe ran the lfs filter");
            assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), index, "{shell}: the index was written");
        }
        // Armed: plain plumbing, on the same repo, runs it.
        git(&["-c", "core.fsmonitor=", "diff-files"]);
        assert!(fired().contains("filter-process"), "{}", fired());
    }
}
