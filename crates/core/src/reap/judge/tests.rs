use super::*;
use crate::reap::probe::fixture::{v4, Reply};
use crate::reap::probe::{parse_probe, Since};

const T: u64 = 1_791_460_800;
const CONTAINER: &str = "db9bb781-8521-46cd-a74a-63c0e46d9be7/123456";

fn list() -> Vec<String> {
    ["alpha", "bravo", "charlie", "delta", "@devtest-boss"].map(String::from).to_vec()
}

fn rp(name: &str, id: &str) -> Pod {
    Pod {
        id: id.into(),
        name: name.into(),
        provider: "runpod".into(),
        status: "RUNNING".into(),
        ssh_ip: Some("10.0.0.1".into()),
        ssh_port: Some(22001),
        locked: Some(false),
        ..Default::default()
    }
}

fn hz(name: &str, id: &str) -> Pod {
    Pod { provider: "hetzner".into(), locked: None, delete_protected: Some(false), ..rp(name, id) }
}

fn policy(pod: &Pod, exp: Option<u64>, idle: Option<u64>) -> PodPolicy {
    PodPolicy::new(pod, exp, idle, T - 86_400, "set", None)
}

/// An observation at control time `t` (pod clock equal), scanning back to just before the
/// previous one ended (continuous coverage).
fn obs(t: u64, verdict: Verdict) -> Observation {
    Observation {
        t,
        clock_ok: true,
        container: Some(CONTAINER.into()),
        pod_t: Some(t),
        verdict,
        reasons: Vec::new(),
        cpu_usec: Some(t),
        net_bytes: Some(t),
        counters_t: Some(t - 12),
        // Back past the longest allowed gap: continuous coverage between neighbours.
        scan_from: Some(t - MAX_GAP - SCAN_MARGIN),
    }
}

/// Spec R1–R13: one case per row of the pre-SSH gate, in the table's order; the first that
/// matches decides, and only a pod past all of them is probed.
#[test]
fn pre_ssh_gate_rows() {
    let l = list();
    let naming = Naming { prefix: "devtest", list: &l };
    let a = rp("devtest-alpha", "ra");
    let pins = vec![Pin { name: "devtest-charlie".into(), id: None, until: None, note: None, set_at: T }];
    let listed = vec![
        a.clone(),
        rp("devtest-bravo", "rb1"),
        rp("devtest-bravo", "rb2"),
        rp("devtest-charlie", "rc"),
        rp("devtest-delta", "rd"),
        rp("devtest-delta-old", "rdo"),
    ];
    let ctx = GateCtx { naming: &naming, runpod_v2: true, pins: &pins, listed: &listed, now: T };
    let p = |pod: &Pod| policy(pod, None, Some(3600));
    let gate = |pol: &PodPolicy, l: Listed, c: &GateCtx| pre_ssh_gate(pol, l, c).map(|p| p.id.clone());
    // (case, policy, listing, row)
    let mut empty = p(&a);
    empty.idle_after_secs = None;
    let renamed = PodPolicy { name: "devtest-zulu".into(), ..p(&a) };
    let terminating = PodPolicy { terminating: Some(Terminating::new("r", Trigger::Idle, T)), ..p(&a) };
    let cases: Vec<(&str, PodPolicy, Pod, Row)> = vec![
        ("R1 no policy", empty, a.clone(), Row::NoPolicy),
        ("R3 vast", p(&a), Pod { provider: "vast".into(), ..a.clone() }, Row::ProviderUnsupported),
        ("R4 off-list", p(&a), rp("devtest-zulu", "ra"), Row::NotOnList),
        ("R4 staff box", p(&a), rp("devtest-boss", "ra"), Row::NotOnList),
        ("R4 a twin", p(&a), rp("devtest-alpha-old", "ra"), Row::NotOnList),
        ("R5 duplicate", p(&a), rp("devtest-bravo", "rb1"), Row::DuplicateOrTwin),
        ("R5 twin listed", p(&a), rp("devtest-delta", "rd"), Row::DuplicateOrTwin),
        ("R6 renamed", renamed, a.clone(), Row::NameMismatch),
        ("R7 locked", p(&a), Pod { locked: Some(true), ..a.clone() }, Row::Locked),
        ("R8 lock unknown", p(&a), Pod { locked: None, ..a.clone() }, Row::LockUnknown),
        ("R9 pinned by name", p(&a), rp("devtest-charlie", "rc"), Row::Pinned),
        ("R10 terminating", terminating, a.clone(), Row::Terminating),
        ("R11 stopped", p(&a), Pod { status: "EXITED".into(), ..a.clone() }, Row::NotRunning),
        ("R11 starting", p(&a), Pod { status: "STARTING".into(), ..a.clone() }, Row::NotRunning),
        ("R12 no endpoint", p(&a), Pod { ssh_ip: None, ..a.clone() }, Row::NoEndpoint),
        ("R12 proxy only", p(&a), Pod { proxy_only: true, ..a.clone() }, Row::NoEndpoint),
        ("R13 expiry far", policy(&a, Some(T + EXPIRY_PROBE_LEAD + 1), None), a.clone(), Row::Scheduled),
    ];
    for (case, pol, pod, row) in cases {
        // The pins name charlie: give the R9 policy charlie's name so R6 passes.
        let pol = if case.starts_with("R9") { PodPolicy { name: "devtest-charlie".into(), ..pol } } else { pol };
        let pol = if case.starts_with("R4") || case.starts_with("R5") { PodPolicy { name: pod.name.clone(), ..pol } } else { pol };
        assert_eq!(gate(&pol, Listed::Pod(&pod), &ctx), Err(row), "{case}");
    }
    assert_eq!(gate(&p(&a), Listed::Failed, &ctx), Err(Row::ListingFailed), "R2 failed");
    assert_eq!(gate(&p(&a), Listed::NotQueried, &ctx), Err(Row::ListingFailed), "R2 not queried");
    assert_eq!(gate(&p(&a), Listed::Gone, &ctx), Err(Row::Pruned));
    // RunPod on REST v1: unsupported (its lock state is unknown).
    let v1 = GateCtx { runpod_v2: false, ..GateCtx { naming: &naming, runpod_v2: true, pins: &pins, listed: &listed, now: T } };
    assert_eq!(gate(&p(&a), Listed::Pod(&a), &v1), Err(Row::ProviderUnsupported), "R3 v1");
    // Passing every row: probed. Expiry-only within the lead: probed too.
    assert_eq!(gate(&p(&a), Listed::Pod(&a), &ctx), Ok("ra".into()));
    assert_eq!(gate(&policy(&a, Some(T + EXPIRY_PROBE_LEAD), None), Listed::Pod(&a), &ctx), Ok("ra".into()));
    // Pinned by id after a rename: still vetoed.
    let by_id = vec![Pin { name: "devtest-old-name".into(), id: Some("ra".into()), until: None, note: None, set_at: T }];
    let ctx2 = GateCtx { pins: &by_id, ..GateCtx { naming: &naming, runpod_v2: true, pins: &pins, listed: &listed, now: T } };
    assert_eq!(gate(&p(&a), Listed::Pod(&a), &ctx2), Err(Row::Pinned));
    // Hetzner: its delete protection is the lock.
    let h = hz("devtest-alpha", "7");
    let hl = vec![h.clone()];
    let hctx = GateCtx { naming: &naming, runpod_v2: false, pins: &[], listed: &hl, now: T };
    assert_eq!(gate(&p(&h), Listed::Pod(&h), &hctx), Ok("7".into()));
    let prot = Pod { delete_protected: Some(true), ..h.clone() };
    assert_eq!(gate(&p(&h), Listed::Pod(&prot), &hctx), Err(Row::Locked));
    let unknown = Pod { delete_protected: None, ..h.clone() };
    assert_eq!(gate(&p(&h), Listed::Pod(&unknown), &hctx), Err(Row::LockUnknown));
}

/// The previous look 15 minutes ago, for classification.
fn prev(r: &Reply) -> Observation {
    Observation {
        t: r.now() - 900,
        clock_ok: true,
        container: Some(CONTAINER.into()),
        pod_t: Some(r.now() - 900),
        verdict: Verdict::Idle,
        reasons: Vec::new(),
        cpu_usec: Some(r.cpu.1.unwrap_or(10_000_000).saturating_sub(10_000_000)),
        net_bytes: Some(r.net.1.unwrap_or(1_000_000).saturating_sub(1_000_000)),
        counters_t: Some(r.t0 + r.window - 900),
        scan_from: None,
    }
}

fn classify_reply(r: &Reply, prev: Option<&Observation>, jobs: JobsOutcome, provider: &str) -> Observation {
    let facts = parse_probe(&r.render(), &r.nonce, r.since);
    classify(prev, &facts, jobs, &ClassifyCtx { provider, listed_id: "pod1", now: r.now(), clock_ok: true, baseline: None })
}

/// A process `name` started an hour after the fixture's container (PID 1 at 123456 jiffies, 100 Hz).
fn late(name: &str) -> (u64, u32, bool, String) {
    (123_456 + 3600 * 100, 4_000_000_001, false, name.to_string())
}

/// The fixture's processes plus `p`.
fn with_proc(p: (u64, u32, bool, String)) -> Reply {
    let mut r = Reply::default();
    r.procs.as_mut().unwrap().push(p);
    r
}

