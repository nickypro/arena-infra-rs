//! The reaper's read-only look at a pod: [`probe_command`] ships [`PROBE_SCRIPT`]
//! (`idle_probe.sh`) in one `Remote::exec`, and [`parse_probe`] reads the reply — strictly.
//!
//! **A reply that can't be parsed never counts as idle.** The reply is framed by
//! `ARENA_PROBE_BEGIN/END <nonce>` (anything outside — a `.bashrc` banner — is ignored), every
//! line inside must be a known `ARENA_PROBE key=value`, scalar keys appear once, repeated keys
//! carry an index `1..n` and a `<key>_n` count, and a POSIX `cksum` of every fact line closes
//! it. So a lost, doubled, cut or edited line, a missing END, another nonce or a reply over
//! [`super::PROBE_OUTPUT_CAP`] is [`ProbeError`] — `Unknown` downstream, never `Idle`.
//!
//! Only facts leave this module: counts, flags and numbers. Raw pod text (process titles,
//! pane commands) is matched here and dropped.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::SocketAddr;

use super::{PROBE_OUTPUT_CAP, SSHD_LINE_CAP, TCP_LINE_CAP};

/// The probe (`idle_probe.sh`), embedded.
pub const PROBE_SCRIPT: &str = include_str!("idle_probe.sh");

/// Where the file scan starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    /// From this pod-clock time (the previous observation's end, less a margin; the backup's start).
    At(u64),
    /// From this many seconds before the probe starts (a first look; a whole idle window).
    Back(u64),
}

impl Since {
    fn arg(self) -> String {
        match self {
            Since::At(t) => format!("@{t}"),
            Since::Back(s) => format!("-{s}"),
        }
    }
}

/// Single-quote for `sh`.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The remote command for one probe. The script goes base64 (no quoting of its own), under
/// `env -i` with a fixed `PATH` (no login or conda wrapper, nothing from the pod's
/// environment but `HOME`), as POSIX `sh`. `$SSH_CONNECTION` is expanded by the login shell
/// *before* `env -i` — how the probe learns its own connection. The arguments are quoted, never
/// spliced into the script. `nonce` must be alphanumeric (the script refuses anything else).
pub fn probe_command(nonce: &str, since: Since, repo: &str, provider: &str) -> String {
    let b64 = crate::base64::encode(PROBE_SCRIPT.as_bytes());
    format!(
        "env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=\"$HOME\" sh -c \"$(printf %s {b64} | base64 -d)\" arena-idle-probe {} {} {} {} \"$SSH_CONNECTION\"",
        sq(nonce),
        sq(&since.arg()),
        sq(repo.trim_end_matches('/')),
        sq(provider)
    )
}

/// A fresh nonce from `seed` (the CLI mixes the time, its pid and a counter): 16 hex digits.
pub fn nonce_from(seed: u64) -> String {
    // splitmix64: a cheap, well-spread mix — the nonce only has to differ per probe.
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    format!("{:016x}", z ^ (z >> 31))
}

/// Why a reply isn't usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeError {
    /// No reply from the probe: no BEGIN, or no END (cut short, killed, never ran).
    Failed,
    /// A reply that doesn't parse: see the module doc.
    Garbled,
}

/// One cumulative counter read at the window's start and end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counter {
    pub start: Option<u64>,
    pub end: Option<u64>,
}

/// The GPU, as the probe saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuFacts {
    /// No `nvidia-smi` on the machine.
    Absent,
    /// All 5 samples answered: GPUs, the busiest GPU's busiest sample, the most memory held
    /// (summed over the GPUs) in any sample.
    Read { gpus: u32, max_util: u32, max_mem_mib: u64 },
    /// `nvidia-smi` failed, hung, listed no GPU or said something unexpected.
    Unreadable,
}

/// `/workspace` on the pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workspace {
    Absent,
    /// There, and nothing on it (a mounted volume, empty) / nothing written since the
    /// container started (a plain directory).
    Clean,
    /// Data there — not in the backup's scope.
    HasData,
    Err,
}

/// Where the repo (`BACKUP_REPO_PATH`) resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoSite {
    UnderHome,
    OutsideHome,
    Absent,
    Err,
}

/// The file scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Files {
    /// A file changed since the scan's start.
    Hit,
    /// None did (the scan finished).
    NoneChanged,
    Timeout,
    Err,
}

