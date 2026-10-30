use super::record::{Event, Outcome, PodRecord, Record, RunRecord, Running};
use super::*;
use crate::archive::state::{LabelRecord, RunRecord as SyncRun};

/// 2026-10-30 21:34:57 UTC, a Friday.
const NOW: u64 = 1_793_396_097;
const MIN: u64 = 60;
const HOUR: u64 = 3600;
const GB: u64 = disk::GB;

const BACKUP_LINE: &str = "*/15 * * * * mkdir -p /root/.local/state/arena-cron && flock -n -o /root/.local/state/arena-cron/backup.lock \
                           /usr/local/bin/arena --config /c pods backup --yes >> /root/.local/state/arena-cron/backup.log 2>&1";
const GIT_ONLY_LINE: &str = "*/15 * * * * flock -n -o /root/.local/state/arena-cron/backup.lock /usr/local/bin/arena --config /c pods backup --no-pull --yes";
const ARCHIVE_LINES: &str = "7,22,37,52 * * * * nice -n 10 /a --config /c archive sync --tier snap >> /root/.local/state/arena-cron/archive.log 2>&1\n\
                             40 * * * * nice -n 10 /a --config /c archive sync --tier big >> /root/.local/state/arena-cron/archive.log 2>&1";

fn tab(lines: &[&str]) -> Result<String, String> {
    Ok(format!("MAILTO=root\n{}\n{}\n{}\n", crate::cron::BEGIN, lines.join("\n"), crate::cron::END))
}

fn pod(name: &str) -> PodIn {
    PodIn { name: format!("devtest-{name}"), cohort: true, up: Some(true), last_commit: None }
}

/// Nothing set up, nothing recorded: two cohort pods.
fn inputs() -> Inputs {
    Inputs {
        now: NOW,
        offset: 0,
        prefix: "devtest".into(),
        config_path: Some("/c".into()),
        cron_config: None,
        crontab: Ok(String::new()),
        record: Ok(None),
        record_path: "/s/devtest/backups.json".into(),
        pods: vec![pod("alpha"), pod("bravo")],
        backup_dir: "/b".into(),
        copied: Ok(BTreeMap::new()),
        labels: Ok(BTreeMap::new()),
        archive: ArchiveIn::None,
        held: Held { cron: None, pods: Ok(BTreeSet::new()), archive: BTreeMap::new(), alive: BTreeSet::new() },
        logs: BTreeMap::new(),
        storage: Vec::new(),
        floor: Ok(5 * GB),
        skips: Ok(Vec::new()),
        repo: Some("owner/repo".into()),
    }
}

fn ev(at: u64, job: Job, outcome: Outcome, detail: &str) -> Event {
    Event::new(at, job, outcome, detail)
}

/// A record of one git run and one copy run, ending `ago` seconds before NOW: `pods`' events.
fn record_with(ago_s: u64, git: &[(&str, Outcome, &str)], copy: &[(&str, Outcome, &str, Option<u64>)]) -> Record {
    let mut r = Record::default();
    let at = NOW - ago_s;
    r.start(Job::Git, 7, git.len() as u32, at - 60);
    let events = git.iter().map(|(p, o, d)| (format!("devtest-{p}"), ev(at - 30, Job::Git, *o, d))).collect();
    r.finish(Job::Git, 7, at - 20, events, format!("{} pod(s)", git.len()));
    r.start(Job::Copy, 7, copy.len() as u32, at - 20);
    let events = copy.iter().map(|(p, o, d, b)| (format!("devtest-{p}"), ev(at, Job::Copy, *o, d).with_bytes(*b))).collect();
    r.finish(Job::Copy, 7, at, events, format!("{} pod(s)", copy.len()));
    r
}

fn job(s: &Status, j: Job) -> &JobStatus {
    s.jobs.iter().find(|x| x.job == j).unwrap()
}

fn pod_of<'a>(s: &'a Status, name: &str) -> &'a PodStatus {
    s.pods.iter().find(|p| p.name == format!("devtest-{name}")).unwrap_or_else(|| panic!("no {name} in {:?}", s.pods.iter().map(|p| &p.name).collect::<Vec<_>>()))
}