/// RS7: what counts as a process of its own, against the container's boot-time ones.
#[test]
fn late_processes_against_the_baseline() {
    let facts = |r: &Reply| parse_probe(&r.render(), "N0NCE", r.since).unwrap();
    let base = facts(&Reply::default());
    let b = |hashes: &[u32]| ProcBaseline { container: CONTAINER.into(), hashes: hashes.iter().copied().collect() };
    // The image's own processes, started at boot: nothing.
    assert_eq!(late_process(&base, None), Some(false));
    // Within BOOT_GRACE of PID 1: still the container's own; one jiffy past it: a process.
    let at = |j: u64| with_proc((123_456 + j, 99, false, "svc".into()));
    assert_eq!(late_process(&facts(&at(BOOT_GRACE * 100)), None), Some(false));
    assert_eq!(late_process(&facts(&at(BOOT_GRACE * 100 + 1)), None), Some(true));
    // Idle by itself: sshd's, systemd's, cron's own, tmux's server, a plain interactive shell.
    for name in ["sshd", "sshd-session", "systemd", "(sd-pam)", "cron", "CRON", "tmux: server"] {
        assert_eq!(late_process(&facts(&with_proc(late(name))), None), Some(false), "{name}");
    }
    let shell = (123_456 + 3600 * 100, 5, true, "bash".to_string());
    assert_eq!(late_process(&facts(&with_proc(shell)), None), Some(false), "an interactive shell");
    // Not: a script run by a shell (the probe's `p`), an editor, a kernel, mosh, a screen.
    for name in ["bash", "vim", "python3", "mosh-server", "screen", "node"] {
        assert_eq!(late_process(&facts(&with_proc(late(name))), None), Some(true), "{name}");
    }
    // A boot-time service restarted later (its command line's cksum in the baseline) isn't —
    // after a reboot, or in a new container, too: the baseline is the FIRST boot's (RS7c).
    let restarted = facts(&with_proc((123_456 + 3600 * 100, 33, false, "jupyter-lab".into())));
    assert_eq!(late_process(&restarted, None), Some(true));
    assert_eq!(late_process(&restarted, Some(&b(&[33]))), Some(false));
    let earlier_boot = |h: &[u32]| ProcBaseline { container: "x/1".into(), ..b(h) };
    assert_eq!(late_process(&restarted, Some(&earlier_boot(&[11, 33]))), Some(false), "the first boot's services, after a reboot");
    assert_eq!(late_process(&restarted, Some(&earlier_boot(&[33]))), Some(true), "PID 1's start.sh isn't in that baseline");
    // RS7c: after a reboot (a Hetzner VM keeps its disk), an EARLY process the first boot
    // didn't have counts — a participant's `@reboot` cron or enabled unit; in the first boot
    // itself the boot window still exempts it; the image's own early services never count.
    let early = facts(&with_proc((123_456 + 50, 77, false, "python".into())));
    assert_eq!(late_process(&early, Some(&b(&[11, 22, 33]))), Some(false), "the first boot's own window");
    assert_eq!(late_process(&early, None), Some(false), "a first look: its boot's window");
    assert_eq!(late_process(&early, Some(&earlier_boot(&[11, 22, 33]))), Some(true), "a later boot: not the image's");
    assert_eq!(late_process(&base, Some(&earlier_boot(&[11, 22, 33]))), Some(false), "the image's services after a reboot");
    // Can't tell: no list, a capped list, no container start.
    assert_eq!(late_process(&facts(&Reply { procs: None, ..Default::default() }), None), None);
    assert_eq!(late_process(&facts(&Reply { proc_overflow: true, ..Default::default() }), None), None);
    // The baseline: the first boot's early cksums, extended only by later looks at that boot;
    // a failed look keeps what was known.
    let first = next_baseline(None, &Ok(base.clone())).unwrap();
    assert_eq!(first, b(&[11, 22, 33]));
    let more = with_proc((123_456 + 50, 44, false, "late-boot".into()));
    assert_eq!(next_baseline(Some(&first), &Ok(facts(&more))).unwrap(), b(&[11, 22, 33, 44]));
    assert_eq!(next_baseline(Some(&first), &Ok(facts(&with_proc(late("python"))))).unwrap(), first, "a late process never joins");
    let moved = Reply { pid1_start: "200000".into(), procs: Some(vec![(200_000, 7, false, "start.sh".into())]), ..Default::default() };
    assert_eq!(next_baseline(Some(&first), &Ok(facts(&moved))).unwrap(), first, "a later boot never adds to it (RS7c)");
    assert_eq!(next_baseline(None, &Ok(facts(&moved))).unwrap().hashes, [7].into_iter().collect(), "a pod first seen there");
    assert_eq!(next_baseline(Some(&first), &Err(ProbeError::Failed)).unwrap(), first);
    assert_eq!(next_baseline(None, &Ok(facts(&Reply { procs: None, ..Default::default() }))), None);
}