/// The sockets, judged (spec §6.1 `tcp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sockets {
    /// ESTABLISHED to a LISTEN port other than sshd's (22, or the port we came in on).
    pub inbound: u32,
    /// ESTABLISHED from a non-listening local port to a non-loopback peer.
    pub outbound: u32,
    /// More socket lines than [`TCP_LINE_CAP`]: some weren't read.
    pub overflow: bool,
}

/// Everything one probe established — facts only, no pod text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFacts {
    /// The id read on the pod (RunPod `RUNPOD_POD_ID`, Hetzner's instance id); may be empty.
    pub id: String,
    pub boot_id: Option<String>,
    pub pid1_start: Option<String>,
    /// Pod clock: the window's start and end, and the probe's end.
    pub t0: u64,
    pub t1: u64,
    pub now: u64,
    /// Pod clock the file scan covered from.
    pub scan_from: u64,
    pub cpu: Counter,
    pub net: Counter,
    pub gpu: GpuFacts,
    /// GPU compute processes listed (`None` = no nvidia-smi, or the query failed).
    pub gpu_procs: Option<u32>,
    pub files: Files,
    /// `None` = the socket tables couldn't be read.
    pub sockets: Option<Sockets>,
    /// Authenticated SSH sessions other than the probe's own; `None` = can't tell (no sshd
    /// process of ours seen, or too many sshd processes).
    pub sessions: Option<u32>,
    /// `who`'s logins: `Some(0)` also when `who` isn't installed; `None` = it failed.
    pub who: Option<u32>,
    /// tmux panes running something other than a shell; `None` = tmux failed.
    pub tmux_busy: Option<u32>,
    /// screen sessions; `None` = unreadable.
    pub screen: Option<u32>,
    pub keep: bool,
    pub workspace: Workspace,
    pub repo: RepoSite,
    /// Mounts strictly under the home; `None` = unknown.
    pub home_mounts: Option<u32>,
}

impl ProbeFacts {
    /// `<boot_id>/<pid1 start>`: the container (or VM boot) — `None` when either is unknown.
    pub fn container(&self) -> Option<String> {
        match (&self.boot_id, &self.pid1_start) {
            (Some(b), Some(p)) => Some(format!("{b}/{p}")),
            _ => None,
        }
    }

    /// The window's length (pod clock, seconds).
    pub fn window(&self) -> u64 {
        self.t1.saturating_sub(self.t0)
    }
}

/// The shells a tmux pane may run without counting as busy.
const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "fish", "-bash", "-zsh", "-sh"];

/// Scalar keys, each exactly once (`required`) or at most once.
const SCALARS: &[(&str, bool)] = &[
    ("v", true),
    ("t0", true),
    ("t1", true),
    ("cpu0", true),
    ("cpu1", true),
    ("net0", true),
    ("net1", true),
    ("gpu_smi", true),
    ("gpu_procs", false),
    ("gpu_rc_n", true),
    ("gpu_n", true),
    ("files", true),
    ("scan_from", true),
    ("tcp_read", true),
    ("tcp_overflow", false),
    ("tcp_n", true),
    ("anc_n", true),
    ("sshd_overflow", false),
    ("sshd_n", true),
    ("self", true),
    ("who", true),
    ("tmux", true),
    ("tmux_panes", false),
    ("pane_n", true),
    ("screen", true),
    ("keep", true),
    ("workspace", true),
    ("repo", true),
    ("home_mounts", true),
    ("id", true),
    ("boot_id", true),
    ("pid1_start", true),
    ("now", true),
];

/// Repeated keys (`<key>=<index> <value>`), each with a `<key>_n` count.
const REPEATED: &[&str] = &["gpu_rc", "gpu", "tcp", "anc", "sshd", "pane"];

/// POSIX `cksum` (CRC-32, polynomial 0x04C11DB7, the length appended) — what the probe closes
/// its reply with.
pub fn cksum(data: &[u8]) -> u32 {
    fn step(crc: u32, byte: u8) -> u32 {
        let mut c = crc ^ (u32::from(byte) << 24);
        for _ in 0..8 {
            c = if c & 0x8000_0000 != 0 { (c << 1) ^ 0x04C1_1DB7 } else { c << 1 };
        }
        c
    }
    let mut crc = data.iter().fold(0u32, |c, b| step(c, *b));
    let mut n = data.len() as u64;
    while n != 0 {
        crc = step(crc, (n & 0xff) as u8);
        n >>= 8;
    }
    !crc
}

