use super::fixture::{v4, Reply};
use super::*;
use crate::reap::judge::{classify, ClassifyCtx, JobsOutcome};
use crate::reap::{Observation, Verdict};

/// POSIX `cksum`, cross-checked against the real one on this machine.
#[cfg(target_os = "linux")]
#[test]
fn cksum_matches_the_posix_tool() {
    use std::io::Write;
    for input in ["", "a", "ARENA_PROBE v=1\n", &"x".repeat(70_000), "näive\u{1}\n"] {
        let mut child = std::process::Command::new("cksum")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
        let want = format!("{} {}", cksum(input.as_bytes()), input.len());
        assert_eq!(out.split_whitespace().take(2).collect::<Vec<_>>().join(" "), want, "{input:?}");
    }
    assert_eq!(cksum(b""), 4_294_967_295);
}

/// The command: env -i, the script base64-decoded under sh, the arguments quoted.
#[test]
fn the_command_ships_the_script_and_quotes_its_arguments() {
    let c = probe_command("abc123", Since::At(1_791_460_000), "/root/ARENA_materials/", "runpod");
    assert!(c.starts_with("env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=\"$HOME\" sh -c \"$(printf %s "), "{c}");
    assert!(c.ends_with(" | base64 -d)\" arena-idle-probe 'abc123' '@1791460000' '/root/ARENA_materials' 'runpod' \"$SSH_CONNECTION\""), "{c}");
    let b64 = c.split("printf %s ").nth(1).unwrap().split(' ').next().unwrap();
    assert_eq!(crate::base64::decode(b64).unwrap(), PROBE_SCRIPT.as_bytes());
    let q = probe_command("n", Since::Back(900), "/root/it's", "hetzner");
    assert!(q.contains("'-900' '/root/it'\\''s' 'hetzner'"), "{q}");
    assert_ne!(nonce_from(1), nonce_from(2));
    assert!(nonce_from(7).len() == 16 && nonce_from(7).bytes().all(|b| b.is_ascii_hexdigit()));
}

/// The default fixture is an idle RunPod pod and parses to those facts; a banner outside the
/// frame is ignored.
#[test]
fn an_idle_reply_parses() {
    let r = Reply::default();
    let f = parse_probe(&r.render(), "N0NCE", r.since).unwrap();
    assert_eq!(f.id, "pod1");
    assert_eq!(f.container().as_deref(), Some("db9bb781-8521-46cd-a74a-63c0e46d9be7/123456"));
    assert_eq!((f.t0, f.t1, f.now, f.scan_from), (r.t0, r.t0 + 10, r.now(), r.t0 - 900));
    assert_eq!(f.cpu, Counter { start: Some(1_000_000_000), end: Some(1_000_050_000) });
    assert_eq!(f.gpu, GpuFacts::Read { gpus: 1, max_util: 0, max_mem_mib: 3 });
    assert_eq!(f.sockets, Some(Sockets { inbound: 0, outbound: 0, overflow: false }));
    assert_eq!(f.sessions, Some(0));
    assert_eq!((f.who, f.tmux_busy, f.screen, f.keep), (Some(0), Some(0), Some(0), false));
    assert_eq!((f.workspace, f.repo, f.home_mounts, f.files), (Workspace::Absent, RepoSite::UnderHome, Some(0), Files::NoneChanged));
}