/// Spec R15–R23: each signal alone flips an idle pod to Active (with its reason) or Unknown;
/// the GPU is required on RunPod, not on Hetzner; no baseline, a counter reset, a container
/// change, skew and drift, keep, NotCovered.
#[test]
fn classify_each_signal() {
    let idle = Reply::default();
    let p = prev(&idle);
    let read0 = JobsOutcome::Read { busy: 0 };
    assert_eq!(classify_reply(&idle, Some(&p), read0, "runpod").verdict, Verdict::Idle);
    let est_in = format!("01 {} {}", v4([172, 17, 0, 2], 8888), v4([100, 64, 0, 1], 3000));
    let est_out = format!("01 {} {}", v4([172, 17, 0, 2], 41000), v4([104, 18, 0, 1], 443));
    let with_tcp = |extra: String| {
        let mut r = Reply::default();
        r.tcp.as_mut().unwrap().push(extra);
        r
    };
    // (case, reply, jobs, verdict, reason)
    let cases: Vec<(&str, Reply, JobsOutcome, Verdict, Reason)> = vec![
        ("session", Reply { sshd: [Reply::default().sshd, vec![(5000, "sshd: root@pts/0".into())]].concat(), ..Default::default() }, read0, Verdict::Active, Reason::Session),
        ("who", Reply { who: "1".into(), ..Default::default() }, read0, Verdict::Active, Reason::Who),
        ("inbound", with_tcp(est_in), read0, Verdict::Active, Reason::Inbound),
        ("outbound", with_tcp(est_out), read0, Verdict::Active, Reason::Outbound),
        ("many sockets", Reply { tcp_overflow: true, ..Default::default() }, read0, Verdict::Active, Reason::ManySockets),
        ("cpu window", Reply { cpu: (Some(1_000_000_000), Some(1_003_000_000)), ..Default::default() }, read0, Verdict::Active, Reason::Cpu),
        ("net window", Reply { net: (Some(5_000_000), Some(7_000_000)), ..Default::default() }, read0, Verdict::Active, Reason::Net),
        ("gpu util", Reply { gpu: Some((0..5).map(|_| (0, vec!["6, 3".to_string()])).collect()), ..Default::default() }, read0, Verdict::Active, Reason::GpuUtil),
        ("gpu mem at 0%", Reply { gpu: Some((0..5).map(|_| (0, vec!["0, 1025".to_string()])).collect()), ..Default::default() }, read0, Verdict::Active, Reason::GpuMem),
        ("gpu procs", Reply { gpu_procs: Some(1), ..Default::default() }, read0, Verdict::Active, Reason::GpuProcs),
        ("tmux", Reply { tmux: "ok", panes: vec!["python".into()], ..Default::default() }, read0, Verdict::Active, Reason::Tmux),
        ("screen", Reply { screen: "1".into(), ..Default::default() }, read0, Verdict::Active, Reason::Screen),
        ("files", Reply { files: "hit", ..Default::default() }, read0, Verdict::Active, Reason::Files),
        ("job", Reply::default(), JobsOutcome::Read { busy: 1 }, Verdict::Active, Reason::Job),
        ("keep", Reply { keep: true, ..Default::default() }, read0, Verdict::Active, Reason::Keep),
        ("workspace", Reply { workspace: "has_data", ..Default::default() }, read0, Verdict::Active, Reason::Workspace),
        ("repo outside", Reply { repo: "outside_home", ..Default::default() }, read0, Verdict::Active, Reason::RepoOutside),
        ("sessions unknown", Reply { anc: vec![1], ..Default::default() }, read0, Verdict::Unknown, Reason::SessionsUnknown),
        ("who failed", Reply { who: "-".into(), ..Default::default() }, read0, Verdict::Unknown, Reason::WhoUnknown),
        ("tcp unreadable", Reply { tcp: None, ..Default::default() }, read0, Verdict::Unknown, Reason::TcpUnknown),
        ("cpu missing", Reply { cpu: (None, None), ..Default::default() }, read0, Verdict::Unknown, Reason::CpuUnknown),
        ("net missing", Reply { net: (None, None), ..Default::default() }, read0, Verdict::Unknown, Reason::NetUnknown),
        ("cpu window reset", Reply { cpu: (Some(9_000_000_000), Some(1_000_050_000)), ..Default::default() }, read0, Verdict::Unknown, Reason::CounterReset),
        ("gpu hung", Reply { gpu: Some(vec![(124, vec![])]), ..Default::default() }, read0, Verdict::Unknown, Reason::GpuUnknown),
        ("no nvidia-smi on runpod", Reply { gpu: None, ..Default::default() }, read0, Verdict::Unknown, Reason::GpuUnknown),
        ("gpu procs query failed", Reply { gpu_procs: None, ..Default::default() }, read0, Verdict::Unknown, Reason::GpuUnknown),
        ("tmux failed", Reply { tmux: "err", ..Default::default() }, read0, Verdict::Unknown, Reason::TmuxUnknown),
        ("screen unreadable", Reply { screen: "err".into(), ..Default::default() }, read0, Verdict::Unknown, Reason::ScreenUnknown),
        ("scan timed out", Reply { files: "timeout", ..Default::default() }, read0, Verdict::Unknown, Reason::FilesTimeout),
        ("scan failed", Reply { files: "err", ..Default::default() }, read0, Verdict::Unknown, Reason::FilesError),
        ("jobs unreadable", Reply::default(), JobsOutcome::Failed, Verdict::Unknown, Reason::JobsUnknown),
        ("workspace err", Reply { workspace: "err", ..Default::default() }, read0, Verdict::Unknown, Reason::WorkspaceUnknown),
        ("repo err", Reply { repo: "err", ..Default::default() }, read0, Verdict::Unknown, Reason::RepoUnknown),
        ("mount under home", Reply { home_mounts: "1".into(), ..Default::default() }, read0, Verdict::Unknown, Reason::HomeMounts),
        ("home mounts unknown", Reply { home_mounts: "-".into(), ..Default::default() }, read0, Verdict::Unknown, Reason::HomeMounts),
        ("window too short", Reply { window: 3, ..Default::default() }, read0, Verdict::Unknown, Reason::WindowBad),
        ("identity", Reply { id: "other".into(), ..Default::default() }, read0, Verdict::Unknown, Reason::Identity),
        ("identity empty", Reply { id: String::new(), ..Default::default() }, read0, Verdict::Unknown, Reason::Identity),
        ("container unknown", Reply { boot_id: String::new(), ..Default::default() }, read0, Verdict::Unknown, Reason::ContainerUnknown),
        // RS2: never copied by the reap's backup, or outside what it reads.
        ("sweep folder written", Reply { uncovered: "hit", ..Default::default() }, read0, Verdict::Active, Reason::Uncovered),
        ("sweep scan timed out", Reply { uncovered: "timeout", ..Default::default() }, read0, Verdict::Unknown, Reason::FilesTimeout),
        ("sweep scan failed", Reply { uncovered: "err", ..Default::default() }, read0, Verdict::Unknown, Reason::FilesError),
        ("data on another mount", Reply { mounts: "has_data", ..Default::default() }, read0, Verdict::Active, Reason::OtherMount),
        ("mounts unreadable", Reply { mounts: "err", ..Default::default() }, read0, Verdict::Unknown, Reason::MountsUnknown),
        // RS6: a home that couldn't be resolved, or is a link to another filesystem.
        ("home a cross-fs link", Reply { home: "link", ..Default::default() }, read0, Verdict::Unknown, Reason::HomeLink),
        ("home unresolvable", Reply { home: "err", ..Default::default() }, read0, Verdict::Unknown, Reason::HomeUnknown),
        // RS7: a process started after boot (a dormant `nohup python train.py`), or no list.
        ("late process", with_proc(late("python")), read0, Verdict::Active, Reason::Process),
        ("mosh-server", with_proc(late("mosh-server")), read0, Verdict::Active, Reason::Process),
        ("processes unreadable", Reply { procs: None, ..Default::default() }, read0, Verdict::Unknown, Reason::ProcessesUnknown),
        // R2-2: what the backup never reads — a link out of the home, data outside it.
        ("link out of the home", Reply { links: "hit", ..Default::default() }, read0, Verdict::Active, Reason::LinkOutside),
        ("links unreadable", Reply { links: "err", ..Default::default() }, read0, Verdict::Unknown, Reason::LinksUnknown),
        ("data outside the home", Reply { roots: "hit", ..Default::default() }, read0, Verdict::Active, Reason::OutsideData),
        ("outside scan out of time", Reply { roots: "timeout", ..Default::default() }, read0, Verdict::Unknown, Reason::OutsideUnknown),
        ("outside scan no anchor", Reply { roots: "err", ..Default::default() }, read0, Verdict::Unknown, Reason::OutsideUnknown),
        // RS7: scheduled work, an open terminal.
        ("scheduled", Reply { sched: "hit", ..Default::default() }, read0, Verdict::Active, Reason::Scheduled),
        ("schedules unreadable", Reply { sched: "err", ..Default::default() }, read0, Verdict::Unknown, Reason::ScheduledUnknown),
        ("terminal", Reply { ttys: "2", ..Default::default() }, read0, Verdict::Active, Reason::Terminal),
        ("terminals unknown", Reply { ttys: "-", ..Default::default() }, read0, Verdict::Unknown, Reason::SessionsUnknown),
        ("process list capped", Reply { proc_overflow: true, ..Default::default() }, read0, Verdict::Unknown, Reason::ProcessesUnknown),
    ];
    for (case, r, jobs, verdict, reason) in cases {
        let o = classify_reply(&r, Some(&prev(&r)), jobs, "runpod");
        assert_eq!(o.verdict, verdict, "{case}: {:?}", o.reasons);
        assert!(o.reasons.contains(&reason), "{case}: {:?}", o.reasons);
    }
    // Activity wins over "can't tell": the row says what the operator needs to see.
    let both = Reply { who: "2".into(), files: "timeout", ..Default::default() };
    assert_eq!(classify_reply(&both, Some(&prev(&both)), read0, "runpod").reasons, [Reason::Who]);
    // The GPU isn't required on Hetzner.
    let h = Reply { gpu: None, ..Default::default() };
    assert_eq!(classify_reply(&h, Some(&prev(&h)), read0, "hetzner").verdict, Verdict::Idle);
    // Cumulative: no baseline; a container change; a counter reset; over the threshold.
    assert!(classify_reply(&idle, None, read0, "runpod").reasons.contains(&Reason::NoBaseline));
    let other_box = Observation { container: Some("x/1".into()), ..prev(&idle) };
    assert!(classify_reply(&idle, Some(&other_box), read0, "runpod").reasons.contains(&Reason::ContainerChanged));
    let reset = Observation { cpu_usec: Some(u64::MAX / 2), ..prev(&idle) };
    assert!(classify_reply(&idle, Some(&reset), read0, "runpod").reasons.contains(&Reason::CounterReset));
    let busy = Observation { cpu_usec: Some(idle.cpu.1.unwrap() - 91_000_000), ..prev(&idle) };
    assert_eq!(classify_reply(&idle, Some(&busy), read0, "runpod").reasons, [Reason::Cpu], "0.101 cores since the last look");
    let calm = Observation { cpu_usec: Some(idle.cpu.1.unwrap() - 89_000_000), ..prev(&idle) };
    assert_eq!(classify_reply(&idle, Some(&calm), read0, "runpod").verdict, Verdict::Idle, "0.099 cores");
    let big_net = Reply { net: (Some(200_000_000), Some(200_002_000)), ..Default::default() };
    let downloads = Observation { net_bytes: Some(200_002_000 - 91_000_000), ..prev(&big_net) };
    assert_eq!(classify_reply(&big_net, Some(&downloads), read0, "runpod").reasons, [Reason::Net], "101 KB/s since the last look");
    let trickle = Observation { net_bytes: Some(200_002_000 - 89_000_000), ..prev(&big_net) };
    assert_eq!(classify_reply(&big_net, Some(&trickle), read0, "runpod").verdict, Verdict::Idle, "99 KB/s");
    let gapped = Observation { t: idle.now() - MAX_GAP - 1, pod_t: Some(idle.now() - MAX_GAP - 1), ..prev(&idle) };
    assert!(classify_reply(&idle, Some(&gapped), read0, "runpod").reasons.contains(&Reason::NoBaseline));
    let unverified = Observation { clock_ok: false, ..prev(&idle) };
    assert!(classify_reply(&idle, Some(&unverified), read0, "runpod").reasons.contains(&Reason::NoBaseline));
    // Skew: 300 s ok, 301 s not. Drift: 120 s ok, 121 s not.
    let facts = parse_probe(&idle.render(), "N0NCE", idle.since);
    let at = |now: u64, p: &Observation| classify(Some(p), &facts, read0, &ClassifyCtx { provider: "runpod", listed_id: "pod1", now, clock_ok: true, baseline: None });
    let mut p2 = prev(&idle);
    assert_eq!(at(idle.now() + 300, &Observation { t: idle.now() + 300 - 900, ..p2.clone() }).verdict, Verdict::Idle);
    let skewed = at(idle.now() + 301, &p2);
    assert_eq!((skewed.verdict, skewed.reasons.clone()), (Verdict::Unknown, vec![Reason::Clock]));
    p2.pod_t = Some(idle.now() - 900 - 120);
    assert_eq!(at(idle.now(), &p2).verdict, Verdict::Idle, "drift 120 s");
    p2.pod_t = Some(idle.now() - 900 - 121);
    assert_eq!(at(idle.now(), &p2).reasons, [Reason::Clock], "drift 121 s");
    // Unverified control clock: never Idle.
    let o = classify(Some(&prev(&idle)), &facts, read0, &ClassifyCtx { provider: "runpod", listed_id: "pod1", now: idle.now(), clock_ok: false, baseline: None });
    assert_eq!((o.verdict, o.clock_ok), (Verdict::Unknown, false));
    // A failed or garbled probe, or one about another pod, keeps no counters (no baseline).
    for f in [Err(ProbeError::Failed), Err(ProbeError::Garbled), parse_probe(&Reply { id: "x".into(), ..Default::default() }.render(), "N0NCE", Since::Back(900))] {
        let o = classify(None, &f, read0, &ClassifyCtx { provider: "runpod", listed_id: "pod1", now: idle.now(), clock_ok: true, baseline: None });
        assert_eq!(o.verdict, Verdict::Unknown);
        assert!(o.cpu_usec.is_none() && o.net_bytes.is_none(), "{o:?}");
    }
}