fn digits(v: &str) -> Option<u64> {
    (!v.is_empty() && v.len() <= 20 && v.bytes().all(|b| b.is_ascii_digit())).then(|| v.parse().ok()).flatten()
}

/// Parse a probe reply for the probe sent with `nonce` and `since`. See the module doc for
/// what makes it [`ProbeError`].
pub fn parse_probe(stdout: &str, nonce: &str, since: Since) -> Result<ProbeFacts, ProbeError> {
    use ProbeError::{Failed, Garbled};
    if stdout.len() > PROBE_OUTPUT_CAP {
        return Err(Garbled);
    }
    let begin = format!("ARENA_PROBE_BEGIN {nonce}");
    let end = format!("ARENA_PROBE_END {nonce}");
    let lines: Vec<&str> = stdout.split('\n').collect();
    let begins: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.starts_with("ARENA_PROBE_BEGIN")).map(|(i, _)| i).collect();
    let ends: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.starts_with("ARENA_PROBE_END")).map(|(i, _)| i).collect();
    if begins.is_empty() || ends.is_empty() {
        return Err(Failed);
    }
    // Exactly one of each, with our nonce, in order: another nonce or a second frame is
    // garbled, not "a reply".
    let ([b], [e]) = (begins.as_slice(), ends.as_slice()) else { return Err(Garbled) };
    if lines[*b] != begin || lines[*e] != end || e <= b {
        return Err(Garbled);
    }
    let body = &lines[b + 1..*e];
    let Some((sum_line, facts)) = body.split_last() else { return Err(Garbled) };
    // The checksum over every fact line, as the script wrote them.
    let mut bytes = Vec::new();
    for l in facts {
        bytes.extend_from_slice(l.as_bytes());
        bytes.push(b'\n');
    }
    let want = format!("ARENA_PROBE sum={} {}", cksum(&bytes), bytes.len());
    if *sum_line != want {
        return Err(Garbled);
    }
    let mut scalars: BTreeMap<&str, &str> = BTreeMap::new();
    let mut repeated: BTreeMap<&str, Vec<(u64, &str)>> = BTreeMap::new();
    for l in facts {
        let rest = l.strip_prefix("ARENA_PROBE ").ok_or(Garbled)?;
        let (k, v) = rest.split_once('=').ok_or(Garbled)?;
        if v.chars().any(|c| c.is_control()) {
            return Err(Garbled);
        }
        if REPEATED.contains(&k) {
            let (i, val) = v.split_once(' ').unwrap_or((v, ""));
            repeated.entry(k).or_default().push((digits(i).ok_or(Garbled)?, val));
        } else if SCALARS.iter().any(|(s, _)| *s == k) {
            if scalars.insert(k, v).is_some() {
                return Err(Garbled);
            }
        } else {
            return Err(Garbled);
        }
    }
    if SCALARS.iter().any(|(k, required)| *required && !scalars.contains_key(k)) {
        return Err(Garbled);
    }
    // Each repeated key: indices exactly 1..=n, n its count line.
    for k in REPEATED {
        let n = digits(scalars.get(format!("{k}_n").as_str()).copied().ok_or(Garbled)?).ok_or(Garbled)?;
        let got = repeated.get(k).map_or(&[][..], Vec::as_slice);
        if got.len() as u64 != n || got.iter().enumerate().any(|(i, (idx, _))| *idx != i as u64 + 1) {
            return Err(Garbled);
        }
    }
    let s = |k: &str| scalars.get(k).copied().unwrap_or("");
    let rep = |k: &str| repeated.get(k).map(|v| v.iter().map(|(_, val)| *val).collect::<Vec<&str>>()).unwrap_or_default();
    if s("v") != "1" {
        return Err(Garbled);
    }
    let t0 = digits(s("t0")).ok_or(Garbled)?;
    let t1 = digits(s("t1")).ok_or(Garbled)?;
    let now = digits(s("now")).ok_or(Garbled)?;
    let scan_from = digits(s("scan_from")).ok_or(Garbled)?;
    if t1 < t0 || now < t1 {
        return Err(Garbled);
    }
    // The scan started where it was asked to: a reply about another window is not this one.
    let asked = match since {
        Since::At(t) => t,
        Since::Back(b) => t0.checked_sub(b).ok_or(Garbled)?,
    };
    if scan_from != asked {
        return Err(Garbled);
    }
    fn cpu_sample(v: &str) -> Result<(Option<&str>, Option<u64>), ProbeError> {
        match v.split_once(' ') {
            Some(("-", "-")) => Ok((None, None)),
            Some((src @ ("cg2" | "cg1" | "proc"), n)) => Ok((Some(src), Some(digits(n).ok_or(ProbeError::Garbled)?))),
            _ => Err(ProbeError::Garbled),
        }
    }
    let (src0, c0) = cpu_sample(s("cpu0"))?;
    let (src1, c1) = cpu_sample(s("cpu1"))?;
    // Two samples from different sources aren't one counter.
    let cpu = if src0.is_some() && src0 == src1 { Counter { start: c0, end: c1 } } else { Counter::default() };
    let net_sample = |v: &str| -> Result<Option<u64>, ProbeError> {
        match v {
            "-" => Ok(None),
            n => Ok(Some(digits(n).ok_or(Garbled)?)),
        }
    };
    let net = Counter { start: net_sample(s("net0"))?, end: net_sample(s("net1"))? };
    let (gpu, gpu_procs) = match s("gpu_smi") {
        "absent" => {
            if scalars.contains_key("gpu_procs") || !rep("gpu_rc").is_empty() || !rep("gpu").is_empty() {
                return Err(Garbled);
            }
            (GpuFacts::Absent, None)
        }
        "present" => {
            let procs = match scalars.get("gpu_procs").copied().ok_or(Garbled)? {
                "-" => None,
                n => Some(digits(n).ok_or(Garbled)? as u32),
            };
            (judge_gpu(&rep("gpu_rc"), &rep("gpu"))?, procs)
        }
        _ => return Err(Garbled),
    };
    let files = match s("files") {
        "hit" => Files::Hit,
        "none" => Files::NoneChanged,
        "timeout" => Files::Timeout,
        "err" => Files::Err,
        _ => return Err(Garbled),
    };
    // Our own connection (`$SSH_CONNECTION`): always there over SSH, so required.
    let self_port = Some(parse_self_port(s("self")).ok_or(Garbled)?);
    let sockets = match s("tcp_read") {
        "ok" => {
            let overflow = match scalars.get("tcp_overflow").copied().ok_or(Garbled)? {
                "0" => false,
                "1" => true,
                _ => return Err(Garbled),
            };
            let tcp = rep("tcp");
            if tcp.len() > TCP_LINE_CAP {
                return Err(Garbled);
            }
            Some(judge_sockets(&tcp, self_port, overflow)?)
        }
        "err" => {
            if scalars.contains_key("tcp_overflow") || !rep("tcp").is_empty() {
                return Err(Garbled);
            }
            None
        }
        _ => return Err(Garbled),
    };
    let anc: HashSet<u64> = rep("anc").iter().map(|a| digits(a).ok_or(Garbled)).collect::<Result<_, _>>()?;
    let sshd_overflow = match scalars.get("sshd_overflow").copied() {
        None => false,
        Some("1") => true,
        Some(_) => return Err(Garbled),
    };
    let sshd = rep("sshd");
    if sshd.len() > SSHD_LINE_CAP {
        return Err(Garbled);
    }
    let sessions = judge_sessions(&sshd, &anc, sshd_overflow)?;
    let who = match s("who") {
        "absent" => Some(0),
        "-" => None,
        n => Some(digits(n).ok_or(Garbled)? as u32),
    };
    let tmux_busy = match s("tmux") {
        "absent" => {
            if scalars.contains_key("tmux_panes") || !rep("pane").is_empty() {
                return Err(Garbled);
            }
            Some(0)
        }
        "err" => None,
        "ok" => {
            let total = digits(scalars.get("tmux_panes").copied().ok_or(Garbled)?).ok_or(Garbled)?;
            let panes = rep("pane");
            // The script sends at most 50 panes and the total.
            if panes.len() as u64 != total.min(50) {
                return Err(Garbled);
            }
            if panes.iter().any(|p| p.is_empty() || p.len() > 40) {
                return Err(Garbled);
            }
            // Past the 50 read, the rest are unknown commands: busy.
            let busy = panes.iter().filter(|p| !SHELLS.contains(p)).count() as u64 + total.saturating_sub(panes.len() as u64);
            Some(busy as u32)
        }
        _ => return Err(Garbled),
    };
    let screen = match s("screen") {
        "absent" => Some(0),
        "err" => None,
        n => Some(digits(n).ok_or(Garbled)? as u32),
    };
    let keep = match s("keep") {
        "0" => false,
        "1" => true,
        _ => return Err(Garbled),
    };
    let workspace = match s("workspace") {
        "absent" => Workspace::Absent,
        "clean" => Workspace::Clean,
        "has_data" => Workspace::HasData,
        "err" => Workspace::Err,
        _ => return Err(Garbled),
    };
    let repo = match s("repo") {
        "under_home" => RepoSite::UnderHome,
        "outside_home" => RepoSite::OutsideHome,
        "absent" => RepoSite::Absent,
        "err" => RepoSite::Err,
        _ => return Err(Garbled),
    };
    let home_mounts = match s("home_mounts") {
        "-" => None,
        n => Some(digits(n).ok_or(Garbled)? as u32),
    };
    let id = s("id");
    if id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) {
        return Err(Garbled);
    }
    let boot_id = s("boot_id");
    if boot_id.len() > 64 || !boot_id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(Garbled);
    }
    let pid1 = s("pid1_start");
    if !pid1.is_empty() && digits(pid1).is_none() {
        return Err(Garbled);
    }
    Ok(ProbeFacts {
        id: id.to_string(),
        boot_id: (!boot_id.is_empty()).then(|| boot_id.to_string()),
        pid1_start: (!pid1.is_empty()).then(|| pid1.to_string()),
        t0,
        t1,
        now,
        scan_from,
        cpu,
        net,
        gpu,
        gpu_procs,
        files,
        sockets,
        sessions,
        who,
        tmux_busy,
        screen,
        keep,
        workspace,
        repo,
        home_mounts,
    })
}

