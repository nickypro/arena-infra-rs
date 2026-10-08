//! `pods idle`: which pods look abandoned — a **read-only report** that never acts.
//!
//! The single biggest saving is stopping expensive pods nobody is using (ops playbook §9),
//! but stopping or terminating a pod someone *is* using destroys their work, and an
//! automatic reaper that guesses wrong once ruins a participant's day. So this only looks
//! and suggests: one bounded, read-only probe per pod, a table, and for the pods that are
//! idle by **every** measure the exact commands an operator could run — backup first, then
//! terminate — printed, never executed.
//!
//! What one probe reads (POSIX `sh`, nothing written anywhere):
//! - **Sessions**: established TCP connections, from `/proc/net/tcp{,6}` (no `ss`/`netstat`
//!   needed). On sshd's port (22, plus the port our own connection came in on) every one is
//!   an SSH session — VS Code Remote-SSH included — except our own, identified exactly by
//!   `$SSH_CONNECTION` (client ip:port, server port). Connections to any *other* listening
//!   port from a non-loopback peer (Jupyter through the provider's HTTP proxy, a TensorBoard
//!   tab) count too, as "other inbound"; loopback ones are local plumbing (a kernel talking to
//!   its server) and don't. If our own connection can't be told apart, the count is unknown.
//!   Not seen: a session through RunPod's `ssh.runpod.io` proxy, which doesn't come in over
//!   the pod's sshd (the cohort connects to the direct endpoint, via our proxy) — one more
//!   reason the report only suggests.
//! - **GPU**: three `nvidia-smi` samples a second apart (one sample can land in a data-loader
//!   gap); the busiest GPU's busiest sample, and the memory in use.
//! - **Files**: the newest mtime under the home and the repo (`BACKUP_REPO_PATH`), from one
//!   `find` bounded to 20 s, skipping `.git`, caches, editor servers and package dirs (what
//!   our own backups and tools touch, or is just big). A scan that didn't finish is unknown.
//! - **Uptime**: PID 1's age. A pod restarted 10 minutes ago has only old, image-made files —
//!   "idle for weeks" by mtime alone — so idleness can't exceed the uptime.
//!
//! Ages are measured on the pod's own clock (it reports `now`), so clock skew between the
//! control box and a pod can't make a pod look idle.
//!
//! A pod is a **candidate** only when every reading is known and all agree: no session or
//! other inbound connection, GPU ≤ [`IDLE_GPU_PCT`] (or no GPU on a CPU-only server), no file
//! change for ≥ N hours, and up ≥ N hours. Anything that can't be read makes it unknown —
//! never a candidate. And only this cohort's machines (`{prefix}-…`, not a staff box's `@`
//! list entry — `teardown --check`'s rule) get commands: a staff box or another prefix's
//! pod that looks idle is named, and left to the operator.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::Serialize;

use crate::fleet::{self, clip, FleetCost};
use crate::jobs::sanitize;
use crate::pod::Pod;
use crate::selector::Naming;
use crate::snapshot::fmt_age;
use crate::table::{self, Align};

/// `--hours` default: a working day's gap — longer than lunch, shorter than a night.
pub const DEFAULT_HOURS: f64 = 6.0;

/// A GPU at or below this utilization (%) counts as idle.
pub const IDLE_GPU_PCT: u32 = 5;

/// Seconds the file scan may take on a pod before it's cut off (then: unknown).
pub const FIND_BUDGET_SECS: u32 = 20;

/// GPU memory in use (MiB) above which a candidate gets a "something still holds the GPU"
/// note: a live notebook kernel with a model loaded is lost with the pod.
const GPU_MEM_NOTE_MIB: u64 = 1024;

/// At most this many socket lines are sent back (`/proc/net/tcp{,6}` ESTABLISHED + LISTEN),
/// so a pod with a huge socket table can't flood the reply. Past it the probe says so
/// (`ARENA_IDLE_TCP_TRUNCATED`) and the session counts are only a lower bound: unknown,
/// never "no sessions" — a participant's connection may be among the lines not sent.
pub const TCP_LINE_CAP: usize = 2000;

/// The probe's socket-list cap as a filter: the first [`TCP_LINE_CAP`] lines through, then —
/// only if any were left out — the truncation marker. (A bare `head -n` would cut silently.)
fn tcp_cap_filter() -> String {
    format!(
        r#"awk -v cap={TCP_LINE_CAP} 'NR <= cap {{ print; next }} {{ cut = 1 }} END {{ if (cut) print "ARENA_IDLE_TCP_TRUNCATED" }}'"#
    )
}

/// Directory names the file scan skips: git internals (our backup commits there), caches,
/// editor servers (they write logs while merely connected), package trees and build caches.
const PRUNE: &[&str] = &[
    ".git",
    ".cache",
    ".ssh",
    ".vscode-server",
    ".vscode-server-insiders",
    ".cursor-server",
    ".windsurf-server",
    "node_modules",
    "__pycache__",
    "site-packages",
    "dist-packages",
    ".npm",
    ".triton",
    ".nv",
];

/// The probe, run as `sh -c '<this>' arena-idle '<repo>'` (so `$1` is the repo path). Only
/// reads. Every line it prints that matters starts with an `ARENA_IDLE_` marker; anything
/// else (a login banner) is ignored. `ARENA_IDLE_END` last, so a cut-off reply is told apart.
fn script() -> String {
    let prune: Vec<String> = PRUNE.iter().map(|n| format!("-name {n}")).collect();
    format!(
        r#"repo=$1
t() {{ s=$1; shift; if command -v timeout >/dev/null 2>&1; then timeout "$s" "$@"; else "$@"; fi; }}
echo "ARENA_IDLE_NOW $(date +%s)"
hz=$(getconf CLK_TCK 2>/dev/null)
up=$(awk -v hz="${{hz:-100}}" 'NR == FNR {{ u = $1; next }} {{ s = $0; sub(/^.*\) /, "", s); split(s, f, " "); if (f[20] > 0) printf "%.0f\n", u - f[20] / hz }}' /proc/uptime /proc/1/stat 2>/dev/null)
[ -n "$up" ] || up=$(ps -o etimes= -p 1 2>/dev/null | tr -d ' ')
echo "ARENA_IDLE_UP ${{up:--}}"
echo "ARENA_IDLE_SELF ${{SSH_CONNECTION:--}}"
for f in /proc/net/tcp /proc/net/tcp6; do
  [ -r "$f" ] && awk 'FNR > 1 && $4 == "01" {{ print "ARENA_IDLE_TCP", $2, $3 }} FNR > 1 && $4 == "0A" {{ print "ARENA_IDLE_LISTEN", $2 }}' "$f"
done | {tcp_cap}
if command -v nvidia-smi >/dev/null 2>&1; then
  for i in 1 2 3; do
    out=$(t 10 nvidia-smi --query-gpu=utilization.gpu,memory.used --format=csv,noheader,nounits 2>&1)
    echo "ARENA_IDLE_GPU_RC $?"
    printf '%s\n' "$out" | head -n 64 | sed 's/^/ARENA_IDLE_GPU /'
    [ "$i" = 3 ] || sleep 1
  done
else
  echo "ARENA_IDLE_GPU_NONE"
fi
home=${{HOME:-/root}}
set -- "$home"
if [ -n "$repo" ] && [ -d "$repo" ]; then
  echo "ARENA_IDLE_REPO yes"
  case "$repo/" in
    "$home"/*) [ -L "$repo" ] && set -- "$home" "$repo" ;;
    *) set -- "$home" "$repo" ;;
  esac
else
  echo "ARENA_IDLE_REPO no"
fi
{{ t {budget} find -H "$@" \( {prune} -o -path '*/.local/lib' \) -prune -o -type f -printf '%T@ %p\n' 2>/dev/null; echo "ARENA_FIND_RC $?"; }} | awk -v repo="$repo" '
  $1 == "ARENA_FIND_RC" {{ rc = $2; next }}
  {{ t = $1 + 0; n++; if (t > all) all = t
    if (repo != "") {{ p = substr($0, length($1) + 2); if ((p == repo || index(p, repo "/") == 1) && t > r) r = t }} }}
  END {{ printf "ARENA_IDLE_FILES %.0f %.0f %d %s\n", all, r, n, (rc == "" ? "-" : rc) }}'
echo "ARENA_IDLE_END"
"#,
        budget = FIND_BUDGET_SECS,
        prune = prune.join(" -o "),
        tcp_cap = tcp_cap_filter(),
    )
}