/// Framing: no BEGIN/END is a failed probe; another nonce, two frames, a missing or wrong sum,
/// an unknown or doubled key, a line that isn't ours inside the frame, or a reply over the cap
/// is garbled.
#[test]
fn framing_and_strictness() {
    let r = Reply::default();
    let ok = r.render();
    let p = |s: &str| parse_probe(s, "N0NCE", r.since);
    assert!(p(&ok).is_ok());
    assert_eq!(p(""), Err(ProbeError::Failed));
    assert_eq!(p("Permission denied (publickey).\n"), Err(ProbeError::Failed));
    assert_eq!(p(&ok.replace("ARENA_PROBE_END N0NCE\n", "")), Err(ProbeError::Failed));
    assert_eq!(parse_probe(&ok, "OTHER", r.since), Err(ProbeError::Garbled));
    assert_eq!(p(&format!("{ok}{ok}")), Err(ProbeError::Garbled));
    let facts = r.fact_lines();
    let reframe = |lines: Vec<String>| Reply::frame("N0NCE", &lines);
    let mut extra = facts.clone();
    extra.push("ARENA_PROBE surprise=1".into());
    assert_eq!(p(&reframe(extra)), Err(ProbeError::Garbled));
    let mut doubled = facts.clone();
    doubled.push("ARENA_PROBE keep=0".into());
    assert_eq!(p(&reframe(doubled)), Err(ProbeError::Garbled));
    let mut noise = facts.clone();
    noise.insert(3, "Welcome!".into());
    assert_eq!(p(&reframe(noise)), Err(ProbeError::Garbled));
    assert_eq!(p(&ok.replace("ARENA_PROBE sum=", "ARENA_PROBE sum=1")), Err(ProbeError::Garbled));
    let big = format!("{}\n{ok}", "x".repeat(crate::reap::PROBE_OUTPUT_CAP));
    assert_eq!(p(&big), Err(ProbeError::Garbled));
    // A reply about another scan window is not this one.
    assert_eq!(parse_probe(&ok, "N0NCE", Since::Back(901)), Err(ProbeError::Garbled));
    assert_eq!(parse_probe(&ok, "N0NCE", Since::At(r.t0 - 900)), Ok(parse_probe(&ok, "N0NCE", r.since).unwrap()));
}

const CTL_NOW_SKEW: u64 = 0;

/// The control box's view: the previous look 15 minutes ago at the same container, quiet.
fn prev_for(r: &Reply) -> Observation {
    let now = r.now() + CTL_NOW_SKEW;
    Observation {
        t: now - 900,
        clock_ok: true,
        container: Some(format!("{}/{}", r.boot_id, r.pid1_start)),
        pod_t: Some(r.now() - 900),
        verdict: Verdict::Idle,
        reasons: Vec::new(),
        cpu_usec: Some(r.cpu.1.unwrap() - 10_000_000),
        net_bytes: Some(r.net.1.unwrap() - 1_000_000),
        counters_t: Some(r.t0 + r.window - 900),
        scan_from: Some(r.t0 - 1800),
    }
}

fn verdict_of(text: &str, r: &Reply, since: Since) -> Verdict {
    let facts = parse_probe(text, &r.nonce, since);
    let ctx = ClassifyCtx { provider: "runpod", listed_id: "pod1", now: r.now() + CTL_NOW_SKEW, clock_ok: true, baseline: None };
    classify(Some(&prev_for(r)), &facts, JobsOutcome::Read { busy: 0 }, &ctx).verdict
}

/// Spec I3, `inv_garbled_never_idle`: starting from a valid idle reply, deleting, duplicating
/// or garbling ANY single line of the frame (BEGIN to END), dropping the END marker or changing
/// the nonce never yields `Idle`. And when the probe itself reports a mangled fact faithfully
/// (the sum recomputed), deleting or doubling a fact or junking its value still never does.
#[test]
fn inv_garbled_never_idle() {
    let r = Reply::default();
    let text = r.render();
    assert_eq!(verdict_of(&text, &r, r.since), Verdict::Idle, "the fixture must start idle");
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.iter().position(|l| l.starts_with("ARENA_PROBE_BEGIN")).unwrap();
    let last = lines.iter().position(|l| l.starts_with("ARENA_PROBE_END")).unwrap();
    let join = |v: Vec<String>| v.join("\n") + "\n";
    let mut tried = 0;
    for i in first..=last {
        let l = lines[i];
        let mut mutants: Vec<Vec<String>> = Vec::new();
        let base: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        let mut del = base.clone();
        del.remove(i);
        mutants.push(del);
        let mut dup = base.clone();
        dup.insert(i, l.to_string());
        mutants.push(dup);
        for junk in [
            "garbage".to_string(),
            l[..l.len() / 2].to_string(),
            format!("{l}\u{1}"),
            format!("{l}0"),
            l.replace("N0NCE", "N0NCF"),
            l.chars().map(|c| if c == '1' { '2' } else { c }).collect(),
            l.chars().rev().collect(),
        ] {
            if junk != l {
                let mut g = base.clone();
                g[i] = junk;
                mutants.push(g);
            }
        }
        for m in mutants {
            tried += 1;
            assert_ne!(verdict_of(&join(m.clone()), &r, r.since), Verdict::Idle, "line {i} {l:?} mutated:\n{}", join(m));
        }
    }
    // Faithfully reported mangling (the sum recomputed over the changed facts).
    let facts = r.fact_lines();
    for i in 0..facts.len() {
        let mut variants: Vec<Vec<String>> = Vec::new();
        let mut del = facts.clone();
        del.remove(i);
        variants.push(del);
        let mut dup = facts.clone();
        dup.insert(i, facts[i].clone());
        variants.push(dup);
        let (k, _) = facts[i].split_once('=').unwrap();
        for junk in ["x", "", "\u{1}", "-1", "1 x", "99999999999999999999999"] {
            let mut g = facts.clone();
            g[i] = format!("{k}={junk}");
            if g[i] != facts[i] {
                variants.push(g);
            }
        }
        for v in variants {
            tried += 1;
            let framed = Reply::frame("N0NCE", &v);
            assert_ne!(verdict_of(&framed, &r, r.since), Verdict::Idle, "fact {i} {:?} changed:\n{framed}", facts[i]);
        }
    }
    assert!(tried > 500, "{tried}");
}

