use super::*;

const T: u64 = 1_791_460_800; // 2026-10-08T12:00:00Z
const TZ: i64 = 2 * 3600; // CEST

fn pod(name: &str) -> Pod {
    Pod { id: "ra".into(), name: name.into(), provider: "runpod".into(), status: "RUNNING".into(), locked: Some(false), ..Default::default() }
}

fn idle_obs(t: u64) -> Observation {
    Observation {
        t,
        clock_ok: true,
        container: Some("c/1".into()),
        pod_t: Some(t),
        verdict: Verdict::Idle,
        reasons: vec![],
        cpu_usec: None,
        net_bytes: None,
        counters_t: None,
        scan_from: Some(t - 3000),
    }
}

/// The ENDS column, every situation of spec §3.4 (decision Q1's "(unreachable)" wording).
#[test]
fn ends_label_every_state() {
    let list: Vec<String> = ["alpha", "@devtest-boss"].map(String::from).to_vec();
    let naming = Naming { prefix: "devtest", list: &list };
    let a = pod("devtest-alpha");
    let pol = |exp: Option<u64>, idle: Option<u64>| PodPolicy::new(&a, exp, idle, T, "set", None);
    let with_last = |mut p: PodPolicy, row: Row| {
        p.last = Some(LastDecision { t: T, row, reasons: vec![] });
        p
    };
    let label = |p: Option<&PodPolicy>, pin: Option<&Pin>, pod: &Pod| ends_label(p, pin, pod, &naming, None, T, TZ);
    assert_eq!(label(None, None, &a), "-");
    assert_eq!(label(Some(&pol(Some(T + 4 * 86_400), None)), None, &a), "10-12 14:00");
    assert_eq!(label(Some(&pol(Some(T + 5 * 3600), None)), None, &a), "19:00 (in 5h)");
    assert_eq!(label(Some(&pol(Some(T - 35 * 60), None)), None, &a), "OVERDUE 35m");
    assert_eq!(label(Some(&with_last(pol(Some(T - 35 * 60), None), Row::Active)), None, &a), "OVERDUE 35m (in use)");
    assert_eq!(label(Some(&with_last(pol(Some(T - 35 * 60), None), Row::ProbeFailed)), None, &a), "OVERDUE 35m (unreachable)");
    let mut idle = pol(None, Some(8 * 3600));
    idle.observations = (0..=9).map(|i| idle_obs(T - 3 * 3600 + i * 1200)).collect();
    assert_eq!(label(Some(&idle), None, &a), "idle 3h/8h");
    let mut both = idle.clone();
    both.expires_at = Some(T + 3600);
    assert_eq!(label(Some(&both), None, &a), "15:00 (in 1h) · idle 3h/8h");
    let pin = Pin { name: a.name.clone(), id: None, until: None, note: None, set_at: T };
    assert_eq!(label(Some(&idle), Some(&pin), &a), "pinned");
    let pin_until = Pin { until: Some(T + 4 * 86_400), ..pin };
    assert_eq!(label(Some(&idle), Some(&pin_until), &a), "pinned→10-12");
    assert_eq!(label(Some(&idle), None, &Pod { locked: Some(true), ..a.clone() }), "locked");
    assert_eq!(label(Some(&with_last(idle.clone(), Row::Keep)), None, &a), "kept on pod");
    assert_eq!(label(Some(&with_last(idle.clone(), Row::NotCovered)), None, &a), "not covered");
    assert_eq!(label(Some(&idle), None, &pod("devtest-zulu")), "ignored (off-list)");
    // RR2: a policy the reaper can't act on never counts down, never reads OVERDUE.
    let overdue = pol(Some(T - 35 * 60), None);
    assert_eq!(ends_label(Some(&overdue), None, &a, &naming, Some("vast"), T, TZ), "not reapable (vast)");
    assert_eq!(label(Some(&idle), None, &pod("devtest-boss")), "ignored (off-list)");
}