/// Single-quote for `sh` (`'\''` for an embedded quote).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The remote command for one pod: the probe under `sh -c` (POSIX sh whatever the login
/// shell is), with the repo path as `$1` — never spliced into the script. A trailing `/` is
/// dropped so the "under the repo" match works.
pub fn probe_command(repo: &str) -> String {
    let repo = match repo.trim_end_matches('/') {
        "" if repo.starts_with('/') => "/",
        r => r,
    };
    format!("sh -c {} arena-idle {}", shell_quote(&script()), shell_quote(repo))
}

/// The GPU as the probe saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Gpu {
    /// The busiest GPU's busiest sample (%), and the most memory in use across the samples.
    Read { gpus: u32, max_util: u32, mem_used_mib: u64 },
    /// No `nvidia-smi` on the machine.
    Absent,
    /// `nvidia-smi` failed, hung or said something unexpected (e.g. `[N/A]` under MIG).
    Unreadable { why: String },
}

/// Everything one probe read. Times are the pod's own clock.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reading {
    /// The pod's clock when it answered (unix seconds).
    pub now: u64,
    /// PID 1's age in seconds — the container's (or VM's) uptime.
    pub uptime_secs: Option<u64>,
    /// Established connections on sshd's port, ours excluded when `self_found`.
    pub ssh_sessions: u32,
    /// Whether our own connection was found (and left out). If not, `ssh_sessions` may
    /// include it, and the count can't decide anything.
    pub self_found: bool,
    /// Established non-loopback connections to other listening ports.
    pub other_inbound: u32,
    /// Whether every socket line was read. `false` past [`TCP_LINE_CAP`]: `ssh_sessions` and
    /// `other_inbound` are then only a lower bound — enough to say "in use", never "idle".
    pub connections_complete: bool,
    pub gpu: Gpu,
    /// Newest file mtime under the home and the repo (`None` = no file found).
    pub newest_file: Option<u64>,
    /// Newest file mtime under the repo.
    pub newest_repo_file: Option<u64>,
    /// Whether the repo directory exists on the pod.
    pub repo_present: bool,
    /// Whether the file scan finished (else the newest found is only a lower bound).
    pub files_complete: bool,
    /// Why the file scan is incomplete.
    pub files_note: Option<String>,
}

/// A `/proc/net/tcp{,6}` address (`0100007F:0016`): the IP as the kernel's host-order u32
/// words (little-endian on every machine we rent), the port big-endian hex. An IPv4-mapped
/// v6 address becomes its IPv4 form, as sshd reports it in `$SSH_CONNECTION`.
pub fn decode_proc_addr(s: &str) -> Option<SocketAddr> {
    let (ip, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |h: &str| u32::from_str_radix(h, 16).ok().map(u32::to_le_bytes);
    let ip = match ip.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(ip)?)),
        32 => {
            let mut b = [0u8; 16];
            for (i, chunk) in b.chunks_mut(4).enumerate() {
                chunk.copy_from_slice(&word(ip.get(i * 8..i * 8 + 8)?)?);
            }
            normalize(IpAddr::V6(Ipv6Addr::from(b)))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// `::ffff:1.2.3.4` → `1.2.3.4`; anything else unchanged.
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// `$SSH_CONNECTION` = `client_ip client_port server_ip server_port` → our client address
/// and the port sshd took the connection on.
fn parse_self(s: &str) -> Option<(SocketAddr, u16)> {
    let f: Vec<&str> = s.split_whitespace().collect();
    let [cip, cport, _sip, sport] = f.as_slice() else { return None };
    let client = SocketAddr::new(normalize(cip.parse().ok()?), cport.parse().ok()?);
    Some((client, sport.parse().ok()?))
}

/// Count sessions: `(ssh, self_found, other_inbound)`. See the module doc for the rules.
fn count_sessions(established: &[(SocketAddr, SocketAddr)], listening: &[SocketAddr], me: Option<(SocketAddr, u16)>) -> (u32, bool, u32) {
    let mut ssh_ports: HashSet<u16> = HashSet::from([22]);
    if let Some((_, port)) = me {
        ssh_ports.insert(port);
    }
    let listen_ports: HashSet<u16> = listening.iter().map(SocketAddr::port).collect();
    let (mut ssh, mut found, mut other) = (0, false, 0);
    for (local, remote) in established {
        let (local, remote) = (SocketAddr::new(normalize(local.ip()), local.port()), SocketAddr::new(normalize(remote.ip()), remote.port()));
        if ssh_ports.contains(&local.port()) {
            if !found && me.is_some_and(|(client, port)| client == remote && port == local.port()) {
                found = true;
            } else {
                ssh += 1;
            }
        } else if listen_ports.contains(&local.port()) && !remote.ip().is_loopback() {
            other += 1;
        }
    }
    (ssh, found, other)
}

/// The GPU lines: one `ARENA_IDLE_GPU_RC n` per sample, then that sample's lines.
fn judge_gpu(none: bool, samples: &[(String, Vec<String>)]) -> Gpu {
    if none {
        return Gpu::Absent;
    }
    let first_line = |lines: &[String]| clip(&sanitize(lines.first().map(String::as_str).unwrap_or("no output")), 120);
    let mut gpus = 0u32;
    let (mut max_util, mut max_mem) = (0u32, 0u64);
    for (rc, lines) in samples {
        match rc.as_str() {
            "0" => {}
            "124" => return Gpu::Unreadable { why: "nvidia-smi timed out".into() },
            other => return Gpu::Unreadable { why: format!("nvidia-smi exit {other}: {}", first_line(lines)) },
        }
        let mut mem = 0u64;
        for line in lines {
            let mut cells = line.split(',').map(str::trim);
            let (Some(u), Some(m), None) = (cells.next(), cells.next(), cells.next()) else {
                return Gpu::Unreadable { why: format!("unexpected nvidia-smi line: {}", clip(&sanitize(line), 80)) };
            };
            let (Ok(u), Ok(m)) = (u.parse::<u32>(), m.parse::<u64>()) else {
                let why = if u.contains("N/A") { "utilization not reported ([N/A] — a MIG slice?)".to_string() } else {
                    format!("unexpected nvidia-smi line: {}", clip(&sanitize(line), 80))
                };
                return Gpu::Unreadable { why };
            };
            max_util = max_util.max(u);
            mem += m;
        }
        max_mem = max_mem.max(mem);
        gpus = gpus.max(lines.len() as u32);
    }
    if samples.is_empty() || gpus == 0 {
        return Gpu::Unreadable { why: "nvidia-smi listed no GPU".into() };
    }
    Gpu::Read { gpus, max_util, mem_used_mib: max_mem }
}

/// Parse the probe's reply. `Err` for a reply that isn't one (no marker, cut short).
pub fn parse_reply(stdout: &str) -> Result<Reading, String> {
    let (mut now, mut up, mut me, mut end) = (None, None, None, false);
    let (mut est, mut listen) = (Vec::new(), Vec::new());
    let (mut gpu_none, mut samples): (bool, Vec<(String, Vec<String>)>) = (false, Vec::new());
    let (mut files, mut repo_present, mut tcp_truncated) = (None, false, false);
    for line in stdout.lines() {
        let line = line.trim_end();
        let Some((tag, rest)) = line.split_once(' ').or(Some((line, ""))) else { continue };
        match tag {
            "ARENA_IDLE_NOW" => now = rest.trim().parse::<u64>().ok(),
            "ARENA_IDLE_UP" => up = rest.trim().parse::<u64>().ok(),
            "ARENA_IDLE_SELF" => me = parse_self(rest),
            "ARENA_IDLE_TCP" => {
                if let Some((l, r)) = rest.split_once(' ') {
                    if let (Some(l), Some(r)) = (decode_proc_addr(l.trim()), decode_proc_addr(r.trim())) {
                        est.push((l, r));
                    }
                }
            }
            "ARENA_IDLE_LISTEN" => listen.extend(decode_proc_addr(rest.trim())),
            "ARENA_IDLE_TCP_TRUNCATED" => tcp_truncated = true,
            "ARENA_IDLE_GPU_NONE" => gpu_none = true,
            "ARENA_IDLE_GPU_RC" => samples.push((rest.trim().to_string(), Vec::new())),
            "ARENA_IDLE_GPU" => {
                if let Some((_, lines)) = samples.last_mut() {
                    if !rest.trim().is_empty() {
                        lines.push(rest.trim().to_string());
                    }
                }
            }
            "ARENA_IDLE_REPO" => repo_present = rest.trim() == "yes",
            "ARENA_IDLE_FILES" => files = Some(rest.split_whitespace().map(String::from).collect::<Vec<_>>()),
            "ARENA_IDLE_END" => end = true,
            _ => {}
        }
    }
    let Some(now) = now else { return Err("no reply from the probe".into()) };
    if !end {
        return Err("the probe's reply was cut short".into());
    }
    let Some(files) = files.filter(|f| f.len() == 4) else { return Err("the probe reported no file scan".into()) };
    let stamp = |s: &str| s.parse::<u64>().ok().filter(|t| *t > 0);
    let (files_complete, files_note) = match files[3].as_str() {
        "0" => (true, None),
        "124" => (false, Some(format!("the file scan timed out after {FIND_BUDGET_SECS}s"))),
        "-" => (false, Some("the file scan didn't report".to_string())),
        rc => (false, Some(format!("the file scan exited {}", clip(&sanitize(rc), 8)))),
    };
    let (ssh_sessions, self_found, other_inbound) = count_sessions(&est, &listen, me);
    Ok(Reading {
        now,
        uptime_secs: up,
        ssh_sessions,
        self_found,
        other_inbound,
        connections_complete: !tcp_truncated,
        gpu: judge_gpu(gpu_none, &samples),
        newest_file: stamp(&files[0]),
        newest_repo_file: stamp(&files[1]).filter(|_| repo_present),
        repo_present,
        files_complete,
        files_note,
    })
}

/// What happened with one pod.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// Not billing (stopped/exited): not probed.
    NotBilling,
    /// Billing but no SSH endpoint yet: can't be probed.
    NoEndpoint,
    /// The probe failed (unreachable, timed out, an unreadable reply).
    Failed(String),
    Read(Reading),
}