/// `quiet_now` (R35): point-in-time signals only — the cumulative counters (with the backup's
/// own traffic in them) are never read — plus the same id and container.
#[test]
fn quiet_now_ignores_cumulative_counters() {
    let r = Reply::default();
    let ctx = QuietCtx { provider: "runpod", id: "pod1", container: CONTAINER, now: r.now(), baseline: None };
    let facts = parse_probe(&r.render(), "N0NCE", r.since);
    assert!(quiet_now(&facts, JobsOutcome::Read { busy: 0 }, &ctx).is_ok());
    // A huge CPU/network count since any earlier look doesn't matter: nothing to compare with.
    let big = Reply { cpu: (Some(u64::MAX / 4), Some(u64::MAX / 4 + 10_000)), net: (Some(1 << 50), Some((1 << 50) + 100)), ..Default::default() };
    assert!(quiet_now(&parse_probe(&big.render(), "N0NCE", big.since), JobsOutcome::Read { busy: 0 }, &ctx).is_ok());
    // …but the window does.
    let busy = Reply { cpu: (Some(0), Some(3_000_000)), ..Default::default() };
    assert_eq!(quiet_now(&parse_probe(&busy.render(), "N0NCE", busy.since), JobsOutcome::Read { busy: 0 }, &ctx).unwrap_err(), [Reason::Cpu]);
    let cases: Vec<(Reply, JobsOutcome, Reason)> = vec![
        (Reply { id: "other".into(), ..Default::default() }, JobsOutcome::Read { busy: 0 }, Reason::Identity),
        (Reply { pid1_start: "999".into(), ..Default::default() }, JobsOutcome::Read { busy: 0 }, Reason::ContainerChanged),
        (Reply { keep: true, ..Default::default() }, JobsOutcome::Read { busy: 0 }, Reason::Keep),
        (Reply { files: "hit", ..Default::default() }, JobsOutcome::Read { busy: 0 }, Reason::Files),
        (Reply::default(), JobsOutcome::Failed, Reason::JobsUnknown),
        (Reply { who: "1".into(), ..Default::default() }, JobsOutcome::Read { busy: 0 }, Reason::Who),
    ];
    for (r2, jobs, reason) in cases {
        let e = quiet_now(&parse_probe(&r2.render(), "N0NCE", r2.since), jobs, &ctx).unwrap_err();
        assert!(e.contains(&reason), "{reason:?}: {e:?}");
    }
    assert_eq!(quiet_now(&Err(ProbeError::Failed), JobsOutcome::Read { busy: 0 }, &ctx).unwrap_err(), [Reason::ProbeFailed]);
}

/// The previous look `gap` seconds before `r`, with `cpu_since` µs of CPU used since it.
fn gap_prev(r: &Reply, gap: u64, cpu_since: u64) -> Observation {
    Observation {
        t: r.now() - gap,
        pod_t: Some(r.now() - gap),
        counters_t: Some(r.t0 + r.window - gap),
        cpu_usec: Some(r.cpu.1.unwrap() - cpu_since),
        net_bytes: Some(r.net.1.unwrap() - 1_000),
        ..prev(r)
    }
}

/// Live W7 B-H1b: a `reap run` 29 s after the cron's read `cpu` — the reaper's own look (the
/// root SSH login starting `user@0.service`, the probe) costs the pod ~5 s of CPU, 0.17 cores
/// over 29 s, and every policy's streak reset. The cumulative check averages over at least
/// CPU_CUM_MIN_SPAN: a look's own cost (up to 18 CPU-seconds) never reads as use, a real job's
/// CPU in a short gap still does, and a gap of CPU_CUM_MIN_SPAN or more is judged as before.
#[test]
fn w7_b_h1b_a_looks_own_cpu_never_reads_as_use() {
    let r = Reply::default();
    let read0 = JobsOutcome::Read { busy: 0 };
    let verdict = |gap: u64, cpu: u64| classify_reply(&r, Some(&gap_prev(&r, gap, cpu)), read0, "runpod");
    // The live case, and the reaper's own cost at any short gap (3× what was measured).
    assert_eq!(verdict(29, 5_300_000).verdict, Verdict::Idle, "the reaper's own look, 29 s after the last");
    for gap in [5, 29, 60, 120, 179] {
        assert_eq!(verdict(gap, 18_000_000).verdict, Verdict::Idle, "{gap} s, 18 CPU-seconds");
    }
    // A real job in a short gap: a full core for 20 s of it (and the look's own 5 s).
    for gap in [25, 29, 60, 179] {
        assert_eq!(verdict(gap, 25_000_000).reasons, [Reason::Cpu], "{gap} s, a core for 20 s");
    }
    // At and past CPU_CUM_MIN_SPAN, exactly the old rule (0.10 cores since the last look).
    for gap in [CPU_CUM_MIN_SPAN, 300, 900, MAX_GAP] {
        let over = gap * 100_000 + gap; // 0.10001 cores
        assert_eq!(verdict(gap, over).reasons, [Reason::Cpu], "{gap} s over");
        assert_eq!(verdict(gap, gap * 100_000 - gap).verdict, Verdict::Idle, "{gap} s under");
    }
    // A job still running is the window's, whatever the gap.
    let busy = Reply { cpu: (Some(1_000_000_000), Some(1_003_000_000)), ..Default::default() };
    let p = gap_prev(&busy, 29, 0);
    assert_eq!(classify_reply(&busy, Some(&p), read0, "runpod").reasons, [Reason::Cpu]);
}

/// Live W7 B-H3b: four minutes after a rehearsal the next look read `cpu, net` — the counters
/// since the deciding look held the reaper's own 1.1 GB backup — so a rehearsal, or any attempt
/// stopped after its backup, reset the streak by itself. The post-backup look is recorded as an
/// observation: its counters aren't judged (the backup is in them) but become the next look's
/// baseline; otherwise it is a real look — what it sees counts.
#[test]
fn w7_b_h3b_the_post_backup_look_is_the_next_baseline_and_a_real_look() {
    let read0 = JobsOutcome::Read { busy: 0 };
    let r = Reply { net: (Some(2_000_000_000), Some(2_000_002_000)), ..Default::default() };
    // The deciding look 2 minutes before: the backup since moved 1.1 GB and burned 60 s of CPU.
    let deciding = Observation { net_bytes: Some(r.net.1.unwrap() - 1_100_000_000), ..gap_prev(&r, 120, 60_000_000) };
    let ctx = |now: u64| ClassifyCtx { provider: "runpod", listed_id: "pod1", now, clock_ok: true, baseline: None };
    let facts = |reply: &Reply| parse_probe(&reply.render(), &reply.nonce, reply.since);
    // As a regular look, the backup reads as use…
    assert_eq!(classify(Some(&deciding), &facts(&r), read0, &ctx(r.now())).reasons, [Reason::Cpu, Reason::Net]);
    // …as the post-backup look it doesn't, and its counters are kept, as they are now.
    let post = classify_post_backup(Some(&deciding), &facts(&r), read0, &ctx(r.now()));
    assert_eq!((post.verdict, post.reasons.clone()), (Verdict::Idle, vec![]));
    assert_eq!((post.cpu_usec, post.net_bytes, post.counters_t), (r.cpu.1, r.net.1, Some(r.t0 + r.window)));
    assert_eq!((post.container.as_deref(), post.pod_t, post.scan_from), (Some(CONTAINER), Some(r.now()), Some(r.t0 - 900)));
    // The next look, 4 minutes on, measured from it: quiet.
    let next = Reply {
        t0: r.t0 + 240,
        cpu: (Some(r.cpu.1.unwrap() + 240_000), Some(r.cpu.1.unwrap() + 245_000)),
        net: (Some(r.net.1.unwrap() + 2_400), Some(r.net.1.unwrap() + 2_500)),
        ..Default::default()
    };
    assert_eq!(classify(Some(&post), &facts(&next), read0, &ctx(next.now())).verdict, Verdict::Idle);
    // A real look: activity counts, what it can't read is unknown.
    let with_session = Reply { sshd: [Reply::default().sshd, vec![(5000, "sshd: root@pts/0".into())]].concat(), ..Default::default() };
    let cases: Vec<(&str, Reply, JobsOutcome, Verdict, Reason)> = vec![
        ("a login meanwhile", with_session, read0, Verdict::Active, Reason::Session),
        ("a file saved since the deciding look's scan began", Reply { files: "hit", ..Default::default() }, read0, Verdict::Active, Reason::Files),
        ("busy right now", Reply { cpu: (Some(1_000_000_000), Some(1_003_000_000)), ..Default::default() }, read0, Verdict::Active, Reason::Cpu),
        ("a job", Reply::default(), JobsOutcome::Read { busy: 1 }, Verdict::Active, Reason::Job),
        ("keep", Reply { keep: true, ..Default::default() }, read0, Verdict::Active, Reason::Keep),
        ("a process started meanwhile", with_proc(late("python")), read0, Verdict::Active, Reason::Process),
        ("jobs unreadable", Reply::default(), JobsOutcome::Failed, Verdict::Unknown, Reason::JobsUnknown),
        ("another pod", Reply { id: "other".into(), ..Default::default() }, read0, Verdict::Unknown, Reason::Identity),
        (
            "another container",
            Reply { pid1_start: "999".into(), procs: Some(vec![(999, 11, false, "start.sh".into())]), ..Default::default() },
            read0,
            Verdict::Unknown,
            Reason::ContainerChanged,
        ),
    ];
    for (case, reply, jobs, verdict, reason) in cases {
        let o = classify_post_backup(Some(&deciding), &facts(&reply), jobs, &ctx(reply.now()));
        assert_eq!(o.verdict, verdict, "{case}: {:?}", o.reasons);
        assert!(o.reasons.contains(&reason), "{case}: {:?}", o.reasons);
    }
    // Its clock: drift against the deciding look, and an unverified control clock.
    let drifted = Observation { pod_t: Some(r.now() - 120 - 121), ..deciding.clone() };
    assert_eq!(classify_post_backup(Some(&drifted), &facts(&r), read0, &ctx(r.now())).reasons, [Reason::Clock]);
    // A backup longer than MAX_GAP (W7LR-3): the drift is still checked against the deciding look.
    let long = gap_prev(&r, 3 * 3600, 0);
    assert_eq!(classify_post_backup(Some(&long), &facts(&r), read0, &ctx(r.now())).verdict, Verdict::Idle);
    let long_drift = Observation { pod_t: Some(r.now() - 3 * 3600 - 121), ..long };
    assert_eq!(classify_post_backup(Some(&long_drift), &facts(&r), read0, &ctx(r.now())).reasons, [Reason::Clock]);
    let unverified = ClassifyCtx { clock_ok: false, ..ctx(r.now()) };
    assert_eq!(classify_post_backup(Some(&deciding), &facts(&r), read0, &unverified).verdict, Verdict::Unknown);
    // No deciding look before it: nothing to measure from.
    assert_eq!(classify_post_backup(None, &facts(&r), read0, &ctx(r.now())).reasons, [Reason::NoBaseline]);
    // A failed probe: no facts, no counters.
    let failed = classify_post_backup(Some(&deciding), &Err(ProbeError::Failed), read0, &ctx(r.now()));
    assert_eq!((failed.verdict, failed.cpu_usec), (Verdict::Unknown, None));
}