/// Each job set up or not, from the crontab and the archive's state: the row says how often, or
/// what sets it up — and a crontab that can't be read is unknown, never "not set up".
#[test]
fn each_job_is_set_up_by_its_crontab_line_or_says_what_sets_it_up() {
    let up = || ArchiveIn::Up {
        host: Box::new(ArchiveHost {
            version: 1,
            prefix: "devtest".into(),
            server_id: 7,
            ipv4: "203.0.113.9".into(),
            location: "nbg1".into(),
            snap_device: "/dev/disk/by-id/scsi-0HC_Volume_1".into(),
            big_device: "/dev/disk/by-id/scsi-0HC_Volume_2".into(),
            access_on: false,
            setup_version: 1,
            snap_gb: 50,
            big_gb: 200,
            host_key_fingerprint: None,
        }),
        snap: Ok(SnapState::default()),
        big: Ok(BigState::default()),
    };
    type Case = (&'static str, Result<String, String>, fn() -> ArchiveIn, [(&'static str, Health); 4]);
    let none: fn() -> ArchiveIn = || ArchiveIn::None;
    let cases: Vec<Case> = vec![
        (
            "nothing",
            Ok(String::new()),
            none,
            [
                ("not set up (no backup line in the crontab) — `arena cron install --backup --pull`", Health::Off),
                ("not set up (no backup line in the crontab) — `arena cron install --backup --pull`", Health::Off),
                ("not set up (no backup archive) — `arena archive up, then arena cron install --archive`", Health::Off),
                ("not set up (no backup archive) — `arena archive up, then arena cron install --archive`", Health::Off),
            ],
        ),
        (
            "git only",
            tab(&[GIT_ONLY_LINE]),
            none,
            [
                ("every 15 min", Health::Unknown),
                ("not set up (the backup line runs `--no-pull`: only a pod git couldn't back up is copied) — `arena cron install --backup --pull`", Health::Off),
                ("not set up (no backup archive) — `arena archive up, then arena cron install --archive`", Health::Off),
                ("not set up (no backup archive) — `arena archive up, then arena cron install --archive`", Health::Off),
            ],
        ),
        (
            "git and copies, archive up without its lines",
            tab(&[BACKUP_LINE]),
            up,
            [
                ("every 15 min", Health::Unknown),
                ("with each git backup (every 15 min)", Health::Unknown),
                ("not set up (the archive is up but no cron line pushes to it) — `arena cron install --archive`", Health::Off),
                ("not set up (the archive is up but no cron line pushes to it) — `arena cron install --archive`", Health::Off),
            ],
        ),
        (
            "everything",
            tab(&[BACKUP_LINE, ARCHIVE_LINES]),
            up,
            [
                ("every 15 min", Health::Unknown),
                ("with each git backup (every 15 min)", Health::Unknown),
                ("hourly at :07 :22 :37 :52", Health::Unknown),
                ("hourly at :40", Health::Unknown),
            ],
        ),
        (
            "an unreadable crontab",
            Err("crontab: permission denied".into()),
            none,
            [
                ("can't read the crontab (crontab: permission denied)", Health::Unknown),
                ("can't read the crontab (crontab: permission denied)", Health::Unknown),
                ("not set up (no backup archive) — `arena archive up, then arena cron install --archive`", Health::Off),
                ("not set up (no backup archive) — `arena archive up, then arena cron install --archive`", Health::Off),
            ],
        ),
    ];
    for (what, crontab, archive, want) in cases {
        let s = judge(&Inputs { crontab, archive: archive(), ..inputs() });
        for (j, (words, health)) in Job::ALL.iter().zip(want) {
            let row = job(&s, *j);
            let said = match &row.setup {
                Setup::Cron { every, .. } => every.clone(),
                Setup::Missing { .. } => row.why.clone().unwrap_or_default(),
                Setup::Unknown { why } => why.clone(),
            };
            assert_eq!((said.as_str(), row.health), (words, health), "{what}: {}", j.name());
        }
    }
    // The next run, in the box's zone.
    let s = judge(&Inputs { crontab: tab(&[BACKUP_LINE, ARCHIVE_LINES]), archive: up(), ..inputs() });
    assert_eq!(job(&s, Job::Git).next, Some(NOW - 57 - 34 * MIN + 45 * MIN));
    assert_eq!(job(&s, Job::ArchiveSnap).next, Some(NOW - 57 - 34 * MIN + 37 * MIN));
    assert_eq!(job(&s, Job::ArchiveBig).next, Some(NOW - 57 - 34 * MIN + 40 * MIN));
}

