//! `ARENA_TUI_DEMO=<n>`: the dashboard over a made-up fleet of `n` pods — for measuring how it
//! behaves at a cohort's size (the trace, `lag.rs`) on a sandbox account with one pod or none,
//! and for looking at it. Nothing leaves this process: no provider is asked anything, no pod is
//! reached, nothing runs as `arena` (every action and the add-pod form say "demo"); the rows'
//! numbers wander a little each refresh so the graphs move. Not in `--help`: a developer's aid.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arena_core::metrics::{GpuStat, PodMetrics};
use arena_core::provider::Provider;
use arena_core::remote::Remote;
use arena_core::selector::Naming;
use arena_core::snapshot::{self, HealthCache};
use arena_core::ssh::{SshOutput, SshTarget};
use arena_core::{Config, Pod, PodSpec};

use crate::state::{dashboard_snapshot, summarize};
use crate::Shared;

/// How many pods `ARENA_TUI_DEMO` asks for (`None`: not a demo). A value that isn't a number of
/// 1–500 is not a demo either.
pub(crate) fn wanted(value: Option<&str>) -> Option<usize> {
    value?.trim().parse::<usize>().ok().filter(|n| (1..=500).contains(n))
}

/// A small deterministic generator: the same `seed`, the same fleet.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// `n` pods named `<prefix>-demoNN` and what a refresh would have read from them, round `round`:
/// mostly running RunPod pods with one or two A4000s, some on Vast, a few Hetzner VMs, two
/// stopped, one unreachable. Pure.
pub(crate) fn fleet(prefix: &str, n: usize, round: u64) -> (Vec<Pod>, HashMap<String, PodMetrics>) {
    let mut pods = Vec::with_capacity(n);
    let mut metrics = HashMap::new();
    for i in 0..n {
        // Each pod's own stream: its numbers move from round to round, its shape never does.
        let mut shape = Rng(0x9E37_79B9_7F4A_7C15 ^ (i as u64 + 1));
        let mut now = Rng((i as u64 + 1).wrapping_mul(0x1000_0001) ^ round.wrapping_mul(0xD6E8_FEB8_6659_FD93) | 1);
        let provider = match i % 10 {
            7 => "vast",
            9 if i % 20 == 9 => "hetzner",
            _ => "runpod",
        };
        let gpus = if provider == "hetzner" { 0 } else { 1 + (shape.below(4) == 0) as u32 };
        let stopped = i % 29 == 28;
        let name = format!("{prefix}-demo{:02}", i + 1);
        let pod = Pod {
            id: format!("demo{i:03}"),
            name: name.clone(),
            provider: provider.into(),
            status: if stopped { "EXITED".into() } else { "RUNNING".into() },
            gpu_type: Some(if provider == "hetzner" { "cx33".into() } else { "NVIDIA RTX A4000".into() }),
            gpu_count: Some(gpus),
            cost_per_hr: Some(if provider == "hetzner" { 0.016 } else { 0.17 * gpus as f64 }),
            ssh_ip: (!stopped).then(|| format!("10.9.{}.{}", i / 250, i % 250 + 1)),
            ssh_port: (!stopped).then_some(22_000 + i as u16),
            ..Default::default()
        };
        if !stopped {
            let m = if i % 23 == 22 {
                PodMetrics { error: Some("ssh: connect timed out".into()), ..Default::default() }
            } else {
                let gpu = |r: &mut Rng| GpuStat {
                    name: Some("NVIDIA RTX A4000".into()),
                    util_pct: Some(r.below(101) as u32),
                    mem_used_mb: Some(r.below(16_000) as u32),
                    mem_total_mb: Some(16_376),
                    temp_c: Some(35 + r.below(45) as u32),
                };
                PodMetrics {
                    gpus: (0..gpus).map(|_| gpu(&mut now)).collect(),
                    branch: Some(format!("{prefix}-w{}d{}", 1 + shape.below(4), 1 + shape.below(5))),
                    origin: Some("git@github.com:example/ARENA_3.0.git".into()),
                    has_name: Some(true),
                    has_key: Some(true),
                    has_api_key: Some(shape.below(8) != 0),
                    disk_used_mb: Some(20_000 + now.below(60_000) as u32),
                    disk_total_mb: Some(100_000),
                    cpu_pct: Some(now.below(100) as u32),
                    host_mem_used_mb: Some(8_000 + now.below(50_000) as u32),
                    host_mem_total_mb: Some(64_000),
                    last_commit: Some(snapshot::unix_now() as i64 - now.below(90_000) as i64),
                    dirty_files: Some(now.below(4) as u32),
                    ..Default::default()
                }
            };
            metrics.insert(name, m);
        }
        pods.push(pod);
    }
    (pods, metrics)
}

