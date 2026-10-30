use super::{collect_snapshot, handle_snapshot, render_snapshot};
use crate::cli::{Cli, Cmd};
use crate::pods::health::record_health;
use arena_core::health::{Check, PodHealth, Status};
use arena_core::selector::Naming;
use arena_core::snapshot::{Issue, ProxyState, PublicHealthStatus, PublicSnapshot, PublicStatus, Reach};
use arena_core::{Config, Error, Pod, PodSpec, Provider, Result};
use async_trait::async_trait;
use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A scripted SSH-port probe: the ports that answer. Records every port asked, so a
/// test sees which pods were probed — and nothing is ever dialed.
struct Answers {
    up: Vec<u16>,
    asked: Mutex<Vec<u16>>,
}

#[async_trait]
impl Reach for Answers {
    async fn answers(&self, _host: &str, port: u16) -> bool {
        self.asked.lock().unwrap().push(port);
        self.up.contains(&port)
    }
}

fn answering(ports: &[u16]) -> Arc<Answers> {
    Arc::new(Answers { up: ports.to_vec(), asked: Mutex::new(Vec::new()) })
}

/// Every pod's SSH port would answer (apple 22001, bloom 22002, the staff box 22003 —
/// which is never asked).
fn all_up() -> Option<Arc<dyn Reach>> {
    Some(answering(&[22001, 22002, 22003]))
}

/// A fleet whose listing is scripted per backend (`Err` = that backend failed to list).
/// Every mutating call panics: `snapshot` must never make one.
struct Fleet {
    backends: Vec<(&'static str, std::result::Result<Vec<Pod>, &'static str>)>,
    enriched: AtomicUsize,
}

#[async_trait]
impl Provider for Fleet {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        Ok(self.backends.iter().filter_map(|(_, r)| r.clone().ok()).flatten().collect())
    }
    async fn list_by_provider(&self) -> Vec<(String, Result<Vec<Pod>>)> {
        self.backends.iter().map(|(n, r)| (n.to_string(), r.clone().map_err(Error::provider))).collect()
    }
    /// The details query: fills RunPod's GPU and price, as the real one does.
    async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
        self.enriched.fetch_add(1, Ordering::SeqCst);
        for p in pods.iter_mut().filter(|p| p.provider == "runpod") {
            p.gpu_type = Some("NVIDIA RTX A4000".into());
            p.gpu_count = Some(1);
            p.cost_per_hr = Some(0.17);
        }
        Ok(())
    }
    async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
        panic!("snapshot must not create")
    }
    async fn stop_pod(&self, _id: &str) -> Result<()> {
        panic!("snapshot must not stop")
    }
    async fn restart_pod(&self, _id: &str) -> Result<()> {
        panic!("snapshot must not restart")
    }
    async fn terminate_pod(&self, _id: &str) -> Result<()> {
        panic!("snapshot must not terminate")
    }
}

fn pod(name: &str, provider: &str, id: &str, ip: &str, port: u16) -> Pod {
    Pod {
        id: id.into(),
        name: name.into(),
        provider: provider.into(),
        status: "RUNNING".into(),
        ssh_ip: Some(ip.into()),
        ssh_port: Some(port),
        ..Default::default()
    }
}

fn apple() -> Pod {
    pod("devtest-apple", "runpod", "rpapple0001", "203.0.113.7", 22001)
}
fn bloom() -> Pod {
    pod("devtest-bloom", "runpod", "rpbloom0002", "203.0.113.8", 22002)
}
fn james() -> Pod {
    pod("james-gpu", "runpod", "rpstaff0003", "203.0.113.9", 22003)
}