fn streak_of(ts: &[u64]) -> Vec<Observation> {
    ts.iter().map(|t| obs(*t, Verdict::Idle)).collect()
}

/// `idle_streak` and `idle_due`: span W ± 1 s, the 40-minute gap boundary, the MIN_OBS floor, an
/// unverified clock or a coverage gap in the middle, and the deciding look's full-window scan.
#[test]
fn idle_streak_and_due_boundaries() {
    let w = 3600; // min_obs = 4
    let pol = |o: Vec<Observation>| PodPolicy { observations: o, ..policy(&rp("devtest-alpha", "ra"), None, Some(w)) };
    let full = |mut o: Vec<Observation>| {
        let l = o.last_mut().unwrap();
        l.scan_from = Some(l.pod_t.unwrap() - w - SCAN_MARGIN);
        o
    };
    let s = streak_of(&[T, T + 1200, T + 2400, T + 3600]);
    assert_eq!(idle_streak(&s), Some(Streak { first_t: T, last_t: T + 3600, count: 4 }));
    assert!(idle_due(&pol(full(s.clone()))), "span = W, 4 obs, full scan");
    assert!(!idle_due(&pol(s.clone())), "the deciding look must scan the whole window");
    assert!(!idle_due(&pol(full(streak_of(&[T + 1, T + 1200, T + 2400, T + 3600])))), "W − 1 s");
    assert!(!idle_due(&pol(full(streak_of(&[T, T + 3600])))), "under MIN_OBS");
    // Gap: 40 min ok, 40 min + 1 s breaks it.
    let mut g = streak_of(&[T, T + MAX_GAP, T + MAX_GAP + 600, T + MAX_GAP + 1200]);
    g[1].scan_from = Some(T);
    assert_eq!(idle_streak(&g).unwrap().count, 4);
    let mut g2 = streak_of(&[T, T + MAX_GAP + 1, T + MAX_GAP + 600, T + MAX_GAP + 1200]);
    g2[1].scan_from = Some(T);
    assert_eq!(idle_streak(&g2).unwrap().count, 3);
    // A clock_ok=false look, an Active one or a coverage gap in the middle restarts it.
    let mut c = streak_of(&[T, T + 600, T + 1200, T + 1800]);
    c[1].clock_ok = false;
    assert_eq!(idle_streak(&c).unwrap().count, 2);
    let mut a = streak_of(&[T, T + 600, T + 1200, T + 1800]);
    a[2].verdict = Verdict::Active;
    assert_eq!(idle_streak(&a).unwrap().count, 1);
    let mut cov = streak_of(&[T, T + 600, T + 1200]);
    cov[2].scan_from = Some(T + 601);
    assert_eq!(idle_streak(&cov).unwrap().count, 1, "a scan starting after the previous look ended");
    let mut moved = streak_of(&[T, T + 600, T + 1200]);
    moved[0].container = Some("other/1".into());
    assert_eq!(idle_streak(&moved).unwrap().count, 2);
    let mut last_unknown = streak_of(&[T, T + 600]);
    last_unknown[1].verdict = Verdict::Unknown;
    assert_eq!(idle_streak(&last_unknown), None);
    assert_eq!(idle_streak(&[]), None);
}

/// Expiry: passed, first seen ≥ 10 min ago (599 s no, 600 s yes), two consecutive Idle looks.
#[test]
fn expiry_due_boundaries() {
    let base = PodPolicy {
        expires_at: Some(T),
        first_due_seen: Some(T),
        observations: streak_of(&[T + 300, T + 600]),
        ..policy(&rp("devtest-alpha", "ra"), None, None)
    };
    assert!(expiry_due(&base, T + 600));
    assert!(!expiry_due(&base, T + 599));
    assert!(!expiry_due(&PodPolicy { first_due_seen: None, ..base.clone() }, T + 9999));
    assert!(!expiry_due(&PodPolicy { expires_at: Some(T + 700), ..base.clone() }, T + 600), "not yet");
    let mut one = base.clone();
    one.observations[0].verdict = Verdict::Active;
    assert!(!expiry_due(&one, T + 600), "only one quiet look");
    // The due rows, in order.
    let now = T + 600;
    assert_eq!(due_row(&base, now, Mode::Apply).row, Row::Candidate);
    assert_eq!(due_row(&base, now, Mode::Observe).row, Row::WouldReap);
    assert_eq!(due_row(&base, now, Mode::Rehearse).row, Row::Candidate);
    assert_eq!(due_row(&base, now, Mode::Apply).trigger, Some(Trigger::Expiry));
    assert_eq!(due_row(&PodPolicy { first_due_seen: None, ..base.clone() }, now, Mode::Apply).row, Row::ExpiryConfirming, "R25");
    assert_eq!(due_row(&PodPolicy { first_due_seen: Some(now - 10), ..base.clone() }, now, Mode::Apply).row, Row::ExpiryConfirming, "R25");
    assert_eq!(due_row(&one, now, Mode::Apply).row, Row::ExpiryWaitQuiet, "R26");
    assert_eq!(due_row(&PodPolicy { retry_after: Some(now + 1), ..base.clone() }, now, Mode::Apply).row, Row::BackoffAfterFailure, "R27");
    assert_eq!(due_row(&PodPolicy { retry_after: Some(now), ..base.clone() }, now, Mode::Apply).row, Row::Candidate);
    assert_eq!(due_row(&PodPolicy { expires_at: Some(now + 50), ..base.clone() }, now, Mode::Apply).row, Row::Scheduled, "R24");
    let idle = PodPolicy { idle_after_secs: Some(3600), expires_at: None, ..base };
    assert_eq!(due_row(&idle, now, Mode::Apply).row, Row::StreakShort, "R24");
}

/// R15–R22's rows and Q1's notifications: an overdue pod in use or unreachable is notified,
/// never acted on.
#[test]
fn observation_rows_and_overdue_notices() {
    let o = |v: Verdict, r: Vec<Reason>| Observation { verdict: v, reasons: r, ..obs(T, v) };
    let cases = [
        (o(Verdict::Unknown, vec![Reason::ProbeFailed]), Row::ProbeFailed, NotifyKind::OverdueUnreachable),
        (o(Verdict::Unknown, vec![Reason::Identity]), Row::Identity, NotifyKind::OverdueUnreachable),
        (o(Verdict::Unknown, vec![Reason::Clock]), Row::Clock, NotifyKind::OverdueUnreachable),
        (o(Verdict::Unknown, vec![Reason::Garbled]), Row::Garbled, NotifyKind::OverdueUnreachable),
        (o(Verdict::Active, vec![Reason::Keep, Reason::Session]), Row::Keep, NotifyKind::OverdueInUse),
        (o(Verdict::Active, vec![Reason::Workspace]), Row::NotCovered, NotifyKind::OverdueInUse),
        (o(Verdict::Active, vec![Reason::RepoOutside]), Row::NotCovered, NotifyKind::OverdueInUse),
        (o(Verdict::Active, vec![Reason::LinkOutside]), Row::NotCovered, NotifyKind::OverdueInUse),
        (o(Verdict::Active, vec![Reason::OutsideData]), Row::NotCovered, NotifyKind::OverdueInUse),
        (o(Verdict::Active, vec![Reason::Uncovered]), Row::NotCovered, NotifyKind::OverdueInUse),
        (o(Verdict::Active, vec![Reason::GpuUtil]), Row::Active, NotifyKind::OverdueInUse),
        (o(Verdict::Unknown, vec![Reason::NoBaseline]), Row::Unknown, NotifyKind::OverdueUnreachable),
    ];
    for (ob, row, kind) in cases {
        assert_eq!(observation_row(&ob, true).row, row);
        assert_eq!(observation_row(&ob, true).notify, Some(kind), "{row:?}");
        assert_eq!(observation_row(&ob, false).notify, None);
    }
}

/// Caps: `--max` only lowers the run cap; the rolling day caps too.
#[test]
fn caps_left_table() {
    let s = ReapSettings { max_per_run: 2, max_per_day: 10, ..Default::default() };
    for (max, used, today, want) in [(None, 0, 0, 2), (Some(1), 0, 0, 1), (Some(5), 0, 0, 2), (None, 1, 0, 1), (None, 2, 0, 0), (None, 0, 9, 1), (None, 0, 10, 0), (None, 0, 11, 0), (Some(0), 0, 0, 0)] {
        assert_eq!(caps_left(&s, max, used, today), want, "{max:?} {used} {today}");
    }
    let order = order_candidates(vec![
        ("idle-short".into(), Trigger::Idle, None, 100),
        ("exp-late".into(), Trigger::Expiry, Some(T + 10), 0),
        ("idle-long".into(), Trigger::Idle, None, 900),
        ("exp-early".into(), Trigger::Expiry, Some(T), 0),
    ]);
    assert_eq!(order, ["exp-early", "exp-late", "idle-long", "idle-short"]);
}