/// Publish round `round` of the demo fleet to `shared`, as a refresh publishes a real one (rows,
/// history, summary, status).
pub(crate) fn publish(shared: &Mutex<Shared>, cfg: &Config, n: usize, round: u64) {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let (pods, metrics) = fleet(prefix, n, round);
    let now = snapshot::unix_now();
    let snap = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &Naming::from_config(cfg), now);
    let mut s = shared.lock().unwrap();
    for pod in &pods {
        let m = metrics.get(&pod.name);
        let pct = |(u, t): (u32, u32)| if t > 0 { (u as u64 * 100 / t as u64) as u32 } else { 0 };
        let (util, mem) = if pod.cpu_only() {
            (m.and_then(|m| m.cpu_pct), m.and_then(|m| m.host_mem_summary()).map(pct))
        } else {
            (m.and_then(|m| m.mean_util()), m.and_then(|m| m.mem_summary()).map(pct))
        };
        s.history.entry(pod.name.clone()).or_default().push(util, m.and_then(|m| m.max_temp()), mem);
    }
    s.summary = summarize(&pods, &metrics);
    s.status = format!("demo: {n} made-up pods, nothing real");
    s.metrics = metrics;
    s.publish(snap);
    s.last_refresh = Some(Instant::now());
    s.refreshing = false;
}

/// The demo's refresh: a new round every `interval` seconds (or at once on `nudge`).
pub(crate) async fn refresh_loop(shared: Arc<Mutex<Shared>>, cfg: Config, n: usize, interval: Arc<std::sync::atomic::AtomicU64>, nudge: Arc<tokio::sync::Notify>) {
    let mut round = 0u64;
    loop {
        publish(&shared, &cfg, n, round);
        round += 1;
        let secs = interval.load(std::sync::atomic::Ordering::Relaxed).max(1);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
            _ = nudge.notified() => {}
        }
    }
}

/// The demo's provider: lists nothing (the refresh above makes the rows), refuses everything.
pub(crate) struct NoProvider;

fn refused() -> arena_core::Error {
    arena_core::Error::NotImplemented("demo: nothing real to act on".into())
}

#[async_trait::async_trait]
impl Provider for NoProvider {
    fn name(&self) -> &'static str {
        "demo"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        "demo: nothing".into()
    }
    async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
        Ok(Vec::new())
    }
    async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
        Err(refused())
    }
    async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
        Err(refused())
    }
    async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
        Err(refused())
    }
    async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
        Err(refused())
    }
}

/// The demo's way to pods: there are none — every call fails at once, never an `ssh`.
pub(crate) struct NoRemote;

#[async_trait::async_trait]
impl Remote for NoRemote {
    async fn exec(&self, _t: &SshTarget, _cmd: &str, _timeout: Option<Duration>) -> arena_core::Result<SshOutput> {
        Err(refused())
    }
    async fn copy(&self, _t: &SshTarget, _l: &str, _r: &str, _timeout: Option<Duration>) -> arena_core::Result<SshOutput> {
        Err(refused())
    }
    async fn copy_recursive(&self, _t: &SshTarget, _l: &str, _r: &str, _timeout: Option<Duration>) -> arena_core::Result<SshOutput> {
        Err(refused())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_is_a_number_of_pods_or_nothing() {
        for (v, want) in [(None, None), (Some("60"), Some(60)), (Some(" 5 "), Some(5)), (Some("0"), None), (Some("501"), None), (Some("x"), None), (Some(""), None)] {
            assert_eq!(wanted(v), want, "{v:?}");
        }
    }

    /// The made-up fleet: `n` pods, the same shape every round (names, providers, GPUs), its
    /// numbers moving; stopped pods unread, one unreachable, Hetzner's CPU-only.
    #[test]
    fn the_demo_fleet_keeps_its_shape_and_moves_its_numbers() {
        let (a, ma) = fleet("devtest", 60, 0);
        let (b, mb) = fleet("devtest", 60, 1);
        assert_eq!(a.len(), 60);
        assert_eq!(a.iter().map(|p| (&p.name, &p.provider, p.gpu_count)).collect::<Vec<_>>(), b.iter().map(|p| (&p.name, &p.provider, p.gpu_count)).collect::<Vec<_>>());
        assert!(a.iter().any(|p| p.provider == "hetzner" && p.cpu_only()) && a.iter().any(|p| p.provider == "vast"));
        assert!(a.iter().any(|p| p.status == "EXITED"));
        assert!(ma.values().any(|m| m.error.is_some()));
        let utils = |m: &HashMap<String, PodMetrics>| {
            let mut v: Vec<(String, Option<u32>)> = m.iter().map(|(n, m)| (n.clone(), m.mean_util())).collect();
            v.sort();
            v
        };
        assert_ne!(utils(&ma), utils(&mb), "the numbers move");
        assert_eq!(a[0].name, "devtest-demo01");
    }
}