/// The 5 GPU samples: each `gpu_rc` is `<sample> <exit>`, each `gpu` `<sample> <util>, <mem>`.
/// Read only when all 5 answered 0 with the same, non-zero number of GPUs; anything else is
/// unreadable. A line that isn't the shape is garbled.
fn judge_gpu(rcs: &[&str], lines: &[&str]) -> Result<GpuFacts, ProbeError> {
    use ProbeError::Garbled;
    let mut rc_of: BTreeMap<u64, u64> = BTreeMap::new();
    for r in rcs {
        let (k, rc) = r.split_once(' ').ok_or(Garbled)?;
        let k = digits(k).filter(|k| (1..=5).contains(k)).ok_or(Garbled)?;
        if rc_of.insert(k, digits(rc).ok_or(Garbled)?).is_some() {
            return Err(Garbled);
        }
    }
    // Samples in order, none skipped: 1, 2, … (a failed one ends them).
    if rc_of.keys().copied().ne(1..=rc_of.len() as u64) {
        return Err(Garbled);
    }
    let mut per: BTreeMap<u64, Vec<(Option<u32>, Option<u64>)>> = BTreeMap::new();
    for l in lines {
        let (k, rest) = l.split_once(' ').ok_or(Garbled)?;
        let k = digits(k).filter(|k| rc_of.contains_key(k)).ok_or(Garbled)?;
        let (u, m) = rest.split_once(',').ok_or(Garbled)?;
        // `[N/A]` (a MIG slice) arrives as `?N/A?`: unreadable, not garbled.
        per.entry(k).or_default().push((digits(u.trim()).map(|v| v as u32), digits(m.trim())));
    }
    let complete = rc_of.len() == 5 && rc_of.values().all(|rc| *rc == 0);
    if !complete {
        return Ok(GpuFacts::Unreadable);
    }
    let counts: BTreeSet<usize> = (1..=5).map(|k| per.get(&k).map_or(0, Vec::len)).collect();
    let gpus = match counts.iter().collect::<Vec<_>>().as_slice() {
        [n] if **n > 0 => **n as u32,
        _ => return Ok(GpuFacts::Unreadable),
    };
    let (mut max_util, mut max_mem) = (0u32, 0u64);
    for samples in per.values() {
        let mut mem = 0u64;
        for (u, m) in samples {
            let (Some(u), Some(m)) = (u, m) else { return Ok(GpuFacts::Unreadable) };
            if *u > 100 {
                return Ok(GpuFacts::Unreadable);
            }
            max_util = max_util.max(*u);
            mem = mem.saturating_add(*m);
        }
        max_mem = max_mem.max(mem);
    }
    Ok(GpuFacts::Read { gpus, max_util, max_mem_mib: max_mem })
}

