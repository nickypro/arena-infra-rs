use super::*;

const NOW: u64 = 1_791_460_800; // 2026-10-08T12:00:00Z (a Thursday)

/// Settings: defaults, every bound, and out of range is an error (never clamped).
#[test]
fn settings_defaults_and_bounds() {
    let s = |text: &str| ReapSettings::from_config(&Config::parse(text));
    let d = s("").unwrap();
    assert_eq!(d, ReapSettings::default());
    assert!(!d.enabled && d.max_per_run == 1 && d.max_per_day == 10 && d.idle_floor_secs == 7200 && d.notify_cmd.is_none());
    assert!(s("REAP_ENABLED=1").unwrap().enabled);
    assert!(!s("REAP_ENABLED=0").unwrap().enabled);
    assert!(!s("REAP_ENABLED=\"\"").unwrap().enabled);
    for bad in ["REAP_ENABLED=yes", "REAP_ENABLED=true", "REAP_ENABLED=2"] {
        assert!(s(bad).unwrap_err().contains("REAP_ENABLED must be 1"), "{bad}");
    }
    // (key, ok values, bad values)
    let cases: &[(&str, &[&str], &[&str])] = &[
        ("REAP_MAX_PER_RUN", &["1", "5"], &["0", "6", "-1", "x", "1.5"]),
        ("REAP_MAX_PER_DAY", &["1", "50"], &["0", "51"]),
        ("REAP_IDLE_FLOOR_MINS", &["15", "1440"], &["14", "1441"]),
        ("REAP_MIN_FREE_GB", &["1", "100000"], &["0", "100001"]),
    ];
    for (key, ok, bad) in cases {
        for v in *ok {
            assert!(s(&format!("{key}={v}")).is_ok(), "{key}={v}");
        }
        for v in *bad {
            let e = s(&format!("{key}={v}")).unwrap_err();
            assert!(e.contains(key) && e.contains(".."), "{key}={v}: {e}");
        }
    }
    assert_eq!(s("REAP_IDLE_FLOOR_MINS=15").unwrap().idle_floor_secs, 900);
    assert_eq!(s("REAP_MIN_FREE_GB=3").unwrap().min_free_bytes, 3_000_000_000);
    assert_eq!(s("REAP_NOTIFY_CMD=\"logger -t arena\"").unwrap().notify_cmd.as_deref(), Some("logger -t arena"));
}

/// `parse_dur`: `(\d+[mhd])+` only.
#[test]
fn durations_parse_strictly() {
    for (t, want) in [("90m", 5400), ("8h", 28_800), ("1d12h", 129_600), ("1h30m", 5400), ("2d", 172_800), ("0m", 0), ("15m", 900)] {
        assert_eq!(parse_dur(t), Ok(want), "{t}");
    }
    for bad in ["", "90", "m", "1x", "1H", "1h ", " 1h", "1.5h", "-1h", "1s", "h1", "1hh", "99999999999999999999d", "18446744073709551615d"] {
        assert!(parse_dur(bad).is_err(), "{bad:?}");
    }
    assert!(check_expire_in(600).is_ok() && check_expire_in(60 * 86_400).is_ok());
    assert!(check_expire_in(599).is_err() && check_expire_in(60 * 86_400 + 1).is_err());
    assert!(check_idle_after(900, 900).is_ok() && check_idle_after(14 * 86_400, 900).is_ok());
    assert!(check_idle_after(899, 900).unwrap_err().contains("REAP_IDLE_FLOOR_MINS"));
    assert!(check_idle_after(14 * 86_400 + 60, 900).is_err());
    assert_eq!([fmt_dur(30), fmt_dur(2700), fmt_dur(7200), fmt_dur(11_100), fmt_dur(86_400), fmt_dur(93_600)], ["30s", "45m", "2h", "3h05m", "1d", "1d2h"]);
}

