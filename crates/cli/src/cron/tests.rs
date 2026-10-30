use super::{strip_arena_block, with_arena_block, CRON_BEGIN, CRON_END};

#[test]
fn install_preserves_other_crontab_entries() {
    let existing = "0 9 * * * /usr/bin/other-job\n# my note\n";
    let line = "0 * * * * arena pods backup --yes".to_string();
    let out = with_arena_block(existing, std::slice::from_ref(&line));
    // keeps the user's entries…
    assert!(out.contains("/usr/bin/other-job"));
    assert!(out.contains("# my note"));
    // …and adds a fenced arena block
    assert!(out.contains(CRON_BEGIN));
    assert!(out.contains(CRON_END));
    assert!(out.contains(&line));
}

#[test]
fn install_is_idempotent_replacing_old_block() {
    let existing = "0 9 * * * keep-me".to_string();
    let v1 = with_arena_block(&existing, &["A".to_string()]);
    let v2 = with_arena_block(&v1, &["B".to_string()]);
    assert!(v2.contains("keep-me"));
    assert!(v2.contains('B'));
    assert!(!v2.contains("* A") && v2.matches(CRON_BEGIN).count() == 1); // only one block
}

#[test]
fn remove_strips_only_the_arena_block() {
    let existing = "keep-me\n# >>> arena-infra-rs >>>\njob\n# <<< arena-infra-rs <<<\nalso-keep\n";
    let out = with_arena_block(existing, &[]);
    assert!(out.contains("keep-me"));
    assert!(out.contains("also-keep"));
    assert!(!out.contains("job"));
    assert!(!out.contains(CRON_BEGIN));
}

#[test]
fn strip_handles_no_block() {
    assert_eq!(strip_arena_block("a\nb"), "a\nb");
}

/// What `cron show` and `teardown --check` read: the block's lines (every block, blank
/// lines dropped), and outside it only *job* lines that mention arena — not comments,
/// not `NAME=value` settings, not other jobs.
#[test]
fn arena_cron_lines_reads_the_block_and_hand_added_arena_jobs() {
    let tab = format!(
        "MAILTO=ops@example.org\nPATH=/home/dev/arena/bin:/usr/bin\n# old arena job, disabled\n\
         0 3 * * * /usr/bin/certbot renew\n{CRON_BEGIN}\n*/15 * * * * arena pods backup --yes\n\n{CRON_END}\n\
         0 9 * * * /usr/local/bin/arena pods up -n 2 --yes\n30 2 * * * destroy_pods --yes apple\n"
    );
    let lines = super::arena_cron_lines(&tab);
    assert_eq!(lines.managed, ["*/15 * * * * arena pods backup --yes"]);
    assert_eq!(lines.unmanaged, ["0 9 * * * /usr/local/bin/arena pods up -n 2 --yes", "30 2 * * * destroy_pods --yes apple"]);
    let none = super::arena_cron_lines("0 3 * * * /usr/bin/certbot renew\n");
    assert!(none.managed.is_empty() && none.unmanaged.is_empty());
    // `cron install` output reads back as exactly its lines.
    let installed = with_arena_block("keep-me\n", &["A arena".to_string(), "B arena".to_string()]);
    assert_eq!(super::arena_cron_lines(&installed).managed, ["A arena", "B arena"]);
}

/// The lines `cron install` renders for these flags (backup per [`install_backup_line`]).
fn cron_job(pull: bool, proxy: bool) -> Vec<String> {
    cron_job_with(pull, proxy, None)
}

fn cron_job_with(pull: bool, proxy: bool, snapshot_dir: Option<&str>) -> Vec<String> {
    cron_kinds(true, pull, proxy, snapshot_dir).into_iter().map(|(_, l)| l).collect()
}

fn cron_kinds(backup: bool, pull: bool, proxy: bool, snapshot_dir: Option<&str>) -> Vec<(super::CronKind, String)> {
    super::cron_lines(&super::CronJob {
        schedule: "*/15 * * * *",
        env_prefix: "",
        exe: "/opt/arena",
        config: "/srv/config.env",
        home: "/home/u",
        backup,
        pull,
        proxy,
        snapshot_dir,
        archive: false,
        archive_state_dir: None,
        kept_logs: &[],
    })
}

#[test]
fn cron_lines_add_the_public_snapshot_only_when_asked() {
    assert!(!cron_job(true, true).iter().any(|l| l.contains("snapshot")));
    let lines = cron_job_with(false, true, Some("/var/www/fleet/"));
    assert_eq!(lines.len(), 3, "{lines:#?}");
    // Every 2 minutes, flock'd like the proxy line, public JSON only, atomically to DIR/fleet.json.
    assert_eq!(
        lines[2],
        "*/2 * * * * mkdir -p /home/u/.local/state/arena-cron && flock -n /home/u/.local/state/arena-cron/snapshot.lock \
         /opt/arena --config /srv/config.env snapshot --public --out '/var/www/fleet/fleet.json' \
         >> /home/u/.local/state/arena-cron/snapshot.log 2>&1"
    );
    // A quote in the directory can't break out of the shell word.
    let odd = cron_job_with(false, false, Some("/srv/it's"));
    assert!(odd[1].contains("--out '/srv/it'\\''s/fleet.json' >>"), "{}", odd[1]);
    // It lives in the arena block, and is recognized as the snapshot line.
    let tab = with_arena_block("0 9 * * * keep-me\n", &lines);
    assert_eq!(tab.matches("snapshot --public").count(), 1);
    assert_eq!(super::CronKind::of(&lines[2]), Some(super::CronKind::Snapshot));
}

