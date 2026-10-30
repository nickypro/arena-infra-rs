//! `ARENA_TUI_DEMO=<n>`: the dashboard over a made-up fleet of `n` pods — for measuring how it
//! behaves at a cohort's size (the trace, `lag.rs`) on a sandbox account with one pod or none,
//! and for looking at it. Nothing leaves this process: no provider is asked anything, no pod is
//! reached, nothing runs as `arena` (every action and the add-pod form say "demo"); the rows'
//! numbers wander a little each refresh so the graphs move. Not in `--help`: a developer's aid.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arena_core::balance::{self, Account, AccountView, BalancePolicy, BalanceState, Burns, Fetched, Live};
use arena_core::health::{Check, PodHealth, Status};
use arena_core::metrics::{normalize_gpu_name, GpuStat, PodMetrics};
use arena_core::openrouter::{Credits, KeyInfo, UsageView};
use arena_core::proxy::{render_nginx, Forward};
use arena_core::provider::Provider;
use arena_core::remote::Remote;
use arena_core::selector::Naming;
use arena_core::snapshot::{self, HealthCache};
use arena_core::ssh::{SshOutput, SshTarget};
use arena_core::{Config, Pod, PodSpec};

use crate::state::{dashboard_snapshot, summarize, RefreshStatus, Tone};
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
/// stopped, one unreachable, and (from 6 pods) a B300 MIG slice and an H100 — the slice read as
/// nvidia-smi reads one: the parent card's name, its memory unreadable. Pure.
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
        // (the provider's GPU type, the name nvidia-smi gives, its memory in MB, $/h per GPU)
        let (card, smi_name, vram, rate) = match i {
            2 => ("NVIDIA B300 SXM6 AC MIG 1g.34gb", "NVIDIA B300 SXM6 AC", None, 0.89),
            5 => ("NVIDIA H100 80GB HBM3", "NVIDIA H100 80GB HBM3", Some(81_559), 2.69),
            _ => ("NVIDIA RTX A4000", "NVIDIA RTX A4000", Some(16_376), 0.17),
        };
        let special = matches!(i, 2 | 5);
        let gpus = if provider == "hetzner" { 0 } else if special { 1 } else { 1 + (shape.below(4) == 0) as u32 };
        let stopped = i % 29 == 28;
        let name = format!("{prefix}-demo{:02}", i + 1);
        let pod = Pod {
            id: format!("demo{i:03}"),
            name: name.clone(),
            provider: provider.into(),
            status: if stopped { "EXITED".into() } else { "RUNNING".into() },
            gpu_type: Some(if provider == "hetzner" { "cx33".into() } else { card.into() }),
            gpu_count: Some(gpus),
            cost_per_hr: Some(if provider == "hetzner" { 0.016 } else { rate * gpus as f64 }),
            ssh_ip: (!stopped).then(|| format!("10.9.{}.{}", i / 250, i % 250 + 1)),
            ssh_port: (!stopped).then_some(22_000 + i as u16),
            ..Default::default()
        };
        if !stopped {
            let m = if i % 23 == 22 {
                PodMetrics { error: Some("ssh: connect timed out".into()), ..Default::default() }
            } else {
                let gpu = |r: &mut Rng| GpuStat {
                    name: Some(normalize_gpu_name(smi_name)),
                    util_pct: Some(r.below(101) as u32),
                    mem_used_mb: vram.map(|v| r.below(v) as u32),
                    mem_total_mb: vram.map(|v| v as u32),
                    temp_c: Some(35 + r.below(45) as u32),
                };
                PodMetrics {
                    gpus: (0..gpus).map(|_| gpu(&mut now)).collect(),
                    branch: Some(format!("autocommit-{prefix}-w{}d{}-demo{:02}", 1 + shape.below(4), 1 + shape.below(5), i + 1)),
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

/// The demo's last deep checks: most pods passed one (minutes to a day and more ago), a few
/// failed or warned, every seventh never checked. Pure.
fn health(pods: &[Pod], now: u64) -> HealthCache {
    let mut cache = HealthCache::new();
    for (i, p) in pods.iter().enumerate().filter(|(i, p)| i % 7 != 0 && p.status == "RUNNING") {
        let (status, check, age) = match i {
            i if i % 11 == 3 => (Status::Fail, Some(("cuda", "RuntimeError: CUDA error: unknown error")), 2 * 3_600),
            i if i % 13 == 4 => (Status::Warn, Some(("driver", "570.86 < 580 (CUDA 13 wants 580)")), 5 * 3_600),
            i if i % 17 == 6 => (Status::Pass, None, 30 * 3_600),
            i => (Status::Pass, None, 60 + i as u64 * 420),
        };
        let checks = check.map(|(name, detail)| Check { name: name.into(), status, detail: detail.into() }).into_iter().collect();
        let h = PodHealth { id: p.id.clone(), name: p.name.clone(), provider: p.provider.clone(), status, checks, facts: None, host: None };
        cache.merge(&[h], now.saturating_sub(age));
    }
    cache
}

/// The demo's proxy file: a forward for each running pod but every ninth, one in eight pointing
/// at an old endpoint (stale). Pure.
fn proxy(pods: &[Pod]) -> String {
    let forwards: Vec<Forward> = pods
        .iter()
        .enumerate()
        .filter(|(i, p)| i % 9 != 4 && p.ssh_ip.is_some())
        .map(|(i, p)| Forward {
            name: p.name.clone(),
            public_port: 9500 + i as u16,
            target_ip: p.ssh_ip.clone().unwrap_or_default(),
            target_port: p.ssh_port.unwrap_or(22) + u16::from(i % 8 == 6),
            provider: Some(p.provider.clone()),
            pod_id: Some(p.id.clone()),
        })
        .collect();
    render_nginx(&forwards)
}

/// The demo's prepaid accounts, read "live": RunPod and Vast with about a week and three days
/// left at the fleet's burn. Pure.
fn balances(pods: &[Pod], now: u64) -> Vec<AccountView> {
    let burn = |provider: &str| pods.iter().filter(|p| p.provider == provider && p.status == "RUNNING").filter_map(|p| p.cost_per_hr).sum::<f64>();
    let fetched: Vec<Fetched> = [(Account::Runpod, 190.0), (Account::Vast, 80.0)]
        .into_iter()
        .map(|(account, hours)| {
            let rate = burn(if account == Account::Runpod { "runpod" } else { "vast" });
            let live = Live { usd: (rate * hours * 100.0).round() / 100.0, provider_burn_per_hr: Some(rate), spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None };
            Fetched { account, key_fp: "demo".into(), live: Some(Ok(live)), billed: None }
        })
        .collect();
    balance::views(&BalanceState::default(), &fetched, &Burns::new(), &BalancePolicy::default(), balance::Mode::Inline, now)
}

/// The demo's OpenRouter keys, one per pod with a $5 daily cap — the fifth spent to it — and the
/// account's credit. Pure.
fn ai(pods: &[Pod], round: u64) -> UsageView {
    let keys: Vec<KeyInfo> = pods
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let today = if i == 4 { 5.0 } else { ((i as u64 * 37 + round) % 450) as f64 / 100.0 };
            KeyInfo {
                hash: format!("demo{i}"),
                name: Some(p.name.clone()),
                limit: Some(5.0),
                limit_remaining: Some(5.0 - today),
                limit_reset: Some("daily".into()),
                usage_daily: Some(today),
                ..Default::default()
            }
        })
        .collect();
    UsageView::from_keys(&keys, Some(Credits { total: 500.0, used: 77.91 }))
}

/// Publish round `round` of the demo fleet to `shared`, as a refresh publishes a real one (rows,
/// history, summary, status) — with deep checks (one running), proxy forwards, balances and
/// OpenRouter keys made up too, so every column and the summary have something to show.
pub(crate) fn publish(shared: &Mutex<Shared>, cfg: &Config, n: usize, round: u64) {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let (pods, metrics) = fleet(prefix, n, round);
    let now = snapshot::unix_now();
    let snap = dashboard_snapshot(&pods, &[], Some(&proxy(&pods)), &health(&pods, now), &Naming::from_config(cfg), now);
    let mut s = shared.lock().unwrap();
    s.deep.begin(pods.iter().skip(1).take(1).cloned().collect());
    s.balance = balances(&pods, now);
    let view = ai(&pods, round);
    let names: Vec<String> = pods.iter().map(|p| p.name.clone()).collect();
    s.ai_cohort = Some(view.cohort(prefix, &names));
    s.ai = Some(view);
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
    s.status = RefreshStatus { short: "demo".into(), tone: Some(Tone::Warn), full: format!("demo: {n} made-up pods, nothing real") };
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

    /// Published, the demo has something in every column and the summary: a B300 MIG slice (its
    /// card as nvidia-smi names it, memory unread), deep checks (one running), live/stale/no
    /// forwards, balances and OpenRouter keys — one at its cap.
    #[test]
    fn the_published_demo_fills_every_column() {
        let shared = Mutex::new(Shared::default());
        let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\n");
        publish(&shared, &cfg, 60, 0);
        let s = shared.lock().unwrap();
        let mig = &s.pods[2];
        assert_eq!(mig.gpu_type.as_deref(), Some("NVIDIA B300 SXM6 AC MIG 1g.34gb"));
        let m = &s.metrics[&mig.name];
        assert_eq!((m.gpus[0].name.as_deref(), m.vram_gb()), (Some("B300 SXM6 AC"), None));
        use arena_core::snapshot::ProxyState;
        for state in [ProxyState::Live, ProxyState::Stale, ProxyState::None] {
            assert!(s.snap.pods.iter().any(|p| p.proxy == state), "{state:?}");
        }
        for status in [Status::Pass, Status::Warn, Status::Fail] {
            assert!(s.snap.pods.iter().any(|p| p.health.as_ref().is_some_and(|h| h.status == status)), "{status:?}");
        }
        assert_eq!(s.deep.running(), 1);
        assert_eq!(s.balance.len(), 2);
        assert_eq!(s.ai_cohort.as_ref().map(|c| c.capped.len()), Some(1));
        assert_eq!(s.status.short, "demo");
    }
}