/// The plan table: header, rows, footer.
#[test]
fn plan_table_renders() {
    let h = PlanHeader {
        now: T,
        tz: TZ,
        mode: Mode::Observe,
        demoted: None,
        max_per_run: 1,
        max_per_day: 10,
        used_today: 2,
        state_path: "/var/lib/arena/arena9/reap.json".into(),
    };
    let rows = vec![PlanRow {
        name: "devtest-foxtrot".into(),
        key: "runpod:f".into(),
        policy: "idle 8h".into(),
        state: "idle 8h05m (33 obs)".into(),
        row: Row::WouldReap,
        next: next_label(Row::WouldReap).into(),
        why: why_label(Row::WouldReap, &[]),
    }];
    let out = render_plan(&h, &rows, &["vast: listing failed — its 3 pod(s) untouched".into()]);
    assert!(out.starts_with("Reap 2026-10-08 14:00 +02:00 · DRY RUN · caps 1/run 10/day (2 used) · state /var/lib/arena/arena9/reap.json\n"), "{out}");
    assert!(out.contains("NAME") && out.contains("WOULD TERMINATE") && out.contains("backup first"), "{out}");
    assert!(out.ends_with("vast: listing failed — its 3 pod(s) untouched\n"));
    let held = render_plan(&PlanHeader { demoted: Some("reaping is on hold".into()), ..h.clone() }, &[], &[]);
    assert!(held.contains("OBSERVE (reaping is on hold)") && held.contains("no pod has a reap policy"), "{held}");
    assert!(render_plan(&PlanHeader { mode: Mode::Apply, ..h }, &[], &[]).contains("· APPLY ·"));
    assert_eq!(why_label(Row::Active, &[Reason::Session, Reason::GpuUtil]), "in use: session, gpu_util");
    assert_eq!(why_label(Row::Unknown, &[Reason::FilesTimeout]), "can't tell: files_timeout");
    assert_eq!(next_label(Row::Locked), "nothing");
}

/// The banner: interactive shells only, on stderr — never breaks rsync/scp over a sourced rc.
#[test]
fn banner_is_interactive_only_and_on_stderr() {
    assert!(BANNER_LINE.contains("case $- in *i*)") && BANNER_LINE.contains(">&2"));
    assert!(!BANNER_LINE.contains('\n'));
    let cmd = notice_write_command("hello 'there'\n");
    assert!(cmd.contains("base64 -d > \"$HOME/.arena/NOTICE.tmp\"") && cmd.contains("grep -qxF --"), "{cmd}");
    assert!(cmd.contains(&crate::base64::encode(b"hello 'there'\n")));
    let p = PodPolicy::new(&pod("devtest-alpha"), Some(T + 3600), Some(8 * 3600), T, "set", None);
    let n = notice_text("devtest-alpha", &p, TZ);
    assert!(n.contains("2026-10-08 15:00 (UTC+02:00)") && n.contains("after 8h with no activity") && n.contains("~/.arena/keep"), "{n}");
    let w = wall_command("devtest-alpha", Trigger::Idle, 3600);
    assert!(w.contains("| wall") && w.contains("in about 1h") && w.ends_with("; true"), "{w}");
}