/// The port sshd took this probe's connection on, from `$SSH_CONNECTION`
/// (`client_ip client_port server_ip server_port`); `None` if it isn't that.
fn parse_self_port(s: &str) -> Option<u16> {
    let f: Vec<&str> = s.split_whitespace().collect();
    match f.as_slice() {
        [_, cport, _, sport] if cport.parse::<u16>().is_ok() => sport.parse().ok(),
        _ => None,
    }
}

/// Judge the socket lines (`<state> <local> <remote>`, `/proc/net/tcp` hex): see [`Sockets`].
/// sshd's port is 22 plus the one this probe came in on — sessions there are counted from the
/// sshd processes instead (so ours can be left out exactly).
fn judge_sockets(lines: &[&str], self_port: Option<u16>, overflow: bool) -> Result<Sockets, ProbeError> {
    use ProbeError::Garbled;
    let decode = |a: &str| crate::idle::decode_proc_addr(a).ok_or(Garbled);
    let mut listen: HashSet<u16> = HashSet::new();
    let mut est: Vec<(SocketAddr, SocketAddr)> = Vec::new();
    for l in lines {
        let f: Vec<&str> = l.split(' ').collect();
        let [st, local, remote] = f.as_slice() else { return Err(Garbled) };
        let (local, remote) = (decode(local)?, decode(remote)?);
        match *st {
            "0A" => {
                listen.insert(local.port());
            }
            "01" => est.push((local, remote)),
            _ => return Err(Garbled),
        }
    }
    let mut ssh_ports: HashSet<u16> = HashSet::from([22]);
    ssh_ports.extend(self_port);
    let mut out = Sockets { overflow, ..Default::default() };
    for (local, remote) in est {
        if listen.contains(&local.port()) {
            if !ssh_ports.contains(&local.port()) {
                out.inbound += 1;
            }
        } else if !remote.ip().is_loopback() && !remote.ip().is_unspecified() {
            out.outbound += 1;
        }
    }
    Ok(out)
}