/// `parse_time`: RFC 3339 and local `YYYY-MM-DD HH:MM`; past, < 10 min and > 60 days refused.
#[test]
fn times_parse_and_are_bounded() {
    let local = |t: &str| -> Option<i64> { (t == "2026-10-08 14:00").then_some((NOW + 3600) as i64) };
    let ok = |t: &str| parse_time(t, NOW, &local);
    assert_eq!(ok("2026-10-08T18:00:00Z"), Ok(NOW + 6 * 3600));
    assert_eq!(ok("2026-10-08T18:00Z"), Ok(NOW + 6 * 3600));
    assert_eq!(ok("2026-10-08t18:00:00.123z"), Ok(NOW + 6 * 3600));
    assert_eq!(ok("2026-10-08T20:00:00+02:00"), Ok(NOW + 6 * 3600));
    assert_eq!(ok("2026-10-08T13:30:00-04:30"), Ok(NOW + 6 * 3600));
    assert_eq!(ok("2026-10-08 14:00"), Ok(NOW + 3600));
    assert!(ok("2026-10-08 15:00").unwrap_err().contains("couldn't resolve"));
    // Past, too soon, too far.
    assert!(ok("2026-10-08T11:00:00Z").unwrap_err().contains("in the past"));
    assert!(ok("2026-10-08T12:00:00Z").unwrap_err().contains("in the past"));
    assert!(ok("2026-10-08T12:09:00Z").unwrap_err().contains("less than 10m away"));
    assert!(ok("2026-10-08T12:10:00Z").is_ok());
    assert!(ok("2026-12-08T12:00:01Z").unwrap_err().contains("more than 60d away"));
    for bad in ["tomorrow", "2026-10-08", "2026-10-08T25:00:00Z", "2026-02-30T10:00:00Z", "2026-10-08T18:00:00", "2026-10-08T18:00:00+2", "2026-10-08 18:00:00", "2026-10-08T18:00:00.Z", "2026-13-01T00:00:00Z"] {
        assert!(ok(bad).is_err(), "{bad}");
    }
}

fn pod(name: &str) -> Pod {
    Pod { id: format!("id-{name}"), name: name.into(), provider: "runpod".into(), status: "RUNNING".into(), ..Default::default() }
}

/// The state round-trips, and parsing is STRICT: unknown fields, other versions and junk are
/// errors — never an empty state.
#[test]
fn state_parses_strictly() {
    let mut s = ReapState::default();
    let mut p = PodPolicy::new(&pod("devtest-a"), Some(NOW + 3600), Some(28_800), NOW, "set", Some("week 2".into()));
    p.observations.push(Observation {
        t: NOW,
        clock_ok: true,
        container: Some("b/1".into()),
        pod_t: Some(NOW + 2),
        verdict: Verdict::Active,
        reasons: vec![Reason::Session, Reason::GpuMem],
        cpu_usec: Some(1),
        net_bytes: Some(2),
        counters_t: Some(NOW - 10),
        scan_from: Some(NOW - 900),
    });
    p.notified.insert(render::NotifyKind::OverdueInUse, NOW);
    s.pods.insert(p.key(), p);
    s.pins.push(Pin { name: "devtest-b".into(), id: Some("id-b".into()), until: None, note: None, set_at: NOW });
    s.attempts.push(NOW - 100);
    let text = s.to_json();
    assert_eq!(ReapState::parse(&text).unwrap(), s);
    assert!(text.contains("\"runpod:id-devtest-a\"") && text.contains("\"gpu_mem\"") && text.contains("\"overdue_in_use\""));
    // Each of these is an error, never "empty".
    let bad = [
        String::new(),
        "{}".to_string(),
        "null".to_string(),
        "[]".to_string(),
        text.replace("\"version\": 1", "\"version\": 2"),
        text.replace("\"version\": 1", "\"version\": \"1\""),
        text.replace("\"set_by\"", "\"set_by_extra\": 1, \"set_by\""),
        text.replace("\"gpu_mem\"", "\"gpu_memory\""),
        text.replace("\"overdue_in_use\"", "\"somebody_said_so\""),
        text.replace("\"verdict\": \"active\"", "\"verdict\": \"idle-ish\""),
        text[..text.len() / 2].to_string(),
        format!("{text}junk"),
    ];
    for b in &bad {
        assert!(ReapState::parse(b).is_err(), "{b}");
    }
}