/// One pod's verdict.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// Idle by every measure for at least the threshold: a candidate.
    Idle { idle_secs: u64 },
    /// Something says it's in use (or too new to tell).
    NotIdle { reasons: Vec<String> },
    /// A reading is missing — never a candidate.
    Unknown { reasons: Vec<String> },
    /// Not billing: nothing to save.
    NotRunning,
}

/// A CPU-only server: no `nvidia-smi` is expected there.
fn cpu_only(pod: &Pod) -> bool {
    pod.provider.eq_ignore_ascii_case("hetzner") || pod.gpu_count == Some(0)
}

/// `1 SSH session` / `2 SSH sessions`.
fn plural(n: u32, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// Judge one pod against `threshold` seconds. Pure. A reason that says "in use" wins over
/// one that says "can't tell": either way it isn't a candidate, and the former is what the
/// operator needs to see.
pub fn judge(pod: &Pod, probe: &Probe, threshold: u64) -> Verdict {
    let r = match probe {
        Probe::NotBilling => return Verdict::NotRunning,
        Probe::NoEndpoint => return Verdict::Unknown { reasons: vec!["no SSH endpoint yet".into()] },
        Probe::Failed(e) => return Verdict::Unknown { reasons: vec![e.clone()] },
        Probe::Read(r) => r,
    };
    let (mut busy, mut unknown) = (Vec::new(), Vec::new());
    if r.self_found {
        if r.ssh_sessions > 0 {
            busy.push(plural(r.ssh_sessions, "SSH session"));
        }
    } else {
        unknown.push(format!("couldn't tell this probe's own connection apart ({} on the SSH port)", r.ssh_sessions));
    }
    if r.other_inbound > 0 {
        busy.push(plural(r.other_inbound, "other inbound connection"));
    }
    if !r.connections_complete {
        unknown.push(format!("over {TCP_LINE_CAP} sockets — not every connection was read"));
    }
    match &r.gpu {
        Gpu::Read { max_util, .. } if *max_util > IDLE_GPU_PCT => busy.push(format!("GPU {max_util}%")),
        Gpu::Read { .. } => {}
        Gpu::Absent if cpu_only(pod) => {}
        Gpu::Absent => unknown.push("no nvidia-smi on a GPU pod".into()),
        Gpu::Unreadable { why } => unknown.push(format!("GPU: {why}")),
    }
    let file_age = r.newest_file.map(|t| r.now.saturating_sub(t));
    match file_age {
        Some(age) if age < threshold => busy.push(format!("a file changed {} ago", fmt_age(age))),
        _ if !r.files_complete => unknown.push(r.files_note.clone().unwrap_or_else(|| "the file scan is incomplete".into())),
        _ => {}
    }
    match r.uptime_secs {
        Some(up) if up < threshold => busy.push(format!("up only {}", fmt_age(up))),
        Some(_) => {}
        None => unknown.push("uptime unknown".into()),
    }
    if !busy.is_empty() {
        return Verdict::NotIdle { reasons: busy };
    }
    if !unknown.is_empty() {
        return Verdict::Unknown { reasons: unknown };
    }
    // Both known and ≥ threshold here: idle since the newest file, but never longer than up.
    let up = r.uptime_secs.unwrap_or(0);
    Verdict::Idle { idle_secs: file_age.map_or(up, |a| a.min(up)) }
}

/// One pod's row of the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IdleRow {
    pub name: String,
    pub id: String,
    pub provider: String,
    pub status: String,
    pub cost_per_hr: Option<f64>,
    pub reading: Option<Reading>,
    pub error: Option<String>,
    #[serde(flatten)]
    pub verdict: Verdict,
    /// One of this cohort's machines (`{prefix}-…`, not a staff box): only these are
    /// candidates — an idle staff box or another prefix's pod gets no command.
    pub cohort: bool,
    /// For a candidate: what the operator could run, in order. Never run by us.
    pub commands: Vec<String>,
}