/// R32–R34's verdicts.
#[test]
fn git_rsync_verify_verdicts() {
    let exit = |code: i32, stdout: &str| StepEnd::Exit { code, stdout: stdout.into() };
    assert_eq!(git_verdict(Some(&exit(0, "PUSHED autocommit-x\n"))), Ok(Some("autocommit-x".into())));
    assert_eq!(git_verdict(Some(&exit(0, "noise\nNO_CHANGES b\n"))), Ok(Some("b".into())));
    assert_eq!(git_verdict(Some(&exit(0, "SKIP main\n"))), Ok(Some("main".into())));
    assert_eq!(git_verdict(None), Ok(None), "no repo: the tiers carry the home");
    for bad in [exit(0, "done\n"), exit(1, "PUSHED b\n"), exit(3, "BIGSTUCK 1 x\n"), StepEnd::Timeout, StepEnd::Killed, StepEnd::Error] {
        assert_eq!(git_verdict(Some(&bad)), Err(Row::GitFailed), "{bad:?}");
    }
    let stats = |n: u64| format!("Number of files: 10 (reg: 8, dir: 2)\nNumber of regular files transferred: {n}\nTotal transferred file size: 0 bytes\n");
    assert_eq!(verify_verdict(&exit(0, &stats(0))), Ok(()));
    assert_eq!(verify_verdict(&exit(0, &stats(3))), Err(Some(3)));
    assert_eq!(verify_verdict(&exit(0, "no stats here")), Err(None));
    for code in [23, 24, 255] {
        assert_eq!(verify_verdict(&exit(code, &stats(0))), Err(None), "{code}");
    }
    assert_eq!(verify_verdict(&StepEnd::Timeout), Err(None));
    // Every rsync pass must exit 0.
    let ev = good_backup();
    assert!(BackupProof::check(&ev).is_ok());
    for code in [23, 24, 255, 1] {
        let mut e = good_backup();
        e.snapshot[1] = exit(code, "");
        assert_eq!(BackupProof::check(&e).unwrap_err().row, Row::RsyncFailed, "{code}");
        let mut e = good_backup();
        e.big = exit(code, "");
        assert_eq!(BackupProof::check(&e).unwrap_err().row, Row::RsyncFailed, "{code}");
    }
    let mut e = good_backup();
    e.big = StepEnd::Timeout;
    assert_eq!(BackupProof::check(&e).unwrap_err().row, Row::RsyncFailed);
    let mut e = good_backup();
    e.verify = exit(0, &stats(2));
    assert_eq!(BackupProof::check(&e).unwrap_err(), BackupFailure { row: Row::VerifyPending, pending: Some(2) });
}

fn good_backup() -> BackupEvidence {
    let ok = |s: &str| StepEnd::Exit { code: 0, stdout: s.into() };
    BackupEvidence {
        key: "runpod:pod1".into(),
        container: CONTAINER.into(),
        started: T + 2,
        finished: T + 100,
        lock_held: true,
        dest_ok: true,
        git: Some(ok("PUSHED autocommit-devtest-w1d1-alpha\n")),
        snapshot: vec![ok(""), ok("")],
        big: ok(""),
        verify: ok("Number of regular files transferred: 0\n"),
        snapshot_dir: "/b/w1d1/devtest-alpha/".into(),
        big_dir: "/big/devtest-alpha/".into(),
        frozen: Some(Frozen {
            snapshot_dir: "/b/reaped/devtest-alpha/20261012T000000Z-runpod-pod1/snapshot".into(),
            big_dir: "/big/reaped/devtest-alpha/20261012T000000Z-runpod-pod1/big".into(),
            matched: true,
        }),
    }
}

/// Everything a ticket is built from, favourable by default.
#[derive(Clone)]
struct Bundle {
    run: RunInputs,
    policy: PodPolicy,
    decision: Decision,
    decided_now: u64,
    wal_saved: bool,
    wal_at: u64,
    backup: BackupEvidence,
    quiet: Reply,
    /// `quiet` parsed (re-parsed by each fault that changes it — see `requiet`).
    quiet_facts: Result<crate::reap::probe::ProbeFacts, ProbeError>,
    quiet_jobs: JobsOutcome,
    quiet_at: u64,
    relist: Option<Option<Pod>>,
    /// Every other pod the final re-listing returned (all providers that answered).
    others: Vec<Pod>,
    /// Another provider's listing failed at the final re-listing (R2-5).
    partial: bool,
    relist_at: u64,
    relist_now: u64,
    state: ReapState,
    /// The config file's `REAP_ENABLED` and the state's hold, re-read right before the
    /// terminate (RS8).
    enabled_now: bool,
    held_now: bool,
}

fn bundle() -> Bundle {
    let pod = Pod { id: "pod1".into(), ..rp("devtest-alpha", "pod1") };
    let w = 900;
    let mut pol = policy(&pod, None, Some(w));
    pol.observations = streak_of(&[T - 1200, T - 900, T - 600, T - 300, T]);
    let mut state = ReapState::default();
    state.pods.insert(pol.key(), pol.clone());
    let decision = due_row(&pol, T, Mode::Apply);
    Bundle {
        run: RunInputs { mode: Mode::Apply, enabled: true, held: false, clock_ok: true, dest_ok: true, caps_left: 1 },
        policy: pol,
        decision,
        decided_now: T + 1,
        wal_saved: true,
        wal_at: T + 1,
        backup: good_backup(),
        // The post-backup look scans from where the deciding look's scan started (RS4).
        quiet: Reply { t0: T + 130 - 22, since: Since::At(T - MAX_GAP - SCAN_MARGIN), ..Default::default() },
        quiet_facts: Err(ProbeError::Failed),
        quiet_jobs: JobsOutcome::Read { busy: 0 },
        quiet_at: T + 130,
        relist: Some(Some(pod)),
        others: vec![rp("devtest-bravo", "pod2"), hz("devtest-charlie", "h3")],
        partial: false,
        relist_at: T + 140,
        relist_now: T + 141,
        state,
        enabled_now: true,
        held_now: false,
    }
    .requiet()
}

impl Bundle {
    /// Parse the post-backup reply again (after a fault changed it).
    fn requiet(mut self) -> Bundle {
        self.quiet_facts = parse_probe(&self.quiet.render(), "N0NCE", self.quiet.since);
        self
    }
}

fn requiet(b: &mut Bundle) {
    b.quiet_facts = parse_probe(&b.quiet.render(), "N0NCE", b.quiet.since);
}

/// The CLI's sequence of proofs, then the ticket.
fn try_ticket(b: &Bundle) -> Option<ReapTicket> {
    let run = RunGate::check(&b.run).ok()?;
    let trigger = TriggerProof::from_decision(&b.policy, &b.decision, b.decided_now).ok()?;
    let wal = b.wal_saved.then(|| WriteAheadProof::saved(trigger.key(), b.wal_at))?;
    let backup = BackupProof::check(&b.backup).ok()?;
    let qctx = QuietCtx { provider: "runpod", id: &b.policy.id, container: trigger.container(), now: b.quiet_at, baseline: b.policy.proc_baseline.as_ref() };
    let quiet = quiet_now(&b.quiet_facts, b.quiet_jobs, &qctx).ok()?;
    let listed = match &b.relist {
        None => Listed::Failed,
        Some(None) => Listed::Gone,
        Some(Some(p)) => Listed::Pod(p),
    };
    let all_ok: Vec<Pod> = b.relist.iter().flatten().cloned().chain(b.others.iter().cloned()).collect();
    let relist = relist_verdict(listed, &all_ok, b.partial, trigger.key(), &b.policy.name, b.relist_at, b.relist_now).ok()?;
    let recheck = recheck_state(&b.state, trigger.key(), trigger.trigger(), &b.policy.observations, b.relist_now).ok()?;
    let run = run.still(b.enabled_now, b.held_now).ok()?;
    ReapTicket::issue(run, trigger, wal, backup, quiet, relist, recheck).ok()
}