/// sshd process titles: only an authenticated connection's (`user`, `user@pts/N`,
/// `user@notty`, also under `sshd-session:`) is a session; the monitor, pre-auth children and
/// the listener never are.
#[test]
fn session_titles() {
    for (t, want) in [
        ("sshd: root@pts/0", true),
        ("sshd: root@notty", true),
        ("sshd: root@pts/0,pts/3", true),
        ("sshd: root@notty     ", true),
        ("sshd-session: root@pts/1", true),
        ("sshd: ubuntu", true),
        ("sshd: root [priv]", false),
        ("sshd: root [preauth]", false),
        ("sshd: root [net]", false),
        ("sshd: unknown [priv]", false),
        ("sshd: /usr/sbin/sshd -D [listener] 0 of 10-100 startups", false),
        ("sshd-auth: root [net]", false),
        ("sshd: root@", false),
        ("sshd: @pts/0", false),
        ("sshd: root@tty1", false),
        ("sshd-exited", false),
        ("bash", false),
    ] {
        assert_eq!(is_session_title(t), want, "{t:?}");
    }
    // Ours (an ancestor) is left out; with none of ours seen, sessions are unknown.
    let r = Reply { sshd: vec![(4100, "sshd: root@notty".into()), (5000, "sshd: root@pts/0".into()), (5001, "sshd: root".into())], ..Default::default() };
    assert_eq!(parse_probe(&r.render(), "N0NCE", r.since).unwrap().sessions, Some(2));
    let r = Reply { anc: vec![4242], ..Default::default() };
    assert_eq!(parse_probe(&r.render(), "N0NCE", r.since).unwrap().sessions, None);
    let r = Reply { sshd_overflow: true, ..Default::default() };
    assert_eq!(parse_probe(&r.render(), "N0NCE", r.since).unwrap().sessions, None);
}