/// A job's last run, from the record: OK, failed (naming the pods), late, running (then never
/// late), killed, pods on main — and nothing recorded or an unreadable record is unknown, never
/// "never" or "ok".
#[test]
fn a_jobs_last_run_says_ok_failed_late_running_or_unknown() {
    let ok = record_with(5 * MIN, &[("alpha", Outcome::Pushed, "→ b"), ("bravo", Outcome::NoChanges, "")], &[("alpha", Outcome::Copied, "", None)]);
    let failed = record_with(5 * MIN, &[("alpha", Outcome::Pushed, ""), ("bravo", Outcome::Failed, "exit 255: ssh: connect timed out")], &[]);
    let late = record_with(3 * HOUR, &[("alpha", Outcome::Pushed, "")], &[("alpha", Outcome::Copied, "", None)]);
    let on_main = record_with(5 * MIN, &[("alpha", Outcome::NotSaved, "on main"), ("bravo", Outcome::Pushed, "")], &[]);
    let mut running = late.clone();
    running.start(Job::Git, 4242, 2, NOW - 2 * MIN);
    let log = || BTreeMap::from([(Kind::Backup, Ok(LogFacts { path: "/l/backup.log".into(), bytes: 9, modified: Some(NOW - 3 * MIN) }))]);
    let alive = |pids: &[u32]| Held { alive: pids.iter().copied().collect(), ..inputs().held };
    struct Case {
        what: &'static str,
        record: Result<Option<Record>, String>,
        held: Held,
        logs: BTreeMap<Kind, Result<LogFacts, String>>,
        git: (Health, Option<&'static str>),
    }
    let cases = [
        Case { what: "recent and ok", record: Ok(Some(ok.clone())), held: alive(&[]), logs: log(), git: (Health::Ok, None) },
        Case { what: "a pod failed", record: Ok(Some(failed)), held: alive(&[]), logs: log(), git: (Health::Fail, Some("1 failed: devtest-bravo")) },
        Case { what: "late", record: Ok(Some(late.clone())), held: alive(&[]), logs: log(), git: (Health::Warn, Some("late: the last run ended 3 h ago (every 15 min)")) },
        Case { what: "running: not late", record: Ok(Some(running.clone())), held: alive(&[4242]), logs: log(), git: (Health::Ok, None) },
        Case {
            what: "on main",
            record: Ok(Some(on_main)),
            held: alive(&[]),
            logs: log(),
            git: (Health::Warn, Some("1 on main/master — not saved to git until `arena pods init-branches`")),
        },
        Case {
            what: "nothing recorded",
            record: Ok(None),
            held: alive(&[]),
            logs: log(),
            git: (Health::Unknown, Some("none recorded yet (this record starts with this arena version) — its cron log was last written 3 min ago")),
        },
        Case {
            what: "unreadable",
            record: Err("/s/backups.json is version 9 (this arena reads 1)".into()),
            held: alive(&[]),
            logs: BTreeMap::new(),
            git: (Health::Unknown, Some("the run record can't be read: /s/backups.json is version 9 (this arena reads 1)")),
        },
    ];
    for c in cases {
        let s = judge(&Inputs { crontab: tab(&[BACKUP_LINE]), record: c.record, held: c.held, logs: c.logs, ..inputs() });
        let g = job(&s, Job::Git);
        assert_eq!((g.health, g.why.as_deref()), c.git, "{}", c.what);
    }
    // Running: since when, said under "running now"; the copy's last run is its own.
    let s = judge(&Inputs { crontab: tab(&[BACKUP_LINE]), record: Ok(Some(running.clone())), held: alive(&[4242]), ..inputs() });
    assert_eq!(job(&s, Job::Git).running_since, Some(NOW - 2 * MIN));
    assert_eq!(s.running, [RunningNow { what: "git backup".into(), since: Some(NOW - 2 * MIN) }]);
    assert_eq!(job(&s, Job::Copy).last.as_ref().map(|l| l.finished), Some(NOW - 3 * HOUR));
    // Killed: the run under way whose pid is gone is a note, and the job is judged by its last.
    let s = judge(&Inputs { crontab: tab(&[BACKUP_LINE]), record: Ok(Some(running)), ..inputs() });
    assert!(s.running.is_empty());
    assert_eq!(s.notes, ["a git backup run started 2 min ago (pid 4242) ended without finishing — killed or crashed"]);
    // The cron's lock held by an arena that keeps no record: running, since unknown.
    let held = Held { cron: Some(Ok(true)), ..inputs().held };
    let s = judge(&Inputs { crontab: tab(&[BACKUP_LINE]), record: Ok(Some(late)), held, ..inputs() });
    assert_eq!(job(&s, Job::Git).running_since, Some(0));
    assert_eq!((job(&s, Job::Git).health, s.running[0].what.as_str()), (Health::Ok, "the cron's backup (pods backup)"));
    // Locks that can't be read: running now is unknown, said why.
    let held = Held { pods: Err("can't read /proc/locks (No such file or directory)".into()), ..inputs().held };
    let s = judge(&Inputs { held, ..inputs() });
    assert_eq!(s.running_unknown.as_deref(), Some("can't read /proc/locks (No such file or directory)"));
    // A pod's lock held: it's being backed up now.
    let held = Held { pods: Ok(BTreeSet::from(["devtest-bravo".to_string()])), ..inputs().held };
    let s = judge(&Inputs { held, ..inputs() });
    assert!(pod_of(&s, "bravo").running && !pod_of(&s, "alpha").running);
    assert_eq!(s.running, [RunningNow { what: "backing up now: devtest-bravo".into(), since: None }]);
}

/// Each pod's git, home copy and archive cells, failing first and then the longest since a good
/// backup: a failure names its job and reason, a quiet pod is late (unless it's stopped or not
/// the cohort's), a copy no record has is read from its marker, and what can't be read is unknown.
#[test]
fn each_pods_backups_are_judged_failing_first_then_oldest() {
    let mut r = record_with(
        10 * MIN,
        &[("alpha", Outcome::Pushed, "→ autocommit-devtest-w3d5-alpha"), ("bravo", Outcome::Failed, "exit 255: ssh: connect timed out"), ("echo", Outcome::NoChanges, "")],
        &[("alpha", Outcome::Copied, "w3d5: 1.8 GB", Some(1_800_000_000)), ("bravo", Outcome::Failed, "[snapshot] rsync exit 24", None), ("echo", Outcome::Copied, "", None)],
    );
    // delta and foxtrot: backed up 5 h ago, nothing since — foxtrot is stopped.
    for p in ["delta", "foxtrot"] {
        let mut d = PodRecord::default();
        d.add(ev(NOW - 5 * HOUR, Job::Git, Outcome::Pushed, ""));
        d.add(ev(NOW - 5 * HOUR, Job::Copy, Outcome::Copied, ""));
        r.pods.insert(format!("devtest-{p}"), d);
    }
    // bravo's copy went through once, a day ago.
    r.pods.get_mut("devtest-bravo").unwrap().copy_ok = Some(NOW - 24 * HOUR);
    let mut snap = SnapState::default();
    snap.labels.insert("w3d4".into(), LabelRecord { ok_finished: NOW - 20 * HOUR, pods_ok: ["devtest-alpha".to_string(), "devtest-echo".to_string()].into(), ..LabelRecord::default() });
    snap.labels.insert("w3d5".into(), LabelRecord { last_tried: NOW - HOUR, pods_ok: ["devtest-alpha".to_string()].into(), ..LabelRecord::default() });
    snap.pods_failing.insert("devtest-echo".into(), "w3d5: NoSpace: no space left on device".into());
    let mut big = BigState::default();
    big.pods_ok_at.insert("devtest-alpha".into(), NOW - 2 * HOUR);
    let host = ArchiveHost {
        version: 1,
        prefix: "devtest".into(),
        server_id: 7,
        ipv4: "203.0.113.9".into(),
        location: "nbg1".into(),
        snap_device: "s".into(),
        big_device: "b".into(),
        access_on: true,
        setup_version: 1,
        snap_gb: 50,
        big_gb: 200,
        host_key_fingerprint: None,
    };
    let pods = vec![
        pod("alpha"),
        pod("bravo"),
        PodIn { last_commit: Some(NOW - 2 * HOUR), ..pod("charlie") },
        pod("delta"),
        pod("echo"),
        PodIn { up: Some(false), ..pod("foxtrot") },
    ];
    let i = Inputs {
        crontab: tab(&[BACKUP_LINE, ARCHIVE_LINES]),
        record: Ok(Some(r)),
        pods,
        copied: Ok(BTreeMap::from([("devtest-charlie".to_string(), NOW - 15 * MIN)])),
        labels: Ok(BTreeMap::from([("devtest-alpha".to_string(), "w3d5".to_string()), ("devtest-charlie".to_string(), "w3d5".to_string())])),
        archive: ArchiveIn::Up { host: Box::new(host), snap: Ok(snap), big: Ok(big) },
        ..inputs()
    };
    let s = judge(&i);
    let rows: Vec<(String, Health, String, String, String, Option<String>)> = s
        .pods
        .iter()
        .map(|p| (p.name.trim_start_matches("devtest-").to_string(), p.health, render::cell(&p.git, NOW), render::cell(&p.copy, NOW), render::cell(&p.archive, NOW), p.why.clone()))
        .collect();
    let want: Vec<(String, Health, String, String, String, Option<String>)> = [
        ("bravo", Health::Fail, "✗ failed 10m (never ok)", "✗ failed 10m (ok 24h)", "? - ", Some("git: exit 255: ssh: connect timed out")),
        ("echo", Health::Fail, "✓ no changes 10m", "✓ copied 10m", "✗ w3d4 20h", Some("archive: snapshots: w3d5: NoSpace: no space left on device")),
        ("delta", Health::Warn, "⚠ pushed 5h", "⚠ copied 5h", "? - ", Some("git: not backed up for 5 h (due every 15 min)")),
        ("charlie", Health::Unknown, "? commit 2h", "✓ w3d5 15m", "? - ", Some("git: no git backup recorded for it yet")),
        ("foxtrot", Health::Unknown, "✓ pushed 5h", "✓ copied 5h", "? - ", Some("archive: not on the archive yet")),
        ("alpha", Health::Ok, "✓ pushed 10m", "✓ 1.8 GB · w3d5 10m", "✓ w3d5 + all files 2h", None),
    ]
    .into_iter()
    .map(|(n, h, g, c, a, w)| (n.to_string(), h, g.to_string(), c.to_string(), a.trim_end().to_string(), w.map(String::from)))
    .collect();
    assert_eq!(rows, want);
    // A staff box isn't the cron's: never late.
    let mut d = PodRecord::default();
    d.add(ev(NOW - 5 * HOUR, Job::Git, Outcome::Pushed, ""));
    let rec = Record { pods: BTreeMap::from([("devtest-golf".to_string(), d)]), ..Record::default() };
    let s = judge(&Inputs { crontab: tab(&[BACKUP_LINE]), record: Ok(Some(rec)), pods: vec![PodIn { cohort: false, ..pod("golf") }], ..inputs() });
    assert_eq!(pod_of(&s, "golf").git.health, Health::Ok);
    // A pod only the record knows (named once, gone since) is shown, as not listed, for three days.
    let s = judge(&Inputs { crontab: tab(&[BACKUP_LINE]), record: Ok(Some(record_with(HOUR, &[("hotel", Outcome::Pushed, "")], &[]))), ..inputs() });
    assert!(!pod_of(&s, "hotel").listed && pod_of(&s, "alpha").listed);
    let s = judge(&Inputs { crontab: tab(&[BACKUP_LINE]), record: Ok(Some(record_with(4 * 86_400, &[("hotel", Outcome::Pushed, "")], &[]))), ..inputs() });
    assert!(!s.pods.iter().any(|p| p.name == "devtest-hotel"));
    // The record unreadable: every git cell unknown, saying why.
    let s = judge(&Inputs { record: Err("can't read /s/backups.json: Permission denied".into()), ..inputs() });
    assert_eq!(pod_of(&s, "alpha").git.why.as_deref(), Some("the run record can't be read: can't read /s/backups.json: Permission denied"));
    // No copy job and nothing on disk: the copy cell is off, not unknown.
    let s = judge(&inputs());
    assert_eq!((pod_of(&s, "alpha").copy.health, pod_of(&s, "alpha").archive.health), (Health::Off, Health::Off));
}

/// Where the copies live, judged by the disk guard: under twice the floor a warning, under it a
/// failure; two places on one filesystem are one row; one that can't be read is unknown.
#[test]
fn the_storage_rows_judge_free_space_by_the_floor() {
    let fs = |dev: u64, mount: &str, free_gb: u64| Ok(Fs { dev, mount: PathBuf::from(mount), free: free_gb * GB, total: 2000 * GB });
    let storage = vec![
        StoreIn { what: "snapshots".into(), path: "/mnt/v1/backup".into(), fs: fs(1, "/mnt/v1", 141) },
        StoreIn { what: "all files".into(), path: "/mnt/v1/backup/big".into(), fs: fs(1, "/mnt/v1", 141) },
    ];
    let s = judge(&Inputs { storage: storage.clone(), ..inputs() });
    assert_eq!(
        s.storage.iter().map(|r| (r.what.as_str(), r.health, r.text.as_str())).collect::<Vec<_>>(),
        [
            ("snapshots + all files", Health::Ok, "141 GB free of 2000 GB on /mnt/v1"),
            ("git", Health::Ok, "each pod's branch (autocommit-devtest-wNdM-<pod>)"),
        ]
    );
    let storage = vec![
        StoreIn { what: "snapshots".into(), path: "/b".into(), fs: fs(1, "/", 3) },
        StoreIn { what: "all files".into(), path: "/v/big".into(), fs: fs(2, "/v", 8) },
        StoreIn { what: "state".into(), path: "/s".into(), fs: Err("Permission denied (os error 13)".into()) },
    ];
    let s = judge(&Inputs { storage, repo: None, ..inputs() });
    let rows: Vec<(Health, &str)> = s.storage.iter().map(|r| (r.health, r.text.as_str())).collect();
    assert_eq!(
        rows,
        [
            (Health::Fail, "3 GB free of 2000 GB on / — UNDER the floor (5 GB): every copy onto it is skipped"),
            (Health::Warn, "8 GB free of 2000 GB on /v — under twice the floor (5 GB, CONTROL_MIN_FREE_GB): the next big copy may be skipped"),
            (Health::Unknown, "free space unknown (Permission denied (os error 13))"),
            (Health::Unknown, "no cohort repo configured (ARENA_REPO_OWNER / ARENA_REPO_NAME): a pod's origin isn't checked"),
        ]
    );
    assert_eq!(s.health, Health::Fail);
}

/// An archive that's down, or whose last push failed, fails its rows; a pod failing there warns.
#[test]
fn the_archive_rows_say_down_failed_or_failing_pods() {
    let host = |server_id| ArchiveHost {
        version: 1,
        prefix: "devtest".into(),
        server_id,
        ipv4: if server_id == 0 { String::new() } else { "203.0.113.9".into() },
        location: "nbg1".into(),
        snap_device: "s".into(),
        big_device: "b".into(),
        access_on: true,
        setup_version: 1,
        snap_gb: 50,
        big_gb: 200,
        host_key_fingerprint: None,
    };
    let ran = |ok| Some(SyncRun { started: NOW - 30 * MIN, finished: NOW - 28 * MIN, ok, all_failed: !ok });
    let snap = |ok, failing: &[&str]| SnapState {
        last_run: ran(ok),
        last_error: (!ok).then(|| crate::archive::state::ErrRecord {
            at: NOW - 28 * MIN,
            label: None,
            class: crate::archive::sync::PushFailure::Unreachable,
            message: "ssh: connect to host 203.0.113.9 port 22: Connection timed out".into(),
        }),
        pods_failing: failing.iter().map(|p| (p.to_string(), "w3d5: InTheWay".to_string())).collect(),
        ..SnapState::default()
    };
    let big = BigState { last_run: ran(true), ..BigState::default() };
    let cases = [
        ("up and ok", host(7), snap(true, &[]), (Health::Ok, None)),
        ("down", host(0), snap(true, &[]), (Health::Fail, Some("the archive VM is down (`arena archive down` ran) — `arena archive up`".to_string()))),
        ("push failed", host(7), snap(false, &[]), (Health::Fail, Some(format!("{}: ssh: connect to host 203.0.113.9 port 22: Connection timed out", crate::archive::sync::PushFailure::Unreachable)))),
        ("a pod failing", host(7), snap(true, &["devtest-bravo"]), (Health::Warn, Some("1 pod(s) failing: devtest-bravo".to_string()))),
    ];
    for (what, h, sn, want) in cases {
        let s = judge(&Inputs { crontab: tab(&[ARCHIVE_LINES]), archive: ArchiveIn::Up { host: Box::new(h), snap: Ok(sn), big: Ok(big.clone()) }, ..inputs() });
        let row = job(&s, Job::ArchiveSnap);
        assert_eq!((row.health, row.why.clone()), want, "{what}");
    }
    // State that can't be read: unknown, not "no push yet".
    let s = judge(&Inputs { crontab: tab(&[ARCHIVE_LINES]), archive: ArchiveIn::Up { host: Box::new(host(7)), snap: Err("sync-snap.json: version 2".into()), big: Ok(big) }, ..inputs() });
    assert_eq!((job(&s, Job::ArchiveSnap).health, job(&s, Job::ArchiveSnap).why.as_deref()), (Health::Unknown, Some("its sync state can't be read: sync-snap.json: version 2")));
    assert_eq!(pod_of(&s, "alpha").archive.health, Health::Unknown);
    let s = judge(&Inputs { archive: ArchiveIn::Unreadable("ARENA_STATE_DIR must be an absolute path (got s)".into()), ..inputs() });
    assert_eq!(job(&s, Job::ArchiveBig).health, Health::Unknown);
}

/// The record: a run of the same result is one history entry (a failure with a new reason is
/// news), each job's last success kept, the runs under way by pid, old pods dropped, and its file
/// read back — another version or a broken file is an error, never an empty record.
#[test]
fn the_record_folds_runs_and_reads_back() {
    let mut p = PodRecord::default();
    for k in 0..3 {
        p.add(ev(NOW - (10 - k) * MIN, Job::Git, Outcome::NoChanges, ""));
    }
    p.add(ev(NOW - 6 * MIN, Job::Git, Outcome::Failed, "exit 255: a"));
    p.add(ev(NOW - 5 * MIN, Job::Git, Outcome::Failed, "exit 255: a"));
    p.add(ev(NOW - 4 * MIN, Job::Git, Outcome::Failed, "exit 128: b"));
    p.add(ev(NOW - 3 * MIN, Job::Copy, Outcome::Copied, "w3d5"));
    let h: Vec<(Outcome, u32, Option<u64>, u64)> = p.history.iter().map(|e| (e.outcome, e.times, e.since, e.at)).collect();
    assert_eq!(
        h,
        [
            (Outcome::Copied, 1, None, NOW - 3 * MIN),
            (Outcome::Failed, 1, None, NOW - 4 * MIN),
            (Outcome::Failed, 2, Some(NOW - 6 * MIN), NOW - 5 * MIN),
            (Outcome::NoChanges, 3, Some(NOW - 10 * MIN), NOW - 8 * MIN),
        ]
    );
    assert_eq!((p.git_ok, p.copy_ok, p.git.as_ref().map(|e| e.detail.as_str())), (Some(NOW - 8 * MIN), Some(NOW - 3 * MIN), Some("exit 128: b")));
    // Each job folds on its own: a quiet pod's git and copy runs interleave as two entries.
    let mut q = PodRecord::default();
    for k in 0..12 {
        q.add(ev(NOW + k * 900, Job::Git, Outcome::NoChanges, "on b"));
        q.add(ev(NOW + k * 900 + 30, Job::Copy, Outcome::Copied, "w3d5"));
    }
    let h: Vec<(Job, u32, Option<u64>, u64)> = q.history.iter().map(|e| (e.job, e.times, e.since, e.at)).collect();
    assert_eq!(h, [(Job::Copy, 12, Some(NOW + 30), NOW + 11 * 900 + 30), (Job::Git, 12, Some(NOW), NOW + 11 * 900)]);
    for k in 0..30 {
        p.add(ev(NOW + k, if k % 2 == 0 { Job::Git } else { Job::Copy }, Outcome::Failed, &format!("reason {k}")));
    }
    assert_eq!(p.history.len(), record::HISTORY_LEN);

    // An older result arriving late (a run that started first and ended last) never replaces a
    // newer one: it moves the last success forward only, and takes its place in the history.
    let mut late = PodRecord::default();
    late.add(ev(NOW, Job::Copy, Outcome::Failed, "x"));
    late.add(ev(NOW - 50, Job::Copy, Outcome::Copied, "older"));
    assert_eq!((late.copy.as_ref().map(|e| e.outcome), late.copy_ok), (Some(Outcome::Failed), Some(NOW - 50)));
    assert_eq!(late.history.iter().map(|e| e.at).collect::<Vec<_>>(), [NOW, NOW - 50]);
    late.add(ev(NOW - 100, Job::Copy, Outcome::Copied, "oldest"));
    assert_eq!(late.copy_ok, Some(NOW - 50), "never back in time");
    // A copy aside (another --dir, --no-big): the history alone.
    late.add_history(ev(NOW - 70, Job::Copy, Outcome::Copied, "aside"));
    assert_eq!(late.history.iter().map(|e| e.detail.as_str()).collect::<Vec<_>>(), ["x", "older", "aside", "oldest"]);
    assert_eq!((late.copy.as_ref().map(|e| e.at), late.copy_ok), (Some(NOW), Some(NOW - 50)));

    let mut r = Record::default();
    r.start(Job::Copy, 11, 2, NOW - 100);
    assert_eq!(r.running, [Running { job: Job::Copy, pid: 11, started: NOW - 100, pods: 2 }]);
    r.pods.insert("devtest-gone".into(), {
        let mut g = PodRecord::default();
        g.add(ev(NOW - 40 * 86_400, Job::Git, Outcome::Pushed, ""));
        g
    });
    r.finish(Job::Copy, 11, NOW, vec![("devtest-a".into(), ev(NOW, Job::Copy, Outcome::Copied, "")), ("devtest-b".into(), ev(NOW, Job::Copy, Outcome::Failed, "x"))], "1 ok, 1 failed".into());
    assert!(r.running.is_empty());
    assert_eq!(
        r.runs.get(&Job::Copy),
        Some(&RunRecord { started: NOW - 100, finished: NOW, pid: 11, ok: false, pods: 2, failed: vec!["devtest-b".into()], summary: "1 ok, 1 failed".into() })
    );
    assert!(!r.pods.contains_key("devtest-gone"), "a pod quiet for 30 days is dropped");
    r.start(Job::Git, 12, 1, NOW);
    r.prune_running(|pid| pid != 12);
    assert!(r.running.is_empty());

    // Its file: written and read back (job names as keys), and refused when it isn't one.
    let dir = Tmp::new("record");
    let path = dir.0.join("devtest").join(record::RECORD_FILE);
    assert_eq!(Record::load(&path), Ok(None));
    let warning = record::update(&path, std::time::Duration::from_secs(1), |rec| {
        rec.start(Job::Git, std::process::id(), 1, NOW);
        rec.finish(Job::Git, std::process::id(), NOW + 5, vec![("devtest-a".into(), ev(NOW + 4, Job::Git, Outcome::Pushed, "→ b"))], "1 pushed".into());
    })
    .unwrap();
    assert_eq!(warning, None);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"git\"") && text.contains("\"pushed\""), "{text}");
    let back = Record::load(&path).unwrap().unwrap();
    assert_eq!(back.runs[&Job::Git].summary, "1 pushed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    std::fs::write(&path, "{\"version\": 9}").unwrap();
    assert!(Record::load(&path).unwrap_err().contains("is version 9"));
    std::fs::write(&path, "not json").unwrap();
    assert!(Record::load(&path).unwrap_err().contains("isn't readable"));
    // A broken record is started afresh by the next write, which says so.
    let warning = record::update(&path, std::time::Duration::from_secs(1), |_| {}).unwrap();
    assert!(warning.is_some_and(|w| w.contains("starting the backups record afresh")));
    assert_eq!(Record::load(&path), Ok(Some(Record::default())));
}

struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("arena-backups-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `/proc/locks` read: held locks by device and inode, waiters left out; a `st_dev` split as
/// Linux does it.
#[test]
fn proc_locks_are_read_by_device_and_inode() {
    let text = "1: FLOCK  ADVISORY  WRITE 2416 08:01:131090 0 EOF\n\
                1: -> FLOCK  ADVISORY  WRITE 2417 08:01:131091 0 EOF\n\
                2: POSIX  ADVISORY  WRITE 801 fd:00:12 0 EOF\n\
                3: OFDLCK ADVISORY  READ  -1 00:19:2 0 EOF\n\
                garbage\n";
    let held = locks::parse_proc_locks(text);
    assert_eq!(held, [(8, 1, 131_090), (0xfd, 0, 12), (0, 0x19, 2)].into_iter().collect());
    assert_eq!(locks::dev_parts(0x0801), (8, 1));
    assert_eq!(locks::dev_parts(0xfd00), (0xfd, 0));
    // glibc's makedev: the major's and the minor's high bits above the old 8:8 split.
    let makedev = |major: u64, minor: u64| ((major & 0xffff_f000) << 32) | ((major & 0xfff) << 8) | ((minor & 0xffff_ff00) << 12) | (minor & 0xff);
    assert_eq!(locks::dev_parts(makedev(0x1234, 0x56789)), (0x1234, 0x56789));
    assert_eq!(locks::dev_parts(makedev(259, 3)), (259, 3));
}

/// A lock held now reads as held — never taken to find out — and free once it's let go of.
#[cfg(target_os = "linux")]
#[test]
fn a_held_lock_reads_as_held_without_taking_it() {
    let dir = Tmp::new("held");
    let path = dir.0.join("backup.lock");
    let f = std::fs::File::create(&path).unwrap();
    assert_eq!(locks::is_held(&path, &locks::held().unwrap()), Ok(false));
    f.lock().unwrap();
    assert_eq!(locks::is_held(&path, &locks::held().unwrap()), Ok(true));
    f.unlock().unwrap();
    assert_eq!(locks::is_held(&path, &locks::held().unwrap()), Ok(false));
    assert_eq!(locks::is_held(&dir.0.join("never"), &locks::held().unwrap()), Ok(false));
}

/// `read` over a real tree: the record, the copies' markers, the newest label per pod, a pod's
/// lock held, the cron line's log, the archive absent — then judged.
#[test]
fn read_finds_the_record_markers_labels_and_locks() {
    let dir = Tmp::new("read");
    let (state, backup) = (dir.0.join("state"), dir.0.join("backup"));
    for p in ["w3d4/devtest-alpha", "w3d5/devtest-alpha", "w3d4/devtest-bravo", "big/devtest-alpha", "custom/devtest-bravo"] {
        std::fs::create_dir_all(backup.join(p)).unwrap();
    }
    disk::mark_copied(&backup, "devtest-alpha").unwrap();
    let cfg = Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR={}\nLOCAL_BACKUP_DIR={}\nCONTROL_MIN_FREE_GB=0\nMACHINE_NAME_LIST=(\n \"alpha\"\n \"bravo\"\n \"@staff-box\"\n)\n",
        state.display(),
        backup.display()
    ));
    let path = state.join("devtest").join(record::RECORD_FILE);
    record::update(&path, std::time::Duration::from_secs(1), |r| {
        r.finish(Job::Git, 1, NOW, vec![("devtest-bravo".into(), ev(NOW, Job::Git, Outcome::Failed, "exit 255"))], "1 failed".into());
    })
    .unwrap();
    let log = dir.0.join("backup.log");
    std::fs::write(&log, "Done: 0 pushed\n").unwrap();
    let line = format!("*/15 * * * * flock -n -o {}/backup.lock /a --config /c pods backup --yes >> {} 2>&1", dir.0.display(), log.display());
    let lock = crate::pull::open_pod_lock(&backup.display().to_string(), "devtest-alpha").unwrap();
    lock.lock().unwrap();
    let pods = cohort_pods(&cfg);
    assert_eq!(pods.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["devtest-alpha", "devtest-bravo"]);
    let i = read(&cfg, Some("/c"), tab(&[&line]), pods, NOW);
    lock.unlock().unwrap();
    assert!(i.record.as_ref().is_ok_and(|r| r.is_some()), "{:?}", i.record);
    assert_eq!(i.labels, Ok(BTreeMap::from([("devtest-alpha".to_string(), "w3d5".to_string()), ("devtest-bravo".to_string(), "w3d4".to_string())])));
    assert!(i.copied.as_ref().is_ok_and(|m| m.contains_key("devtest-alpha") && m.len() == 1), "{:?}", i.copied);
    assert_eq!(i.archive, ArchiveIn::None);
    assert_eq!(i.cron_config, Some(("/c".to_string(), true)));
    assert!(matches!(&i.logs[&Kind::Backup], Ok(l) if l.bytes == 15), "{:?}", i.logs);
    #[cfg(target_os = "linux")]
    {
        assert_eq!(i.held.pods, Ok(BTreeSet::from(["devtest-alpha".to_string()])));
        assert_eq!(i.held.cron, Some(Ok(false)));
    }
    let s = judge(&i);
    assert_eq!(pod_of(&s, "bravo").health, Health::Fail);
    assert_eq!(pod_of(&s, "alpha").copy.text, "w3d5");
    assert_eq!(s.storage[0].what, "snapshots + all files");
}

