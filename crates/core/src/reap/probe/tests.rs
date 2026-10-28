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
    let c = probe_command("abc123", Since::At(1_791_460_000), "/root/ARENA_materials/", "runpod", Some(1_791_000_000));
    assert!(c.starts_with("env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=\"$HOME\" sh -c \"$(printf %s "), "{c}");
    assert!(c.ends_with(" | base64 -d)\" arena-idle-probe 'abc123' '@1791460000' '/root/ARENA_materials' 'runpod' \"$SSH_CONNECTION\" '1791000000'"), "{c}");
    let b64 = c.split("printf %s ").nth(1).unwrap().split(' ').next().unwrap();
    assert_eq!(crate::base64::decode(b64).unwrap(), PROBE_SCRIPT.as_bytes());
    let q = probe_command("n", Since::Back(900), "/root/it's", "hetzner", None);
    assert!(q.contains("'-900' '/root/it'\\''s' 'hetzner'") && q.ends_with("\"$SSH_CONNECTION\" '-'"), "{q}");
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

/// The tests' shorter waits (a 1 s window and GPU timeout, GPU samples back to back) are theirs
/// only: unset, as `env -i` leaves them on a pod, the probe waits 10 s, 8 s and 1 s.
#[test]
fn the_probe_waits_its_real_times_unless_a_test_says() {
    for line in ["WIN=${ARENA_PROBE_TEST_WINDOW:-10}\n", "GPU_T=${ARENA_PROBE_TEST_GPU_TIMEOUT:-8}\n", "GPU_GAP=${ARENA_PROBE_TEST_GPU_GAP:-1}\n", "sleep \"$GPU_GAP\"\n"] {
        assert!(PROBE_SCRIPT.contains(line), "{line}");
    }
}