impl IdleRow {
    /// Idle by every measure and one of this cohort's machines.
    fn is_candidate(&self) -> bool {
        self.cohort && matches!(self.verdict, Verdict::Idle { .. })
    }
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IdleReport {
    pub hours: f64,
    pub gpu_idle_pct: u32,
    pub pods: Vec<IdleRow>,
    /// The candidates' names, in table order (cohort machines only).
    pub candidates: Vec<String>,
    /// Pods idle by every measure that aren't this cohort's (staff boxes, other prefixes):
    /// named, never given a command.
    pub idle_not_cohort: Vec<String>,
    /// What the candidates cost per hour together (USD and EUR kept apart).
    pub saving: FleetCost,
    /// Pods that couldn't be judged.
    pub unknown: usize,
}

/// How the commands name a pod: its full name, or its id when the name isn't unique here
/// (`terminate` refuses a shared name) or is empty.
fn pod_ref(pod: &Pod, all: &[Pod]) -> String {
    let shared = all.iter().filter(|p| p.name == pod.name).count() > 1;
    if pod.name.is_empty() || shared {
        pod.id.clone()
    } else {
        pod.name.clone()
    }
}

/// The suggested commands for a candidate: back up (git push + rsync of the home to the
/// control box), then terminate.
pub fn commands(pod: &Pod, all: &[Pod]) -> Vec<String> {
    let r = pod_ref(pod, all);
    vec![format!("arena pods backup {r}"), format!("arena pods terminate {r}")]
}

/// Build the report: `pods` and `probes` pair up by position; `naming` says which pods are
/// this cohort's. Pure.
pub fn report(pods: &[Pod], probes: &[Probe], hours: f64, naming: &Naming) -> IdleReport {
    let threshold = (hours * 3600.0).round().max(0.0) as u64;
    let mut rows = Vec::new();
    let mut idle_pods = Vec::new();
    for (pod, probe) in pods.iter().zip(probes) {
        let verdict = judge(pod, probe, threshold);
        let cohort = naming.is_cohort(&pod.name);
        let candidate = cohort && matches!(verdict, Verdict::Idle { .. });
        if candidate {
            idle_pods.push(pod.clone());
        }
        rows.push(IdleRow {
            name: pod.name.clone(),
            id: pod.id.clone(),
            provider: pod.provider.clone(),
            status: pod.status.clone(),
            cost_per_hr: pod.cost_per_hr,
            reading: match probe {
                Probe::Read(r) => Some(r.clone()),
                _ => None,
            },
            error: match probe {
                Probe::Failed(e) => Some(e.clone()),
                _ => None,
            },
            commands: if candidate { commands(pod, pods) } else { Vec::new() },
            cohort,
            verdict,
        });
    }
    IdleReport {
        hours,
        gpu_idle_pct: IDLE_GPU_PCT,
        candidates: rows.iter().filter(|r| r.is_candidate()).map(|r| r.name.clone()).collect(),
        idle_not_cohort: rows
            .iter()
            .filter(|r| !r.cohort && matches!(r.verdict, Verdict::Idle { .. }))
            .map(|r| r.name.clone())
            .collect(),
        saving: fleet::fleet_cost(&idle_pods),
        unknown: rows.iter().filter(|r| matches!(r.verdict, Verdict::Unknown { .. })).count(),
        pods: rows,
    }
}

/// `6h`, `1.5h`.
fn fmt_hours(h: f64) -> String {
    if h.fract() == 0.0 {
        format!("{h:.0}h")
    } else {
        format!("{h}h")
    }
}

/// The table's cells for one row.
fn cells(row: &IdleRow) -> Vec<String> {
    let pod = Pod { provider: row.provider.clone(), status: row.status.clone(), cost_per_hr: row.cost_per_hr, ..Default::default() };
    let mut out = vec![row.name.clone(), row.provider.clone(), fleet::price_label(&pod)];
    match &row.reading {
        None => {
            let blank = if matches!(row.verdict, Verdict::NotRunning) { "-" } else { "?" };
            out.extend(std::iter::repeat_n(blank.to_string(), 7));
        }
        Some(r) => {
            let age = |t: Option<u64>| t.map(|t| fmt_age(r.now.saturating_sub(t)));
            let unsure_conn = if r.connections_complete { "" } else { "?" };
            let unsure_ssh = if r.self_found { unsure_conn } else { "?" };
            out.push(format!("{}{unsure_ssh}", r.ssh_sessions));
            out.push(format!("{}{unsure_conn}", r.other_inbound));
            let (util, mem) = match &r.gpu {
                Gpu::Read { max_util, mem_used_mib, .. } => (format!("{max_util}%"), format!("{:.1}G", *mem_used_mib as f64 / 1024.0)),
                Gpu::Absent => ("-".into(), "-".into()),
                Gpu::Unreadable { .. } => ("?".into(), "?".into()),
            };
            out.push(util);
            out.push(mem);
            let unsure = if r.files_complete { "" } else { "?" };
            out.push(match age(r.newest_file) {
                Some(a) => format!("{a}{unsure}"),
                None if r.files_complete => "none".into(),
                None => "?".into(),
            });
            out.push(match (r.repo_present, age(r.newest_repo_file)) {
                (false, _) => "-".into(),
                (true, Some(a)) => format!("{a}{unsure}"),
                (true, None) if r.files_complete => "none".into(),
                (true, None) => "?".into(),
            });
            out.push(r.uptime_secs.map(fmt_age).unwrap_or_else(|| "?".into()));
        }
    }
    out.push(match &row.verdict {
        Verdict::Idle { idle_secs } if row.cohort => format!("IDLE {}", fmt_age(*idle_secs)),
        Verdict::Idle { idle_secs } => format!("idle {} — not a cohort machine, no suggestion", fmt_age(*idle_secs)),
        Verdict::NotIdle { reasons } => format!("in use: {}", reasons.join(", ")),
        Verdict::Unknown { reasons } => format!("? {}", clip(&reasons.join("; "), 100)),
        Verdict::NotRunning => format!("- not billing ({})", row.status),
    });
    out
}

/// The report as text: the table, then the candidates with their commands (printed, never
/// run), or a line saying there are none.
pub fn render(report: &IdleReport) -> String {
    if report.pods.is_empty() {
        return "(no pods)\n".into();
    }
    let headers = ["NAME", "PROVIDER", "$/H", "SSH", "OTHER", "GPU%", "GPU MEM", "NEWEST FILE", "REPO", "UP", "VERDICT"];
    let align = [
        Align::Left,
        Align::Left,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Left,
    ];
    let rows: Vec<Vec<String>> = report.pods.iter().map(cells).collect();
    let mut out = table::render(&headers, &align, &rows);
    let h = fmt_hours(report.hours);
    out.push_str(
        "SSH/OTHER = live inbound connections (this probe's own left out; `?` = couldn't tell it apart, or too many sockets \
         to read them all) · GPU% = busiest of 3 samples · NEWEST FILE/REPO = age of the newest change (caches, .git and editor servers skipped)\n",
    );
    let idle: Vec<&IdleRow> = report.pods.iter().filter(|r| r.is_candidate()).collect();
    if idle.is_empty() {
        out.push_str(&format!("\nNo cohort machine is idle for ≥{h} by every measure — nothing to suggest.\n"));
    } else {
        out.push_str(&format!(
            "\nIdle for ≥{h} — no SSH session or other inbound connection, GPU ≤{}%, no file change, up ≥{h}:\n",
            report.gpu_idle_pct
        ));
        for row in &idle {
            let Verdict::Idle { idle_secs } = row.verdict else { continue };
            let pod = Pod { provider: row.provider.clone(), status: row.status.clone(), cost_per_hr: row.cost_per_hr, ..Default::default() };
            out.push_str(&format!("  {} ({}, {}/h, idle {})\n", row.name, row.provider, fleet::price_label(&pod), fmt_age(idle_secs)));
            for c in &row.commands {
                out.push_str(&format!("    {c}\n"));
            }
            if let Some(Reading { gpu: Gpu::Read { mem_used_mib, .. }, .. }) = &row.reading {
                if *mem_used_mib >= GPU_MEM_NOTE_MIB {
                    out.push_str(&format!(
                        "    note: {:.1} GiB of GPU memory is still allocated (a live notebook kernel?) — it's lost with the pod\n",
                        *mem_used_mib as f64 / 1024.0
                    ));
                }
            }
        }
        let s = &report.saving;
        let mut per = Vec::new();
        if s.priced_usd > 0 {
            per.push(format!("{}/h (≈ {}/day)", fleet::fmt_money("$", s.usd_per_hr), fleet::fmt_money("$", s.usd_per_hr * 24.0)));
        }
        if s.priced_eur > 0 {
            per.push(format!("{}/h", fleet::fmt_money("€", s.eur_per_hr)));
        }
        let unpriced = if s.unpriced > 0 { format!(", {} unpriced", s.unpriced) } else { String::new() };
        let total = if per.is_empty() { String::new() } else { format!(": {} together", per.join(" + ")) };
        out.push_str(&format!("  {} candidate(s){total}{unpriced}\n", idle.len()));
        out.push_str(
            "Nothing was changed. These are for you to run, after checking with the group: back up first (git push + \
             rsync of the home), then terminate. `arena pods stop` keeps the name instead, but on RunPod/Vast it discards \
             the container disk too (it asks for --wipe-ok), and a Hetzner server bills until it's deleted.\n",
        );
    }
    if !report.idle_not_cohort.is_empty() {
        out.push_str(&format!(
            "Also idle, but not this cohort's machines (a staff `@` box or another prefix) — no command suggested: {}\n",
            report.idle_not_cohort.join(", ")
        ));
    }
    if report.unknown > 0 {
        out.push_str(&format!(
            "{} pod(s) couldn't be judged (VERDICT `?`) — an unknown is never a candidate.\n",
            report.unknown
        ));
    }
    out
}

/// Test fixtures for the probe's reply, shared with the CLI's tests.
#[cfg(any(test, feature = "test-util"))]
pub mod fixture {
    /// Little-endian `/proc/net/tcp` hex for an IPv4 `a.b.c.d:port`.
    pub fn v4(ip: [u8; 4], port: u16) -> String {
        format!("{:08X}:{port:04X}", u32::from_le_bytes(ip))
    }