/// Sockets: inbound = established to a listening port other than sshd's (22, or the port we
/// came in on), loopback peers included; outbound = from a non-listening port to a
/// non-loopback peer — IPv4, IPv6 and v4-mapped alike.
#[test]
fn sockets_inbound_and_outbound() {
    let listen = |port: u16| format!("0A {} {}", v4([0, 0, 0, 0], port), v4([0, 0, 0, 0], 0));
    let est = |l: String, r: String| format!("01 {l} {r}");
    let me = v4([172, 17, 0, 2], 22);
    let v6 = |hex: &str, port: u16| format!("{hex}:{port:04X}");
    // (case, extra lines, inbound, outbound)
    let cases: Vec<(&str, Vec<String>, u32, u32)> = vec![
        ("only us", vec![], 0, 0),
        ("jupyter from the provider proxy", vec![est(v4([172, 17, 0, 2], 8888), v4([100, 64, 0, 1], 3000))], 1, 0),
        ("a kernel to its server over loopback", vec![est(v4([127, 0, 0, 1], 8888), v4([127, 0, 0, 1], 52000))], 1, 0),
        ("its client end (ephemeral → loopback)", vec![est(v4([127, 0, 0, 1], 52000), v4([127, 0, 0, 1], 8888))], 0, 0),
        ("an API eval", vec![est(v4([172, 17, 0, 2], 41000), v4([104, 18, 0, 1], 443))], 0, 1),
        ("another ssh session's socket (counted by process)", vec![est(me.clone(), v4([198, 51, 100, 7], 61000))], 0, 0),
        ("ipv6 outbound", vec![est(v6("00000000000000000000000002000000", 41001), v6("B80D0120000000000000000001000000", 443))], 0, 1),
        ("ipv6 loopback is not outbound", vec![est(v6("00000000000000000000000002000000", 41002), v6("00000000000000000000000001000000", 9000))], 0, 0),
        ("v4-mapped outbound", vec![est(v6("0000000000000000FFFF00000200A8C0", 41003), v6("0000000000000000FFFF0000097100CB", 443))], 0, 1),
    ];
    for (case, extra, inbound, outbound) in cases {
        let mut r = Reply::default();
        r.tcp.as_mut().unwrap().push(listen(8888));
        r.tcp.as_mut().unwrap().extend(extra);
        let s = parse_probe(&r.render(), "N0NCE", r.since).unwrap().sockets.unwrap();
        assert_eq!((s.inbound, s.outbound), (inbound, outbound), "{case}");
    }
    // sshd on another port, as our own connection says.
    let mut r = Reply { self_conn: "203.0.113.9 50000 172.17.0.2 2222".into(), ..Default::default() };
    r.tcp = Some(vec![listen(2222), est(v4([172, 17, 0, 2], 2222), v4([203, 0, 113, 9], 50000))]);
    assert_eq!(parse_probe(&r.render(), "N0NCE", r.since).unwrap().sockets.unwrap().inbound, 0);
    let r = Reply { tcp: None, ..Default::default() };
    assert_eq!(parse_probe(&r.render(), "N0NCE", r.since).unwrap().sockets, None);
    let r = Reply { tcp_overflow: true, ..Default::default() };
    assert!(parse_probe(&r.render(), "N0NCE", r.since).unwrap().sockets.unwrap().overflow);
    let bad = Reply { tcp: Some(vec!["01 zz:1 00:2".into()]), ..Default::default() };
    assert_eq!(parse_probe(&bad.render(), "N0NCE", bad.since), Err(ProbeError::Garbled));
    let bad = Reply { self_conn: "x".into(), ..Default::default() };
    assert_eq!(parse_probe(&bad.render(), "N0NCE", bad.since), Err(ProbeError::Garbled));
}

/// GPU: five samples answered 0 with the same GPUs → read (busiest, most memory); anything
/// else → unreadable.
#[test]
fn gpu_samples() {
    let gpu = |samples: Vec<(u64, Vec<&str>)>| {
        let r = Reply { gpu: Some(samples.into_iter().map(|(rc, l)| (rc, l.into_iter().map(String::from).collect())).collect()), ..Default::default() };
        parse_probe(&r.render(), "N0NCE", r.since).map(|f| f.gpu)
    };
    let five = |l: Vec<&'static str>| (0..5).map(|_| (0, l.clone())).collect::<Vec<_>>();
    assert_eq!(gpu(five(vec!["0, 3", "7, 900"])), Ok(GpuFacts::Read { gpus: 2, max_util: 7, max_mem_mib: 903 }));
    let mut varied = five(vec!["0, 3"]);
    varied[3] = (0, vec!["40, 5000"]);
    assert_eq!(gpu(varied), Ok(GpuFacts::Read { gpus: 1, max_util: 40, max_mem_mib: 5000 }));
    for (case, samples) in [
        ("hung on the first", vec![(124, vec![])]),
        ("failed later", vec![(0, vec!["0, 3"]), (9, vec![])]),
        ("no GPU listed", five(vec![])),
        ("a MIG slice", five(vec!["?N/A?, 4000"])),
        ("util over 100", five(vec!["101, 3"])),
        ("four samples", (0..4).map(|_| (0, vec!["0, 3"])).collect()),
    ] {
        assert_eq!(gpu(samples), Ok(GpuFacts::Unreadable), "{case}");
    }
    let mut uneven = five(vec!["0, 3"]);
    uneven[2] = (0, vec!["0, 3", "0, 3"]);
    assert_eq!(gpu(uneven), Ok(GpuFacts::Unreadable));
    assert_eq!(gpu(five(vec!["0 3"])), Err(ProbeError::Garbled));
    let r = Reply { gpu: None, ..Default::default() };
    assert_eq!(parse_probe(&r.render(), "N0NCE", r.since).unwrap().gpu, GpuFacts::Absent);
}