/// Whether an sshd process title is an authenticated connection's: `sshd: <user>` or
/// `sshd: <user>@<ttys>` (also `sshd-session:` — OpenSSH ≥ 9.8). OpenSSH titles the
/// post-authentication child `<user>` (a connection with no session open: a port-forward, a
/// multiplexing master) and `<user>@pts/N` / `<user>@notty` while sessions run. The monitor
/// (`[priv]`), pre-auth children (`[net]`, `[preauth]`, `[accepted]`) and the listener carry
/// brackets or a path, so internet scanners never count.
pub fn is_session_title(title: &str) -> bool {
    let t = title.trim_end();
    let Some(rest) = t.strip_prefix("sshd: ").or_else(|| t.strip_prefix("sshd-session: ")) else { return false };
    let (user, ttys) = match rest.split_once('@') {
        Some((u, ttys)) => (u, Some(ttys)),
        None => (rest, None),
    };
    let user_ok = !user.is_empty()
        && user.len() <= 64
        && user.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'$'))
        && !user.starts_with('-');
    let ttys_ok = ttys.is_none_or(|t| {
        !t.is_empty() && t.split(',').all(|tty| tty == "notty" || tty.strip_prefix("pts/").is_some_and(|n| digits(n).is_some()))
    });
    user_ok && ttys_ok
}