    /// One probe reply. `sessions` = remote addresses on :22 besides ours (`203.0.113.9:50000`
    /// is ours, from `$SSH_CONNECTION`); `gpu` = per-sample `util, mem` lines (`None` = no
    /// nvidia-smi); `newest`/`repo` = mtime ages in seconds before `now` (`None` = no file).
    pub struct Reply {
        pub now: u64,
        pub up: Option<u64>,
        pub sessions: Vec<[u8; 4]>,
        pub jupyter_from: Vec<[u8; 4]>,
        pub gpu: Option<Vec<&'static str>>,
        pub newest_age: Option<u64>,
        pub repo_age: Option<u64>,
        pub find_rc: &'static str,
        /// The socket list was cut at the cap (`ARENA_IDLE_TCP_TRUNCATED`).
        pub tcp_truncated: bool,
    }

    impl Default for Reply {
        fn default() -> Self {
            Reply {
                now: 1_791_460_800,
                up: Some(3 * 86_400),
                sessions: Vec::new(),
                jupyter_from: Vec::new(),
                gpu: Some(vec!["0, 3"]),
                newest_age: Some(9 * 3600),
                repo_age: Some(10 * 3600),
                find_rc: "0",
                tcp_truncated: false,
            }
        }
    }

    impl Reply {
        pub fn render(&self) -> String {
            let mut s = format!("Welcome to the pod!\nARENA_IDLE_NOW {}\n", self.now);
            s.push_str(&format!("ARENA_IDLE_UP {}\n", self.up.map_or("-".into(), |u| u.to_string())));
            s.push_str("ARENA_IDLE_SELF 203.0.113.9 50000 172.17.0.2 22\n");
            let me = v4([172, 17, 0, 2], 22);
            s.push_str(&format!("ARENA_IDLE_LISTEN {}\n", v4([0, 0, 0, 0], 22)));
            s.push_str(&format!("ARENA_IDLE_LISTEN {}\n", v4([0, 0, 0, 0], 8888)));
            s.push_str(&format!("ARENA_IDLE_TCP {me} {}\n", v4([203, 0, 113, 9], 50000)));
            for (i, ip) in self.sessions.iter().enumerate() {
                s.push_str(&format!("ARENA_IDLE_TCP {me} {}\n", v4(*ip, 40000 + i as u16)));
            }
            for (i, ip) in self.jupyter_from.iter().enumerate() {
                s.push_str(&format!("ARENA_IDLE_TCP {} {}\n", v4([172, 17, 0, 2], 8888), v4(*ip, 41000 + i as u16)));
            }
            // A kernel talking to its server over loopback: plumbing, never a session.
            s.push_str(&format!("ARENA_IDLE_TCP {} {}\n", v4([127, 0, 0, 1], 8888), v4([127, 0, 0, 1], 52000)));
            if self.tcp_truncated {
                s.push_str("ARENA_IDLE_TCP_TRUNCATED\n");
            }
            match &self.gpu {
                None => s.push_str("ARENA_IDLE_GPU_NONE\n"),
                Some(lines) => {
                    for _ in 0..3 {
                        s.push_str("ARENA_IDLE_GPU_RC 0\n");
                        for l in lines {
                            s.push_str(&format!("ARENA_IDLE_GPU {l}\n"));
                        }
                    }
                }
            }
            s.push_str("ARENA_IDLE_REPO yes\n");
            let stamp = |age: Option<u64>| age.map_or(0, |a| self.now - a);
            s.push_str(&format!(
                "ARENA_IDLE_FILES {} {} 1234 {}\nARENA_IDLE_END\n",
                stamp(self.newest_age),
                stamp(self.repo_age),
                self.find_rc
            ));
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{v4, Reply};
    use super::*;

    fn pod(name: &str, provider: &str, cost: Option<f64>) -> Pod {
        Pod {
            id: format!("id-{name}"),
            name: name.into(),
            provider: provider.into(),
            status: "RUNNING".into(),
            cost_per_hr: cost,
            gpu_count: Some(1),
            ..Default::default()
        }
    }

    const H6: u64 = 6 * 3600;

    /// `MACHINE_NAME_LIST` with a staff box (`@devtest-boss`: prefixed, but not the cohort's).
    fn list() -> Vec<String> {
        ["apple", "bloom", "cloud", "dune", "@devtest-boss"].map(String::from).to_vec()
    }

    #[test]
    fn proc_addresses_decode_little_endian_words_and_unmap_v4() {
        let cases = [
            ("0100007F:0016", "127.0.0.1:22"),
            ("0200A8C0:22B8", "192.168.0.2:8888"),
            ("00000000:0016", "0.0.0.0:22"),
            ("00000000000000000000000001000000:0016", "[::1]:22"),
            // ::ffff:203.0.113.9 — how sshd's IPv4 clients appear in tcp6.
            ("0000000000000000FFFF0000097100CB:C350", "203.0.113.9:50000"),
            ("B80D0120000000000000000001000000:01BB", "[2001:db8::1]:443"),
        ];
        for (hex, want) in cases {
            assert_eq!(decode_proc_addr(hex).map(|a| a.to_string()).as_deref(), Some(want), "{hex}");
        }
        for bad in ["", "0100007F", "0100007F:XYZ1", "01007F:0016", "zz00007F:0016"] {
            assert_eq!(decode_proc_addr(bad), None, "{bad}");
        }
        assert_eq!(v4([127, 0, 0, 1], 22), "0100007F:0016");
    }

    #[test]
    fn our_own_connection_is_left_out_and_vs_code_counts() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        let me = parse_self("203.0.113.9 50000 172.17.0.2 22");
        assert_eq!(me, Some((a("203.0.113.9:50000"), 22)));
        let listen = [a("0.0.0.0:22"), a("0.0.0.0:8888")];
        // (case, established (local, remote), ssh, found, other)
        let cases: Vec<(&str, Vec<(&str, &str)>, u32, bool, u32)> = vec![
            ("only us", vec![("172.17.0.2:22", "203.0.113.9:50000")], 0, true, 0),
            ("us + VS Code via the proxy", vec![("172.17.0.2:22", "203.0.113.9:50000"), ("172.17.0.2:22", "198.51.100.7:61000")], 1, true, 0),
            ("same client ip, other port = another session", vec![("172.17.0.2:22", "203.0.113.9:50000"), ("172.17.0.2:22", "203.0.113.9:50001")], 1, true, 0),
            ("v4-mapped us", vec![("[::ffff:172.17.0.2]:22", "[::ffff:203.0.113.9]:50000")], 0, true, 0),
            ("jupyter through the provider's proxy", vec![("172.17.0.2:22", "203.0.113.9:50000"), ("172.17.0.2:8888", "100.64.0.1:3000")], 0, true, 1),
            ("loopback kernel plumbing", vec![("172.17.0.2:22", "203.0.113.9:50000"), ("127.0.0.1:8888", "127.0.0.1:52000")], 0, true, 0),
            ("outbound (git push to :22) isn't inbound", vec![("172.17.0.2:22", "203.0.113.9:50000"), ("172.17.0.2:45678", "140.82.112.3:22")], 0, true, 0),
            ("a loopback SSH session still counts", vec![("172.17.0.2:22", "203.0.113.9:50000"), ("127.0.0.1:22", "127.0.0.1:40000")], 1, true, 0),
            ("us not found", vec![("172.17.0.2:22", "198.51.100.7:61000")], 1, false, 0),
        ];
        for (case, est, ssh, found, other) in cases {
            let est: Vec<(SocketAddr, SocketAddr)> = est.iter().map(|(l, r)| (a(l), a(r))).collect();
            assert_eq!(count_sessions(&est, &listen, me), (ssh, found, other), "{case}");
        }
        // No $SSH_CONNECTION: every :22 connection counts, and ours can't be told apart.
        let est = vec![(a("172.17.0.2:22"), a("203.0.113.9:50000"))];
        assert_eq!(count_sessions(&est, &listen, None), (1, false, 0));
        // sshd on another port (as seen by our own connection) counts as SSH too.
        let me2 = parse_self("203.0.113.9 50000 172.17.0.2 2222");
        let est = vec![(a("172.17.0.2:2222"), a("203.0.113.9:50000")), (a("172.17.0.2:2222"), a("198.51.100.7:1"))];
        assert_eq!(count_sessions(&est, &[a("0.0.0.0:2222")], me2), (1, true, 0));
        for bad in ["", "-", "1.2.3.4 x 5.6.7.8 22", "1.2.3.4 1 5.6.7.8"] {
            assert_eq!(parse_self(bad), None, "{bad}");
        }
    }