/// The real script, run here under `sh` with stand-in commands (no network): framing, the key
/// set, the checksum and each GPU case. `HOME` is a fresh temp dir; the test window is 1 s.
#[cfg(target_os = "linux")]
#[test]
fn the_script_runs_under_sh() {
    let root = std::env::temp_dir().join(format!("arena-reap-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, home) = (root.join("bin"), root.join("home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let stub = |name: &str, body: &str| {
        let p = bin.join(name);
        crate::remote::write_test_script(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
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
            .arg("ARENA_PROBE_TEST_GPU_GAP=0")
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
        assert!(f.t1 > f.t0 && f.now >= f.t1, "{case}");
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
    let root = std::env::temp_dir().join(format!("arena-reap-probe-rv6-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, real, link) = (root.join("bin"), root.join("home_real"), root.join("home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let stub = |name: &str, body: &str| {
        let p = bin.join(name);
        crate::remote::write_test_script(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
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
        .arg("ARENA_PROBE_TEST_GPU_GAP=0")
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
    // R2-2 and RS7a/b.
    let f = p(&Reply::default()).unwrap();
    assert_eq!((f.links, f.roots, f.sched, f.ttys), (Files::NoneChanged, Files::NoneChanged, Some(false), Some(0)));
    let f = p(&Reply { links: "hit", roots: "timeout", sched: "hit", ttys: "3", ..Default::default() }).unwrap();
    assert_eq!((f.links, f.roots, f.sched, f.ttys), (Files::Hit, Files::Timeout, Some(true), Some(3)));
    let f = p(&Reply { links: "err", roots: "err", sched: "err", ttys: "-", ..Default::default() }).unwrap();
    assert_eq!((f.links, f.roots, f.sched, f.ttys), (Files::Err, Files::Err, None, None));
    // A shell (`i`) and a name with a space (tmux's server) parse; idle by kind or by name.
    let procs = vec![(130_000, 5, true, "bash".to_string()), (130_001, 6, false, "tmux: server".to_string()), (130_002, 7, false, "python3".to_string())];
    let f = p(&Reply { procs: Some(procs), ..Default::default() }).unwrap();
    assert_eq!(f.procs.unwrap().iter().map(|x| x.idle).collect::<Vec<_>>(), [true, true, false]);
    assert_eq!(p(&Reply { procs: None, ..Default::default() }).unwrap().procs, None);
    assert_eq!(p(&Reply { proc_overflow: true, ..Default::default() }).unwrap().procs, None);
    // Anything else is garbled, never "no processes".
    for bad in ["home=maybe", "uncovered=", "mounts=some", "hz=0", "proc_read=maybe", "links=some", "roots=", "sched=maybe", "ttys=x", "ttys=99999999999"] {
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
    use std::process::{Command, Stdio};
    let root = std::env::temp_dir().join(format!("arena-reap-probe-rs7-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, real, link) = (root.join("bin"), root.join("home_real"), root.join("home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(real.join("ARENA_materials/TEMP_FOLDER_FOR_SWEEPS/run1")).unwrap();
    std::fs::create_dir_all(real.join(".ipython/profile_default")).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let p = bin.join("nvidia-smi");
    crate::remote::write_test_script(&p, "#!/bin/sh\ncase \"$1\" in --query-gpu*) echo '0, 3';; *) ;; esac\n").unwrap();
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
        .arg("ARENA_PROBE_TEST_GPU_GAP=0")
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

/// Review RR (robustness): on a VM with utmp (Hetzner), Ubuntu's tmux (linked with
/// libutempter) registers EVERY pane as a `who` login — `root pts/1 … (tmux(4011013).%12)`.
/// The probe's tmux rule says panes running only a shell are not busy, but the same idle,
/// detached tmux session is then counted by `who` as a login, so the pod is `Active` (`who`)
/// forever: the tmux exemption never applies where utmp exists. Run on the real script with a
/// `who` that reports one tmux pane and a tmux whose only pane runs zsh.
#[cfg(target_os = "linux")]
#[test]
fn review_rr_idle_tmux_pane_is_not_a_who_login() {
    let root = std::env::temp_dir().join(format!("arena-reap-rr-who-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, home) = (root.join("bin"), root.join("home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let stub = |name: &str, body: &str| {
        let p = bin.join(name);
        crate::remote::write_test_script(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    };
    // What `who` prints on Ubuntu 24.04 for one detached tmux session with one pane.
    stub("who", "echo 'root     pts/1        2026-10-12 08:41 (tmux(4011013).%0)'");
    stub("tmux", "echo zsh");
    let out = std::process::Command::new("env")
        .arg("-i")
        .arg(format!("PATH={}:/usr/sbin:/usr/bin:/sbin:/bin", bin.display()))
        .arg(format!("HOME={}", home.display()))
        .arg("ARENA_PROBE_TEST_WINDOW=1")
        .arg("ARENA_PROBE_TEST_GPU_GAP=0")
        .args(["sh", "-c", PROBE_SCRIPT, "arena-idle-probe", "T3ST", "-900", "", "hetzner", "203.0.113.9 50000 10.0.0.2 22"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let f = parse_probe(&stdout, "T3ST", Since::Back(900)).unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(f.tmux_busy, Some(0), "the pane runs a shell: not busy");
    // The same pane, seen through utmp, must not count as a login either.
    assert_eq!(f.who, Some(0), "an idle tmux pane's utmp entry counted as a login: the tmux exemption is void wherever utmp exists");
}

/// Stand-in commands for running the real probe script here: `bin/<name>` with `body`.
#[cfg(target_os = "linux")]
fn review2_stub(bin: &std::path::Path, name: &str, body: &str) {
    let p = bin.join(name);
    crate::remote::write_test_script(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
}

/// The real probe under `sh`, HOME = `home`, PATH = `bin` first.
#[cfg(target_os = "linux")]
fn review2_run_probe(bin: &std::path::Path, home: &std::path::Path, since: &str, provider: &str) -> String {
    let out = std::process::Command::new("env")
        .arg("-i")
        .arg(format!("PATH={}:/usr/sbin:/usr/bin:/sbin:/bin", bin.display()))
        .arg(format!("HOME={}", home.display()))
        .arg("ARENA_PROBE_TEST_WINDOW=1")
        .arg("ARENA_PROBE_TEST_GPU_GAP=0")
        .arg("ARENA_PROBE_TEST_GPU_TIMEOUT=1")
        .args(["sh", "-c", PROBE_SCRIPT, "arena-idle-probe", "T3ST", since, "", provider, "203.0.113.9 50000 10.0.0.2 22"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// REVIEW-2 R2-1 (fails on 8464ea3): the sweep-folder veto (RS2) only looks at files changed
/// "since the container started" — PID 1's start. On a Hetzner VM that is the last BOOT, and the
/// disk survives every reboot (`pods restart` is a hard `reset`; `pods stop`/`start` a power
/// cycle; the participant's own `reboot`). A sweep whose results sit in
/// `TEMP_FOLDER_FOR_SWEEPS/` from before the reboot reads `uncovered=none`; the reap's tier
/// never copies that folder (`REAP_VETOED`), so the reap destroys the only copy. Here the real
/// script runs with PID 1's start (via a stand-in `awk` for `/proc/1/stat` only) placed just
/// after the sweep file was written — a reboot in between.
#[cfg(target_os = "linux")]
#[test]
fn review2_r2_1_sweep_results_from_before_a_vm_reboot_are_neither_copied_nor_vetoed() {
    let root = std::env::temp_dir().join(format!("arena-reap-rv2-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, home) = (root.join("bin"), root.join("home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(home.join("ARENA_materials/TEMP_FOLDER_FOR_SWEEPS/run1")).unwrap();
    std::fs::write(home.join("ARENA_materials/TEMP_FOLDER_FOR_SWEEPS/run1/ckpt.pt"), "a finished sweep's only copy").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let reboot = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let real_awk = String::from_utf8(std::process::Command::new("sh").args(["-c", "command -v awk"]).output().unwrap().stdout).unwrap();
    let real_awk = real_awk.trim();
    // `awk … /proc/1/stat` (the container's start) answers "PID 1 started at `reboot`"; every
    // other awk call is the real one.
    review2_stub(
        &bin,
        "awk",
        &format!(
            "for a in \"$@\"; do last=\"$a\"; done\nif [ \"$last\" = /proc/1/stat ]; then\n  bt=$({real_awk} '$1 == \"btime\" {{ print $2 }}' /proc/stat)\n  echo $(( ({reboot} - bt) * $(getconf CLK_TCK) ))\n  exit 0\nfi\nexec {real_awk} \"$@\""
        ),
    );
    review2_stub(&bin, "nvidia-smi", "case \"$1\" in --query-gpu*) echo '0, 3';; *) ;; esac");
    review2_stub(&bin, "curl", "exit 7");
    let stdout = review2_run_probe(&bin, &home, &format!("@{reboot}"), "hetzner");
    let _ = std::fs::remove_dir_all(&root);
    let f = parse_probe(&stdout, "T3ST", Since::At(reboot)).unwrap_or_else(|e| panic!("{e:?}\n{stdout}"));
    assert_eq!(f.files, Files::NoneChanged, "nothing changed since the reboot:\n{stdout}");
    assert_ne!(
        f.uncovered,
        Files::NoneChanged,
        "a sweep folder holding results from before the VM's last reboot reads as covered — the reap's backup never copies it:\n{stdout}"
    );
}

/// REVIEW-2 mutation guard (passes on 8464ea3): the sweep-folder veto looks back to the
/// container's start, not to the scan's `since` — a sweep written after the container started
/// but before the last look still vetoes. (Mutant `-newerct "@$cs"` → `"@$S"` in the
/// `uncovered` scan survived every test before this one.)
#[cfg(target_os = "linux")]
#[test]
fn review2_mut_sweep_veto_reaches_back_to_the_container_start() {
    let root = std::env::temp_dir().join(format!("arena-reap-rv2-mutk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, home) = (root.join("bin"), root.join("home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(home.join("ARENA_materials/TEMP_FOLDER_FOR_SWEEPS/run1")).unwrap();
    std::fs::write(home.join("ARENA_materials/TEMP_FOLDER_FOR_SWEEPS/run1/ckpt.pt"), "w").unwrap();
    review2_stub(&bin, "nvidia-smi", "case \"$1\" in --query-gpu*) echo '0, 3';; *) ;; esac");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let stdout = review2_run_probe(&bin, &home, &format!("@{since}"), "runpod");
    let _ = std::fs::remove_dir_all(&root);
    let f = parse_probe(&stdout, "T3ST", Since::At(since)).unwrap_or_else(|e| panic!("{e:?}\n{stdout}"));
    assert_eq!(f.files, Files::NoneChanged, "{stdout}");
    assert_eq!(f.uncovered, Files::Hit, "a sweep written since the container started (before the last look) must veto:\n{stdout}");
}

/// REVIEW-2 mutation guard (passes on 8464ea3 where /dev/shm is another filesystem): a home
/// that is a link to ANOTHER filesystem is `home=link` (can't tell). (Mutant: the `link` branch
/// reporting `ok` survived every test before this one — the only symlinked-home test links
/// within one filesystem.)
#[cfg(target_os = "linux")]
#[test]
fn review2_mut_a_home_linked_to_another_filesystem_is_unknown() {
    let shm = std::path::Path::new("/dev/shm");
    let dev = |p: &std::path::Path| std::fs::metadata(p).map(|m| std::os::unix::fs::MetadataExt::dev(&m));
    let tmp = std::env::temp_dir();
    if !shm.is_dir() || dev(shm).ok() == dev(&tmp).ok() {
        eprintln!("no second filesystem at /dev/shm — skipping");
        return;
    }
    let root = tmp.join(format!("arena-reap-rv2-mutd-{}", std::process::id()));
    let real = shm.join(format!("arena-reap-rv2-mutd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&real);
    let (bin, link) = (root.join("bin"), root.join("home"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    review2_stub(&bin, "nvidia-smi", "case \"$1\" in --query-gpu*) echo '0, 3';; *) ;; esac");
    let stdout = review2_run_probe(&bin, &link, "-900", "runpod");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&real);
    let f = parse_probe(&stdout, "T3ST", Since::Back(900)).unwrap_or_else(|e| panic!("{e:?}\n{stdout}"));
    assert_eq!(f.home, HomeState::Link, "{stdout}");
}

/// REVIEW-2 R2-2 (fails on 8464ea3): data written OUTSIDE the home on the root filesystem is
/// neither copied by the reap (its tiers copy the home; `rsync -a` copies a symlink as a link)
/// nor seen by the probe (the file scan doesn't follow links; the mounts fact only looks at
/// OTHER mounts). Here: `~/data -> /…/outside` (the classic "big outputs elsewhere" link, made
/// long ago), and a training run writes `outside/ckpt.pt` after the last look. Every fact the
/// judge uses for coverage reads clean; the pod is reaped and the checkpoint is lost.
#[cfg(target_os = "linux")]
#[test]
fn review2_r2_2_data_behind_a_link_out_of_the_home_is_neither_copied_nor_seen() {
    let root = std::env::temp_dir().join(format!("arena-reap-rv2-outside-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, home, outside) = (root.join("bin"), root.join("home"), root.join("outside"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, home.join("data")).unwrap();
    review2_stub(&bin, "nvidia-smi", "case \"$1\" in --query-gpu*) echo '0, 3';; *) ;; esac");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(outside.join("ckpt.pt"), "the run's only checkpoint").unwrap();
    let stdout = review2_run_probe(&bin, &home, &format!("@{since}"), "runpod");
    let _ = std::fs::remove_dir_all(&root);
    let f = parse_probe(&stdout, "T3ST", Since::At(since)).unwrap_or_else(|e| panic!("{e:?}\n{stdout}"));
    // Fixed (R2-2) with a fact of its own: the link out of the home is `links=hit` (not covered).
    assert!(
        f.files != Files::NoneChanged || f.uncovered != Files::NoneChanged || f.home != HomeState::Ok || f.links != Files::NoneChanged,
        "a file written after the last look behind ~/data -> outside the home reads as quiet and covered:\n{stdout}"
    );
    assert_eq!(f.links, Files::Hit, "{stdout}");
}

/// Live W7 B-H0: a fresh Hetzner VM never read as idle — arena's own setup (`uv`) had left five
/// 0-byte `/tmp/uv-setuptools-<hash>.lock` files (mode 666): "data written outside the home since
/// the VM was created", for as long as the VM lived (on a VM the anchor is the creation, which
/// outlives reboots). An empty regular file holds no data to lose: passed over — but a file with
/// a single byte in it still vetoes, wherever it sits (a directory with data in it too); a
/// symlink or a directory alone was never one (regular files only). The real script, `/tmp`
/// stood in for by `ARENA_PROBE_TEST_ROOTS`.
#[cfg(target_os = "linux")]
#[test]
fn w7_b_h0_empty_files_outside_the_home_hold_nothing_to_lose() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("arena-reap-probe-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, home, tmp) = (root.join("bin"), root.join("home"), root.join("tmp"));
    for d in [&bin, &home, &tmp] {
        std::fs::create_dir_all(d).unwrap();
    }
    review2_stub(&bin, "nvidia-smi", "case \"$1\" in --query-gpu*) echo '0, 3';; *) ;; esac");
    let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let roots = || -> Files {
        let out = std::process::Command::new("env")
            .arg("-i")
            .arg(format!("PATH={}:/usr/sbin:/usr/bin:/sbin:/bin", bin.display()))
            .arg(format!("HOME={}", home.display()))
            .arg("ARENA_PROBE_TEST_WINDOW=1")
            .arg("ARENA_PROBE_TEST_GPU_GAP=0")
            .arg("ARENA_PROBE_TEST_GPU_TIMEOUT=1")
            .arg(format!("ARENA_PROBE_TEST_ROOTS={}", tmp.display()))
            .args(["sh", "-c", PROBE_SCRIPT, "arena-idle-probe", "T3ST", "-900", "", "hetzner", "203.0.113.9 50000 10.0.0.2 22"])
            .arg(created.to_string())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        parse_probe(&stdout, "T3ST", Since::Back(900)).unwrap_or_else(|e| panic!("{e:?}\n{stdout}")).roots
    };
    // What the setup's uv leaves, as it leaves it; an empty dir; a link.
    for h in ["0f3a9c", "1b2c7d", "2d4e8e", "3f5a1f", "4b6c2a"] {
        let lock = tmp.join(format!("uv-setuptools-{h}.lock"));
        std::fs::write(&lock, "").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o666)).unwrap();
    }
    std::fs::create_dir_all(tmp.join("run/empty")).unwrap();
    std::fs::write(tmp.join("run/empty/DONE"), "").unwrap();
    std::os::unix::fs::symlink("/etc/hostname", tmp.join("a-link")).unwrap();
    let only_empty = roots();
    // One byte is data.
    std::fs::write(tmp.join("uv-setuptools-5c7d3b.lock"), "x").unwrap();
    let one_byte = roots();
    std::fs::remove_file(tmp.join("uv-setuptools-5c7d3b.lock")).unwrap();
    // A directory with data in it.
    std::fs::write(tmp.join("run/empty/ckpt.pt"), "the run's only copy").unwrap();
    let in_dir = roots();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(only_empty, Files::NoneChanged, "empty files (uv's locks), an empty file in a dir, a link: nothing to lose");
    assert_eq!(one_byte, Files::Hit, "a file with a byte in it is data");
    assert_eq!(in_dir, Files::Hit, "a file with data in a directory is data");
}

/// The real script's R2-1/R2-2/RS7 parts, in a hermetic stand-in (`ARENA_PROBE_TEST_ROOTS` for
/// the places outside the home, `ARENA_PROBE_TEST_SYSROOT` for the crontabs and timers):
/// - links out of the home: to a data dir → hit; to the system or dangling → none;
/// - outside the home: a file written after the pod's creation → hit, even when the VM booted
///   after it (Hetzner: the disk outlives reboots); an older one, or junk (site-packages,
///   `/tmp/systemd-private-*`-like) → none; no creation time on Hetzner → can't tell;
/// - scheduled work: a crontab line or an added timer → hit; comments, a packaged (linked)
///   timer, snapd's → none;
/// - a process on a pseudo-terminal in another session → counted; not one on a console tty, with
///   no terminal, in the probe's own session or a tmux pane's own shell. Read from stand-in
///   process lists (`ARENA_PROBE_TEST_PROCROOT`) — synthetic rows, and the real rows of a pty the
///   test opens itself — never this shared box's own: other users opening and closing terminals
///   meanwhile made a before/after count of the live `/proc` flaky.
#[cfg(target_os = "linux")]
#[test]
fn the_script_sees_outside_the_home_schedules_and_terminals() {
    use std::process::{Command, Stdio};
    let root = std::env::temp_dir().join(format!("arena-reap-probe-r22-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, home, data, sys) = (root.join("bin"), root.join("home"), root.join("data"), root.join("sys"));
    for d in [&bin, &home, &data, &sys] {
        std::fs::create_dir_all(d).unwrap();
    }
    review2_stub(&bin, "nvidia-smi", "case \"$1\" in --query-gpu*) echo '0, 3';; *) ;; esac");
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let run = |home: &std::path::Path, roots: &str, provider: &str, created: &str| -> ProbeFacts {
        let out = Command::new("env")
            .arg("-i")
            .arg(format!("PATH={}:/usr/sbin:/usr/bin:/sbin:/bin", bin.display()))
            .arg(format!("HOME={}", home.display()))
            .arg("ARENA_PROBE_TEST_WINDOW=1")
            .arg("ARENA_PROBE_TEST_GPU_GAP=0")
            .arg("ARENA_PROBE_TEST_GPU_TIMEOUT=1")
            .arg(format!("ARENA_PROBE_TEST_ROOTS={roots}"))
            .arg(format!("ARENA_PROBE_TEST_SYSROOT={}", sys.display()))
            .args(["sh", "-c", PROBE_SCRIPT, "arena-idle-probe", "T3ST", "-900", "", provider, "203.0.113.9 50000 10.0.0.2 22", created])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        parse_probe(&stdout, "T3ST", Since::Back(900)).unwrap_or_else(|e| panic!("{e:?}\n{stdout}"))
    };
    // Old data, then the pod's "creation", then new data.
    std::fs::create_dir_all(data.join("old")).unwrap();
    std::fs::write(data.join("old/ckpt.pt"), "from before").unwrap();
    std::fs::create_dir_all(data.join("venv/lib/python3.11/site-packages/x")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let created = now();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(data.join("venv/lib/python3.11/site-packages/x/mod.py"), "an install").unwrap();
    let roots = data.display().to_string();
    // Only junk and old files there: none; no creation on Hetzner: can't tell.
    assert_eq!(run(&home, &roots, "hetzner", &created.to_string()).roots, Files::NoneChanged);
    assert_eq!(run(&home, &roots, "hetzner", "-").roots, Files::Err);
    assert_eq!(run(&home, &roots, "hetzner", &(now() + 3600).to_string()).roots, Files::Err, "a creation in the future is no anchor");
    // New data after the creation: hit — on Hetzner even though this box booted long before.
    std::fs::write(data.join("old/new_ckpt.pt"), "the run's only copy").unwrap();
    assert_eq!(run(&home, &roots, "hetzner", &created.to_string()).roots, Files::Hit);
    // Links: to the system, dangling — none; to a data dir — hit.
    std::os::unix::fs::symlink("/usr/bin", home.join("bin")).unwrap();
    std::os::unix::fs::symlink(root.join("nowhere"), home.join("gone")).unwrap();
    assert_eq!(run(&home, "", "runpod", "-").links, Files::NoneChanged);
    std::os::unix::fs::symlink(data.join("old"), home.join("outputs")).unwrap();
    assert_eq!(run(&home, "", "runpod", "-").links, Files::Hit);
    std::fs::remove_file(home.join("outputs")).unwrap();
    // Scheduled work.
    std::fs::create_dir_all(sys.join("var/spool/cron/crontabs")).unwrap();
    std::fs::create_dir_all(sys.join("etc/systemd/system/timers.target.wants")).unwrap();
    std::fs::write(sys.join("var/spool/cron/crontabs/root"), "# m h dom mon dow command\nMAILTO=\"\"\n\n").unwrap();
    std::fs::write(sys.join("etc/systemd/system/snap.lxd.daily.timer"), "[Timer]\n").unwrap();
    std::os::unix::fs::symlink("/lib/systemd/system/apt-daily.timer", sys.join("etc/systemd/system/timers.target.wants/apt-daily.timer")).unwrap();
    assert_eq!(run(&home, "", "runpod", "-").sched, Some(false), "comments, snapd's, a packaged one");
    std::fs::write(sys.join("var/spool/cron/crontabs/root"), "# m h dom mon dow command\n17 3 * * * cd ~/run && python train.py\n").unwrap();
    assert_eq!(run(&home, "", "runpod", "-").sched, Some(true), "a crontab line");
    std::fs::write(sys.join("var/spool/cron/crontabs/root"), "").unwrap();
    std::fs::write(sys.join("etc/systemd/system/train.timer"), "[Timer]\nOnCalendar=daily\n").unwrap();
    assert_eq!(run(&home, "", "runpod", "-").sched, Some(true), "an added timer");
    std::fs::remove_file(sys.join("etc/systemd/system/train.timer")).unwrap();
    // Terminals, from stand-in process lists only.
    let ttys_in = |procs: &std::path::Path| -> Option<u32> {
        let out = Command::new("env")
            .arg("-i")
            .arg(format!("PATH={}:/usr/sbin:/usr/bin:/sbin:/bin", bin.display()))
            .arg(format!("HOME={}", home.display()))
            .arg("ARENA_PROBE_TEST_WINDOW=1")
            .arg("ARENA_PROBE_TEST_GPU_GAP=0")
            .arg("ARENA_PROBE_TEST_GPU_TIMEOUT=1")
            .arg("ARENA_PROBE_TEST_ROOTS=")
            .arg(format!("ARENA_PROBE_TEST_SYSROOT={}", sys.display()))
            .arg(format!("ARENA_PROBE_TEST_PROCROOT={}", procs.display()))
            .args(["sh", "-c", PROBE_SCRIPT, "arena-idle-probe", "T3ST", "-900", "", "runpod", "203.0.113.9 50000 10.0.0.2 22", "-"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        parse_probe(&stdout, "T3ST", Since::Back(900)).unwrap_or_else(|e| panic!("{e:?}\n{stdout}")).ttys
    };
    let session_of = |stat: &str| -> u32 { stat.rsplit_once(") ").unwrap().1.split_whitespace().nth(3).unwrap().parse().unwrap() };
    // The probe runs in this test's session: rows in it are the probe's own, never counted.
    let mine = session_of(&std::fs::read_to_string("/proc/self/stat").unwrap());
    // Synthetic rows as the kernel writes them (`pid (comm) state ppid pgrp session tty_nr …
    // starttime …`; tty_nr = major << 8 | minor). Pids above any pid_max: never a real ancestor.
    let synthetic = root.join("procs-synthetic");
    std::fs::create_dir_all(&synthetic).unwrap();
    assert_eq!(ttys_in(&synthetic), Some(0), "nothing listed: no terminal");
    let row = |pid: u32, ppid: u32, sid: u32, tty: u32, comm: &str| {
        let d = synthetic.join(pid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        let stat = format!("{pid} ({comm}) S {ppid} {pid} {sid} {tty} -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 12345 0 0\n");
        std::fs::write(d.join("stat"), stat).unwrap();
        std::fs::write(d.join("cmdline"), format!("{comm}\0")).unwrap();
        std::fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
    };
    row(5_000_001, 1, 5_000_001, (136 << 8) | 3, "bash"); // a login on /dev/pts/3: counted
    row(5_000_002, 5_000_001, 5_000_001, (143 << 8) | 7, "python"); // major 143, the last pts one: counted
    row(5_000_003, 1, 5_000_003, (4 << 8) | 1, "agetty"); // /dev/tty1: a console, not a pseudo-terminal
    row(5_000_004, 1, 5_000_004, (144 << 8) | 1, "x"); // major 144: past the pts range
    row(5_000_005, 1, 5_000_005, 0, "train"); // no terminal at all
    row(5_000_006, 1, 5_000_006, 0, "tmux: server");
    row(5_000_007, 5_000_006, 5_000_007, (136 << 8) | 9, "zsh"); // a tmux pane's own shell: the tmux fact's
    row(5_000_008, 1, mine, (136 << 8) | 4, "sleep"); // in the probe's own session
    assert_eq!(ttys_in(&synthetic), Some(2));

    // A real pseudo-terminal of the test's own — `sleep` under `script`, in a session of its own —
    // copied, row by row as the kernel wrote it, into an otherwise empty stand-in: what the
    // probe counts is exactly its processes on that pty.
    let mut term = Command::new("setsid").args(["script", "-qfc", "sleep 39", "/dev/null"]).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
    let real = root.join("procs-real");
    let pty_major = |stat: &str| -> bool {
        let tty: u32 = stat.rsplit_once(") ").unwrap().1.split_whitespace().nth(4).unwrap().parse().unwrap();
        (136..=143).contains(&((tty >> 8) & 4095))
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let on_pty = loop {
        // The script's process tree, by parent links.
        let mut tree = vec![term.id()];
        let rows: Vec<(u32, String)> = std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|e| Some((e.file_name().to_str()?.parse::<u32>().ok()?, std::fs::read_to_string(e.path().join("stat")).ok()?)))
            .collect();
        let parent = |stat: &str| -> u32 { stat.rsplit_once(") ").unwrap().1.split_whitespace().nth(1).unwrap().parse().unwrap() };
        while let Some(more) = rows.iter().find(|(p, st)| !tree.contains(p) && tree.contains(&parent(st))).map(|(p, _)| *p) {
            tree.push(more);
        }
        let _ = std::fs::remove_dir_all(&real);
        let mut n = 0;
        for pid in &tree {
            let (from, to) = (std::path::Path::new("/proc").join(pid.to_string()), real.join(pid.to_string()));
            let copied: Option<String> = (|| {
                let stat = std::fs::read_to_string(from.join("stat")).ok()?;
                let cmdline = std::fs::read(from.join("cmdline")).ok()?;
                let comm = std::fs::read(from.join("comm")).ok()?;
                std::fs::create_dir_all(&to).ok()?;
                std::fs::write(to.join("stat"), &stat).ok()?;
                std::fs::write(to.join("cmdline"), &cmdline).ok()?;
                std::fs::write(to.join("comm"), comm).ok()?;
                (!cmdline.is_empty()).then_some(stat)
            })();
            n += copied.is_some_and(|st| pty_major(&st)) as u32;
        }
        if n > 0 || std::time::Instant::now() > deadline {
            break n;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let counted = ttys_in(&real);
    let _ = term.kill();
    let _ = term.wait();
    let _ = std::fs::remove_dir_all(&root);
    assert!(on_pty >= 1, "`script` ran its command on the pty it opened");
    assert_eq!(counted, Some(on_pty), "exactly the test's own processes on its pty");
}