/// RR14: the notice command run for real, twice, in a temp HOME: one banner line per rc file;
/// a non-interactive `sh -c` / `bash -c` sourcing the rc prints NOTHING (stdout or stderr —
/// rsync and scp run through it); an interactive bash prints the notice, on stderr only.
#[cfg(unix)]
#[test]
fn banner_is_silent_unless_interactive_and_then_on_stderr() {
    use std::process::{Command, Stdio};
    let home = std::env::temp_dir().join(format!("arena-reap-banner-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".bashrc"), "# the image's\n").unwrap();
    std::fs::write(home.join(".zshrc"), "# the image's\n").unwrap();
    let text = "arena: devtest-alpha is set to be shut down at 18:00\n";
    let run = |prog: &str, args: &[&str]| {
        Command::new(prog)
            .args(args)
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    for _ in 0..2 {
        let o = run("sh", &["-c", &notice_write_command(text)]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    assert_eq!(std::fs::read_to_string(home.join(".arena/NOTICE")).unwrap(), text);
    for rc in [".bashrc", ".zshrc"] {
        let body = std::fs::read_to_string(home.join(rc)).unwrap();
        assert_eq!(body.lines().filter(|l| *l == BANNER_LINE).count(), 1, "{rc}:\n{body}");
    }
    // Non-interactive: nothing on either stream.
    for (prog, script) in [("sh", ". \"$HOME/.bashrc\""), ("bash", ". \"$HOME/.bashrc\"; true")] {
        let o = run(prog, &["-c", script]);
        assert!(o.stdout.is_empty() && o.stderr.is_empty(), "{prog}: {:?} / {:?}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    }
    // Interactive (bash reads ~/.bashrc): the notice on stderr, stdout untouched.
    if Command::new("bash").arg("--version").output().is_ok() {
        let o = run("bash", &["-i", "-c", "true"]);
        assert!(o.stdout.is_empty(), "{:?}", String::from_utf8_lossy(&o.stdout));
        assert!(String::from_utf8_lossy(&o.stderr).contains("set to be shut down"), "{:?}", String::from_utf8_lossy(&o.stderr));
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// The cron line, both modes.
#[test]
fn cron_line_both_modes() {
    let env = vec![("ARENA_STATE_DIR".to_string(), "/var/lib/arena".to_string())];
    let observe = cron_line(false, "/usr/local/bin/arena", "/root/arena/config.env", "/root", &env);
    assert_eq!(
        observe,
        "7,22,37,52 * * * * PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin ARENA_STATE_DIR=/var/lib/arena flock -n -o /root/.arena-reap-cron.lock /usr/local/bin/arena --config /root/arena/config.env pods reap run >> /root/arena-reap-cron.log 2>&1"
    );
    let apply = cron_line(true, "/usr/local/bin/arena", "/root/arena/config.env", "/root", &[]);
    assert!(apply.contains(" pods reap run --apply --yes >> "), "{apply}");
    assert!(!apply.contains("ARENA_STATE_DIR"));
}

/// Notifications: fixed words, one line, rate-limited.
#[test]
fn notifications() {
    let l = notify_line("devtest", NotifyKind::Reaped, Some("devtest-alpha"), "backup /b/x\nrm -rf /");
    assert_eq!(l, "arena reap [devtest] reaped devtest-alpha: backup /b/xrm -rf /");
    assert!(may_notify(None, T));
    assert!(!may_notify(Some(T - NOTIFY_REPEAT + 1), T));
    assert!(may_notify(Some(T - NOTIFY_REPEAT), T));
    assert!(may_notify(Some(T + 5), T), "a clock gone back doesn't silence it");
}

/// Spec I15, `log_lines_are_fixed_vocabulary`: a log line carries only enums, numbers, our own
/// pod names and backup paths — a hostile pod's output can't get in (only facts leave the
/// parser), and the JSON round-trips strictly.
#[test]
fn log_lines_are_fixed_vocabulary() {
    use crate::reap::judge::{classify, ClassifyCtx, JobsOutcome};
    use crate::reap::probe::fixture::Reply;
    use crate::reap::probe::parse_probe;
    let hostile = Reply {
        panes: vec!["$(rm-rf)".into(), "AKIAXXXXSECRET".into()],
        tmux: "ok",
        sshd: [Reply::default().sshd, vec![(9, "sshd: root@pts/0 10.1.2.3 sk-live-123".into())]].concat(),
        ..Default::default()
    };
    let text = hostile.render();
    let facts = parse_probe(&text, "N0NCE", hostile.since);
    let o = classify(None, &facts, JobsOutcome::Read { busy: 0 }, &ClassifyCtx { provider: "runpod", listed_id: "pod1", now: hostile.now(), clock_ok: true, baseline: None });
    let line = LogLine {
        t: T,
        run: "r1".into(),
        mode: Mode::Observe,
        kind: "eval".into(),
        pod: Some("devtest-alpha".into()),
        key: Some("runpod:pod1".into()),
        row: crate::reap::judge::observation_row(&o, false).row,
        reasons: o.reasons.clone(),
        trigger: None,
        streak_obs: None,
        streak_secs: None,
        paths: vec![],
    };
    let json = line.to_json();
    for leak in ["rm-rf", "AKIA", "SECRET", "10.1.2.3", "sk-live", "pts/0", "Welcome"] {
        assert!(!json.contains(leak), "{leak} leaked: {json}");
    }
    assert!(json.contains("\"row\":\"active\"") && json.contains("\"tmux\""), "{json}");
    let back: LogLine = serde_json::from_str(&json).unwrap();
    assert_eq!(back, line);
    assert!(serde_json::from_str::<LogLine>(&json.replace("\"tmux\"", "\"free text\"")).is_err());
}

/// Times in the zone and as spans.
#[test]
fn time_formats() {
    assert_eq!(fmt_local(T, 0), "2026-10-08 12:00");
    assert_eq!(fmt_local(T, -5 * 3600), "2026-10-08 07:00");
    assert_eq!(fmt_local(T + 11 * 3600 + 59 * 60, 3600), "2026-10-09 00:59");
    assert_eq!((fmt_tz(TZ), fmt_tz(-16_200), fmt_tz(0)), ("+02:00".to_string(), "-04:30".to_string(), "+00:00".to_string()));
    assert_eq!([fmt_span(59), fmt_span(3600), fmt_span(6780), fmt_span(90_000)], ["0m", "1h", "1h53m", "1d1h"]);
    let p = PodPolicy::new(&pod("devtest-alpha"), Some(T + 3600), Some(8 * 3600), T, "set", None);
    assert_eq!(policy_label(&p, T, TZ), "expiry 15:00 (in 1h) + idle 8h");
    assert_eq!(state_label(&p, Row::StreakShort, T), "in 1h");
    assert_eq!(state_label(&p, Row::ProbeFailed, T), "?");
    assert_eq!(state_label(&PodPolicy { expires_at: Some(T - 1200), ..p }, Row::ProbeFailed, T), "OVERDUE 20m");
}