/// Housekeeping: ended pins, day-old attempts, old tombstones and observations go; the rest stay.
#[test]
fn tidy_drops_only_what_expired() {
    let mut s = ReapState::default();
    s.pins.push(Pin { name: "a".into(), id: None, until: Some(NOW - 1), note: None, set_at: 0 });
    s.pins.push(Pin { name: "b".into(), id: None, until: Some(NOW + 1), note: None, set_at: 0 });
    s.pins.push(Pin { name: "c".into(), id: None, until: None, note: None, set_at: 0 });
    s.attempts = vec![NOW - 86_400, NOW - 86_399, NOW];
    s.reaped.insert("runpod:x".into(), Tombstone { at: NOW - TOMBSTONE_KEEP, name: "x".into(), trigger: None, snapshot_dir: None, big_dir: None, branch: None, recovered: false, seen_after: 0 });
    s.reaped.insert("runpod:y".into(), Tombstone { at: NOW - 10, name: "y".into(), trigger: None, snapshot_dir: None, big_dir: None, branch: None, recovered: false, seen_after: 0 });
    let mut p = PodPolicy::new(&pod("devtest-a"), None, Some(3600), NOW, "set", None);
    for i in 0..1200u64 {
        p.observations.push(Observation { t: NOW - 20_000 + i * 10, clock_ok: true, container: None, pod_t: None, verdict: Verdict::Unknown, reasons: vec![], cpu_usec: None, net_bytes: None, counters_t: None, scan_from: None });
    }
    s.pods.insert(p.key(), p);
    let gone = s.tidy(NOW);
    assert_eq!(gone.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["a"]);
    assert_eq!(s.pins.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["b", "c"]);
    assert_eq!(s.attempts, [NOW - 86_399, NOW]);
    assert_eq!(s.reaped.keys().collect::<Vec<_>>(), ["runpod:y"]);
    let obs = &s.pods.values().next().unwrap().observations;
    // window = max(1 h idle, 1 h lead) + 2 h = 3 h.
    assert!(obs.iter().all(|o| NOW - o.t <= 3 * 3600) && obs.len() <= OBS_KEEP_MAX && !obs.is_empty());
}

/// A clock gone back still counts its attempts (more counted = fewer reaps, the safe side).
#[test]
fn attempts_today_counts_future_ones() {
    let s = ReapState { attempts: vec![NOW - 90_000, NOW - 100, NOW + 500], ..Default::default() };
    assert_eq!(s.attempts_today(NOW), 2);
}

/// Pins hold by name OR id until their end.
#[test]
fn pins_hold_by_name_or_id() {
    let p = Pin { name: "devtest-a".into(), id: Some("id1".into()), until: Some(NOW + 60), note: None, set_at: NOW };
    assert!(p.holds("devtest-a", "other", NOW));
    assert!(p.holds("devtest-renamed", "id1", NOW));
    assert!(!p.holds("devtest-b", "id2", NOW));
    assert!(!p.holds("devtest-a", "id1", NOW + 60), "ended");
}

/// The lock a terminate needs to be free of: RunPod's lock, Hetzner's delete protection;
/// anything else (Vast, an unreported state) is unknown.
#[test]
fn lock_state_per_provider() {
    let with = |provider: &str, locked: Option<bool>, protected: Option<bool>| Pod {
        provider: provider.into(),
        locked,
        delete_protected: protected,
        ..Default::default()
    };
    assert_eq!(lock_state(&with("runpod", Some(false), Some(true))), Some(false));
    assert_eq!(lock_state(&with("runpod", None, Some(false))), None);
    assert_eq!(lock_state(&with("hetzner", Some(false), Some(true))), Some(true));
    assert_eq!(lock_state(&with("hetzner", None, Some(false))), Some(false));
    assert_eq!(lock_state(&with("hetzner", None, None)), None);
    assert_eq!(lock_state(&with("vast", Some(false), Some(false))), None);
    assert_eq!(min_obs(15 * 60), 4);
    assert_eq!(min_obs(8 * 3600), 16);
    assert_eq!(min_obs(8 * 3600 + 1), 17);
}

/// The identity guard every backup step runs behind: true only on the pod itself.
#[cfg(target_os = "linux")]
#[test]
fn identity_condition_holds_only_on_the_pod() {
    let c = identity_condition("runpod", "abc").unwrap();
    assert!(c.contains("RUNPOD_POD_ID") && c.contains("'abc'"), "{c}");
    let h = identity_condition("hetzner", "123").unwrap();
    assert!(h.contains("/var/lib/cloud/data/instance-id") && h.ends_with("= '123' ]"), "{h}");
    assert_eq!(identity_condition("vast", "1"), None);
    // On this machine (no RUNPOD_POD_ID in PID 1's env, or unreadable): false.
    let st = std::process::Command::new("sh").args(["-c", &format!("{c} && echo yes || echo no")]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&st.stdout).trim(), "no");
}