/// tmux: panes that aren't a shell are busy; one past the 50 read counts busy too.
#[test]
fn tmux_panes() {
    let busy = |panes: Vec<&str>| {
        let r = Reply { tmux: "ok", panes: panes.into_iter().map(String::from).collect(), ..Default::default() };
        parse_probe(&r.render(), "N0NCE", r.since).unwrap().tmux_busy
    };
    assert_eq!(busy(vec![]), Some(0));
    assert_eq!(busy(vec!["bash", "zsh", "-bash"]), Some(0));
    assert_eq!(busy(vec!["bash", "python", "nvtop"]), Some(2));
    let r = Reply { tmux: "err", ..Default::default() };
    assert_eq!(parse_probe(&r.render(), "N0NCE", r.since).unwrap().tmux_busy, None);
}

/// The real script, run here under `sh` with stand-in commands (no network): framing, the key
/// set, the checksum and each GPU case. `HOME` is a fresh temp dir; the test window is 1 s.
#[cfg(target_os = "linux")]
#[test]
fn the_script_runs_under_sh() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("arena-reap-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, home) = (root.join("bin"), root.join("home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let stub = |name: &str, body: &str| {
        let p = bin.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    stub("who", "exit 0");
    stub("tmux", "echo bash; echo python3");
    // (case, nvidia-smi body, expected GPU)
    let cases: [(&str, &str, GpuFacts); 4] = [
        ("ok", "case \"$1\" in --query-gpu*) echo '12, 900';; *) ;; esac", GpuFacts::Read { gpus: 1, max_util: 12, max_mem_mib: 900 }),
        ("fail", "echo boom >&2; exit 9", GpuFacts::Unreadable),
        ("hang", "sleep 30", GpuFacts::Unreadable),
        ("zero", "exit 0", GpuFacts::Unreadable),
    ];
    for (case, body, want) in cases {
        stub("nvidia-smi", body);
        let out = std::process::Command::new("env")
            .arg("-i")
            .arg(format!("PATH={}:/usr/sbin:/usr/bin:/sbin:/bin", bin.display()))
            .arg(format!("HOME={}", home.display()))
            .arg("ARENA_PROBE_TEST_WINDOW=1")
            .arg("ARENA_PROBE_TEST_GPU_TIMEOUT=1")
            .args(["sh", "-c", PROBE_SCRIPT, "arena-idle-probe", "T3ST", "-900", "", "none", "203.0.113.9 50000 10.0.0.2 22"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let f = parse_probe(&stdout, "T3ST", Since::Back(900)).unwrap_or_else(|e| panic!("{case}: {e:?}\n{stdout}"));
        assert_eq!(f.gpu, want, "{case}");
        assert_eq!(f.tmux_busy, Some(1), "{case}: python3 in a pane");
        assert_eq!(f.who, Some(0));
        assert_eq!(f.repo, RepoSite::Absent);
        assert!(f.t1 >= f.t0 + 1 && f.now >= f.t1, "{case}");
        // A fresh HOME was created moments ago: its own ctime is a change.
        assert_eq!(f.files, Files::Hit, "{case}");
        assert!(f.boot_id.is_some(), "{case}");
    }
    // A nonce that isn't alphanumeric: nothing at all.
    let out = std::process::Command::new("sh").args(["-c", PROBE_SCRIPT, "arena-idle-probe", "bad nonce", "-900", "", "x", ""]).output().unwrap();
    assert!(out.stdout.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// REVIEW RS6 (fails today): the file scan is `find "$HOME" …` with find's default `-P`, so a
/// HOME that is a symlink (e.g. `/root -> /data/root`) is examined as the link itself and never
/// descended into — the scan reports `files none` (quiet) instead of a hit or "can't tell".
/// `/workspace` has the same shape (`find /workspace -xdev …` on a symlinked `/workspace` reports
/// `clean`). Unknown must never read as quiet.
#[cfg(target_os = "linux")]
#[test]
fn review_rs6_a_symlinked_home_is_scanned_not_skipped() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("arena-reap-probe-rv6-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, real, link) = (root.join("bin"), root.join("home_real"), root.join("home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let stub = |name: &str, body: &str| {
        let p = bin.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    stub("nvidia-smi", "case \"$1\" in --query-gpu*) echo '0, 3';; *) ;; esac");
    // The link (and the dir) are older than the scan's start; the participant's file is newer.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(real.join("notebook.ipynb"), "{}").unwrap();
    let out = std::process::Command::new("env")
        .arg("-i")
        .arg(format!("PATH={}:/usr/sbin:/usr/bin:/sbin:/bin", bin.display()))
        .arg(format!("HOME={}", link.display()))
        .arg("ARENA_PROBE_TEST_WINDOW=1")
        .arg("ARENA_PROBE_TEST_GPU_TIMEOUT=1")
        .args(["sh", "-c", PROBE_SCRIPT, "arena-idle-probe", "T3ST", &format!("@{since}"), "", "runpod", "203.0.113.9 50000 10.0.0.2 22"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let f = parse_probe(&stdout, "T3ST", Since::At(since)).unwrap_or_else(|e| panic!("{e:?}\n{stdout}"));
    let _ = std::fs::remove_dir_all(&root);
    assert_ne!(f.files, Files::NoneChanged, "a file written under a symlinked HOME after the scan's start reads as `none` (quiet)");
}

/// RS2/RS6/RS7 facts: the home as resolved, the sweep-folder scan, other mounts, and the
/// process lines (`start cksum i|p name`) — each value strict.
#[test]
fn home_mounts_and_processes_parse() {
    let p = |r: &Reply| parse_probe(&r.render(), "N0NCE", r.since);
    let f = p(&Reply::default()).unwrap();
    assert_eq!((f.home, f.uncovered, f.mounts, f.hz), (HomeState::Ok, Files::NoneChanged, Mounts::Clean, 100));
    assert_eq!(
        f.procs.as_deref(),
        Some(&[ProcFact { start: 123_456, hash: 11, idle: false }, ProcFact { start: 123_500, hash: 22, idle: true }, ProcFact { start: 123_900, hash: 33, idle: false }][..])
    );
    for (home, want) in [("link", HomeState::Link), ("err", HomeState::Err)] {
        assert_eq!(p(&Reply { home, ..Default::default() }).unwrap().home, want);
    }
    for (u, want) in [("hit", Files::Hit), ("timeout", Files::Timeout), ("err", Files::Err)] {
        assert_eq!(p(&Reply { uncovered: u, ..Default::default() }).unwrap().uncovered, want);
    }
    for (m, want) in [("has_data", Mounts::HasData), ("err", Mounts::Err)] {
        assert_eq!(p(&Reply { mounts: m, ..Default::default() }).unwrap().mounts, want);
    }
    // A shell (`i`) and a name with a space (tmux's server) parse; idle by kind or by name.
    let procs = vec![(130_000, 5, true, "bash".to_string()), (130_001, 6, false, "tmux: server".to_string()), (130_002, 7, false, "python3".to_string())];
    let f = p(&Reply { procs: Some(procs), ..Default::default() }).unwrap();
    assert_eq!(f.procs.unwrap().iter().map(|x| x.idle).collect::<Vec<_>>(), [true, true, false]);
    assert_eq!(p(&Reply { procs: None, ..Default::default() }).unwrap().procs, None);
    assert_eq!(p(&Reply { proc_overflow: true, ..Default::default() }).unwrap().procs, None);
    // Anything else is garbled, never "no processes".
    for bad in ["home=maybe", "uncovered=", "mounts=some", "hz=0", "proc_read=maybe"] {
        let (k, _) = bad.split_once('=').unwrap();
        let lines: Vec<String> =
            Reply::default().fact_lines().into_iter().map(|l| if l.starts_with(&format!("ARENA_PROBE {k}=")) { format!("ARENA_PROBE {bad}") } else { l }).collect();
        assert!(lines.contains(&format!("ARENA_PROBE {bad}")), "{bad}");
        assert_eq!(parse_probe(&Reply::frame("N0NCE", &lines), "N0NCE", Since::Back(900)), Err(ProbeError::Garbled), "{bad}");
    }
    for bad in ["123 11 x start.sh", "123 11 p", "x 11 p a", "123 99999999999 p a", "123 11 p 0123456789012345678901234"] {
        let lines: Vec<String> =
            Reply::default().fact_lines().into_iter().map(|l| if l.starts_with("ARENA_PROBE proc=1 ") { format!("ARENA_PROBE proc=1 {bad}") } else { l }).collect();
        assert!(lines.contains(&format!("ARENA_PROBE proc=1 {bad}")), "{bad}");
        assert_eq!(parse_probe(&Reply::frame("N0NCE", &lines), "N0NCE", Since::Back(900)), Err(ProbeError::Garbled), "{bad}");
    }
}

/// The real script (RS2, RS6, RS7): a file in a sweep folder since the container started is
/// `uncovered`; a dot-dir notebook is a plain change; a process in another session is listed
/// with its command line's cksum — a dormant `sleep` as `p`, a login shell (`-bash`) as `i` —
/// and the probe's own session is not; a home linked within one filesystem is resolved (`ok`).
#[cfg(target_os = "linux")]
#[test]
fn the_script_lists_processes_and_sweep_folders() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    let root = std::env::temp_dir().join(format!("arena-reap-probe-rs7-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, real, link) = (root.join("bin"), root.join("home_real"), root.join("home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(real.join("ARENA_materials/TEMP_FOLDER_FOR_SWEEPS/run1")).unwrap();
    std::fs::create_dir_all(real.join(".ipython/profile_default")).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let p = bin.join("nvidia-smi");
    std::fs::write(&p, "#!/bin/sh\ncase \"$1\" in --query-gpu*) echo '0, 3';; *) ;; esac\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(real.join("ARENA_materials/TEMP_FOLDER_FOR_SWEEPS/run1/ckpt.pt"), "w").unwrap();
    std::fs::write(real.join(".ipython/profile_default/notes.ipynb"), "{}").unwrap();
    // Two processes in sessions of their own: a dormant job, and a login shell waiting on input.
    let mut sleeper = Command::new("setsid").args(["sleep", "37"]).stdin(Stdio::null()).spawn().unwrap();
    let mut shell = Command::new("setsid").args(["bash", "-c", "exec -a -bash cat"]).stdin(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let out = Command::new("env")
        .arg("-i")
        .arg(format!("PATH={}:/usr/sbin:/usr/bin:/sbin:/bin", bin.display()))
        .arg(format!("HOME={}", link.display()))
        .arg("ARENA_PROBE_TEST_WINDOW=1")
        .arg("ARENA_PROBE_TEST_GPU_TIMEOUT=1")
        .args(["sh", "-c", PROBE_SCRIPT, "arena-idle-probe", "T3ST", "-900", "", "runpod", "203.0.113.9 50000 10.0.0.2 22"])
        .output()
        .unwrap();
    let _ = sleeper.kill();
    let _ = shell.kill();
    let _ = sleeper.wait();
    let _ = shell.wait();
    let _ = std::fs::remove_dir_all(&root);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let f = parse_probe(&stdout, "T3ST", Since::Back(900)).unwrap_or_else(|e| panic!("{e:?}\n{stdout}"));
    assert_eq!(f.home, HomeState::Ok, "a link within one filesystem is resolved:\n{stdout}");
    assert_eq!((f.files, f.uncovered), (Files::Hit, Files::Hit), "{stdout}");
    if stdout.contains("proc_overflow=1") {
        eprintln!("more than 300 processes on this machine: the process half is skipped");
        return;
    }
    let procs = f.procs.unwrap_or_else(|| panic!("no process list:\n{stdout}"));
    let find = |cmdline: &str| procs.iter().find(|p| p.hash == cksum(cmdline.as_bytes())).copied();
    assert_eq!(find("sleep 37 ").map(|p| p.idle), Some(false), "the dormant job, a process of its own:\n{stdout}");
    assert_eq!(find("-bash ").map(|p| p.idle), Some(true), "a login shell, idle by itself:\n{stdout}");
    // This test's own process (the probe's session) isn't listed.
    let me = std::fs::read("/proc/self/cmdline").unwrap();
    let me: String = String::from_utf8_lossy(&me).replace('\0', " ");
    assert!(find(&me).is_none(), "the probe's own session is listed:\n{stdout}");
}