/// `arena backups`' text: the jobs with what's wrong under each, running now, the pods with their
/// reasons, where the copies live, what isn't covered and how to restore.
#[test]
fn the_text_says_each_job_pod_and_place() {
    let r = record_with(5 * MIN, &[("alpha", Outcome::Pushed, ""), ("bravo", Outcome::Failed, "exit 255: ssh: connect timed out")], &[("alpha", Outcome::Copied, "", Some(3_300_000_000))]);
    let s = judge(&Inputs { crontab: tab(&[GIT_ONLY_LINE]), record: Ok(Some(r)), ..inputs() });
    let text = render::text(&s);
    for want in [
        "Backups — devtest · 2026-10-30 21:34 UTC",
        "✗  git backup          every 15 min  ✗ 5 min ago: 2 pod(s)",
        "    └ 1 failed: devtest-bravo",
        "-  home copy           not set up    ✓ 5 min ago: 1 pod(s)  -",
        "    └ not set up (the backup line runs `--no-pull`: only a pod git couldn't back up is copied) — `arena cron install --backup --pull`",
        "Running now: nothing",
        "✗  devtest-bravo  ✗ failed 5m (never ok)",
        "    └ git: exit 255: ssh: connect timed out",
        "✓  devtest-alpha  ✓ pushed 5m",
        "3.3 GB 5m",
        "github.com/owner/repo",
        "Not covered: dot-directories",
        "Restore: `arena pods restore <pod>`",
    ] {
        assert!(text.contains(want), "no {want:?} in\n{text}");
    }
}

/// The docs' inventory is this module's ([`INVENTORY`]), word for word.
#[test]
fn the_docs_inventory_is_the_models() {
    let docs = include_str!("../../../../docs/BACKUPS.md");
    assert!(docs.contains(&render::inventory_table()), "docs/BACKUPS.md's inventory table differs from backups::INVENTORY — paste:\n{}", render::inventory_table());
}