    #[test]
    fn gpu_samples_take_the_busiest_and_fail_closed() {
        let s = |rc: &str, lines: &[&str]| (rc.to_string(), lines.iter().map(|l| l.to_string()).collect::<Vec<_>>());
        assert_eq!(judge_gpu(true, &[]), Gpu::Absent);
        assert_eq!(
            judge_gpu(false, &[s("0", &["0, 300", "2, 10"]), s("0", &["40, 9000", "0, 10"]), s("0", &["1, 300", "0, 10"])]),
            Gpu::Read { gpus: 2, max_util: 40, mem_used_mib: 9010 }
        );
        let cases = [
            (vec![s("124", &[])], "nvidia-smi timed out"),
            (vec![s("9", &["NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver"])], "nvidia-smi exit 9: NVIDIA-SMI has failed"),
            (vec![s("0", &["[N/A], 4000"])], "[N/A] — a MIG slice?"),
            (vec![s("0", &["0, 3, 7"])], "unexpected nvidia-smi line"),
            (vec![s("0", &[])], "listed no GPU"),
            (vec![], "listed no GPU"),
            (vec![s("0", &["0, 3"]), s("1", &["\u{1b}[31mboom"])], "nvidia-smi exit 1: boom"),
        ];
        for (samples, want) in cases {
            match judge_gpu(false, &samples) {
                Gpu::Unreadable { why } => {
                    assert!(why.contains(want), "{samples:?}: {why}");
                    assert!(!why.contains('\u{1b}'), "terminal escapes are stripped: {why:?}");
                }
                other => panic!("{samples:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_reply_parses_and_a_cut_one_is_an_error() {
        let r = parse_reply(&Reply { sessions: vec![[198, 51, 100, 7]], ..Default::default() }.render()).unwrap();
        assert_eq!(r.now, 1_791_460_800);
        assert_eq!(r.uptime_secs, Some(3 * 86_400));
        assert_eq!((r.ssh_sessions, r.self_found, r.other_inbound), (1, true, 0));
        assert_eq!(r.gpu, Gpu::Read { gpus: 1, max_util: 0, mem_used_mib: 3 });
        assert_eq!(r.newest_file, Some(1_791_460_800 - 9 * 3600));
        assert_eq!(r.newest_repo_file, Some(1_791_460_800 - 10 * 3600));
        assert!(r.repo_present && r.files_complete && r.files_note.is_none());
        let t = parse_reply(&Reply { find_rc: "124", up: None, ..Default::default() }.render()).unwrap();
        assert_eq!((t.files_complete, t.files_note.as_deref(), t.uptime_secs), (false, Some("the file scan timed out after 20s"), None));
        let full = Reply::default().render();
        let cut = full.replace("ARENA_IDLE_END\n", "");
        assert_eq!(parse_reply(&cut).unwrap_err(), "the probe's reply was cut short");
        assert_eq!(parse_reply("Permission denied (publickey).\n").unwrap_err(), "no reply from the probe");
        let nofiles = full.lines().filter(|l| !l.starts_with("ARENA_IDLE_FILES")).collect::<Vec<_>>().join("\n");
        assert_eq!(parse_reply(&nofiles).unwrap_err(), "the probe reported no file scan");
        // Every socket line read, unless the probe says the list was cut.
        assert!(r.connections_complete);
        let cut = parse_reply(&Reply { tcp_truncated: true, ..Default::default() }.render()).unwrap();
        assert!(!cut.connections_complete && cut.self_found, "{cut:?}");
    }

    /// Review finding: the socket list is capped, and a cut used to be silent — on a pod with
    /// thousands of sockets a participant's session could fall past the cap while ours was
    /// still read, so the pod looked idle. The cap now says so, and the counts are then a
    /// lower bound: "in use" if they already show a session, else unknown — never idle.
    #[test]
    fn a_cut_socket_list_is_never_idle() {
        let gpu_pod = pod("devtest-apple", "runpod", Some(0.25));
        let read = |r: Reply| Probe::Read(parse_reply(&r.render()).unwrap());
        assert_eq!(
            judge(&gpu_pod, &read(Reply { tcp_truncated: true, ..Default::default() }), H6),
            Verdict::Unknown { reasons: vec![format!("over {TCP_LINE_CAP} sockets — not every connection was read")] }
        );
        // What was read already shows a session: in use, which says more than "unknown".
        assert_eq!(
            judge(&gpu_pod, &read(Reply { tcp_truncated: true, sessions: vec![[198, 51, 100, 7]], ..Default::default() }), H6),
            Verdict::NotIdle { reasons: vec!["1 SSH session".into()] }
        );
        // The table marks both counts as uncertain.
        let list = list();
        let naming = Naming { prefix: "devtest", list: &list };
        let rep = report(&[gpu_pod], &[read(Reply { tcp_truncated: true, ..Default::default() })], 6.0, &naming);
        assert_eq!(cells(&rep.pods[0])[3..5], ["0?", "0?"]);
        assert!(rep.candidates.is_empty() && rep.unknown == 1, "{rep:?}");
        let text = render(&rep);
        assert!(text.contains("No cohort machine is idle") && !text.contains("arena pods terminate"), "{text}");
    }

    /// The cap filter itself, under a real `sh`/`awk`: up to the cap every line passes and
    /// nothing is added; past it, exactly the cap's worth, then the marker.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_socket_cap_marks_a_cut() {
        use std::process::Command;
        let run = |n: usize| {
            let out = Command::new("sh").arg("-c").arg(format!("seq 1 {n} | {}", tcp_cap_filter())).output().unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap().lines().map(String::from).collect::<Vec<_>>()
        };
        for n in [0, 5, TCP_LINE_CAP] {
            let got = run(n);
            assert_eq!(got.len(), n, "{n}");
            assert!(!got.iter().any(|l| l == "ARENA_IDLE_TCP_TRUNCATED"), "{n}");
        }
        let got = run(TCP_LINE_CAP + 1);
        assert_eq!(got.len(), TCP_LINE_CAP + 1);
        assert_eq!(got[TCP_LINE_CAP - 1], TCP_LINE_CAP.to_string());
        assert_eq!(got.last().map(String::as_str), Some("ARENA_IDLE_TCP_TRUNCATED"));
        assert_eq!(run(3 * TCP_LINE_CAP).len(), TCP_LINE_CAP + 1);
        // And the probe uses it (no silent `head` left).
        assert!(script().contains(&tcp_cap_filter()) && !script().contains("head -n 2000"));
    }

    /// The verdict table: (case, reply, pod, verdict).
    #[test]
    fn only_a_pod_idle_by_every_measure_is_a_candidate() {
        let gpu_pod = pod("devtest-apple", "runpod", Some(0.25));
        let cpu_pod = pod("devtest-hz", "hetzner", Some(0.006));
        let read = |r: Reply| Probe::Read(parse_reply(&r.render()).unwrap());
        let busy = |rs: &[&str]| Verdict::NotIdle { reasons: rs.iter().map(|s| s.to_string()).collect() };
        let unknown = |rs: &[&str]| Verdict::Unknown { reasons: rs.iter().map(|s| s.to_string()).collect() };
        let cases: Vec<(&str, Probe, &Pod, Verdict)> = vec![
            ("idle: newest file 9h, up 3d", read(Reply::default()), &gpu_pod, Verdict::Idle { idle_secs: 9 * 3600 }),
            ("idle is capped by uptime", read(Reply { up: Some(7 * 3600), newest_age: Some(30 * 86_400), ..Default::default() }), &gpu_pod, Verdict::Idle { idle_secs: 7 * 3600 }),
            ("no files at all: idle since boot", read(Reply { newest_age: None, repo_age: None, ..Default::default() }), &gpu_pod, Verdict::Idle { idle_secs: 3 * 86_400 }),
            ("a VS Code session", read(Reply { sessions: vec![[198, 51, 100, 7]], ..Default::default() }), &gpu_pod, busy(&["1 SSH session"])),
            ("a Jupyter tab via the provider proxy", read(Reply { jupyter_from: vec![[100, 64, 0, 1]], ..Default::default() }), &gpu_pod, busy(&["1 other inbound connection"])),
            ("GPU busy", read(Reply { gpu: Some(vec!["87, 20000"]), ..Default::default() }), &gpu_pod, busy(&["GPU 87%"])),
            ("GPU at the threshold is idle", read(Reply { gpu: Some(vec!["5, 300"]), ..Default::default() }), &gpu_pod, Verdict::Idle { idle_secs: 9 * 3600 }),
            ("a recent file", read(Reply { newest_age: Some(20 * 60), ..Default::default() }), &gpu_pod, busy(&["a file changed 20m ago"])),
            ("just restarted", read(Reply { up: Some(600), ..Default::default() }), &gpu_pod, busy(&["up only 10m"])),
            ("busy beats unknown", read(Reply { sessions: vec![[1, 2, 3, 4], [5, 6, 7, 8]], gpu: Some(vec!["[N/A], 3"]), ..Default::default() }), &gpu_pod, busy(&["2 SSH sessions"])),
            ("no nvidia-smi on a GPU pod", read(Reply { gpu: None, ..Default::default() }), &gpu_pod, unknown(&["no nvidia-smi on a GPU pod"])),
            ("no nvidia-smi on a CPU server is fine", read(Reply { gpu: None, ..Default::default() }), &cpu_pod, Verdict::Idle { idle_secs: 9 * 3600 }),
            ("an unfinished file scan", read(Reply { find_rc: "124", ..Default::default() }), &gpu_pod, unknown(&["the file scan timed out after 20s"])),
            ("an unfinished scan that already found a change", read(Reply { find_rc: "124", newest_age: Some(60), ..Default::default() }), &gpu_pod, busy(&["a file changed 1m ago"])),
            ("uptime unknown", read(Reply { up: None, ..Default::default() }), &gpu_pod, unknown(&["uptime unknown"])),
            ("unreachable", Probe::Failed("timed out after 1m".into()), &gpu_pod, unknown(&["timed out after 1m"])),
            ("no endpoint", Probe::NoEndpoint, &gpu_pod, unknown(&["no SSH endpoint yet"])),
            ("stopped", Probe::NotBilling, &gpu_pod, Verdict::NotRunning),
        ];
        for (case, probe, p, want) in cases {
            assert_eq!(judge(p, &probe, H6), want, "{case}");
        }
        // Ours not found: unknown, whatever else.
        let mut r = parse_reply(&Reply::default().render()).unwrap();
        r.self_found = false;
        assert!(matches!(judge(&gpu_pod, &Probe::Read(r), H6), Verdict::Unknown { reasons } if reasons[0].starts_with("couldn't tell this probe's own")));
    }

    #[test]
    fn the_report_suggests_commands_and_never_more() {
        let pods = vec![
            pod("devtest-apple", "runpod", Some(0.25)),
            pod("devtest-bloom", "runpod", Some(0.40)),
            pod("devtest-cloud", "vast", Some(0.30)),
            Pod { status: "EXITED".into(), ..pod("devtest-dune", "runpod", Some(0.20)) },
            Pod { id: "v-77".into(), ..pod("devtest-apple", "vast", Some(0.10)) }, // a second apple: by id
            pod("devtest-boss", "runpod", Some(0.50)), // a staff box
            pod("otherco-x", "runpod", Some(0.60)),    // another prefix on the account
        ];
        let read = |r: Reply| Probe::Read(parse_reply(&r.render()).unwrap());
        let probes = vec![
            read(Reply { gpu: Some(vec!["0, 12288"]), ..Default::default() }),
            read(Reply { sessions: vec![[198, 51, 100, 7]], ..Default::default() }),
            Probe::Failed("exit 255: Connection refused".into()),
            Probe::NotBilling,
            read(Reply::default()),
            read(Reply::default()),
            read(Reply::default()),
        ];
        let list = list();
        let naming = Naming { prefix: "devtest", list: &list };
        let r = report(&pods, &probes, 6.0, &naming);
        assert_eq!(r.candidates, ["devtest-apple", "devtest-apple"]);
        // Idle too, but not this cohort's: named, never given a command, not in the saving.
        assert_eq!(r.idle_not_cohort, ["devtest-boss", "otherco-x"]);
        assert!(r.pods[5].commands.is_empty() && r.pods[6].commands.is_empty());
        assert_eq!(r.unknown, 1);
        assert_eq!(r.pods[0].commands, ["arena pods backup id-devtest-apple", "arena pods terminate id-devtest-apple"]);
        assert!(r.pods[1].commands.is_empty() && r.pods[2].commands.is_empty() && r.pods[3].commands.is_empty());
        assert!((r.saving.usd_per_hr - 0.35).abs() < 1e-9, "{:?}", r.saving);
        let text = render(&r);
        for needle in [
            "NAME           PROVIDER    $/H  SSH  OTHER  GPU%  GPU MEM  NEWEST FILE  REPO  UP  VERDICT",
            "devtest-apple  runpod    $0.25    0      0    0%    12.0G           9h   10h  3d  IDLE 9h",
            "devtest-bloom  runpod    $0.40    1      0    0%     0.0G           9h   10h  3d  in use: 1 SSH session",
            "devtest-cloud  vast      $0.30    ?      ?     ?        ?            ?     ?   ?  ? exit 255: Connection refused",
            "devtest-dune   runpod        -    -      -     -        -            -     -   -  - not billing (EXITED)",
            "devtest-apple  vast      $0.10    0      0    0%     0.0G           9h   10h  3d  IDLE 9h",
            "Idle for ≥6h — no SSH session or other inbound connection, GPU ≤5%, no file change, up ≥6h:",
            "  devtest-apple (runpod, $0.25/h, idle 9h)\n    arena pods backup id-devtest-apple\n    arena pods terminate id-devtest-apple\n",
            "    note: 12.0 GiB of GPU memory is still allocated",
            "  2 candidate(s): $0.35/h (≈ $8.40/day) together",
            "Nothing was changed.",
            "devtest-boss   runpod    $0.50    0      0    0%     0.0G           9h   10h  3d  idle 9h — not a cohort machine, no suggestion",
            "Also idle, but not this cohort's machines (a staff `@` box or another prefix) — no command suggested: devtest-boss, otherco-x",
            "1 pod(s) couldn't be judged",
        ] {
            assert!(text.contains(needle), "`{needle}` missing:\n{text}");
        }
        assert!(!text.contains("terminate devtest-boss") && !text.contains("terminate otherco-x"), "{text}");
        // Only staff/other idle pods: no candidates section, just the note.
        let staff = render(&report(&pods[5..], &probes[5..], 6.0, &naming));
        assert!(staff.contains("No cohort machine is idle for ≥6h by every measure — nothing to suggest.") && !staff.contains("arena pods"), "{staff}");
        // A unique name is used as is.
        let solo = report(&pods[..1], &probes[..1], 6.0, &naming);
        assert_eq!(solo.pods[0].commands, ["arena pods backup devtest-apple", "arena pods terminate devtest-apple"]);
        // A longer threshold: nobody qualifies, and it says so.
        let none = render(&report(&pods, &probes, 12.0, &naming));
        assert!(none.contains("No cohort machine is idle for ≥12h by every measure — nothing to suggest."), "{none}");
        assert!(!none.contains("arena pods terminate"), "{none}");
        // JSON carries the verdicts flat, and the commands only for candidates.
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["pods"][0]["verdict"], "idle");
        assert_eq!(v["pods"][0]["idle_secs"], 9 * 3600);
        assert_eq!(v["pods"][1]["verdict"], "not_idle");
        assert_eq!(v["pods"][2]["verdict"], "unknown");
        assert_eq!(v["pods"][2]["error"], "exit 255: Connection refused");
        assert_eq!(v["pods"][3]["verdict"], "not_running");
        assert_eq!(v["pods"][0]["reading"]["gpu"]["state"], "read");
        assert_eq!((v["pods"][0]["cohort"].clone(), v["pods"][5]["cohort"].clone()), (true.into(), false.into()));
        assert_eq!(render(&report(&[], &[], 6.0, &naming)), "(no pods)\n");
    }

    /// The probe under a real `sh` (as sshd hands a command to the login shell), on a scratch
    /// home: its reply parses, the GPU stub's samples are read, and the file scan sees the
    /// notebook but skips `.git`, caches and the editor server — which were touched later.
    /// Nothing outside the scratch dir is read for files; skipped where a tool is missing.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_probe_runs_under_a_real_sh_and_its_reply_parses() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        let have = |t: &str| Command::new("sh").arg("-c").arg(format!("command -v {t}")).output().is_ok_and(|o| o.status.success());
        if !["awk", "find", "touch", "date", "getconf", "sed", "head"].iter().all(|t| have(t)) {
            eprintln!("a tool the probe needs is missing — skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("arena-idle-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (home, bin) = (dir.join("home"), dir.join("bin"));
        let repo = home.join("ARENA_materials");
        for d in [&bin, &repo.join(".git"), &home.join(".cache"), &home.join(".vscode-server")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let files = [
            (home.join("notes.txt"), now - 3 * 86_400),
            (repo.join("day1.ipynb"), now - 10 * 3600),
            (repo.join(".git/index"), now - 60),
            (home.join(".cache/pip.log"), now - 60),
            (home.join(".vscode-server/remote.log"), now - 60),
        ];
        for (f, t) in &files {
            std::fs::write(f, "x").unwrap();
            let ok = Command::new("touch").arg("-d").arg(format!("@{t}")).arg(f).status().is_ok_and(|s| s.success());
            if !ok {
                eprintln!("touch -d @epoch unsupported — skipping");
                return;
            }
        }
        let smi = bin.join("nvidia-smi");
        std::fs::write(&smi, "#!/bin/sh\necho '7, 512'\n").unwrap();
        std::fs::set_permissions(&smi, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let out = Command::new("sh")
            .arg("-c")
            .arg(probe_command(repo.to_str().unwrap()))
            .env("HOME", &home)
            .env("PATH", path)
            .env_remove("SSH_CONNECTION")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let r = parse_reply(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}\n{}", String::from_utf8_lossy(&out.stderr)));
        assert!(r.now.abs_diff(now) < 60, "{r:?}");
        assert!(r.uptime_secs.is_some(), "PID 1's age: {stdout}");
        assert_eq!(r.gpu, Gpu::Read { gpus: 1, max_util: 7, mem_used_mib: 512 });
        assert!(r.files_complete && r.repo_present, "{r:?}");
        assert_eq!(r.newest_file, Some(now - 10 * 3600), "the notebook, not .git/.cache/.vscode-server: {stdout}");
        assert_eq!(r.newest_repo_file, Some(now - 10 * 3600));
        assert!(!r.self_found, "no $SSH_CONNECTION: our own connection can't be told apart");
    }

    #[test]
    fn the_probe_command_carries_the_repo_as_an_argument() {
        let c = probe_command("/root/ARENA materials/");
        assert!(c.starts_with("sh -c '"), "{c}");
        assert!(c.ends_with(" arena-idle '/root/ARENA materials'"), "{c}");
        let q = probe_command("/root/it's");
        assert!(q.ends_with(" arena-idle '/root/it'\\''s'"), "{q}");
        assert!(probe_command("/").ends_with(" arena-idle '/'"));
        // Read-only by construction: nothing in the script writes, kills or changes anything.
        let s = script();
        for word in [" rm ", "kill", " mv ", "tee ", "touch", "mkdir", ">>", "nvidia-smi -r", "shutdown", "reboot"] {
            assert!(!s.contains(word), "`{word}` in the probe:\n{s}");
        }
        // Its only redirections are to /dev/null (stderr/stdout of probes) or `2>&1`.
        for (i, _) in s.match_indices(">/") {
            assert!(s[i..].starts_with(">/dev/null"), "a write in the probe: {}", &s[i..(i + 30).min(s.len())]);
        }
        assert!(s.contains("-prune") && s.contains("-name .git") && s.contains("-name .vscode-server"));
    }
}