type Fault = (&'static str, fn(&mut Bundle));

/// Every single unfavourable gate input (spec I4's gate inputs).
fn faults() -> Vec<Fault> {
    fn last(b: &mut Bundle) -> &mut Observation {
        b.policy.observations.last_mut().unwrap()
    }
    vec![
        ("observe mode", |b| b.run.mode = Mode::Observe),
        ("rehearse mode", |b| b.run.mode = Mode::Rehearse),
        ("disabled", |b| b.run.enabled = false),
        ("held", |b| b.run.held = true),
        ("clock unverified", |b| b.run.clock_ok = false),
        ("dest unhealthy", |b| b.run.dest_ok = false),
        ("capped", |b| b.run.caps_left = 0),
        ("not a candidate", |b| b.decision = Decision::of(Row::WouldReap)),
        ("no trigger", |b| b.decision.trigger = None),
        ("deciding look active", |b| last(b).verdict = Verdict::Active),
        ("deciding look stale", |b| b.decided_now = T + OBS_FRESH + 1),
        ("deciding look unverified clock", |b| last(b).clock_ok = false),
        ("deciding look partial scan", |b| last(b).scan_from = Some(T - 100)),
        ("streak broken", |b| b.policy.observations[2].verdict = Verdict::Unknown),
        ("write-ahead not saved", |b| b.wal_saved = false),
        ("write-ahead after the backup started", |b| b.wal_at = T + 3),
        ("backup lock busy", |b| b.backup.lock_held = false),
        ("backup dest unhealthy", |b| b.backup.dest_ok = false),
        ("git failed", |b| b.backup.git = Some(StepEnd::Exit { code: 1, stdout: String::new() })),
        ("git no sentinel", |b| b.backup.git = Some(StepEnd::Exit { code: 0, stdout: "ok\n".into() })),
        ("git timed out", |b| b.backup.git = Some(StepEnd::Timeout)),
        ("snapshot exit 23", |b| b.backup.snapshot[0] = StepEnd::Exit { code: 23, stdout: String::new() }),
        ("snapshot .git exit 24", |b| b.backup.snapshot[1] = StepEnd::Exit { code: 24, stdout: String::new() }),
        ("snapshot not run", |b| b.backup.snapshot.clear()),
        ("big exit 255", |b| b.backup.big = StepEnd::Exit { code: 255, stdout: String::new() }),
        ("big timed out", |b| b.backup.big = StepEnd::Timeout),
        ("verify pending", |b| b.backup.verify = StepEnd::Exit { code: 0, stdout: "Number of regular files transferred: 4\n".into() }),
        ("verify no stats", |b| b.backup.verify = StepEnd::Exit { code: 0, stdout: String::new() }),
        ("verify failed", |b| b.backup.verify = StepEnd::Exit { code: 23, stdout: "Number of regular files transferred: 0\n".into() }),
        ("backup of another container", |b| b.backup.container = "other/1".into()),
        ("backup before the decision", |b| b.backup.started = T - 1),
        ("backup of another pod", |b| b.backup.key = "runpod:other".into()),
        ("backup not frozen", |b| b.backup.frozen = None),
        ("frozen copy doesn't match", |b| b.backup.frozen.iter_mut().for_each(|f| f.matched = false)),
        ("frozen into the live snapshot dir", |b| { let d = b.backup.snapshot_dir.clone(); b.backup.frozen.iter_mut().for_each(|f| f.snapshot_dir.clone_from(&d)) }),
        ("frozen into the live big dir", |b| { let d = b.backup.big_dir.clone(); b.backup.frozen.iter_mut().for_each(|f| f.big_dir.clone_from(&d)) }),
        ("quiet scan starts after the decision's", |b| {
            b.quiet.since = Since::At(T - MAX_GAP - SCAN_MARGIN + 1);
            requiet(b)
        }),
        ("a process started after the backup", |b| {
            b.quiet.procs.get_or_insert_with(Vec::new).push((900_000, 77, false, "python".into()));
            requiet(b)
        }),
        ("process list unreadable after", |b| {
            b.quiet.procs = None;
            requiet(b)
        }),
        ("sweep folder written", |b| {
            b.quiet.uncovered = "hit";
            requiet(b)
        }),
        ("data on another mount", |b| {
            b.quiet.mounts = "has_data";
            requiet(b)
        }),
        ("home unresolvable", |b| {
            b.quiet.home = "err";
            requiet(b)
        }),
        ("a link out of the home after", |b| {
            b.quiet.links = "hit";
            requiet(b)
        }),
        ("data outside the home after", |b| {
            b.quiet.roots = "hit";
            requiet(b)
        }),
        ("work scheduled after", |b| {
            b.quiet.sched = "hit";
            requiet(b)
        }),
        ("a terminal opened after", |b| {
            b.quiet.ttys = "1";
            requiet(b)
        }),
        ("quiet probe failed", |b| {
            b.quiet.nonce = "LOST".into();
            requiet(b)
        }),
        ("quiet probe other pod", |b| {
            b.quiet.id = "other".into();
            requiet(b)
        }),
        ("quiet probe new container", |b| {
            b.quiet.pid1_start = "1".into();
            requiet(b)
        }),
        ("session after the backup", |b| {
            b.quiet.sshd.push((6000, "sshd: root@pts/1".into()));
            requiet(b)
        }),
        ("keep after the backup", |b| {
            b.quiet.keep = true;
            requiet(b)
        }),
        ("files changed during the backup", |b| {
            b.quiet.files = "hit";
            requiet(b)
        }),
        ("cpu busy after the backup", |b| {
            b.quiet.cpu = (Some(0), Some(9_000_000));
            requiet(b)
        }),
        ("jobs unreadable after", |b| b.quiet_jobs = JobsOutcome::Failed),
        ("a job started", |b| b.quiet_jobs = JobsOutcome::Read { busy: 1 }),
        ("quiet before the backup ended", |b| b.backup.finished = T + 131),
        ("relist failed", |b| b.relist = None),
        ("gone before the reap", |b| b.relist = Some(None)),
        ("renamed since", |b| b.relist = Some(Some(Pod { name: "devtest-bravo".into(), ..rp("devtest-bravo", "pod1") }))),
        ("stopped since", |b| b.relist = Some(Some(Pod { status: "EXITED".into(), ..rp("devtest-alpha", "pod1") }))),
        ("locked since", |b| b.relist = Some(Some(Pod { locked: Some(true), ..rp("devtest-alpha", "pod1") }))),
        ("lock state unknown", |b| b.relist = Some(Some(Pod { locked: None, ..rp("devtest-alpha", "pod1") }))),
        ("relist stale", |b| b.relist_now = b.relist_at + RELIST_MAX_AGE + 1),
        ("a -new twin listed at the relist", |b| b.others.push(rp("devtest-alpha-new", "pod9"))),
        ("a -old twin listed elsewhere", |b| b.others.push(hz("devtest-alpha-old", "h9"))),
        ("another pod of the same name", |b| b.others.push(hz("devtest-alpha", "h8"))),
        ("another provider's listing failed", |b| b.partial = true),
        ("REAP_ENABLED=0 mid-run", |b| b.enabled_now = false),
        ("held mid-run", |b| b.held_now = true),
        ("relist before the quiet probe", |b| b.relist_at = T + 120),
        ("hold since", |b| b.state.hold = Some(Hold { reason: "x".into(), set_at: T, auto: false })),
        ("pinned since", |b| b.state.pins.push(Pin { name: "devtest-alpha".into(), id: None, until: None, note: None, set_at: T })),
        ("pinned by id since", |b| b.state.pins.push(Pin { name: "x".into(), id: Some("pod1".into()), until: None, note: None, set_at: T })),
        ("policy cleared since", |b| b.state.pods.clear()),
        ("idle window raised since", |b| {
            for p in b.state.pods.values_mut() {
                p.idle_after_secs = Some(86_400);
            }
        }),
    ]
}

/// Spec I4, `inv_ticket_iff_all_gates`: over every single fault, every pair, and 10⁵ seeded
/// random combinations of the gate inputs, a ticket is produced ⇔ every gate is favourable.
#[test]
fn inv_ticket_iff_all_gates() {
    ticket_iff_all_gates(100_000);
}

/// [`inv_ticket_iff_all_gates`] over 10⁶ random combinations, about 100 s:
/// `cargo test --release -p arena-core --lib -- --ignored inv_ticket_iff_all_gates_deep`.
#[test]
#[ignore = "10⁶ combinations, about 100 s"]
fn inv_ticket_iff_all_gates_deep() {
    ticket_iff_all_gates(1_000_000);
}

fn ticket_iff_all_gates(combinations: u32) {
    let base = bundle();
    assert!(try_ticket(&base).is_some(), "the favourable bundle must yield a ticket");
    let fs = faults();
    for (name, f) in &fs {
        let mut b = base.clone();
        f(&mut b);
        assert!(try_ticket(&b).is_none(), "single fault `{name}` still produced a ticket");
    }
    for i in 0..fs.len() {
        for j in i + 1..fs.len() {
            let mut b = base.clone();
            (fs[i].1)(&mut b);
            (fs[j].1)(&mut b);
            assert!(try_ticket(&b).is_none(), "faults `{}` + `{}` produced a ticket", fs[i].0, fs[j].0);
        }
    }
    // splitmix64, seeded: reproducible.
    let mut seed: u64 = 0x5EED0FA4E4;
    let mut next = || {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let (mut with_ticket, mut without) = (0u32, 0u32);
    for _ in 0..combinations {
        let r = next();
        // Each fault on with probability 1/32 (so ~1 in 5 combinations is fault-free).
        let picked: Vec<usize> = (0..fs.len()).filter(|k| (r.rotate_left(*k as u32 * 5) & 31) == 0).collect();
        let mut b = base.clone();
        for k in &picked {
            (fs[*k].1)(&mut b);
        }
        let got = try_ticket(&b).is_some();
        assert_eq!(got, picked.is_empty(), "faults {:?}", picked.iter().map(|k| fs[*k].0).collect::<Vec<_>>());
        if got {
            with_ticket += 1;
        } else {
            without += 1;
        }
    }
    assert!(with_ticket > combinations / 20 && without > combinations / 20, "{with_ticket} / {without}");
}

/// Spec I6, `inv_vetoes_both_triggers`: locked, protected, lock-unknown, pinned, keep-marked,
/// NotCovered and held pods are never terminated, whatever the trigger.
#[test]
fn inv_vetoes_both_triggers() {
    let l = list();
    let naming = Naming { prefix: "devtest", list: &l };
    for trigger in [Trigger::Expiry, Trigger::Idle] {
        // Before any SSH: locked / protected / unknown / pinned never pass the gate.
        let a = rp("devtest-alpha", "ra");
        let mut pol = policy(&a, Some(T - 3600), Some(900));
        pol.first_due_seen = Some(T - 3000);
        let pins = [Pin { name: "devtest-alpha".into(), id: None, until: None, note: None, set_at: T }];
        for (pod, pins, row) in [
            (Pod { locked: Some(true), ..a.clone() }, &[][..], Row::Locked),
            (Pod { locked: None, ..a.clone() }, &[][..], Row::LockUnknown),
            (a.clone(), &pins[..], Row::Pinned),
        ] {
            let listed = vec![pod.clone()];
            let ctx = GateCtx { naming: &naming, runpod_v2: true, pins, listed: &listed, now: T };
            assert_eq!(pre_ssh_gate(&pol, Listed::Pod(&pod), &ctx).err(), Some(row), "{trigger:?}");
        }
        let h = Pod { delete_protected: Some(true), ..hz("devtest-alpha", "7") };
        let hl = vec![h.clone()];
        let ctx = GateCtx { naming: &naming, runpod_v2: true, pins: &[], listed: &hl, now: T };
        assert_eq!(pre_ssh_gate(&pol, Listed::Pod(&h), &ctx).err(), Some(Row::Locked));
        // On the pod: keep and NotCovered are Active, so neither trigger is ever due.
        for r in [Reply { keep: true, ..Default::default() }, Reply { workspace: "has_data", ..Default::default() }, Reply { repo: "outside_home", ..Default::default() }] {
            let o = classify_reply(&r, Some(&prev(&r)), JobsOutcome::Read { busy: 0 }, "runpod");
            assert_eq!(o.verdict, Verdict::Active);
            let mut p = pol.clone();
            p.observations = streak_of(&[T - 1200, T - 900, T - 600, T - 300]);
            p.observations.push(Observation { t: T, ..o });
            assert!(!expiry_due(&p, T) && !idle_due(&p), "{trigger:?}");
            assert_ne!(due_row(&p, T, Mode::Apply).row, Row::Candidate);
        }
        // Held: the run gate refuses.
        let held = RunInputs { mode: Mode::Apply, enabled: true, held: true, clock_ok: true, dest_ok: true, caps_left: 1 };
        assert_eq!(RunGate::check(&held), Err(Row::Held));
    }
}

/// Spec I7: a failed listing means nothing for that provider; an unverified clock means no
/// actions at all (`inv_partial_listing`, `inv_clock`).
#[test]
fn inv_partial_listing_and_clock() {
    let l = list();
    let naming = Naming { prefix: "devtest", list: &l };
    let a = rp("devtest-alpha", "ra");
    let pol = policy(&a, None, Some(900));
    let ctx = GateCtx { naming: &naming, runpod_v2: true, pins: &[], listed: &[], now: T };
    assert_eq!(pre_ssh_gate(&pol, Listed::Failed, &ctx).err(), Some(Row::ListingFailed));
    let unverified = RunInputs { mode: Mode::Apply, enabled: true, held: false, clock_ok: false, dest_ok: true, caps_left: 5 };
    assert_eq!(RunGate::check(&unverified), Err(Row::ClockUnverified));
    // And the observations of an unverified run never extend a streak.
    let mut o = streak_of(&[T, T + 600, T + 1200]);
    o[2].clock_ok = false;
    assert_eq!(idle_streak(&o), None);
}

/// Spec I1, `inv_default_off`: across the flag matrix, only `--apply` with REAP_ENABLED=1 (from
/// the config file) passes the run gate — the environment can't turn it on.
#[test]
fn inv_default_off() {
    for (file, env_on) in [("", false), ("", true), ("REAP_ENABLED=0", true), ("REAP_ENABLED=1", false)] {
        let mut cfg = Config::parse(file);
        // As `Config::load` applies the environment.
        let lookup = |k: &str| (env_on && k == "REAP_ENABLED").then(|| "1".to_string());
        cfg_apply(&mut cfg, lookup);
        let enabled = ReapSettings::from_config(&cfg).unwrap().enabled;
        assert_eq!(enabled, file == "REAP_ENABLED=1", "{file:?} env={env_on}");
        for mode in [Mode::Observe, Mode::Rehearse, Mode::Apply] {
            let r = RunInputs { mode, enabled, held: false, clock_ok: true, dest_ok: true, caps_left: 1 };
            assert_eq!(RunGate::check(&r).is_ok(), mode == Mode::Apply && enabled, "{file:?} {mode:?}");
        }
    }
}

/// `Config::apply_overrides` is private; this is its rule for one lookup, via the public parse.
fn cfg_apply(cfg: &mut Config, lookup: impl Fn(&str) -> Option<String>) {
    for (k, v) in cfg.values.iter_mut() {
        if !Config::is_file_only(k) {
            if let Some(o) = lookup(k) {
                *v = o;
            }
        }
    }
}

/// Spec §8 F1–F3 and pruning: a pending terminate with the pod listed is cleared
/// (Interrupted); with it gone, a recovered tombstone; a failed listing changes nothing; a
/// policy whose pod is gone is pruned; a reaped pod still billing two runs on sets a hold.
#[test]
fn reconcile_with_the_listing() {
    let mut s = ReapState::default();
    let a = rp("devtest-alpha", "ra");
    let b = rp("devtest-bravo", "rb");
    let c = hz("devtest-charlie", "7");
    let d = rp("devtest-delta", "rd");
    let e = rp("devtest-echo", "re");
    let frozen = FrozenDirs { snapshot_dir: "/b/reaped/devtest-bravo/S-runpod-rb-attempt/snapshot".into(), big_dir: "/big/reaped/devtest-bravo/S-runpod-rb-attempt/big".into() };
    // Its terminate sent (the mark is saved right before the call, after the freeze is recorded).
    let sent = Terminating { frozen: Some(frozen.clone()), sent_at: Some(T + 30), ..Terminating::new("r1", Trigger::Idle, T) };
    let tw = |p: &Pod, t: &Terminating| PodPolicy { terminating: Some(t.clone()), ..policy(p, None, Some(900)) };
    s.pods.insert(key("runpod", "ra"), tw(&a, &sent));
    s.pods.insert(key("runpod", "rb"), tw(&b, &sent));
    s.pods.insert(key("hetzner", "7"), tw(&c, &sent));
    s.pods.insert(key("runpod", "rd"), policy(&d, None, Some(900)));
    // W7LR-1: frozen, but its terminate never sent (the run died before; the pod went otherwise).
    s.pods.insert(key("runpod", "re"), tw(&e, &Terminating { sent_at: None, ..sent.clone() }));
    let runpod = vec![a.clone()];
    let listing = |p: &str| -> Option<&[Pod]> {
        match p {
            "runpod" => Some(&runpod),
            _ => None,
        }
    };
    let ev = reconcile(&mut s, &listing, T + 60);
    let rows: Vec<(Row, &str)> = ev.iter().map(|e| (e.row, e.name.as_str())).collect();
    assert!(rows.contains(&(Row::Interrupted, "devtest-alpha")));
    assert!(rows.contains(&(Row::RecoveredReaped, "devtest-bravo")));
    assert!(rows.contains(&(Row::Pruned, "devtest-delta")));
    assert!(rows.contains(&(Row::GoneBeforeReap, "devtest-echo")));
    assert_eq!(ev.len(), 4, "{rows:?}");
    assert!(s.pods[&key("runpod", "ra")].terminating.is_none());
    let rb = &s.reaped[&key("runpod", "rb")];
    assert!(rb.recovered && !rb.not_reaped && rb.snapshot_dir.as_deref() == Some(frozen.snapshot_dir.as_str()), "{rb:?}");
    let re = &s.reaped[&key("runpod", "re")];
    assert!(re.recovered && re.not_reaped && re.big_dir.as_deref() == Some(frozen.big_dir.as_str()), "{re:?}");
    assert!(s.pods[&key("hetzner", "7")].terminating.is_some(), "Hetzner's listing failed: untouched");
    assert!(!s.pods.contains_key(&key("runpod", "rd")));
    // F3: a reaped pod still listed and billing — once noted, twice a hold.
    let mut s = ReapState::default();
    let tomb = Tombstone { at: T, name: "devtest-alpha".into(), trigger: Some(Trigger::Idle), snapshot_dir: None, big_dir: None, branch: None, recovered: false, seen_after: 0, not_reaped: false };
    s.reaped.insert(key("runpod", "ra"), tomb.clone());
    let still = |p: &str| -> Option<&[Pod]> { (p == "runpod").then_some(&runpod[..]) };
    reconcile(&mut s, &still, T + 900);
    assert!(s.hold.is_none());
    let ev = reconcile(&mut s, &still, T + 1800);
    assert_eq!(ev[0].row, Row::StillBilling);
    assert!(s.hold.as_ref().is_some_and(|h| h.auto && h.reason.contains("still listed and billing")));
    // …but a pod the reaper never terminated (W7LR-1) isn't its to vouch for.
    let mut s = ReapState::default();
    s.reaped.insert(key("runpod", "ra"), Tombstone { not_reaped: true, ..tomb });
    for t in [T + 900, T + 1800] {
        assert!(reconcile(&mut s, &still, t).is_empty());
    }
    assert!(s.hold.is_none());
}

/// The rows' spec numbers are what the docs and logs cite.
#[test]
fn row_codes() {
    assert_eq!((Row::NoPolicy.code(), Row::Locked.code(), Row::Reaped.code(), Row::TerminateFailed.code()), ("R1", "R7", "R38", "R40"));
    assert_eq!(Row::WouldReap.word(), "would_reap");
}

/// `plan_since`: continuous coverage from the previous look; the whole window when this look
/// may decide; the first look 15 minutes.
#[test]
fn plan_since_cases() {
    let a = rp("devtest-alpha", "ra");
    let mut p = policy(&a, None, Some(3600));
    assert_eq!(plan_since(&p, T), Since::Back(FIRST_SCAN_BACK));
    p.observations = streak_of(&[T - 600]);
    assert_eq!(plan_since(&p, T), Since::At(T - 600 - SCAN_MARGIN));
    p.observations = streak_of(&[T - 3600, T - 2400, T - 1200]);
    assert_eq!(plan_since(&p, T), Since::Back(3600 + SCAN_MARGIN), "this look can make it due");
    p.observations = streak_of(&[T - MAX_GAP - 1]);
    assert_eq!(plan_since(&p, T), Since::Back(FIRST_SCAN_BACK));
}