#[test]
fn snapshot_cron_dir_must_be_an_existing_plain_directory() {
    use super::snapshot_cron_dir;
    let base = std::env::temp_dir().join(format!("arena-cron-snap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("www")).unwrap();
    std::fs::create_dir_all(base.join("50%off")).unwrap();
    std::fs::write(base.join("file"), "x").unwrap();
    let got = snapshot_cron_dir(&base.join("www").join("..").join("www")).unwrap();
    assert_eq!(std::path::Path::new(&got), std::fs::canonicalize(base.join("www")).unwrap(), "absolute + normalized");
    assert!(snapshot_cron_dir(&base.join("missing")).is_err());
    assert!(snapshot_cron_dir(&base.join("file")).unwrap_err().to_string().contains("not a directory"));
    assert!(snapshot_cron_dir(&base.join("50%off")).unwrap_err().to_string().contains("`%`"));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn cron_lines_add_the_proxy_resync_only_when_asked() {
    // The backup line is flock'd too: a tick that finds the last backup still running
    // (a wedged pod's rsync, up to BACKUP_TIMEOUT_SECS) exits instead of stacking. Its lock
    // and log are in the cron folder, which it recreates first.
    assert_eq!(
        cron_job(false, false),
        ["*/15 * * * * mkdir -p /home/u/.local/state/arena-cron && flock -n -o /home/u/.local/state/arena-cron/backup.lock \
          /opt/arena --config /srv/config.env pods backup --no-pull --yes >> /home/u/.local/state/arena-cron/backup.log 2>&1"]
    );
    // The proxy line carries a PATH with /usr/sbin (cron's default lacks it, and that's
    // where nginx lives on Ubuntu) and a `flock -n` so a slow tick can't pile up.
    let both = cron_job(true, true);
    assert_eq!(
        both,
        [
            "*/15 * * * * mkdir -p /home/u/.local/state/arena-cron && flock -n -o /home/u/.local/state/arena-cron/backup.lock \
             /opt/arena --config /srv/config.env pods backup --yes >> /home/u/.local/state/arena-cron/backup.log 2>&1",
            "*/5 * * * * mkdir -p /home/u/.local/state/arena-cron && PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
             flock -n /home/u/.local/state/arena-cron/proxy.lock /opt/arena --config /srv/config.env proxy apply --yes \
             >> /home/u/.local/state/arena-cron/proxy.log 2>&1",
        ]
    );
    // Without the backup line selected, only the asked-for lines render.
    let kinds: Vec<super::CronKind> = cron_kinds(false, false, true, Some("/w")).into_iter().map(|(k, _)| k).collect();
    assert_eq!(kinds, [super::CronKind::Proxy, super::CronKind::Snapshot]);
}

/// `cron install --health HH:MM` (spec health-cron): `pods test --deep` once a day at that
/// time — cron's, this box's zone — under `flock -n -o` like the backup line, its lock and log
/// in the cron folder (or the log the installed line chose, its folder recreated first), the
/// state dir carried as the archive's lines carry it, read-only (no `--yes`), recognized as
/// the health line, and its time read back for `cron show`.
#[test]
fn health_cron_line_runs_the_deep_check_daily() {
    use super::{health_cron_line, health_when, CronKind, HealthAt};
    let at = HealthAt::parse("07:05").unwrap();
    let line = health_cron_line(at, "/opt/arena", "/srv/config.env", "/home/u", "/var/lib/arena", None);
    assert_eq!(
        line,
        "5 7 * * * mkdir -p /home/u/.local/state/arena-cron && ARENA_STATE_DIR=/var/lib/arena flock -n -o \
         /home/u/.local/state/arena-cron/health.lock /opt/arena --config /srv/config.env pods test --deep \
         >> /home/u/.local/state/arena-cron/health.log 2>&1"
    );
    assert_eq!(CronKind::of(&line), Some(CronKind::Health));
    assert!(!line.contains("--yes"), "read-only: {line}");
    assert_eq!(HealthAt::of_line(&line), Some(at));
    assert_eq!(health_when(at, Some("BST +0100"), None), "daily at 07:05, this box's time zone (BST +0100)");
    assert_eq!(health_when(at, None, None), "daily at 07:05, this box's time zone");
    // A zone the crontab sets is said beside the box's (review ai-review codex #1, claude #7).
    assert_eq!(
        health_when(at, Some("BST +0100"), Some(("CRON_TZ", "UTC"))),
        "daily at 07:05, this box's time zone (BST +0100) — this crontab sets CRON_TZ=UTC: a cron that reads it runs \
         the line at 07:05 there instead"
    );
    // On one of the reaper's minutes, a warning (review ai-review claude #1, measured live).
    for minute in [11, 27, 41, 57] {
        let w = super::health_minute_warning(HealthAt { hour: 7, minute }).unwrap_or_default();
        assert!(w.starts_with("warning: the reaper looks at :11/:27/:41/:57: a deep check at :") && w.contains(&format!(":{minute:02} reads")), "{w}");
    }
    for minute in [0, 5, 20, 30, 59] {
        assert_eq!(super::health_minute_warning(HealthAt { hour: 7, minute }), None, ":{minute}");
    }
    // A kept log stays where it is, its folder recreated first.
    let kept = health_cron_line(at, "/opt/arena", "/c", "/home/u", "/s", Some("/home/u/logs/health.log"));
    assert!(kept.starts_with("5 7 * * * mkdir -p /home/u/.local/state/arena-cron /home/u/logs && ARENA_STATE_DIR=/s "), "{kept}");
    assert!(kept.ends_with(" pods test --deep >> /home/u/logs/health.log 2>&1"), "{kept}");
    // A schedule that isn't once a day at a fixed time (a hand edit) isn't read as one.
    for l in ["*/30 7 * * * a pods test --deep", "5 7 * * 1 a pods test --deep", "@daily a pods test --deep", "5 24 * * * a pods test --deep", "5"] {
        assert_eq!(HealthAt::of_line(l), None, "{l}");
    }
    assert_eq!(HealthAt::of_line("05 7 * * * a pods test --deep"), Some(at));
}

/// The zone a crontab sets for the health line (`CRON_TZ=`/`TZ=` on a line of its own,
/// before it): `CRON_TZ` over `TZ`, the last of each before the line; comments, a job's
/// inline env and a value with a space aren't one (review: a later setting used to win).
#[test]
fn cron_tz_reads_the_crontabs_own_zone_setting() {
    use super::cron_tz;
    let cases: &[(&str, Option<(&str, &str)>)] = &[
        ("CRON_TZ=Europe/London\n0 3 * * * x\n", Some(("CRON_TZ", "Europe/London"))),
        ("  TZ = UTC\n", Some(("TZ", "UTC"))),
        ("CRON_TZ=\"America/New_York\"\nTZ='Asia/Tokyo'\n", Some(("CRON_TZ", "America/New_York"))),
        ("TZ=Asia/Tokyo\n", Some(("TZ", "Asia/Tokyo"))),
        // Only what comes before the health line applies to it.
        (
            "CRON_TZ=UTC\n20 7 * * * flock -n -o /h/.local/state/arena-cron/health.lock /opt/arena --config /c pods test --deep >> /h/.local/state/arena-cron/health.log 2>&1\nCRON_TZ=Asia/Tokyo\n",
            Some(("CRON_TZ", "UTC")),
        ),
        ("# CRON_TZ=Europe/London\n", None),
        ("0 3 * * * CRON_TZ=UTC /opt/arena pods backup\n", None),
        ("MAILTO=ops@example.org\nPATH=/usr/bin\n", None),
        ("CRON_TZ=\n", None),
        ("CRON_TZ=Europe/London with words\n", None),
        ("", None),
    ];
    for (tab, want) in cases {
        assert_eq!(cron_tz(tab), *want, "{tab:?}");
    }
}

/// `--health` takes `HH:MM` (or `H:MM`) in this box's time zone: an hour 0–23 and two-digit
/// minutes — never a guess at `7:5` (07:05 or 07:50?), `25:00` or `07:60`.
#[test]
fn health_time_must_be_hh_mm() {
    use super::HealthAt;
    for (s, hour, minute) in [("07:05", 7, 5), ("7:05", 7, 5), ("00:00", 0, 0), ("23:59", 23, 59), (" 07:30 ", 7, 30)] {
        assert_eq!(HealthAt::parse(s).unwrap(), HealthAt { hour, minute }, "{s}");
        assert_eq!(HealthAt::parse(s).unwrap().to_string(), format!("{hour:02}:{minute:02}"));
    }
    for s in ["25:00", "24:00", "7:5", "07:60", "07:5", "7", "0730", "07:05:00", "+7:05", "-1:00", "07:+5", "", ":", "007:05", "07h05", "\u{0667}:05"] {
        let e = HealthAt::parse(s).unwrap_err().to_string();
        assert_eq!(e, format!("--health takes HH:MM in this box's time zone, e.g. 07:30 (got {s:?})"), "{s}");
    }
}

/// Finding #25: a refusal for lack of a terminal (no --yes) exits 2 — it exited 0 after
/// "aborted.", indistinguishable from success for a script. Cron runs without a terminal,
/// so every mutating cron line must carry --yes (the read-only snapshot needs none).
#[test]
fn no_terminal_refusal_exits_2_and_every_mutating_cron_line_says_yes() {
    use crate::{exit_code, NoTerminal};
    use std::process::ExitCode;
    assert_eq!(exit_code(&Ok(())), ExitCode::SUCCESS);
    assert_eq!(exit_code(&Err(anyhow::anyhow!("boom"))), ExitCode::FAILURE);
    assert_eq!(exit_code(&Err(NoTerminal.into())), ExitCode::from(2));
    // Still 2 when a caller added context on the way up.
    assert_eq!(exit_code(&Err(anyhow::Error::new(NoTerminal).context("stopping pods"))), ExitCode::from(2));
    // Without a terminal (as under cargo test, or cron), `confirm` refuses with it.
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        let e = crate::confirm(false, "Will do a thing.").unwrap_err();
        assert!(e.downcast_ref::<NoTerminal>().is_some() && e.to_string().contains("pass --yes"), "{e}");
        assert!(crate::confirm(true, "Will do a thing.").unwrap());
    }
    for (backup, pull, proxy, snapshot) in [(true, false, true, Some("/w")), (true, true, false, None)] {
        for (kind, line) in cron_kinds(backup, pull, proxy, snapshot) {
            let mutating = !matches!(kind, super::CronKind::Snapshot);
            assert_eq!(line.contains(" --yes "), mutating, "{line}");
        }
    }
}

/// `--start-date` goes before `flock` (an inline env assignment applies to the command it
/// prefixes; flock hands it on to arena) — after it, flock would try to run it — and after
/// the `mkdir -p … &&`, which would otherwise take it.
#[test]
fn cron_backup_line_keeps_the_start_date_ahead_of_flock() {
    let lines = super::cron_lines(&super::CronJob {
        schedule: "0 * * * *",
        env_prefix: "ARENA_START_DATE=2026-10-05 ",
        exe: "/opt/arena",
        config: "/srv/config.env",
        home: "/root",
        backup: true,
        pull: true,
        proxy: false,
        snapshot_dir: None,
        archive: false,
        archive_state_dir: None,
        kept_logs: &[],
    });
    assert_eq!(
        lines,
        [(
            super::CronKind::Backup,
            "0 * * * * mkdir -p /root/.local/state/arena-cron && ARENA_START_DATE=2026-10-05 flock -n -o \
             /root/.local/state/arena-cron/backup.lock /opt/arena --config /srv/config.env pods backup --yes \
             >> /root/.local/state/arena-cron/backup.log 2>&1"
                .to_string()
        )]
    );
}

/// Each managed line's identity, read from the subcommand it runs — the lines earlier
/// versions wrote (no flock, no tag, the logs in the home) included; hand edits and
/// look-alikes are `None`.
#[test]
fn cron_kind_of_lines_table() {
    use super::CronKind::{self, *};
    let cases: &[(&str, Option<CronKind>)] = &[
        // This version's: the cron folder (and a kept log's) recreated first, the lock in there.
        ("*/15 * * * * mkdir -p /h/.local/state/arena-cron && ARENA_START_DATE=2026-01-05 flock -n -o /h/.local/state/arena-cron/backup.lock /opt/arena --config /c pods backup --yes >> /h/.local/state/arena-cron/backup.log 2>&1", Some(Backup)),
        ("*/5 * * * * mkdir -p /h/.local/state/arena-cron /h/logs/cron && PATH=/usr/sbin:/usr/bin flock -n /h/.local/state/arena-cron/proxy.lock /opt/arena --config /c proxy apply --yes >> /h/logs/cron/arena-proxy-cron.log 2>&1", Some(Proxy)),
        ("*/2 * * * * mkdir -p /h/.local/state/arena-cron && flock -n /h/.local/state/arena-cron/snapshot.lock /opt/arena --config /c snapshot --public --out '/w/fleet.json' >> /h/.local/state/arena-cron/snapshot.log 2>&1", Some(Snapshot)),
        ("40 * * * * mkdir -p /h/.local/state/arena-cron /h/logs && nice -n 10 /opt/arena --config /c archive sync --tier big >> /h/logs/archive.log 2>&1", Some(ArchiveBig)),
        ("11 * * * * mkdir -p /h/.local/state/arena-cron && PATH=/bin flock -n -o /h/.local/state/arena-cron/reap.lock /opt/arena --config /c pods reap run >> /h/.local/state/arena-cron/reap.log 2>&1", Some(Reap)),
        ("5 7 * * * mkdir -p /h/.local/state/arena-cron && ARENA_STATE_DIR=/s flock -n -o /h/.local/state/arena-cron/health.lock /opt/arena --config /c pods test --deep >> /h/.local/state/arena-cron/health.log 2>&1", Some(Health)),
        // a663f65's: one folder under the state root.
        ("*/15 * * * * mkdir -p /h/.local/state/arena/cron && ARENA_START_DATE=2026-01-05 flock -n -o /h/.local/state/arena/cron/backup.lock /opt/arena --config /c pods backup --yes >> /h/.local/state/arena/cron/backup.log 2>&1", Some(Backup)),
        ("*/5 * * * * mkdir -p /h/.local/state/arena/cron && PATH=/usr/sbin:/usr/bin flock -n /h/.local/state/arena/cron/proxy.lock /opt/arena --config /c proxy apply --yes >> /h/.local/state/arena/cron/proxy.log 2>&1", Some(Proxy)),
        ("*/2 * * * * mkdir -p /h/.local/state/arena/cron && flock -n /h/.local/state/arena/cron/snapshot.lock /opt/arena --config /c snapshot --public --out '/w/fleet.json' >> /h/.local/state/arena/cron/snapshot.log 2>&1", Some(Snapshot)),
        ("7,22,37,52 * * * * mkdir -p /h/.local/state/arena/cron && ARENA_STATE_DIR=/s nice -n 10 /opt/arena --config /c archive sync --tier snap >> /h/.local/state/arena/cron/archive.log 2>&1", Some(ArchiveSnap)),
        ("40 * * * * mkdir -p /h/.local/state/arena/cron && nice -n 10 /opt/arena --config /c archive sync --tier big >> /h/.local/state/arena/cron/archive.log 2>&1", Some(ArchiveBig)),
        ("11 * * * * mkdir -p /h/.local/state/arena/cron && PATH=/bin flock -n -o /h/.local/state/arena/cron/reap.lock /opt/arena --config /c pods reap run >> /h/.local/state/arena/cron/reap.log 2>&1", Some(Reap)),
        // Written by an earlier `cron install` (no flock on the backup line).
        ("*/15 * * * * /usr/local/bin/arena --config /c pods backup --no-pull --yes >> /root/arena-cron.log 2>&1", Some(Backup)),
        ("*/15 * * * * ARENA_START_DATE=2026-01-05 /opt/arena --config /c pods backup --yes >> /h/arena-cron.log 2>&1", Some(Backup)),
        ("*/15 * * * * flock -n /h/.arena-backup-cron.lock /opt/arena --config /c pods backup --yes >> /h/arena-cron.log 2>&1", Some(Backup)),
        ("*/15 * * * * flock -n -o /h/.arena-backup-cron.lock /opt/arena --config /c pods backup --yes >> /h/arena-cron.log 2>&1", Some(Backup)),
        ("*/5 * * * * PATH=/usr/sbin:/usr/bin flock -n /h/.arena-proxy-cron.lock /opt/arena --config /c proxy apply --yes >> /h/arena-proxy-cron.log 2>&1", Some(Proxy)),
        ("*/2 * * * * flock -n /h/.arena-snapshot-cron.lock /opt/arena --config /c snapshot --public --out '/w/fleet.json' >> /h/arena-snapshot-cron.log 2>&1", Some(Snapshot)),
        ("*/2\t*\t*\t*\t*\t/opt/arena --config /c snapshot --public", Some(Snapshot)), // tab-separated
        // Not one of ours (or more than one): kept as is, never guessed at.
        ("0 3 * * * /opt/arena --config /c pods backup-status", None),
        ("0 3 * * * /opt/arena --config /c proxy plan", None),
        ("0 3 * * * /opt/arena --config /c pods list >> /h/.arena-proxy-cron.lock.snapshot", None),
        ("*/15 * * * * arena pods backup --yes && arena proxy apply --yes", None),
        // The torch check isn't the health line; a deep check chained to another job is two.
        ("0 3 * * * /opt/arena --config /c pods test >> /h/torch.log 2>&1", None),
        ("0 7 * * * /opt/arena --config /c pods test --deep && /opt/arena --config /c pods backup --yes", None),
        ("# a note", None),
        // Comments are never a kind: a paused (commented-out) line, a note naming one.
        ("# */15 * * * * flock -n /h/.arena-backup-cron.lock /opt/arena --config /c pods backup --yes", None),
        ("  #*/5 * * * * /opt/arena --config /c proxy apply --yes", None),
        ("# snapshot page is served by caddy from /srv/www", None),
        ("# run `arena pods backup` by hand before a reimage", None),
        ("MAILTO=ops@example.org", None),
    ];
    for (line, want) in cases {
        assert_eq!(CronKind::of(line), *want, "{line}");
    }
}

#[test]
fn install_selects_the_backup_line_only_when_asked_or_nothing_else_is() {
    use super::install_backup_line as b;
    // (backup, schedule, start_date, pull, proxy, snapshot) -> writes the backup line
    let cases = [
        ((false, false, false, false, false, false), true), // bare `cron install`
        ((false, false, false, false, true, false), false), // --proxy only
        ((false, false, false, false, false, true), false), // --snapshot only
        ((false, false, false, false, true, true), false),
        ((true, false, false, false, true, false), true), // --backup --proxy
        ((false, false, false, true, true, false), true), // --pull implies it
        ((false, false, true, false, false, true), true), // --start-date implies it
        ((false, true, false, false, true, false), true), // --schedule implies it
    ];
    for ((backup, schedule, start, pull, proxy, snapshot), want) in cases {
        assert_eq!(b(backup, schedule, start, pull, proxy, snapshot), want, "{backup} {schedule} {start} {pull} {proxy} {snapshot}");
    }
}

/// The bug this fixes: a later `cron install --proxy` used to rewrite the whole block,
/// silently dropping an earlier `--pull` / `--start-date` backup line and the snapshot
/// line. Now each install replaces only its own lines, in place; `remove --<kind>` drops
/// only that kind; other crontab entries and hand edits inside the block survive.
#[test]
fn cron_install_and_remove_merge_by_identity() {
    use super::{cron_diff, merge_cron_block, CronKind::*};
    let legacy_backup =
        "*/15 * * * * ARENA_START_DATE=2026-01-05 /opt/arena --config /srv/config.env pods backup --yes >> /home/u/arena-cron.log 2>&1";
    let hand = "# operator: backups paused for maintenance until Friday";
    let snap = &cron_kinds(false, false, false, Some("/w"))[0];
    let before: Vec<String> = vec![legacy_backup.into(), hand.into(), snap.1.clone()];

    // `cron install --proxy`: backup (with its --pull/start date), the note and the
    // snapshot line stay; the proxy line is added at the end.
    let proxy = cron_kinds(false, false, true, None);
    let after = merge_cron_block(&before, &[Proxy], &proxy);
    assert_eq!(after, [legacy_backup, hand, snap.1.as_str(), proxy[0].1.as_str()]);
    // Re-running it is a no-op (so nothing is written).
    assert_eq!(merge_cron_block(&after, &[Proxy], &proxy), after);

    // `cron install --backup` (no --pull now): the backup line is replaced in place —
    // the legacy line gets the flock'd one — and nothing else moves.
    let backup = cron_kinds(true, false, false, None);
    let replaced = merge_cron_block(&after, &[Backup], &backup);
    assert_eq!(replaced, [backup[0].1.as_str(), hand, snap.1.as_str(), proxy[0].1.as_str()]);
    assert_eq!(
        cron_diff(&after, &replaced),
        [
            format!("- {legacy_backup}"),
            format!("+ {}", backup[0].1),
            format!("  {hand}"),
            format!("  {}", snap.1),
            format!("  {}", proxy[0].1),
        ]
    );

    // Duplicates of a kind (two backup lines from a hand edit) collapse into the new one.
    let doubled: Vec<String> = vec![legacy_backup.into(), legacy_backup.replace("*/15", "0")];
    assert_eq!(merge_cron_block(&doubled, &[Backup], &backup), [backup[0].1.as_str()]);

    // `cron remove --proxy --snapshot`: just those; the backup line and the note stay.
    let removed = merge_cron_block(&replaced, &[Proxy, Snapshot], &[]);
    assert_eq!(removed, [backup[0].1.as_str(), hand]);
    assert_eq!(cron_diff(&replaced, &removed)[..2], [format!("- {}", snap.1), format!("- {}", proxy[0].1)]);

    // Written back, other crontab entries are untouched and there's one block.
    let tab = with_arena_block("0 9 * * * keep-me\n", &removed);
    assert!(tab.starts_with("0 9 * * * keep-me\n") && tab.matches(CRON_BEGIN).count() == 1, "{tab}");
    assert_eq!(super::arena_cron_lines(&tab).managed, removed);
    // Removing the last line leaves no empty block behind.
    assert_eq!(with_arena_block(&tab, &merge_cron_block(&[backup[0].1.clone()], &[Backup], &[])), "0 9 * * * keep-me\n");
}

/// `line` (logging to arena's default) as 026c308 wrote it: no `mkdir -p … &&`, and its
/// lock and log directly in `home` under their old names ([`arena_core::cron::Job::legacy_log`]).
fn legacy_cron_line(line: &str, home: &str) -> String {
    use arena_core::cron::{self, Job};
    let mut old = line.replacen(&format!("mkdir -p {} && ", cron::dir(home)), "", 1);
    for job in Job::ALL {
        if let Some(log) = job.legacy_log() {
            old = old.replace(&format!(" >> {} ", cron::log(home, job)), &format!(" >> {home}/{log} "));
        }
        if let Some(lock) = job.legacy_lock() {
            old = old.replace(&format!(" {} ", cron::lock(home, job)), &format!(" {home}/{lock} "));
        }
    }
    old
}

/// `line` (logging to arena's default) as a663f65 wrote it: the same, in its folder under the
/// state root ([`arena_core::cron::PREV_DIR`]).
fn prev_cron_line(line: &str, home: &str) -> String {
    line.replace(&arena_core::cron::dir(home), &format!("{home}/{}", arena_core::cron::PREV_DIR))
}

/// Exactly what 026c308's `cron install --pull --start-date 2026-10-05 --proxy --archive
/// --reap observe` wrote for root, and the job that renders today's lines for the same flags.
fn root_cron_026c308() -> Vec<String> {
    [
        "*/15 * * * * ARENA_START_DATE=2026-10-05 flock -n -o /root/.arena-backup-cron.lock /usr/local/bin/arena \
         --config /root/arena/config.env pods backup --yes >> /root/arena-cron.log 2>&1",
        "*/5 * * * * PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin flock -n \
         /root/.arena-proxy-cron.lock /usr/local/bin/arena --config /root/arena/config.env proxy apply --yes \
         >> /root/arena-proxy-cron.log 2>&1",
        "7,22,37,52 * * * * ARENA_START_DATE=2026-10-05 ARENA_STATE_DIR=/var/lib/arena nice -n 10 /usr/local/bin/arena \
         --config /root/arena/config.env archive sync --tier snap >> /root/arena-archive-cron.log 2>&1",
        "40 * * * * ARENA_START_DATE=2026-10-05 ARENA_STATE_DIR=/var/lib/arena nice -n 10 /usr/local/bin/arena \
         --config /root/arena/config.env archive sync --tier big >> /root/arena-archive-cron.log 2>&1",
        "11,27,41,57 * * * * PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin flock -n -o \
         /root/.arena-reap-cron.lock /usr/local/bin/arena --config /root/arena/config.env pods reap run \
         >> /root/arena-reap-cron.log 2>&1",
    ]
    .map(String::from)
    .to_vec()
}

/// Today's lines for those flags, root's, logging where `kept` says (else the default).
fn root_cron_now(kept: &[(super::CronKind, String)]) -> Vec<(super::CronKind, String)> {
    let job = super::CronJob {
        schedule: "*/15 * * * *",
        env_prefix: "ARENA_START_DATE=2026-10-05 ",
        exe: "/usr/local/bin/arena",
        config: "/root/arena/config.env",
        home: "/root",
        backup: true,
        pull: true,
        proxy: true,
        snapshot_dir: None,
        archive: true,
        archive_state_dir: Some("/var/lib/arena"),
        kept_logs: kept,
    };
    let mut lines = super::cron_lines(&job);
    let reap = kept.iter().find(|(k, _)| *k == super::CronKind::Reap).map(|(_, l)| l.as_str());
    lines.push((super::CronKind::Reap, arena_core::reap::render::cron_line(false, "/usr/local/bin/arena", "/root/arena/config.env", "/root", reap, &[])));
    lines
}

/// The upgrade: a crontab holding the lines 026c308 wrote — each logging (and locking) at
/// arena's old default directly in the home — or a663f65's (in the state root) is upgraded in
/// place by `cron install`: each line replaced by its new one, never doubled, since a line's
/// kind is the command it runs, not its files. The tidy plan names exactly the old default
/// files those replaced lines used: never a line the install didn't touch, nor a path the
/// crontab still names.
#[test]
fn cron_install_replaces_the_old_lines_in_place() {
    use super::{cron_diff, kept_logs, merge_cron_block, tidy_plan, CronKind::*, OldCronFile};
    let before = root_cron_026c308();
    assert_eq!(kept_logs(&before, "/root"), [], "arena's own old defaults: nothing kept");
    let new = root_cron_now(&[]);
    let kinds: Vec<super::CronKind> = new.iter().map(|(k, _)| *k).collect();
    assert_eq!(kinds, [Backup, Proxy, ArchiveSnap, ArchiveBig, Reap]);
    let tab = |lines: &[String]| with_arena_block("0 9 * * * keep-me\n", lines);

    // The same install today: each line replaced where it stood, one per kind — the same
    // line but for the `mkdir -p` and where its files are.
    let after = merge_cron_block(&before, &kinds, &new);
    assert_eq!(after, new.iter().map(|(_, l)| l.clone()).collect::<Vec<_>>());
    for (old, now) in before.iter().zip(&after) {
        assert!(now.contains(" >> /root/.local/state/arena-cron/") && !now.contains("/root/arena-"), "{now}");
        assert_eq!(legacy_cron_line(now, "/root"), *old, "only the folder and the files change");
    }
    for k in &kinds {
        assert_eq!(after.iter().filter(|l| super::CronKind::of(l) == Some(*k)).count(), 1, "one {} line", k.label());
    }
    assert_eq!(cron_diff(&before, &after).iter().filter(|l| l.starts_with("- ")).count(), 5, "every old line goes");
    let log = |old: &str, job: &str| OldCronFile::Log { old: format!("/root/{old}"), new: format!("/root/.local/state/arena-cron/{job}.log") };
    let lock = |old: &str| OldCronFile::Lock(format!("/root/{old}"));
    assert_eq!(
        tidy_plan(&kinds, &before, &after, "/root", &tab(&after)),
        [
            log("arena-cron.log", "backup"),
            lock(".arena-backup-cron.lock"),
            log("arena-proxy-cron.log", "proxy"),
            lock(".arena-proxy-cron.lock"),
            log("arena-archive-cron.log", "archive"),
            log("arena-reap-cron.log", "reap"),
            lock(".arena-reap-cron.lock"),
        ]
    );
    // Again: nothing changes, nothing to tidy.
    assert_eq!(merge_cron_block(&after, &kinds, &new), after);
    assert_eq!(tidy_plan(&kinds, &after, &after, "/root", &tab(&after)), []);

    // `cron install --proxy` alone: only the proxy line's old files go.
    let proxy_only = merge_cron_block(&before, &[Proxy], &new[1..2]);
    assert_eq!(proxy_only[1], new[1].1);
    assert_eq!(proxy_only.iter().filter(|l| *l != &new[1].1).collect::<Vec<_>>(), before.iter().filter(|l| *l != &before[1]).collect::<Vec<_>>());
    assert_eq!(tidy_plan(&[Proxy], &before, &proxy_only, "/root", &tab(&proxy_only)), [log("arena-proxy-cron.log", "proxy"), lock(".arena-proxy-cron.lock")]);
    // A paused (commented-out) old backup line would use the old files again once
    // un-paused: they stay.
    let paused = format!("# {}", before[0]);
    let with_paused: Vec<String> = vec![before[0].clone(), paused.clone()];
    let rewritten = merge_cron_block(&with_paused, &[Backup], &new[..1]);
    assert_eq!(rewritten, [new[0].1.clone(), paused]);
    assert_eq!(tidy_plan(&[Backup], &with_paused, &rewritten, "/root", &tab(&rewritten)), []);

    // a663f65's lines (one folder in the state root) are arena's own too: moved to the new
    // folder, their files tidied from those exact paths.
    let prev: Vec<String> = after.iter().map(|l| prev_cron_line(l, "/root")).collect();
    assert!(prev.iter().all(|l| l.contains("/root/.local/state/arena/cron/") && !l.contains("arena-cron")), "{prev:?}");
    assert_eq!(kept_logs(&prev, "/root"), []);
    let upgraded = merge_cron_block(&prev, &kinds, &new);
    assert_eq!(upgraded, after);
    let plan = tidy_plan(&kinds, &prev, &upgraded, "/root", &tab(&upgraded));
    assert_eq!(plan.len(), 7, "{plan:?}");
    assert_eq!(plan[..2], [log(".local/state/arena/cron/backup.log", "backup"), lock(".local/state/arena/cron/backup.lock")]);
}

/// CL1: a line that logs somewhere someone chose (a folder logrotate rotates, another home)
/// keeps that log on a re-install — only the lock moves into arena's folder, and the mkdir
/// makes the kept log's folder too. Nothing of that kind is tidied. Again is a no-op.
#[test]
fn cron_install_keeps_a_log_path_someone_chose() {
    use super::{kept_logs, merge_cron_block, tidy_plan, CronKind::*};
    let chosen = |l: &str, log: &str| l.replace(" >> /root/arena-", &format!(" >> {log}/arena-"));
    let mut before = root_cron_026c308();
    before[0] = chosen(&before[0], "/srv/x/logs/cron");
    before[1] = chosen(&before[1], "/home/other");
    let kept = kept_logs(&before, "/root");
    assert_eq!(kept, [(Backup, "/srv/x/logs/cron/arena-cron.log".to_string()), (Proxy, "/home/other/arena-proxy-cron.log".to_string())]);
    let new = root_cron_now(&kept);
    let kinds: Vec<super::CronKind> = new.iter().map(|(k, _)| *k).collect();
    let after = merge_cron_block(&before, &kinds, &new);
    assert_eq!(
        after[0],
        "*/15 * * * * mkdir -p /root/.local/state/arena-cron /srv/x/logs/cron && ARENA_START_DATE=2026-10-05 flock -n -o \
         /root/.local/state/arena-cron/backup.lock /usr/local/bin/arena --config /root/arena/config.env pods backup --yes \
         >> /srv/x/logs/cron/arena-cron.log 2>&1"
    );
    assert!(after[1].starts_with("*/5 * * * * mkdir -p /root/.local/state/arena-cron /home/other && PATH="), "{}", after[1]);
    assert!(after[1].ends_with(" >> /home/other/arena-proxy-cron.log 2>&1") && after[1].contains(" /root/.local/state/arena-cron/proxy.lock "));
    // The kept kinds tidy nothing (not even their old locks); the others as before.
    let plan = tidy_plan(&kinds, &before, &after, "/root", &with_arena_block("", &after));
    assert!(
        plan.iter().all(|f| !format!("{f:?}").contains("backup") && !format!("{f:?}").contains("proxy") && !format!("{f:?}").contains("/root/arena-cron.log")),
        "{plan:?}"
    );
    assert_eq!(plan.len(), 3, "the archive's log, the reap's log and lock: {plan:?}");
    // Re-installed: read back as kept again, the same lines, nothing to change.
    assert_eq!(kept_logs(&after, "/root"), kept);
    assert_eq!(merge_cron_block(&after, &kinds, &root_cron_now(&kept_logs(&after, "/root"))), after);
}

/// The home `cron install`/`remove` work in (CL2/CL5): `$HOME` set, absolute, a directory the
/// effective user owns, free of whitespace, `%` and newlines — `sudo -E` is refused, by name.
/// The arena binary and config path go into the lines unquoted: the same characters refused.
#[cfg(unix)]
#[test]
fn cron_home_must_be_this_users_and_fit_a_crontab_line() {
    use super::{cron_home, crontab_word};
    use std::os::unix::fs::MetadataExt;
    let base = std::env::temp_dir().join(format!("arena-cron-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("home")).unwrap();
    std::fs::write(base.join("file"), "").unwrap();
    let home = base.join("home").display().to_string();
    let uid = std::fs::metadata(&home).unwrap().uid();
    assert_eq!(cron_home(Some(&home), uid).unwrap(), home);
    let refused = |h: Option<&str>, euid: u32| cron_home(h, euid).unwrap_err().to_string();
    let file = base.join("file").display().to_string();
    let missing = base.join("missing").display().to_string();
    let cases: Vec<(Option<&str>, u32, String)> = vec![
        (None, uid, "HOME isn't set — run as the user whose crontab this is (sudo -i / sudo -H)".into()),
        (Some(""), uid, "HOME isn't set".into()),
        (Some("home/dev"), uid, "HOME=home/dev isn't an absolute path".into()),
        (Some("/home/a b"), uid, "HOME=\"/home/a b\" holds a space (U+0020), which a crontab line can't carry".into()),
        (Some("/home/50%"), uid, "HOME=\"/home/50%\" holds % (U+0025), which a crontab line can't carry".into()),
        (Some("/home/a\nb"), uid, "HOME=\"/home/a\\nb\" holds a newline (U+000A)".into()),
        (Some(&file), uid, format!("HOME={file} isn't a directory")),
        (Some(&missing), uid, format!("HOME={missing}: ")),
        (
            Some(&home),
            uid + 1,
            format!(
                "HOME={home} isn't this user's home (owner uid {uid}, running as {}) — run as the user whose crontab this is \
                 (sudo -i / sudo -H), not sudo -E",
                uid + 1
            ),
        ),
    ];
    for (h, euid, want) in cases {
        let e = refused(h, euid);
        assert!(e.contains(&want), "{h:?}: {e}");
    }
    assert!(crontab_word("the config path", "/srv/arena/config.env").is_ok());
    for bad in ["/srv/my arena/config.env", "/srv/50%/config.env", "/srv/a\nb", "/srv/a\tb"] {
        assert!(crontab_word("the arena binary", bad).unwrap_err().to_string().starts_with("the arena binary ("), "{bad:?}");
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// Review pr4: a crontab line carries HOME, the arena binary and the config path unquoted, and
/// refusing only whitespace and `%` let through paths `sh` can't run (a valid, owned HOME
/// `/srv/o'neil`, a config in `course(2026)/`: a syntax error on every tick, so arena never
/// ran) or runs as something else (`&`, `;`, `$`, a backtick). `cron install` now refuses
/// any character outside letters, digits and `./_+@:,=-`, in one line naming it, before
/// anything is written — and the archive's state dir, carried the same way, by the same rule.
#[cfg(unix)]
#[tokio::test]
async fn cron_install_refuses_a_path_a_crontab_line_cant_carry_unquoted() {
    use super::{cron_unsafe_char, crontab_word};
    // (the path, the character as named)
    let cases = [
        ("/srv/o'neil", "' (U+0027)"),
        ("/srv/course(2026)/config.env", "( (U+0028)"),
        ("/srv/a&b", "& (U+0026)"),
        ("/srv/a;b", "; (U+003B)"),
        ("/srv/$HOME/x", "$ (U+0024)"),
        ("/srv/a`id`", "` (U+0060)"),
        ("/srv/my arena", "a space (U+0020)"),
        ("/srv/a\tb", "a tab (U+0009)"),
        ("/srv/a\nb", "a newline (U+000A)"),
        ("/srv/50%", "% (U+0025)"),
        ("/srv/~/x", "~ (U+007E)"),
        ("/srv/a\"b", "\" (U+0022)"),
        ("/srv/a|b", "| (U+007C)"),
        ("/srv/a#b", "# (U+0023)"),
        ("/srv/a*", "* (U+002A)"),
        ("/srv/caf\u{e9}", "\u{e9} (U+00E9)"),
    ];
    for (path, named) in cases {
        for what in ["HOME", "the arena binary", "the config path"] {
            let e = crontab_word(what, path).unwrap_err().to_string();
            assert_eq!(
                e,
                format!(
                    "{what} ({path:?}) can't go into a crontab line: it holds {named} — the line carries it unquoted, so only \
                     letters, digits and `./_+@:,=-` may be in it; use a path without it"
                )
            );
            assert!(!e.contains('\n'), "one line: {e:?}");
        }
    }
    for ok in ["/root", "/home/dev/.local/bin/arena", "/srv/arena-2026/config.env", "/opt/a_b+c@d:e,f=g/x.y"] {
        assert_eq!((crontab_word("the config path", ok).ok(), cron_unsafe_char(ok)), (Some(()), None), "{ok}");
    }
    assert_eq!(
        crontab_word("the config path", "config.env").unwrap_err().to_string(),
        "the config path (\"config.env\") can't go into a crontab line: it isn't an absolute path"
    );
    // End to end: refused before anything is written — a HOME this user owns, a config path.
    let mut s = CronStub::new("unsafe-path");
    let home = s.dir.join("o'neil");
    std::fs::create_dir_all(&home).unwrap();
    let e = s.run_as(&["install", "--backup"], true, Some(home.to_str().unwrap())).await.unwrap_err().to_string();
    assert!(e.starts_with(&format!("HOME ({:?}) can't go into a crontab line: it holds ' (U+0027)", home.display().to_string())), "{e}");
    s.config = s.dir.join("course(2026)").join("config.env");
    std::fs::create_dir_all(s.config.parent().unwrap()).unwrap();
    std::fs::write(&s.config, "").unwrap();
    let e = s.run_as(&["install", "--backup"], true, Some(&s.home)).await.unwrap_err().to_string();
    assert!(e.starts_with("the config path (") && e.contains("course(2026)/config.env\") can't go into a crontab line: it holds ( (U+0028)"), "{e}");
    assert!(!s.dir.join("tab").exists(), "no crontab written");
    // The archive's state dir, carried as `ARENA_STATE_DIR=<root>`: the same rule.
    for (dir, named) in [("/srv/course(2026)/state", "( (U+0028)"), ("/srv/a&b/state", "& (U+0026)"), ("/srv/o'neil/state", "' (U+0027)")] {
        let cfg = arena_core::Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR={dir}\n"));
        let e = crate::archive::cron_state_root(&cfg).unwrap_err();
        assert!(e.starts_with(&format!("the state dir {dir:?} holds {named} — a crontab line carries it unquoted")), "{e}");
    }
    let cfg = arena_core::Config::parse("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR=/srv/arena-state\n");
    assert_eq!(crate::archive::cron_state_root(&cfg).unwrap(), "/srv/arena-state");
}

/// The tidy (CL2/CL7/CL12): exactly the old default files the replaced lines named — never
/// `$HOME` plus a name. Each old log is moved (hard-linked, or appended to a new log that's
/// there already); each old lock deleted; regular files of this user's only — a symlink is
/// left alone, never followed, and so is a new log that's a symlink or a directory; a stale
/// file at an old default name the replaced line didn't use stays; files of a kind that
/// wasn't rewritten stay. A tidy cut short (old and new already one file) just unlinks.
#[cfg(unix)]
#[test]
fn tidy_moves_exactly_the_old_files_the_replaced_lines_named() {
    use super::{merge_cron_block, tidy_line, tidy_old_cron_files, tidy_plan, CronKind::*, CronTidy};
    use arena_core::cron::{self, Job};
    use std::os::unix::fs::MetadataExt;
    let home = std::env::temp_dir().join(format!("arena-cron-tidy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let h = home.display().to_string();
    let euid = std::fs::metadata(&home).unwrap().uid();
    let put = |name: &str, text: &str| {
        std::fs::create_dir_all(home.join(name).parent().unwrap()).unwrap();
        std::fs::write(home.join(name), text).unwrap()
    };
    let read = |path: &str| std::fs::read_to_string(path).unwrap();
    // Lines logging to arena's defaults in this home, as 026c308 wrote them — but the
    // archive's as a663f65 did (its folder under the state root).
    let job = super::CronJob {
        schedule: "*/15 * * * *",
        env_prefix: "",
        exe: "/a",
        config: "/c",
        home: &h,
        backup: true,
        pull: false,
        proxy: true,
        snapshot_dir: Some("/w"),
        archive: true,
        archive_state_dir: None,
        kept_logs: &[],
    };
    let mut new = super::cron_lines(&job);
    new.push((Reap, arena_core::reap::render::cron_line(false, "/a", "/c", &h, None, &[])));
    let before: Vec<String> = new
        .iter()
        .map(|(k, l)| if matches!(k, ArchiveSnap | ArchiveBig) { prev_cron_line(l, &h) } else { legacy_cron_line(l, &h) })
        .collect();
    let kinds: Vec<super::CronKind> = new.iter().map(|(k, _)| *k).collect();
    let after = merge_cron_block(&before, &kinds, &new);
    let plan = tidy_plan(&kinds, &before, &after, &h, &with_arena_block("", &after));

    put("arena-cron.log", "old backup\n");
    put(".arena-backup-cron.lock", "");
    // The new proxy log is there already (a tick ran): the old one is appended to it.
    put("arena-proxy-cron.log", "old proxy\n");
    put(".arena-proxy-cron.lock", "");
    put(".local/state/arena-cron/proxy.log", "new proxy\n");
    // The new snapshot log is a symlink, the new archive log a directory: left alone.
    put("arena-snapshot-cron.log", "old snapshot\n");
    put("elsewhere.log", "not arena's\n");
    std::os::unix::fs::symlink(home.join("elsewhere.log"), cron::log(&h, Job::Snapshot)).unwrap();
    put(".local/state/arena/cron/archive.log", "old archive\n");
    std::fs::create_dir_all(cron::log(&h, Job::Archive)).unwrap();
    // A stale file at the archive's 026c308 name: the replaced (a663f65) line didn't use it.
    put("arena-archive-cron.log", "stale\n");
    // The old reap log is a symlink: left alone, never followed; its old lock a symlink too.
    std::os::unix::fs::symlink(home.join("elsewhere.log"), home.join("arena-reap-cron.log")).unwrap();
    std::os::unix::fs::symlink(home.join("elsewhere.log"), home.join(".arena-reap-cron.lock")).unwrap();

    // Not this user's (another euid): nothing touched.
    let other = tidy_old_cron_files(&plan, &h, euid + 1);
    assert!(other.moved.is_empty() && other.failed.is_empty() && other.left.len() == 5, "{other:?}");
    assert_eq!(read(&home.join("arena-cron.log").display().to_string()), "old backup\n");

    let t = tidy_old_cron_files(&plan, &h, euid);
    assert_eq!(
        t,
        CronTidy {
            moved: vec!["~/arena-cron.log".into(), "~/arena-proxy-cron.log".into()],
            left: vec![
                "~/arena-snapshot-cron.log (~/.local/state/arena-cron/snapshot.log isn't a regular file)".into(),
                "~/.local/state/arena/cron/archive.log (~/.local/state/arena-cron/archive.log isn't a regular file)".into(),
                "~/arena-reap-cron.log (not a regular file of this user's)".into(),
            ],
            failed: vec![],
        }
    );
    assert_eq!(read(&cron::log(&h, Job::Backup)), "old backup\n");
    assert_eq!(read(&cron::log(&h, Job::Proxy)), "new proxy\nold proxy\n");
    assert_eq!(read(&home.join("elsewhere.log").display().to_string()), "not arena's\n");
    assert!(std::fs::symlink_metadata(home.join(".arena-reap-cron.lock")).unwrap().file_type().is_symlink());
    let mut top: Vec<String> = std::fs::read_dir(&home).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    top.sort();
    assert_eq!(
        top,
        [".arena-reap-cron.lock", ".local", "arena-archive-cron.log", "arena-reap-cron.log", "arena-snapshot-cron.log", "elsewhere.log"],
        "moved: backup, proxy; their locks gone; left: the snapshot's (its new log a symlink), the stale archive name, the reap symlinks"
    );
    assert_eq!(read(&home.join("arena-archive-cron.log").display().to_string()), "stale\n");
    assert_eq!(
        tidy_line(&t).as_deref(),
        Some(
            "· cron logs now in ~/.local/state/arena-cron/ (moved: ~/arena-cron.log, ~/arena-proxy-cron.log; left alone: \
             ~/arena-snapshot-cron.log (~/.local/state/arena-cron/snapshot.log isn't a regular file), \
             ~/.local/state/arena/cron/archive.log (~/.local/state/arena-cron/archive.log isn't a regular file), \
             ~/arena-reap-cron.log (not a regular file of this user's))"
        )
    );
    assert_eq!(tidy_line(&CronTidy::default()), None);
    // A tidy cut short between its link and its unlink: the old log already is the new one —
    // just unlinked, never appended to itself.
    put("arena-cron.log", "once\n");
    std::fs::remove_file(cron::log(&h, Job::Backup)).unwrap();
    std::fs::hard_link(home.join("arena-cron.log"), cron::log(&h, Job::Backup)).unwrap();
    let t = tidy_old_cron_files(&plan[..1], &h, euid);
    assert_eq!(t.moved, ["~/arena-cron.log"]);
    assert_eq!(read(&cron::log(&h, Job::Backup)), "once\n");
    assert!(!home.join("arena-cron.log").exists());
    let _ = std::fs::remove_dir_all(&home);
}

/// Every line recreates its folders first — arena's cron folder, and a kept log's own: cron's
/// `sh` doesn't run a command whose `>>` (or flock's lock) points into a missing directory, so
/// without it a deleted folder would silently stop the backups. Each line `cron install`
/// renders — its schedule stripped — run by a real `sh` the way cron would, with both folders
/// deleted before each: a fake arena runs, sees the line's inline env (ARENA_START_DATE,
/// ARENA_STATE_DIR) through `&&` and flock/nice, and its output lands in the recreated log.
#[cfg(unix)]
#[test]
fn every_cron_line_runs_with_its_folders_deleted() {
    use super::{cron_lines, CronJob, CronKind};
    use arena_core::cron;
    if std::process::Command::new("flock").arg("--version").output().is_err() {
        eprintln!("skipped: no flock here");
        return;
    }
    let home = std::env::temp_dir().join(format!("arena-cron-sh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let h = home.display().to_string();
    let exe = home.join("fake-arena");
    arena_core::remote::write_test_script(&exe, "#!/bin/sh\necho \"fake arena ran: $*\"\nenv | grep '^ARENA_' | sort\n").unwrap();
    let exe = exe.display().to_string();
    let kept = [(CronKind::Proxy, format!("{h}/logs/cron/arena-proxy-cron.log")), (CronKind::ArchiveBig, format!("{h}/logs/big.log"))];
    let job = CronJob {
        schedule: "*/15 * * * *",
        env_prefix: "ARENA_START_DATE=2026-10-05 ",
        exe: &exe,
        config: "/c",
        home: &h,
        backup: true,
        pull: false,
        proxy: true,
        snapshot_dir: Some(&h),
        archive: true,
        archive_state_dir: Some(&h),
        kept_logs: &kept,
    };
    let mut lines = cron_lines(&job);
    let state = vec![("ARENA_STATE_DIR".to_string(), h.clone())];
    let reap_log = format!("{h}/logs/reap/reap.log");
    lines.push((CronKind::Reap, arena_core::reap::render::cron_line(false, &exe, "/c", &h, Some(&reap_log), &state)));
    let health_log = format!("{h}/logs/health/health.log");
    lines.push((CronKind::Health, super::health_cron_line(super::HealthAt { hour: 7, minute: 30 }, &exe, "/c", &h, &h, Some(&health_log))));
    assert_eq!(lines.len(), 7);
    for (kind, line) in &lines {
        let _ = std::fs::remove_dir_all(cron::dir(&h));
        let _ = std::fs::remove_dir_all(home.join("logs"));
        let command = line.splitn(6, ' ').nth(5).unwrap();
        let out = std::process::Command::new("sh")
            .args(["-c", command])
            .env_clear()
            .env("HOME", &h)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}: {command}\n{}", kind.label(), String::from_utf8_lossy(&out.stderr));
        let log_path = super::cron_log_target(line).unwrap();
        let log = std::fs::read_to_string(log_path).unwrap_or_default();
        assert!(log.starts_with("fake arena ran: --config /c "), "{}: the log in the recreated folder: {log:?}", kind.label());
        let start = log.contains("\nARENA_START_DATE=2026-10-05\n");
        let state = log.contains(&format!("\nARENA_STATE_DIR={h}\n"));
        let want = match kind {
            CronKind::Backup => (true, false),
            CronKind::ArchiveSnap | CronKind::ArchiveBig => (true, true),
            CronKind::Reap | CronKind::Health => (false, true),
            CronKind::Proxy | CronKind::Snapshot => (false, false),
        };
        assert_eq!((start, state), want, "{}: the env arena saw: {log:?}", kind.label());
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// The inline env a line carries (`ARENA_START_DATE`, the archive's `ARENA_STATE_DIR`) is read
/// from every version's line: this one's after its `mkdir -p … &&` (one folder or more), the
/// older ones' straight after the schedule — never from an argument.
#[test]
fn env_assignments_are_read_from_every_versions_line() {
    use super::env_assignment;
    // (line, ARENA_START_DATE, ARENA_STATE_DIR)
    let cases: &[(&str, Option<&str>, Option<&str>)] = &[
        (
            "40 * * * * mkdir -p /h/.local/state/arena-cron /h/logs && ARENA_START_DATE=2026-10-01 ARENA_STATE_DIR=/s nice -n 10 \
             /a --config /c archive sync --tier big >> /h/logs/archive.log 2>&1",
            Some("2026-10-01"),
            Some("/s"),
        ),
        (
            "40 * * * * mkdir -p /h/.local/state/arena/cron && ARENA_START_DATE=2026-10-01 ARENA_STATE_DIR=/s nice -n 10 /a --config /c \
             archive sync --tier big >> /h/.local/state/arena/cron/archive.log 2>&1",
            Some("2026-10-01"),
            Some("/s"),
        ),
        (
            "40 * * * * ARENA_START_DATE=2026-10-01 ARENA_STATE_DIR=/s nice -n 10 /a --config /c archive sync --tier big >> /h/arena-archive-cron.log 2>&1",
            Some("2026-10-01"),
            Some("/s"),
        ),
        ("@hourly mkdir -p /d && ARENA_STATE_DIR=/s /a archive sync --tier big", None, Some("/s")),
        ("40 * * * * mkdir -p /d && nice -n 10 /a --x ARENA_STATE_DIR=/s", None, None),
        ("40 * * * * mkdir -p && ARENA_STATE_DIR=/s /a", None, None),
        ("40 * * * * nice ARENA_START_DATE=2026-10-01", None, None),
        ("40 * * * *", None, None),
    ];
    for (line, start, state) in cases {
        assert_eq!(env_assignment(line, "ARENA_START_DATE").as_deref(), *start, "{line}");
        assert_eq!(env_assignment(line, "ARENA_STATE_DIR").as_deref(), *state, "{line}");
    }
}

/// Review finding: comments inside the block are nobody's line. A paused backup line and
/// a note that mentions the snapshot survive a targeted install/remove of that kind, and
/// a paused backup line doesn't count as "there is a backup".
#[test]
fn cron_merge_leaves_comments_alone() {
    use super::{merge_cron_block, CronKind::*};
    let paused = "# */15 * * * * flock -n /h/.arena-backup-cron.lock /opt/arena --config /c pods backup --yes";
    let note = "# snapshot page is served by caddy from /srv/www";
    let snap = cron_kinds(false, false, false, Some("/w")).remove(0);
    let before: Vec<String> = vec![paused.into(), note.into(), snap.1.clone()];
    // `cron remove --snapshot`: the real line goes, the note stays.
    assert_eq!(merge_cron_block(&before, &[Snapshot], &[]), [paused, note]);
    // `cron install --snapshot DIR` replaces the real line in place, not the note.
    let snap2 = cron_kinds(false, false, false, Some("/w2")).remove(0);
    assert_eq!(merge_cron_block(&before, &[Snapshot], std::slice::from_ref(&snap2)), [paused, note, snap2.1.as_str()]);
    // `cron install --backup`: the paused line stays paused; the live one is added.
    let backup = cron_kinds(true, false, false, None);
    assert_eq!(merge_cron_block(&before, &[Backup], &backup), [paused, note, snap.1.as_str(), backup[0].1.as_str()]);
    // `cron remove --backup` leaves the paused line (plain `remove` clears the block).
    assert_eq!(merge_cron_block(&before, &[Backup], &[]), before);
    assert!(!before.iter().any(|l| super::CronKind::of(l) == Some(Backup)), "a paused line isn't a backup");
}

/// Merge of wave 7 archive + reaper: the backup line, the archive's two lines and the reap
/// line render into one block side by side — each read back as exactly its own kind; the
/// backup and reap lines under `flock -n -o` (the archive's lock each tier in-process); the
/// archive's lines carrying their state root — and a targeted remove or re-install of one
/// kind never touches the others.
#[test]
fn archive_and_reap_cron_lines_live_side_by_side() {
    use super::{cron_lines, merge_cron_block, CronJob, CronKind::*};
    let job = CronJob {
        schedule: "*/15 * * * *",
        env_prefix: "ARENA_START_DATE=2026-10-05 ",
        exe: "/opt/arena",
        config: "/srv/config.env",
        home: "/home/u",
        backup: true,
        pull: true,
        proxy: false,
        snapshot_dir: None,
        archive: true,
        archive_state_dir: Some("/var/lib/arena"),
        kept_logs: &[],
    };
    let reap = |apply: bool| (Reap, arena_core::reap::render::cron_line(apply, "/opt/arena", "/srv/config.env", "/home/u", None, &[]));
    let mut lines = cron_lines(&job);
    lines.push(reap(false));
    let mut kinds: Vec<super::CronKind> = lines.iter().map(|(k, _)| *k).collect();
    kinds.sort_by_key(|k| k.label());
    assert_eq!(kinds, [ArchiveBig, ArchiveSnap, Backup, Reap]);
    for (k, l) in &lines {
        assert_eq!(super::CronKind::of(l), Some(*k), "{l}");
    }
    let line = |k: super::CronKind| lines.iter().find(|(x, _)| *x == k).map(|(_, l)| l.clone()).unwrap();
    assert!(line(Backup).contains("flock -n -o ") && line(Reap).contains("flock -n -o "), "{lines:#?}");
    assert!(line(ArchiveSnap).contains("/var/lib/arena") && line(ArchiveBig).contains("/var/lib/arena"), "{lines:#?}");
    let all: Vec<super::CronKind> = lines.iter().map(|(k, _)| *k).collect();
    let block = merge_cron_block(&[], &all, &lines);
    assert_eq!(block.len(), 4);
    // `cron remove --reap` / `--archive`: only those.
    let no_reap = merge_cron_block(&block, &[Reap], &[]);
    assert_eq!(no_reap, block.iter().filter(|l| super::CronKind::of(l) != Some(Reap)).cloned().collect::<Vec<_>>());
    let no_archive = merge_cron_block(&block, &[ArchiveSnap, ArchiveBig], &[]);
    assert!(no_archive.iter().all(|l| !matches!(super::CronKind::of(l), Some(ArchiveSnap | ArchiveBig))) && no_archive.len() == 2);
    // `cron install --reap apply`: the reap line replaced in place, the rest untouched.
    let apply = reap(true);
    let swapped = merge_cron_block(&block, &[Reap], std::slice::from_ref(&apply));
    assert_eq!(swapped.len(), 4);
    for (before, after) in block.iter().zip(&swapped) {
        if super::CronKind::of(before) == Some(Reap) {
            assert_eq!(after, &apply.1);
        } else {
            assert_eq!(after, before);
        }
    }
}

/// Live W7 step 0: the reap line ran at :07/:22/:37/:52 — the archive snap push's very
/// minutes, both at the backup roots at once. The reap line runs on minutes no other arena line
/// uses — every one `cron install` renders: the */15 backup, the archive's snap and big pushes,
/// the */5 proxy re-sync and the */2 dashboard snapshot (W7LR note a). Read from the rendered
/// lines, as cron reads them.
#[test]
fn the_reap_line_runs_on_minutes_no_other_line_uses() {
    use super::{cron_lines, CronJob, CronKind::*};
    // The minutes a crontab line's first field names (`*/n`, `a,b,…`, `n`).
    let minutes = |line: &str| -> std::collections::BTreeSet<u32> {
        let field = line.split_whitespace().next().unwrap();
        field
            .split(',')
            .flat_map(|part| match part.strip_prefix("*/") {
                Some(step) => (0..60).step_by(step.parse().unwrap()).collect::<Vec<u32>>(),
                None => vec![part.parse().unwrap()],
            })
            .collect()
    };
    let job = CronJob {
        schedule: super::BACKUP_CRON_SCHEDULE,
        env_prefix: "",
        exe: "/opt/arena",
        config: "/srv/config.env",
        home: "/home/u",
        backup: true,
        pull: true,
        proxy: true,
        snapshot_dir: Some("/srv/www"),
        archive: true,
        archive_state_dir: None,
        kept_logs: &[],
    };
    let lines = cron_lines(&job);
    let kinds: Vec<super::CronKind> = lines.iter().map(|(k, _)| *k).collect();
    assert_eq!(kinds, [Backup, Proxy, Snapshot, ArchiveSnap, ArchiveBig], "every other line arena renders");
    let reap = minutes(&arena_core::reap::render::cron_line(true, "/opt/arena", "/srv/config.env", "/home/u", None, &[]));
    assert_eq!(reap.len(), 4, "four times an hour: {reap:?}");
    for (k, l) in &lines {
        let other = minutes(l);
        assert!(reap.is_disjoint(&other), "the reap line shares minutes with the {} line: {reap:?} vs {other:?}", k.label());
    }
}

/// Review finding: the preview counts copies. Identical duplicates collapsing into one
/// show the removed copy as `- line` — the write never removes a line the preview hid.
#[test]
fn cron_diff_shows_every_removed_copy() {
    use super::{cron_diff, merge_cron_block, CronKind::*};
    let a = cron_job(false, false).remove(0);
    let doubled = vec![a.clone(), a.clone()];
    let after = merge_cron_block(&doubled, &[Backup], &[(Backup, a.clone())]);
    assert_eq!(after, std::slice::from_ref(&a));
    assert_eq!(cron_diff(&doubled, &after), [format!("- {a}"), format!("  {a}")]);
    // (case, before, after, diff)
    let l = |s: &str| s.to_string();
    type Case = (&'static str, Vec<String>, Vec<String>, Vec<String>);
    let cases: Vec<Case> = vec![
        ("unchanged", vec![l("A"), l("B")], vec![l("A"), l("B")], vec![l("  A"), l("  B")]),
        ("replace in place", vec![l("A"), l("B")], vec![l("X"), l("B")], vec![l("- A"), l("+ X"), l("  B")]),
        ("three copies to one", vec![l("A"), l("A"), l("A")], vec![l("A")], vec![l("- A"), l("- A"), l("  A")]),
        ("a copy added", vec![l("A")], vec![l("A"), l("A")], vec![l("  A"), l("+ A")]),
        ("all gone", vec![l("A"), l("B")], vec![], vec![l("- A"), l("- B")]),
        ("from nothing", vec![], vec![l("A")], vec![l("+ A")]),
    ];
    for (case, before, after, want) in cases {
        assert_eq!(cron_diff(&before, &after), want, "{case}");
    }
}

/// An installed backup line's options read back, from every version's line shape.
#[test]
fn backup_opts_read_back_from_an_installed_line() {
    use super::BackupOpts;
    let o = |schedule: &str, start: Option<&str>, pull: bool| {
        Some(BackupOpts { schedule: schedule.into(), start_date: start.map(String::from), pull })
    };
    let cases: &[(&str, Option<BackupOpts>)] = &[
        // This version's line (the cron folder recreated first), with the start date and --pull.
        (
            "*/15 * * * * mkdir -p /h/.local/state/arena/cron && ARENA_START_DATE=2026-01-05 flock -n -o \
             /h/.local/state/arena/cron/backup.lock /opt/arena --config /c pods backup --yes >> /h/.local/state/arena/cron/backup.log 2>&1",
            o("*/15 * * * *", Some("2026-01-05"), true),
        ),
        ("@hourly mkdir -p /d && ARENA_START_DATE=2026-02-02 flock -n -o /d/backup.lock /opt/arena pods backup --no-pull --yes", o("@hourly", Some("2026-02-02"), false)),
        // The previous version's (flock -n -o, its lock and log in the home).
        (
            "*/15 * * * * ARENA_START_DATE=2026-01-05 flock -n -o /h/.arena-backup-cron.lock /opt/arena --config /c pods backup --yes >> /h/arena-cron.log 2>&1",
            o("*/15 * * * *", Some("2026-01-05"), true),
        ),
        // K1's (flock -n), git-only, hourly.
        ("0 * * * * flock -n /h/.arena-backup-cron.lock /opt/arena --config /c pods backup --no-pull --yes >> /h/arena-cron.log 2>&1", o("0 * * * *", None, false)),
        // An earlier version's (no flock): env straight before the binary.
        ("*/15 * * * * ARENA_START_DATE=2026-01-05 /usr/local/bin/arena --config /c pods backup --yes >> /root/arena-cron.log 2>&1", o("*/15 * * * *", Some("2026-01-05"), true)),
        // Tabs, a macro schedule, another env var first.
        ("@hourly\tPATH=/usr/bin ARENA_START_DATE=2026-02-02 /opt/arena pods backup --no-pull --yes", o("@hourly", Some("2026-02-02"), false)),
        // A start date that isn't one, one that isn't a leading env assignment (a hand edit
        // behind `cd … &&`, an argument): left out, the rest read (review fb2 F7) — the install
        // says so ([`super::unread_start_date`]).
        ("*/15 * * * * ARENA_START_DATE=soon /opt/arena pods backup --yes", o("*/15 * * * *", None, true)),
        ("*/15 * * * * /opt/arena pods backup --yes # ARENA_START_DATE=2026-01-05", o("*/15 * * * *", None, true)),
        ("*/30 * * * * cd /root && ARENA_START_DATE=2026-10-05 flock -n /l /opt/arena pods backup --no-pull --yes", o("*/30 * * * *", None, false)),
        // Several folders made first (a kept log elsewhere): still read.
        (
            "*/15 * * * * mkdir -p /h/.local/state/arena-cron /h/logs/cron && ARENA_START_DATE=2026-01-05 flock -n -o \
             /h/.local/state/arena-cron/backup.lock /opt/arena pods backup --no-pull --yes >> /h/logs/cron/arena-cron.log 2>&1",
            o("*/15 * * * *", Some("2026-01-05"), false),
        ),
        // Not a backup line, a paused one, or no schedule to read.
        ("*/5 * * * * /opt/arena proxy apply --yes", None),
        ("# */15 * * * * /opt/arena pods backup --yes", None),
        ("* * * pods backup", None),
    ];
    for (line, want) in cases {
        assert_eq!(&BackupOpts::of_line(line), want, "{line}");
    }
}

/// Review fb2 F7: an installed backup line whose start date can't be read (a hand edit's `cd
/// /root && ARENA_START_DATE=…`) read as unreadable altogether, so a re-install dropped its
/// schedule and `--pull` with the date. Now only the date goes, and the note says so; when
/// the flags decide the date, or the line isn't rewritten, there's nothing to say.
#[test]
fn an_unreadable_start_date_drops_only_the_date() {
    use super::{backup_read_note, resolve_backup_opts, unread_start_date, BackupFlags, BackupOpts};
    let line = "*/30 * * * * cd /root && ARENA_START_DATE=2026-10-05 /usr/local/bin/arena --config /c pods backup --yes >> \
                /root/logs/cron/arena-cron.log 2>&1";
    let read = BackupOpts::of_line(line).unwrap();
    assert_eq!(read, BackupOpts { schedule: "*/30 * * * *".into(), start_date: None, pull: true });
    assert!(unread_start_date(line));
    let none = BackupFlags { schedule: None, start_date: None, no_start_date: false, pull: false, no_pull: false };
    let (opts, kept) = resolve_backup_opts(Some(&read), &none);
    assert_eq!(opts, BackupOpts { schedule: "*/30 * * * *".into(), start_date: None, pull: true });
    assert_eq!(kept, ["schedule `*/30 * * * *`", "--pull"]);
    assert_eq!(
        backup_read_note(true, Some(line), Some(&read), &none).unwrap(),
        "note: couldn't read the installed backup line's ARENA_START_DATE (hand-edited?) — the new line has none (--start-date \
         YYYY-MM-DD sets one); its schedule and --pull are kept; check the diff"
    );
    // The flags decide the date, or the backup line isn't rewritten: nothing to say.
    assert_eq!(backup_read_note(true, Some(line), Some(&read), &BackupFlags { start_date: Some("2026-10-05"), ..none }), None);
    assert_eq!(backup_read_note(true, Some(line), Some(&read), &BackupFlags { no_start_date: true, ..none }), None);
    assert_eq!(backup_read_note(false, Some(line), Some(&read), &none), None);
    // A line read in full: nothing; none installed: nothing; no readable schedule: the flags alone, said.
    let fine = "*/15 * * * * ARENA_START_DATE=2026-10-05 /opt/arena pods backup --yes";
    assert!(!unread_start_date(fine) && !unread_start_date("*/15 * * * * /opt/arena pods backup --yes"));
    assert_eq!(backup_read_note(true, Some(fine), BackupOpts::of_line(fine).as_ref(), &none), None);
    assert_eq!(backup_read_note(true, None, None, &none), None);
    assert!(backup_read_note(true, Some("* * * pods backup"), None, &none).unwrap().contains("rewritten from these flags alone"));
}

/// Review fb2 F8: a hand edit's `>>PATH 2>&1` (no space), `>> PATH` or `>>PATH` is a log
/// someone chose, kept like arena's own `>> PATH 2>&1`. Any other ending isn't read: the new
/// line logs to the default, as before — now said by the install's note, naming the line's
/// redirection, not only by the diff. A line that logs nowhere has nothing to say.
#[test]
fn a_lines_log_is_read_in_its_hand_edited_forms() {
    use super::{cron_log_target, kept_logs, unread_log, unread_logs_note, CronKind::*};
    let cmd = "*/15 * * * * /usr/local/bin/arena --config /c pods backup --yes";
    // (ending, the log read, the redirection not read)
    let cases: &[(&str, Option<&str>, Option<&str>)] = &[
        (" >> /root/logs/cron/arena-cron.log 2>&1", Some("/root/logs/cron/arena-cron.log"), None),
        (" >>/root/logs/cron/arena-cron.log 2>&1", Some("/root/logs/cron/arena-cron.log"), None),
        (" >> /root/logs/cron/arena-cron.log", Some("/root/logs/cron/arena-cron.log"), None),
        (" >>/root/logs/cron/arena-cron.log", Some("/root/logs/cron/arena-cron.log"), None),
        ("\t>>\t/x.log\t2>&1", Some("/x.log"), None),
        (" > /x.log 2>&1", None, Some("> /x.log 2>&1")),
        (" >> /x.log 2>>/x.err", None, Some(">> /x.log 2>>/x.err")),
        (" >> /x.log 2>&1 # mine", None, Some(">> /x.log 2>&1 # mine")),
        (" >>\"/x y.log\" 2>&1", None, Some(">>\"/x y.log\" 2>&1")),
        (" 2>>/x.err", None, Some("2>>/x.err")),
        (" >>", None, Some(">>")),
        (" >>&2", None, Some(">>&2")),
        (" 2>&1", None, None),
        ("", None, None),
    ];
    for (end, log, unread) in cases {
        let line = format!("{cmd}{end}");
        assert_eq!((cron_log_target(&line), unread_log(&line).as_deref()), (*log, *unread), "{line:?}");
    }
    // The live shape (`>>PATH 2>&1`, review F8's scenario) is a chosen log: kept.
    let attached = format!("{cmd} >>/srv/x/logs/cron/arena-cron.log 2>&1");
    assert_eq!(kept_logs(std::slice::from_ref(&attached), "/root"), [(Backup, "/srv/x/logs/cron/arena-cron.log".to_string())]);
    assert_eq!(unread_logs_note(std::slice::from_ref(&attached), &[Backup]), None);
    // The note: only the kinds being rewritten, the first line of each, its redirection named.
    let before = vec![
        format!("{cmd} > /x.log 2>&1"),
        format!("{cmd} >> /second.log 2>>/err"),
        "*/5 * * * * /usr/local/bin/arena --config /c proxy apply --yes >> /p.log 2>>/p.err".to_string(),
        "*/5 * * * * /usr/local/bin/arena --config /c snapshot --public --out /w/fleet.json".to_string(),
    ];
    assert_eq!(
        unread_logs_note(&before, &[Backup, Proxy, Snapshot]).unwrap(),
        "note: couldn't read where these installed lines log — their new lines log to arena's default \
         (~/.local/state/arena-cron/) instead; end a line with `>> PATH 2>&1` for a re-install to keep its log; check the \
         diff: backup (`> /x.log 2>&1`), proxy (`>> /p.log 2>>/p.err`)"
    );
    assert!(unread_logs_note(&before, &[Proxy]).unwrap().ends_with("check the diff: proxy (`>> /p.log 2>>/p.err`)"));
    assert_eq!(unread_logs_note(&before, &[Snapshot]), None);
}

/// Review fb2 F7/F8 end to end: a hand-edited backup line — its start date behind `cd … &&`,
/// its log `>>PATH` with no space — re-installed keeps its schedule, `--pull` and log; only the
/// date goes.
#[cfg(unix)]
#[tokio::test]
async fn cron_install_keeps_what_it_can_read_of_a_hand_edited_line() {
    let s = CronStub::new("handedit");
    let log = format!("{}/logs/cron/arena-cron.log", s.dir.display());
    s.set_tab(&format!(
        "{}\n*/30 * * * * cd /root && ARENA_START_DATE=2026-10-05 /usr/local/bin/arena --config /c pods backup --yes >>{log} 2>&1\n{}\n",
        super::CRON_BEGIN,
        super::CRON_END
    ));
    s.run(&["install", "--backup"]).await;
    let lines = s.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("*/30 * * * * mkdir -p "), "the schedule kept: {lines:?}");
    assert!(lines[0].ends_with(&format!(" pods backup --yes >> {log} 2>&1")), "--pull and the log kept: {lines:?}");
    assert!(!lines[0].contains("ARENA_START_DATE") && !lines[0].contains("cd /root"), "{lines:?}");
}

/// Review fb2 F10: the effective uid is the owner of a file the process just made — not
/// `/proc/self`'s, which the kernel makes root's for a process that isn't dumpable. Here that
/// is the current user: the HOME check passes for a home this user owns, `id -u` agrees, and
/// the file is gone afterwards.
#[cfg(unix)]
#[test]
fn the_effective_uid_is_this_users_and_its_home_passes() {
    use super::{created_file_owner, cron_home, effective_uid};
    use std::os::unix::fs::MetadataExt;
    let base = std::env::temp_dir().join(format!("arena-euid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("home")).unwrap();
    let mine = std::fs::metadata(base.join("home")).unwrap().uid();
    assert_eq!(created_file_owner(&base), Some(mine));
    assert_eq!(std::fs::read_dir(&base).unwrap().count(), 1, "only home/: the file made is removed");
    assert_eq!(effective_uid().unwrap(), mine);
    let home = base.join("home").display().to_string();
    assert_eq!(cron_home(Some(&home), effective_uid().unwrap()).unwrap(), home);
    let id = std::process::Command::new("id").arg("-u").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&id.stdout).trim().parse::<u32>().unwrap(), mine, "`id -u` agrees");
    // Nowhere to make one: no answer (effective_uid falls back to /proc/self, then `id -u`).
    assert_eq!(created_file_owner(&base.join("missing")), None);
    let _ = std::fs::remove_dir_all(&base);
}

/// Review ai-review claude #10: `cron install` read the effective uid a second time for the
/// tidy, after it had written the crontab — a failure there reported the install as failed
/// once it was in, and the operator re-ran or rolled back for nothing. Now it's read once,
/// before anything is written: with every read after the first failing, the install goes
/// through and says nothing failed; with the first failing, nothing is written.
#[cfg(unix)]
#[tokio::test]
async fn cron_install_reads_the_uid_once_before_it_writes_the_crontab() {
    let s = CronStub::new("euid-once");
    super::EUID_ANSWERS.with(|n| n.set(Some(0)));
    let refused = s.run_as(&["install", "--pull", "--start-date", "2026-01-05"], true, Some(&s.home)).await;
    assert!(refused.unwrap_err().to_string().contains("the effective uid can't be read"));
    assert_eq!(s.tab(), "", "nothing written");
    super::EUID_ANSWERS.with(|n| n.set(Some(1)));
    s.run(&["install", "--pull", "--start-date", "2026-01-05"]).await;
    assert_eq!(super::EUID_ANSWERS.with(std::cell::Cell::get), Some(0), "read once");
    assert_eq!(s.lines().len(), 1, "{}", s.tab());
    super::EUID_ANSWERS.with(|n| n.set(None));
}

/// The backup line a (re-)install writes: each option from its flag, else the installed
/// line's, else the default — and what was kept, for the note. The review finding: a
/// re-install (`cron install --backup`, to pick up the flock) used to drop the installed
/// line's `ARENA_START_DATE` and `--pull`.
#[test]
fn resolve_backup_opts_keeps_what_the_flags_dont_change() {
    use super::{resolve_backup_opts, BackupFlags, BackupOpts};
    let installed = BackupOpts { schedule: "0 * * * *".into(), start_date: Some("2026-01-05".into()), pull: true };
    let plain = BackupOpts { schedule: "*/15 * * * *".into(), start_date: None, pull: false };
    let none = BackupFlags { schedule: None, start_date: None, no_start_date: false, pull: false, no_pull: false };
    let want = |schedule: &str, start: Option<&str>, pull: bool| BackupOpts { schedule: schedule.into(), start_date: start.map(String::from), pull };
    // (case, installed, flags, opts, kept)
    type Case<'a> = (&'a str, Option<&'a BackupOpts>, BackupFlags<'a>, BackupOpts, Vec<&'a str>);
    let cases: Vec<Case> = vec![
        ("fresh: the defaults", None, BackupFlags { ..none }, want("*/15 * * * *", None, false), vec![]),
        ("fresh with flags", None, BackupFlags { pull: true, start_date: Some("2026-03-01"), ..none }, want("*/15 * * * *", Some("2026-03-01"), true), vec![]),
        (
            "`--backup` keeps everything",
            Some(&installed),
            BackupFlags { ..none },
            want("0 * * * *", Some("2026-01-05"), true),
            vec!["schedule `0 * * * *`", "ARENA_START_DATE=2026-01-05", "--pull"],
        ),
        ("--no-pull", Some(&installed), BackupFlags { no_pull: true, ..none }, want("0 * * * *", Some("2026-01-05"), false), vec!["schedule `0 * * * *`", "ARENA_START_DATE=2026-01-05"]),
        ("--no-start-date", Some(&installed), BackupFlags { no_start_date: true, ..none }, want("0 * * * *", None, true), vec!["schedule `0 * * * *`", "--pull"]),
        (
            "flags win",
            Some(&installed),
            BackupFlags { schedule: Some("*/30 * * * *"), start_date: Some("2026-02-02"), pull: true, ..none },
            want("*/30 * * * *", Some("2026-02-02"), true),
            vec![],
        ),
        ("defaults kept aren't news", Some(&plain), BackupFlags { ..none }, plain.clone(), vec![]),
        ("--pull on a git-only line", Some(&plain), BackupFlags { pull: true, ..none }, want("*/15 * * * *", None, true), vec![]),
    ];
    for (case, inst, flags, opts, kept) in cases {
        let (got, got_kept) = resolve_backup_opts(inst, &flags);
        assert_eq!(got, opts, "{case}");
        assert_eq!(got_kept, kept, "{case}");
    }
}

/// `arena cron …` end to end against a stub `crontab` over a file (the real crontab is never
/// touched), in a temp dir whose `home/` is the HOME the lines run in.
#[cfg(unix)]
struct CronStub {
    dir: std::path::PathBuf,
    stub: String,
    config: std::path::PathBuf,
    home: String,
}

#[cfg(unix)]
impl CronStub {
    fn new(tag: &str) -> CronStub {
        let dir = std::env::temp_dir().join(format!("arena-cron-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("www")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let stub = dir.join("crontab");
        arena_core::remote::write_test_script(
            &stub,
            "#!/bin/sh\ntab=\"$(dirname \"$0\")/tab\"\ncase \"$1\" in\n  \
             -l) [ -f \"$tab\" ] && exec cat \"$tab\"; echo 'no crontab for u' >&2; exit 1;;\n  \
             -) cat > \"$tab\";;\nesac\n",
        )
        .unwrap();
        let config = dir.join("config.env");
        std::fs::write(&config, "").unwrap();
        CronStub { stub: stub.display().to_string(), config, home: dir.join("home").display().to_string(), dir }
    }

    /// `arena cron <args>`, confirming with `--yes` (`yes`), with this `home` as `$HOME`.
    async fn run_as(&self, args: &[&str], yes: bool, home: Option<&str>) -> anyhow::Result<()> {
        use clap::Parser;
        let argv = ["arena", "cron"].iter().chain(args).copied();
        let crate::cli::Cmd::Cron(c) = crate::cli::Cli::try_parse_from(argv).unwrap().cmd else { unreachable!() };
        super::handle_cron(c, &arena_core::Config::default(), &self.config, yes, &self.stub, home).await
    }

    async fn run(&self, args: &[&str]) {
        self.run_as(args, true, Some(&self.home)).await.unwrap()
    }

    fn tab(&self) -> String {
        std::fs::read_to_string(self.dir.join("tab")).unwrap_or_default()
    }

    fn set_tab(&self, text: &str) {
        std::fs::write(self.dir.join("tab"), text).unwrap()
    }

    fn lines(&self) -> Vec<String> {
        super::arena_cron_lines(&self.tab()).managed
    }

    /// What's directly in the home, sorted.
    fn home_entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.home).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }
}

#[cfg(unix)]
impl Drop for CronStub {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `arena cron …` end to end: a later `--proxy` keeps the earlier `--pull --start-date`
/// backup line byte for byte, `--snapshot` adds its line, `remove --proxy` drops only that
/// one, a dry run writes nothing, 026c308's lines are upgraded in place — their old default
/// files moved into the cron folder only once the crontab is written: never on a dry run, a
/// refused prompt or a no-op — and plain `remove` clears the block, the user's own entries
/// surviving throughout.
#[cfg(unix)]
#[tokio::test]
async fn cron_commands_merge_lines_into_the_real_crontab_text() {
    use std::io::IsTerminal;
    let s = CronStub::new("e2e");
    let (home, www) = (s.home.clone(), s.dir.join("www").display().to_string());
    let mkdir = format!("mkdir -p {} && ", arena_core::cron::dir(&home));

    // No crontab yet: the backup line with --pull and the start date, flock'd.
    s.run(&["install", "--pull", "--start-date", "2026-01-05"]).await;
    let backup = s.lines();
    assert_eq!(backup.len(), 1, "{backup:?}");
    assert!(
        backup[0].starts_with(&format!("*/15 * * * * {mkdir}ARENA_START_DATE=2026-01-05 flock -n -o "))
            && backup[0].contains(" pods backup --yes >> "),
        "{backup:?}"
    );
    // The user adds their own entry by hand.
    s.set_tab(&format!("0 9 * * * /usr/bin/keep-me\n{}", s.tab()));

    // `--proxy` then `--snapshot`: each adds only its own line.
    s.run(&["install", "--proxy"]).await;
    s.run(&["install", "--snapshot", &www]).await;
    let all = s.lines();
    assert_eq!(all.len(), 3, "{all:?}");
    assert_eq!(all[0], backup[0], "the backup line is untouched");
    assert!(all[1].contains(" proxy apply --yes ") && all[2].contains(" snapshot --public --out "), "{all:?}");

    // A dry run changes nothing; `remove --proxy` drops just that line.
    s.run(&["remove", "--backup", "--dry-run"]).await;
    assert_eq!(s.lines(), all);
    s.run(&["remove", "--proxy"]).await;
    assert_eq!(s.lines(), [all[0].clone(), all[2].clone()]);
    // Re-installing what's there is a no-op.
    s.run(&["install", "--snapshot", &www]).await;
    assert_eq!(s.lines(), [all[0].clone(), all[2].clone()]);

    // Re-installing the backup line (to pick up a new line shape) keeps its --pull and
    // start date — even from an older version's line, which it upgrades in place — and
    // only the opposite flags drop them. The other lines never move.
    let legacy = legacy_cron_line(&all[0], &home).replace(" flock -n -o ", " flock -n ");
    s.set_tab(&s.tab().replace(&all[0], &legacy));
    assert_eq!(s.lines()[0], legacy);
    s.run(&["install", "--backup"]).await;
    assert_eq!(s.lines(), [all[0].clone(), all[2].clone()], "the legacy line upgraded, --pull and the start date kept");
    s.run(&["install", "--no-pull"]).await;
    let git_only = s.lines();
    assert!(
        git_only[0].starts_with(&format!("*/15 * * * * {mkdir}ARENA_START_DATE=2026-01-05 flock -n -o "))
            && git_only[0].contains(" pods backup --no-pull --yes >> "),
        "{git_only:?}"
    );
    s.run(&["install", "--schedule", "0 * * * *"]).await;
    let hourly = s.lines();
    assert!(hourly[0].starts_with(&format!("0 * * * * {mkdir}ARENA_START_DATE=2026-01-05 flock")) && hourly[0].contains("--no-pull"), "{hourly:?}");
    s.run(&["install", "--no-start-date", "--pull"]).await;
    let undated = s.lines();
    assert!(undated[0].starts_with(&format!("0 * * * * {mkdir}flock -n -o ")) && undated[0].contains(" pods backup --yes >> "), "{undated:?}");
    assert_eq!(undated[1], all[2], "the snapshot line never moved");

    // 026c308's lines, their logs and locks directly in the home, and those files there.
    let old: Vec<String> = undated.iter().map(|l| legacy_cron_line(l, &home)).collect();
    assert!(old.iter().zip(&undated).all(|(o, n)| o != n && !o.contains(".local/state")), "{old:?}");
    s.set_tab(&s.tab().replace(&undated[0], &old[0]).replace(&undated[1], &old[1]));
    assert_eq!(s.lines(), old);
    let put = |f: &str, text: &str| std::fs::write(std::path::Path::new(&home).join(f), text).unwrap();
    for f in ["arena-cron.log", ".arena-backup-cron.lock", "arena-snapshot-cron.log", ".arena-snapshot-cron.lock", "arena-proxy-cron.log"] {
        put(f, &format!("{f}\n"));
    }
    let files = s.home_entries();
    // A dry run, and a prompt refused (no terminal, no --yes): nothing written, nothing tidied.
    s.run(&["install", "--backup", "--snapshot", &www, "--dry-run"]).await;
    assert_eq!((s.lines(), s.home_entries()), (old.clone(), files.clone()), "a dry run tidies nothing");
    if !std::io::stdin().is_terminal() {
        let e = s.run_as(&["install", "--backup", "--snapshot", &www], false, Some(&home)).await.unwrap_err();
        assert!(e.downcast_ref::<crate::NoTerminal>().is_some(), "{e}");
        assert_eq!((s.lines(), s.home_entries()), (old.clone(), files.clone()), "a refused prompt tidies nothing");
    }
    // Written: each line replaced in place — never doubled — the backup line keeping its
    // options, read back from the old line; then the rewritten lines' old logs move into the
    // cron folder and their locks go. The proxy's old log stays: no line was rewritten for it.
    s.run(&["install", "--backup", "--snapshot", &www]).await;
    assert_eq!(s.lines(), undated, "upgraded in place, one line each");
    assert_eq!(s.home_entries(), [".local", "arena-proxy-cron.log"]);
    for (job, was) in [(arena_core::cron::Job::Backup, "arena-cron.log"), (arena_core::cron::Job::Snapshot, "arena-snapshot-cron.log")] {
        assert_eq!(std::fs::read_to_string(arena_core::cron::log(&home, job)).unwrap(), format!("{was}\n"));
    }
    // A no-op install tidies nothing either: an old default log put back stays.
    put("arena-cron.log", "again\n");
    s.run(&["install", "--backup", "--snapshot", &www]).await;
    assert_eq!(s.home_entries(), [".local", "arena-cron.log", "arena-proxy-cron.log"]);

    // Plain `remove`: the whole block goes; the user's entry stays.
    s.run(&["remove"]).await;
    assert_eq!(s.tab(), "0 9 * * * /usr/bin/keep-me\n");
}

/// CL1 end to end: an installed backup line logging to a folder someone chose (its lock in
/// another home) is re-installed with that log kept, its folder in the mkdir, the lock in
/// arena's folder — and nothing tidied: the chosen log, the line's old lock, and old-default
/// files sitting in this HOME all stay. Again: nothing to change.
#[cfg(unix)]
#[tokio::test]
async fn cron_install_keeps_a_chosen_log_and_tidies_nothing_of_it() {
    use arena_core::cron::{self, Job};
    let s = CronStub::new("kept");
    let home = s.home.clone();
    let logs = s.dir.join("srv/x/logs/cron").display().to_string();
    let other = s.dir.join("oldhome").display().to_string();
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let chosen = format!("{logs}/arena-cron.log");
    let line = format!(
        "*/15 * * * * ARENA_START_DATE=2026-10-05 flock -n -o {other}/.arena-backup-cron.lock /usr/local/bin/arena --config /c \
         pods backup --yes >> {chosen} 2>&1"
    );
    s.set_tab(&with_arena_block("", std::slice::from_ref(&line)));
    let mut old_files = vec![chosen.clone()];
    for d in [&home, &other] {
        for f in ["arena-cron.log", ".arena-backup-cron.lock"] {
            old_files.push(format!("{d}/{f}"));
        }
    }
    for f in &old_files {
        std::fs::write(f, "old\n").unwrap();
    }
    s.run(&["install", "--backup"]).await;
    let now = s.lines();
    assert_eq!(now.len(), 1, "{now:?}");
    assert!(
        now[0].starts_with(&format!(
            "*/15 * * * * mkdir -p {} {logs} && ARENA_START_DATE=2026-10-05 flock -n -o {} ",
            cron::dir(&home),
            cron::lock(&home, Job::Backup)
        )) && now[0].ends_with(&format!(" pods backup --yes >> {chosen} 2>&1")),
        "{now:?}"
    );
    for f in &old_files {
        assert_eq!(std::fs::read_to_string(f).unwrap(), "old\n", "{f} untouched");
    }
    assert!(!std::path::Path::new(&cron::log(&home, Job::Backup)).exists());
    s.run(&["install", "--backup"]).await;
    assert_eq!(s.lines(), now, "kept again: nothing to change");
}

/// CL2/CL5 end to end: `install` and `remove` refuse a HOME they can't use (unset, relative,
/// not a directory) and a config path a crontab line can't carry — before anything changes.
#[cfg(unix)]
#[tokio::test]
async fn cron_install_and_remove_refuse_a_home_or_config_a_line_cant_carry() {
    let mut s = CronStub::new("refuse");
    s.run(&["install", "--backup"]).await;
    let tab = s.tab();
    let file = s.dir.join("config.env").display().to_string();
    for home in [None, Some("home/dev"), Some(file.as_str())] {
        for args in [&["install", "--backup", "--no-pull"][..], &["remove"][..]] {
            let e = s.run_as(args, true, home).await.unwrap_err().to_string();
            assert!(e.starts_with("HOME") && e.contains("run as the user whose crontab this is"), "{home:?} {args:?}: {e}");
        }
    }
    s.config = s.dir.join("50%.env");
    std::fs::write(&s.config, "").unwrap();
    let e = s.run_as(&["install", "--backup", "--no-pull"], true, Some(&s.home)).await.unwrap_err().to_string();
    assert!(e.starts_with("the config path (") && e.contains("it holds % (U+0025)"), "{e}");
    assert_eq!(s.tab(), tab, "nothing changed");
}

/// `cron install --health` end to end (spec health-cron): its one line, no backup line brought
/// along; kept byte for byte by later installs of other lines — each flag names its own line,
/// so an install without `--health` keeps it (the spec's "dropped" predates the merge by
/// identity); replaced in place by a new time; dropped by `remove --health` alone; a time that
/// isn't `HH:MM` refused before anything is written.
#[cfg(unix)]
#[tokio::test]
async fn cron_health_line_is_a_line_of_its_own() {
    use super::{health_cron_line, CronKind, HealthAt};
    let s = CronStub::new("health");
    let kinds = |s: &CronStub| s.lines().iter().filter_map(|l| CronKind::of(l)).collect::<Vec<_>>();
    let e = s.run_as(&["install", "--health", "7:5"], true, Some(&s.home)).await.unwrap_err().to_string();
    assert!(e.starts_with("--health takes HH:MM"), "{e}");
    assert!(!s.dir.join("tab").exists(), "nothing written");
    s.run(&["install", "--health", "07:30"]).await;
    let root = crate::archive::cron_state_root(&arena_core::Config::default()).unwrap();
    let exe = std::env::current_exe().unwrap().display().to_string();
    let cfg = std::fs::canonicalize(&s.config).unwrap().display().to_string();
    let at = |hour, minute| health_cron_line(HealthAt { hour, minute }, &exe, &cfg, &s.home, &root, None);
    assert_eq!(s.lines(), [at(7, 30)]);
    s.run(&["install", "--proxy"]).await;
    s.run(&["install", "--backup", "--pull"]).await;
    assert_eq!(kinds(&s), [CronKind::Health, CronKind::Proxy, CronKind::Backup]);
    assert_eq!(s.lines()[0], at(7, 30), "kept byte for byte");
    s.run(&["install", "--health", "6:05"]).await;
    assert_eq!(kinds(&s), [CronKind::Health, CronKind::Proxy, CronKind::Backup]);
    assert_eq!(s.lines()[0], at(6, 5));
    s.run(&["remove", "--health"]).await;
    assert_eq!(kinds(&s), [CronKind::Proxy, CronKind::Backup]);
}

#[test]
fn cron_flags_parse() {
    use super::CronCmd;
    use crate::cli::{Cli, Cmd};
    use clap::Parser;
    let cron = |args: &[&str]| match Cli::try_parse_from(["arena", "cron"].iter().chain(args).copied()).unwrap().cmd {
        Cmd::Cron(c) => c,
        _ => unreachable!(),
    };
    assert!(matches!(
        cron(&["install"]),
        CronCmd::Install { backup: false, schedule: None, pull: false, proxy: false, snapshot: None, dry_run: false, .. }
    ));
    assert!(matches!(cron(&["install", "--proxy", "--dry-run"]), CronCmd::Install { proxy: true, dry_run: true, .. }));
    assert!(matches!(
        cron(&["install", "--backup", "--schedule", "0 * * * *"]),
        CronCmd::Install { backup: true, schedule: Some(s), .. } if s == "0 * * * *"
    ));
    assert!(matches!(
        cron(&["install", "--no-pull", "--no-start-date"]),
        CronCmd::Install { no_pull: true, no_start_date: true, pull: false, start_date: None, .. }
    ));
    // Each opposite pair is refused together.
    for bad in [&["install", "--pull", "--no-pull"][..], &["install", "--start-date", "2026-01-05", "--no-start-date"]] {
        assert!(Cli::try_parse_from(["arena", "cron"].iter().chain(bad).copied()).is_err(), "{bad:?}");
    }
    assert!(matches!(
        cron(&["remove"]),
        CronCmd::Remove { backup: false, proxy: false, snapshot: false, archive: false, reap: false, health: false, dry_run: false }
    ));
    assert!(matches!(cron(&["remove", "--health"]), CronCmd::Remove { health: true, backup: false, .. }));
    assert!(matches!(cron(&["install", "--health", "07:30"]), CronCmd::Install { health: Some(t), backup: false, .. } if t == "07:30"));
    assert!(matches!(cron(&["install"]), CronCmd::Install { health: None, .. }));
    assert!(matches!(cron(&["remove", "--snapshot", "--dry"]), CronCmd::Remove { snapshot: true, dry_run: true, .. }));
    assert_eq!(super::CronKind::selected(true, false, true, false), [super::CronKind::Backup, super::CronKind::Snapshot]);
    assert_eq!(super::CronKind::selected(false, false, false, true), [super::CronKind::ArchiveSnap, super::CronKind::ArchiveBig]);
}