/// runpod lists apple, bloom and a staff box; vast (with cloud on it) failed to list.
fn fleet() -> Fleet {
    Fleet {
        backends: vec![("runpod", Ok(vec![bloom(), james(), apple()])), ("vast", Err("list pods HTTP 429"))],
        enriched: AtomicUsize::new(0),
    }
}

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tmp(tag: &str) -> Tmp {
    let d = std::env::temp_dir().join(format!("arena-snapshot-cli-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

/// A local proxy (forwards: apple to its endpoint, bloom to an old one) and a state dir
/// in `dir`, plus a key that must never surface.
fn cfg(dir: &Path) -> Config {
    use arena_core::proxy::{render_nginx, Forward};
    let fwd = |name: &str, port: u16, ip: &str, tport: u16| Forward {
        name: name.into(),
        public_port: port,
        target_ip: ip.into(),
        target_port: tport,
        provider: Some("runpod".into()),
        pod_id: None,
    };
    let proxy = dir.join("proxy.conf");
    std::fs::write(
        &proxy,
        render_nginx(&[fwd("devtest-apple", 9500, "203.0.113.7", 22001), fwd("devtest-bloom", 9501, "198.51.100.1", 40000)]),
    )
    .unwrap();
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(\n \"apple\"\n \"bloom\"\n \"cloud\"\n \"@james-gpu\"\n)\n\
         RUNPOD_API_KEY=rpa_TESTSECRETKEY\nSSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH={}\n\
         SSH_PROXY_RELOAD_CMD=\"\"\nSSH_PROXY_STARTING_PORT=9500\nARENA_STATE_DIR={}\n",
        proxy.display(),
        dir.join("state").display()
    ))
}

/// No prepaid account configured: no balance in the snapshot.
fn no_balance() -> crate::money::BalanceCtx {
    crate::money::BalanceCtx { api: Arc::new(arena_core::balance::FakeBalanceApi::new()), path: None }
}

fn checked(p: &Pod, status: Status, checks: &[(&str, Status, &str)]) -> PodHealth {
    PodHealth {
        id: p.id.clone(),
        name: p.name.clone(),
        provider: p.provider.clone(),
        status,
        checks: checks.iter().map(|(n, s, d)| Check { name: n.to_string(), status: *s, detail: d.to_string() }).collect(),
        facts: None,
        host: None,
    }
}

#[test]
fn snapshot_flags_parse() {
    let parse = |argv: &[&str]| Cli::try_parse_from(["arena"].iter().chain(argv).copied()).map(|c| c.cmd);
    match parse(&["snapshot", "--public", "--out", "/tmp/x/fleet.json"]).unwrap() {
        Cmd::Snapshot { json: false, public: true, out: Some(p), no_probe: false } => {
            assert_eq!(p, Path::new("/tmp/x/fleet.json"))
        }
        _ => panic!("wrong parse"),
    }
    assert!(matches!(parse(&["snapshot"]).unwrap(), Cmd::Snapshot { json: false, public: false, out: None, no_probe: false }));
    assert!(matches!(parse(&["snapshot", "--public", "--no-probe"]).unwrap(), Cmd::Snapshot { no_probe: true, .. }));
    assert!(matches!(parse(&["snap", "--json"]).unwrap(), Cmd::Snapshot { json: true, .. }));
}

/// Invariant 9: the snapshot's balance comes from the state file — the fake panics on
/// any balance call — and only the internal forms show it; `--public` has none of it.
#[tokio::test]
async fn snapshot_makes_no_balance_calls() {
    use arena_core::balance::{self, Account, FakeBalanceApi, Fetched, Live, Mode};
    let dir = tmp("balance");
    let cfg = cfg(&dir.0);
    let state_file = dir.0.join("state").join("devtest").join(arena_core::snapshot::BALANCE_FILE);
    // A reading recorded earlier (as `arena balance` would have).
    let fp = FakeBalanceApi::fp(Account::Runpod);
    let live = Live { usd: 512.34, provider_burn_per_hr: Some(0.5), spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None };
    let at = arena_core::snapshot::unix_now() - 3_600;
    let f = Fetched { account: Account::Runpod, key_fp: fp, live: Some(Ok(live)), billed: None };
    balance::record(&state_file, |s| balance::apply(s, &[f], &balance::Burns::new(), Mode::Full, at)).unwrap();
    let before = std::fs::read(&state_file).unwrap();
    let bal = crate::money::BalanceCtx {
        api: Arc::new(FakeBalanceApi::new().with(Account::Runpod).panicking()),
        path: Some(Ok(state_file.clone())),
    };
    let (snap, _) = collect_snapshot(&fleet(), &cfg, arena_core::snapshot::unix_now(), None, &bal).await.unwrap();
    assert_eq!(std::fs::read(&state_file).unwrap(), before, "nothing written");
    assert_eq!(snap.balance.len(), 1);
    assert_eq!(snap.balance[0].source, balance::Source::Estimate, "an hour-old reading is an estimate");
    let naming = Naming::from_config(&cfg);
    let table = render_snapshot(&snap, &naming, false, false).unwrap();
    assert!(table.lines().last().unwrap().starts_with("balance: runpod ~$511."), "{table}");
    let json = render_snapshot(&snap, &naming, true, false).unwrap();
    assert!(json.contains("\"balance\"") && json.contains("\"source\": \"estimate\""), "{json}");
    let public = render_snapshot(&snap, &naming, false, true).unwrap();
    for needle in ["balance", "credit", "runway", "511.", "512."] {
        assert!(!public.contains(needle), "`{needle}` in the public snapshot: {public}");
    }
}

#[tokio::test]
async fn snapshot_joins_listing_proxy_and_cached_health_without_touching_anything() {
    let dir = tmp("collect");
    let cfg = cfg(&dir.0);
    let warn = record_health(
        &cfg,
        &[
            checked(&apple(), Status::Pass, &[]),
            checked(&bloom(), Status::Fail, &[("cuda", Status::Fail, "Error 999 on 203.0.113.8")]),
        ],
        None,
    );
    assert_eq!(warn, None);
    let f = fleet();
    // apple's (and the staff box's) sshd answer; bloom's doesn't.
    let probe = answering(&[22001, 22003]);
    let (snap, warnings) = collect_snapshot(&f, &cfg, arena_core::snapshot::unix_now(), Some(probe.clone()), &no_balance()).await.unwrap();
    assert_eq!(f.enriched.load(Ordering::SeqCst), 1, "one details query, like `pods list`");
    let mut asked = probe.asked.lock().unwrap().clone();
    asked.sort();
    assert_eq!(asked, [22001, 22002], "one probe per cohort machine — the staff box is never dialed");
    assert_eq!(snap.partial, ["vast"]);
    assert!(warnings.iter().any(|w| w.contains("vast failed to list (provider error: list pods HTTP 429)")), "{warnings:?}");
    let row = |n: &str| snap.pods.iter().find(|p| p.pod.name == n).unwrap();
    assert_eq!((row("devtest-apple").proxy, row("devtest-apple").proxy_port), (ProxyState::Live, Some(9500)));
    assert_eq!((row("devtest-bloom").proxy, row("devtest-bloom").proxy_port), (ProxyState::Stale, Some(9501)));
    assert_eq!(row("james-gpu").proxy, ProxyState::None);
    assert_eq!(row("devtest-apple").health.as_ref().unwrap().status, Status::Pass);
    assert_eq!(row("devtest-bloom").health.as_ref().unwrap().issues, [Issue::GpuError]);
    assert!(row("james-gpu").health.is_none());
    assert_eq!(snap.cost.billing, 3);
    assert_eq!(
        [row("devtest-apple").reachable, row("devtest-bloom").reachable, row("james-gpu").reachable],
        [Some(true), Some(false), None]
    );

    // The table: pods list's columns + SSH, PROXY and HEALTH, and the missing provider named.
    let table = render_snapshot(&snap, &Naming::from_config(&cfg), false, false).unwrap();
    assert!(
        table.contains("devtest-apple  runpod    rpapple0001  run     1×RTX A4000  $0.17  203.0.113.7:22001  ok    :9500  "),
        "{table}"
    );
    let bloom_row = table.lines().find(|l| l.starts_with("devtest-bloom")).unwrap();
    assert!(bloom_row.contains(":22002  down  :9501 stale  fail ") && bloom_row.contains("s GPU error"), "{table}");
    assert!(table.ends_with("partial: vast failed to list — its pods are missing above\n"), "{table}");
    // --json is the internal view (ids and endpoints included — never published).
    let json = render_snapshot(&snap, &Naming::from_config(&cfg), true, false).unwrap();
    assert!(json.contains("\"proxy\": \"live\"") && json.contains("rpapple0001"), "{json}");
    // --public wins over --json.
    let public = render_snapshot(&snap, &Naming::from_config(&cfg), true, true).unwrap();
    assert!(public.contains("\"machines\"") && !public.contains("rpapple0001"), "{public}");
    // …where bloom, listed running but silent on its SSH port, isn't `up`.
    let public: PublicSnapshot = serde_json::from_str(&public).unwrap();
    let status: Vec<(&str, PublicStatus)> = public.machines.iter().map(|m| (m.name.as_str(), m.status)).collect();
    assert_eq!(status, [("apple", PublicStatus::Up), ("bloom", PublicStatus::Starting)]);

    // `--no-probe`: nothing probed (SSH `-`), and a listed-running pod with an endpoint is up.
    let (snap, _) = collect_snapshot(&fleet(), &cfg, arena_core::snapshot::unix_now(), None, &no_balance()).await.unwrap();
    assert!(snap.pods.iter().all(|p| p.reachable.is_none()));
    let public = arena_core::snapshot::public_snapshot(&snap, &Naming::from_config(&cfg));
    assert!(public.machines.iter().all(|m| m.status == PublicStatus::Up), "{public:?}");
}

#[tokio::test]
async fn snapshot_public_out_is_written_atomically_and_holds_only_allowlisted_facts() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp("public");
    let cfg = cfg(&dir.0);
    record_health(
        &cfg,
        &[checked(&bloom(), Status::Fail, &[("ssh", Status::Fail, "exit 255: connect to host 203.0.113.8 port 22002")])],
        None,
    );
    let www = dir.0.join("www");
    std::fs::create_dir(&www).unwrap();
    let out = www.join("fleet.json");
    std::fs::write(&out, "{\"old\": true}").unwrap();
    handle_snapshot(&fleet(), &cfg, false, true, Some(&out), all_up()).await.unwrap();

    let text = std::fs::read_to_string(&out).unwrap();
    let public: PublicSnapshot = serde_json::from_str(&text).unwrap();
    assert!(!public.complete, "vast didn't answer");
    let rows: Vec<(&str, &str, PublicStatus, PublicHealthStatus, Option<Issue>)> =
        public.machines.iter().map(|m| (m.name.as_str(), m.gpu.as_str(), m.status, m.health.status, m.health.reason)).collect();
    assert_eq!(
        rows,
        [
            ("apple", "1×RTX A4000", PublicStatus::Up, PublicHealthStatus::Unknown, None),
            ("bloom", "1×RTX A4000", PublicStatus::Up, PublicHealthStatus::Fail, Some(Issue::Unreachable)),
        ]
    );
    for needle in ["203.0.113", "198.51.100", "rpa_", "rpapple", "rpbloom", "rpstaff", "james", "devtest", "runpod", "vast", "22001", "9500", "0.17"] {
        assert!(!text.contains(needle), "`{needle}` in {text}");
    }
    // Readable by a web server; no temp file left next to it.
    assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o644);
    assert_eq!(std::fs::read_dir(&www).unwrap().count(), 1);

    // The internal JSON to a file is owner-only.
    let internal = www.join("internal.json");
    handle_snapshot(&fleet(), &cfg, true, false, Some(&internal), all_up()).await.unwrap();
    assert_eq!(std::fs::metadata(&internal).unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
async fn snapshot_with_no_provider_answering_fails_and_leaves_the_old_file() {
    let dir = tmp("down");
    let cfg = cfg(&dir.0);
    let out = dir.0.join("fleet.json");
    std::fs::write(&out, "previous").unwrap();
    let down = Fleet {
        backends: vec![("runpod", Err("list pods HTTP 502")), ("vast", Err("list pods HTTP 429"))],
        enriched: AtomicUsize::new(0),
    };
    let err = handle_snapshot(&down, &cfg, false, true, Some(&out), all_up()).await.unwrap_err();
    assert!(err.to_string().contains("no provider listed its pods"), "{err}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "previous", "the page goes stale rather than empty");
}

/// The provider holding every machine fails while another configured one answers empty
/// (a Hetzner set up but unused): the page keeps its machines and goes stale instead of
/// reading "No machines." — while a real teardown (everyone answered, nothing listed)
/// and a partial listing that still shows machines are published as usual.
#[tokio::test]
async fn snapshot_never_blanks_the_page_when_the_provider_with_the_machines_fails() {
    let dir = tmp("blank");
    let cfg = cfg(&dir.0);
    let out = dir.0.join("fleet.json");
    let fleet_of = |runpod: std::result::Result<Vec<Pod>, &'static str>| Fleet {
        backends: vec![("runpod", runpod), ("hetzner", Ok(vec![]))],
        enriched: AtomicUsize::new(0),
    };
    handle_snapshot(&fleet_of(Ok(vec![apple()])), &cfg, false, true, Some(&out), all_up()).await.unwrap();
    let good = std::fs::read_to_string(&out).unwrap();
    assert!(good.contains("\"apple\""), "{good}");

    let err = handle_snapshot(&fleet_of(Err("list pods HTTP 429")), &cfg, false, true, Some(&out), all_up()).await.unwrap_err();
    assert!(err.to_string().starts_with("runpod failed to list and nothing else listed a machine — kept "), "{err}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), good, "the old page is kept");

    // Printing (no file to protect) still works, flagged incomplete.
    handle_snapshot(&fleet_of(Err("list pods HTTP 429")), &cfg, false, true, None, all_up()).await.unwrap();
    // A partial listing that still shows a machine is published, flagged incomplete.
    let partial = Fleet {
        backends: vec![("runpod", Ok(vec![apple()])), ("vast", Err("list pods HTTP 429"))],
        enriched: AtomicUsize::new(0),
    };
    handle_snapshot(&partial, &cfg, false, true, Some(&out), all_up()).await.unwrap();
    let public: PublicSnapshot = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!((public.complete, public.machines.len()), (false, 1));
    // Everyone answered and nothing is left: the empty page is the truth.
    handle_snapshot(&fleet_of(Ok(vec![])), &cfg, false, true, Some(&out), all_up()).await.unwrap();
    let public: PublicSnapshot = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!((public.complete, public.machines.len()), (true, 0));
    // And from there, an outage has nothing to blank: published, flagged incomplete.
    handle_snapshot(&fleet_of(Err("list pods HTTP 429")), &cfg, false, true, Some(&out), all_up()).await.unwrap();
    let public: PublicSnapshot = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!((public.complete, public.machines.len()), (false, 0));
}

#[tokio::test]
async fn a_corrupt_health_cache_or_missing_proxy_file_is_a_warning_not_a_failure() {
    let dir = tmp("corrupt");
    let mut cfg = cfg(&dir.0);
    let state = dir.0.join("state").join("devtest");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("health.json"), "{truncated").unwrap();
    std::fs::remove_file(dir.0.join("proxy.conf")).unwrap();
    let (snap, warnings) = collect_snapshot(&fleet(), &cfg, 1_791_460_800, all_up(), &no_balance()).await.unwrap();
    assert!(snap.pods.iter().all(|p| p.health.is_none() && p.proxy == ProxyState::None), "{snap:#?}");
    assert_eq!(warnings.iter().filter(|w| w.contains("ignoring the health cache")).count(), 1, "{warnings:?}");
    // A remote proxy is never fetched (no SSH): its forwards are unknown.
    cfg.values.insert("PROXY_LOCAL".into(), "false".into());
    cfg.values.insert("SSH_PROXY_HOST".into(), "proxy.example".into());
    let (snap, _) = collect_snapshot(&fleet(), &cfg, 1_791_460_800, all_up(), &no_balance()).await.unwrap();
    assert!(snap.pods.iter().all(|p| p.proxy == ProxyState::Unknown));
}