/// Count the sessions among the sshd processes (`<pid> <title>`), leaving out the probe's own
/// (its ancestors). `None` — can't tell — when no ancestor of the probe is an sshd process
/// (the process list isn't what we think it is, or we came in some other way), or when there
/// were more sshd processes than the probe reads. A line that isn't `<pid> sshd…` is garbled.
fn judge_sessions(lines: &[&str], anc: &HashSet<u64>, overflow: bool) -> Result<Option<u32>, ProbeError> {
    use ProbeError::Garbled;
    let mut ours_seen = false;
    let mut sessions = 0u32;
    for l in lines {
        let (pid, title) = l.split_once(' ').ok_or(Garbled)?;
        let pid = digits(pid).ok_or(Garbled)?;
        if !title.starts_with("sshd") {
            return Err(Garbled);
        }
        if anc.contains(&pid) {
            ours_seen = true;
        } else if is_session_title(title) {
            sessions += 1;
        }
    }
    Ok((ours_seen && !overflow).then_some(sessions))
}

/// Test fixtures: a probe reply built from facts, framed and checksummed like the script's
/// (shared with the CLI's tests).
#[cfg(any(test, feature = "test-util"))]
pub mod fixture {
    use super::*;

    /// What a fixture pod reports. `Default` is an idle RunPod pod.
    #[derive(Debug, Clone)]
    pub struct Reply {
        pub nonce: String,
        pub since: Since,
        pub id: String,
        pub boot_id: String,
        pub pid1_start: String,
        pub t0: u64,
        pub window: u64,
        /// Probe end − window end.
        pub tail: u64,
        pub cpu: (Option<u64>, Option<u64>),
        pub cpu_src: &'static str,
        pub net: (Option<u64>, Option<u64>),
        /// `None` = no nvidia-smi; else per sample `(rc, lines)`.
        pub gpu: Option<Vec<(u64, Vec<String>)>>,
        pub gpu_procs: Option<u32>,
        pub files: &'static str,
        /// `None` = `tcp_read=err`.
        pub tcp: Option<Vec<String>>,
        pub tcp_overflow: bool,
        pub anc: Vec<u64>,
        pub sshd: Vec<(u64, String)>,
        pub sshd_overflow: bool,
        pub self_conn: String,
        pub who: String,
        pub tmux: &'static str,
        pub panes: Vec<String>,
        pub screen: String,
        pub keep: bool,
        pub workspace: &'static str,
        pub repo: &'static str,
        pub home_mounts: String,
    }

    /// `/proc/net/tcp` hex for an IPv4 `a.b.c.d:port`.
    pub fn v4(ip: [u8; 4], port: u16) -> String {
        crate::idle::fixture::v4(ip, port)
    }

    impl Default for Reply {
        fn default() -> Self {
            let me = v4([172, 17, 0, 2], 22);
            Reply {
                nonce: "N0NCE".into(),
                since: Since::Back(900),
                id: "pod1".into(),
                boot_id: "db9bb781-8521-46cd-a74a-63c0e46d9be7".into(),
                pid1_start: "123456".into(),
                t0: 1_791_460_800,
                window: 10,
                tail: 12,
                cpu: (Some(1_000_000_000), Some(1_000_050_000)),
                cpu_src: "cg2",
                net: (Some(5_000_000), Some(5_002_000)),
                gpu: Some((0..5).map(|_| (0, vec!["0, 3".to_string()])).collect()),
                gpu_procs: Some(0),
                files: "none",
                tcp: Some(vec![
                    format!("0A {} {}", v4([0, 0, 0, 0], 22), v4([0, 0, 0, 0], 0)),
                    format!("0A {} {}", v4([0, 0, 0, 0], 8888), v4([0, 0, 0, 0], 0)),
                    format!("01 {me} {}", v4([203, 0, 113, 9], 50000)),
                ]),
                tcp_overflow: false,
                anc: vec![4242, 4100, 4099],
                sshd: vec![
                    (12, "sshd: /usr/sbin/sshd -D [listener] 0 of 10-100 startups ".into()),
                    (4099, "sshd: root [priv]    ".into()),
                    (4100, "sshd: root@notty     ".into()),
                ],
                sshd_overflow: false,
                self_conn: "203.0.113.9 50000 172.17.0.2 22".into(),
                who: "0".into(),
                tmux: "absent",
                panes: Vec::new(),
                screen: "absent".into(),
                keep: false,
                workspace: "absent",
                repo: "under_home",
                home_mounts: "0".into(),
            }
        }
    }

    impl Reply {
        /// The fact lines, in the script's order (without the frame or the sum).
        pub fn fact_lines(&self) -> Vec<String> {
            let mut out: Vec<String> = Vec::new();
            let mut o = |k: &str, v: String| out.push(format!("ARENA_PROBE {k}={v}"));
            let t1 = self.t0 + self.window;
            let opt = |v: Option<u64>| v.map_or("-".to_string(), |n| n.to_string());
            let cpu = |v: Option<u64>| v.map_or("- -".to_string(), |n| format!("{} {n}", self.cpu_src));
            o("v", "1".into());
            o("t0", self.t0.to_string());
            o("cpu0", cpu(self.cpu.0));
            o("net0", opt(self.net.0));
            o("t1", t1.to_string());
            o("cpu1", cpu(self.cpu.1));
            o("net1", opt(self.net.1));
            let (mut nrc, mut ngpu) = (0, 0);
            match &self.gpu {
                None => o("gpu_smi", "absent".into()),
                Some(samples) => {
                    o("gpu_smi", "present".into());
                    for (k, (rc, lines)) in samples.iter().enumerate() {
                        nrc += 1;
                        o("gpu_rc", format!("{nrc} {} {rc}", k + 1));
                        for l in lines {
                            ngpu += 1;
                            o("gpu", format!("{ngpu} {} {l}", k + 1));
                        }
                    }
                    o("gpu_procs", self.gpu_procs.map_or("-".to_string(), |n| n.to_string()));
                }
            }
            o("gpu_rc_n", nrc.to_string());
            o("gpu_n", ngpu.to_string());
            o("files", self.files.into());
            let scan_from = match self.since {
                Since::At(t) => t,
                Since::Back(b) => self.t0 - b,
            };
            o("scan_from", scan_from.to_string());
            match &self.tcp {
                None => {
                    o("tcp_read", "err".into());
                    o("tcp_n", "0".into());
                }
                Some(lines) => {
                    o("tcp_read", "ok".into());
                    for (i, l) in lines.iter().enumerate() {
                        o("tcp", format!("{} {l}", i + 1));
                    }
                    o("tcp_overflow", if self.tcp_overflow { "1" } else { "0" }.into());
                    o("tcp_n", lines.len().to_string());
                }
            }
            for (i, a) in self.anc.iter().enumerate() {
                o("anc", format!("{} {a}", i + 1));
            }
            o("anc_n", self.anc.len().to_string());
            for (i, (pid, t)) in self.sshd.iter().enumerate() {
                o("sshd", format!("{} {pid} {t}", i + 1));
            }
            if self.sshd_overflow {
                o("sshd_overflow", "1".into());
            }
            o("sshd_n", self.sshd.len().to_string());
            o("self", self.self_conn.clone());
            o("who", self.who.clone());
            o("tmux", self.tmux.into());
            if self.tmux == "ok" {
                o("tmux_panes", self.panes.len().to_string());
                for (i, p) in self.panes.iter().enumerate() {
                    o("pane", format!("{} {p}", i + 1));
                }
            }
            o("pane_n", if self.tmux == "ok" { self.panes.len() } else { 0 }.to_string());
            o("screen", self.screen.clone());
            o("keep", if self.keep { "1" } else { "0" }.into());
            o("workspace", self.workspace.into());
            o("repo", self.repo.into());
            o("home_mounts", self.home_mounts.clone());
            o("id", self.id.clone());
            o("boot_id", self.boot_id.clone());
            o("pid1_start", self.pid1_start.clone());
            o("now", (t1 + self.tail).to_string());
            out
        }

        /// `lines` framed and summed as the script does, with a banner line before it.
        pub fn frame(nonce: &str, lines: &[String]) -> String {
            let mut body = String::new();
            for l in lines {
                body.push_str(l);
                body.push('\n');
            }
            format!(
                "Welcome to the pod!\nARENA_PROBE_BEGIN {nonce}\n{body}ARENA_PROBE sum={} {}\nARENA_PROBE_END {nonce}\n",
                cksum(body.as_bytes()),
                body.len()
            )
        }

        /// The whole reply.
        pub fn render(&self) -> String {
            Self::frame(&self.nonce, &self.fact_lines())
        }

        /// The probe's end on the pod clock.
        pub fn now(&self) -> u64 {
            self.t0 + self.window + self.tail
        }
    }
}

#[cfg(test)]
mod tests;
