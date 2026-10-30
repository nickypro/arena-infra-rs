//! `arena backups`: what gets backed up, where and when, and how it's doing — the status model
//! (`arena_core::backups`) printed, or as JSON; the dashboard's backups screen (`b`) draws the same.
//! And the run record behind it (`backups.json`), written here for the git backup and the home
//! copy: best effort — a record that can't be written is a warning, never a failed backup.

use std::path::{Path, PathBuf};

use anyhow::Result;
use arena_core::backups::record::{self, Event};
use arena_core::backups::{self, Job};
use arena_core::Config;

/// The run record's file, or `None` where it isn't written: in this crate's unit tests unless
/// the test's config names a state dir (a test must never write the developer's own record, as
/// [`crate::disk`]'s skips).
fn record_path(cfg: &Config) -> Option<Result<PathBuf, String>> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    Some(arena_core::snapshot::state_file_path_for(cfg, record::RECORD_FILE))
}

/// Change the record at `path`, saying (on stderr) what went wrong if anything did.
fn write(path: &Path, f: impl FnOnce(&mut record::Record)) {
    match record::update(path, arena_core::disk::RECORD_LOCK_WAIT, f) {
        Ok(warning) => warning.iter().for_each(|w| eprintln!("{w}")),
        Err(e) => eprintln!("warning: couldn't write the backups record {} ({e}) — `arena backups` won't show this run", path.display()),
    }
}

/// What a run counts for in the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    /// No pods named, as the cron's run: the job's run (its last run, when it's next due, late),
    /// and each pod's result.
    Fleet,
    /// Pods named: their results alone — `pods backup apple` by hand never stands in for the cron.
    Named,
    /// Not the configured backup (another `--dir`, `--no-big`, `--no-git`, a label or a remote
    /// path of its own): the pods' histories alone, the words saying how — so it never reads as a
    /// fresh backup (review r1 #1).
    Aside(String),
}

/// A run of a backup job, recorded as it starts (so one killed reads as interrupted) and as it
/// ends ([`Run::finish`]), as its [`Scope`] says.
pub(crate) struct Run {
    job: Job,
    path: Option<PathBuf>,
    scope: Scope,
}

impl Run {
    /// Note that a run of `job` over `pods` pods starts now.
    pub(crate) fn start(cfg: &Config, job: Job, pods: usize, scope: Scope) -> Run {
        let path = match record_path(cfg) {
            None => None,
            Some(Ok(p)) => Some(p),
            Some(Err(e)) => {
                eprintln!("warning: this {} isn't recorded for `arena backups`: {e}", job.name());
                None
            }
        };
        if let (Some(p), Scope::Fleet) = (&path, &scope) {
            let now = arena_core::snapshot::unix_now();
            write(p, |r| r.start(job, std::process::id(), pods as u32, now));
        }
        Run { job, path, scope }
    }

    /// The run ended: each pod's event (stamped when that pod's backup ended), and the run in a
    /// few words.
    pub(crate) fn finish(self, events: Vec<(String, Event)>, summary: String) {
        let Some(p) = &self.path else { return };
        let now = arena_core::snapshot::unix_now();
        match self.scope {
            Scope::Fleet => write(p, |r| r.finish(self.job, std::process::id(), now, events, summary)),
            _ if events.is_empty() => {}
            Scope::Named => write(p, |r| r.add_events(events)),
            Scope::Aside(how) => {
                let events = events.into_iter().map(|(pod, mut e)| {
                    e.detail = if e.detail.is_empty() { format!("not the configured backup ({how})") } else { format!("not the configured backup ({how}): {}", e.detail) };
                    (pod, e)
                });
                write(p, |r| r.add_history(events.collect()));
            }
        }
    }
}

/// Record events for pods outside a run (a pod a run left out because another backup held its
/// lock).
pub(crate) fn note_pods(cfg: &Config, events: Vec<(String, Event)>) {
    if events.is_empty() {
        return;
    }
    if let Some(Ok(p)) = record_path(cfg) {
        write(&p, |r| r.add_events(events));
    }
}

/// An event now.
pub(crate) fn event(job: Job, outcome: record::Outcome, detail: impl Into<String>) -> Event {
    Event::new(arena_core::snapshot::unix_now(), job, outcome, detail)
}

/// The run's scope for a selection: fleet-wide unless pods were named.
pub(crate) fn scope_of(named: bool) -> Scope {
    if named {
        Scope::Named
    } else {
        Scope::Fleet
    }
}

/// `arena backups [--json]`: read what's there (the crontab, the record, the backup dir, the
/// archive's state, the locks, the filesystems — nothing on a pod or a provider), judge it, print
/// it. `crontab`: the `crontab -l` output (`arena_core::backups::read_crontab` for real). Exits 0:
/// it's a look (the text says what's failing).
pub(crate) fn handle_backups(cfg: &Config, config_path: &Path, json: bool, crontab: Result<String, String>) -> Result<()> {
    let now = arena_core::snapshot::unix_now();
    let config = std::path::absolute(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    let inputs = backups::read(cfg, Some(&config.display().to_string()), crontab, backups::cohort_pods(cfg), now);
    let status = backups::judge(&inputs);
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print!("{}", backups::render::text(&status));
        println!("(record: {})", inputs.record_path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arena_core::backups::record::{Outcome, Record};
    use arena_core::backups::{self, Health, Job};
    use arena_core::remote::{FakeRemote, FakeReply};
    use arena_core::{Config, Pod};

    use crate::{handle_full_backup, Selected};

    fn pod(name: &str, port: u16) -> Pod {
        Pod {
            id: format!("id-devtest-{name}"),
            name: format!("devtest-{name}"),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            ssh_ip: Some("10.0.0.1".into()),
            ssh_port: Some(port),
            ..Default::default()
        }
    }

    /// A real `pods backup` (scripted pods, a stand-in rsync) leaves its record — the git run and
    /// the copy run, each pod's outcome — and `arena backups` judges and says it: apple pushed and
    /// copied, bloom's push and copy failed with their reasons, failing first.
    #[tokio::test]
    async fn a_backup_run_is_recorded_and_arena_backups_says_it() {
        let base = std::env::temp_dir().join(format!("arena-cli-backups-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("backups")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        arena_core::remote::write_test_script(
            base.join("rsync"),
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\n\
             case \"$last\" in *devtest-bloom*) echo 'rsync: connection unexpectedly closed' >&2; exit 12;; esac\n\
             mkdir -p \"$last/ARENA_materials\" && echo work > \"$last/ARENA_materials/notes.py\"\n",
        )
        .unwrap();
        let cfg = Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom)\nBACKUP_REPO_PATH=/root/ARENA_materials\n\
             SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\nLOCAL_BACKUP_DIR={}\nARENA_STATE_DIR={}\n",
            base.join("backups").display(),
            base.join("state").display()
        ));
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22001", [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")]);
        fake.script("10.0.0.1:22002", [FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22: Connection timed out")]);
        let sel = Selected::all(vec![pod("apple", 22001), pod("bloom", 22002)]);
        let rsync = base.join("rsync").display().to_string();
        let err = handle_full_backup(fake, &cfg, &sel, false, None, false, true, &rsync).await.unwrap_err();
        assert!(err.to_string().contains("1 pod(s) failed to back up"), "{err}");

        let rec = Record::load(&base.join("state/devtest/backups.json")).unwrap().expect("a record");
        assert!(rec.running.is_empty(), "both runs finished");
        let git = &rec.runs[&Job::Git];
        assert_eq!((git.ok, git.failed.as_slice(), git.summary.as_str()), (false, &["devtest-bloom".to_string()][..], "1 pushed, 0 no changes, 0 on main/master, 1 failed"));
        let copy = &rec.runs[&Job::Copy];
        assert_eq!((copy.ok, copy.failed.as_slice()), (false, &["devtest-bloom".to_string()][..]));
        let (w, d) = crate::resolve_week_day(&cfg, None, None).unwrap();
        let apple = &rec.pods["devtest-apple"];
        assert_eq!(apple.git.as_ref().map(|e| (e.outcome, e.detail.as_str())), Some((Outcome::Pushed, "→ autocommit-a")));
        let c = apple.copy.as_ref().unwrap();
        assert_eq!(c.outcome, Outcome::Copied);
        assert!(c.detail.starts_with(&format!("w{w}d{d}: ")) && c.detail.ends_with(" new"), "{}", c.detail);
        assert!(c.bytes.is_some_and(|b| b > 0), "{:?}", c.bytes);
        let bloom = &rec.pods["devtest-bloom"];
        assert_eq!(bloom.git.as_ref().map(|e| (e.outcome, e.detail.as_str())), Some((Outcome::Failed, "exit 255: ssh: connect to host 10.0.0.1 port 22: Connection timed out")));
        let c = bloom.copy.as_ref().unwrap();
        assert_eq!(c.outcome, Outcome::Failed);
        assert!(c.detail.contains("rsync exit 12"), "{}", c.detail);

        // `arena backups`: the cron line runs it with pulls; bloom first, failing; apple ok.
        let tab = format!(
            "{}\n*/15 * * * * flock -n -o {}/backup.lock /a --config /c pods backup --yes >> {}/backup.log 2>&1\n{}\n",
            arena_core::cron::BEGIN,
            base.display(),
            base.display(),
            arena_core::cron::END
        );
        let now = arena_core::snapshot::unix_now();
        let s = backups::judge(&backups::read(&cfg, Some("/c"), Ok(tab.clone()), backups::cohort_pods(&cfg), now));
        assert_eq!(s.pods.iter().map(|p| (p.name.as_str(), p.health)).collect::<Vec<_>>(), [("devtest-bloom", Health::Fail), ("devtest-apple", Health::Ok)]);
        assert_eq!(s.jobs[0].why.as_deref(), Some("1 failed: devtest-bloom"));
        let text = backups::render::text(&s);
        for want in ["git backup          every 15 min", "home copy           with each git backup (every 15 min)", "└ git: exit 255: ssh: connect to host", "✓ pushed now"] {
            assert!(text.contains(want), "no {want:?} in\n{text}");
        }
        // The command itself, text and JSON.
        super::handle_backups(&cfg, std::path::Path::new("/c"), false, Ok(tab.clone())).unwrap();
        super::handle_backups(&cfg, std::path::Path::new("/c"), true, Ok(tab)).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A pull that isn't the configured backup — another `--dir`, `--no-big` — goes into the pods'
    /// histories alone, saying how: never a fresh home copy, never the job's run (review r1 #1).
    /// The configured one, pods named, is the pods' results — not the job's run either.
    #[tokio::test]
    async fn a_copy_aside_is_history_only_and_a_named_one_never_the_jobs_run() {
        let base = std::env::temp_dir().join(format!("arena-cli-backups-aside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("backups")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        arena_core::remote::write_test_script(base.join("rsync"), "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\nmkdir -p \"$last\" && echo w > \"$last/f\"\n").unwrap();
        let cfg = Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
             LOCAL_BACKUP_DIR={}\nARENA_STATE_DIR={}\n",
            base.join("backups").display(),
            base.join("state").display()
        ));
        let remote: Arc<dyn arena_core::remote::Remote> = Arc::new(FakeRemote::new());
        let rsync = base.join("rsync").display().to_string();
        let all = Selected::all(vec![pod("apple", 22001)]);
        let load = || Record::load(&base.join("state/devtest/backups.json")).unwrap().unwrap();
        let elsewhere = base.join("elsewhere").display().to_string();
        let pull = |dir: String, no_big: bool, sel: Selected| {
            let (remote, cfg, rsync) = (remote.clone(), cfg.clone(), rsync.clone());
            async move { crate::pods::pull::handle_pull(&remote, &cfg, None, &dir, None, None, false, no_big, &sel, false, true, &rsync).await.unwrap() }
        };
        pull(elsewhere.clone(), false, all.clone()).await;
        let rec = load();
        let apple = &rec.pods["devtest-apple"];
        assert!(rec.runs.is_empty() && apple.copy.is_none() && apple.copy_ok.is_none(), "{rec:?}");
        assert!(apple.history[0].detail.starts_with(&format!("not the configured backup (into {elsewhere}): w")), "{:?}", apple.history);
        let backups_dir = base.join("backups").display().to_string();
        pull(backups_dir.clone(), true, all.clone()).await;
        let rec = load();
        assert!(rec.runs.is_empty() && rec.pods["devtest-apple"].copy.is_none());
        assert!(rec.pods["devtest-apple"].history[0].detail.starts_with("not the configured backup (--no-big)"));
        // Named: the pod's copy, not the job's run.
        pull(backups_dir.clone(), false, Selected { pods: all.pods.clone(), named: true }).await;
        let rec = load();
        assert!(rec.runs.is_empty());
        assert_eq!(rec.pods["devtest-apple"].copy.as_ref().map(|e| e.outcome), Some(Outcome::Copied));
        // The configured backup, fleet-wide: the job's run too.
        pull(backups_dir, false, all).await;
        assert_eq!(load().runs[&Job::Copy].summary.split(" (").next(), Some("1 copied, 0 failed"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Without a state dir named, this crate's tests never write the developer's own record.
    #[test]
    fn tests_write_no_record_without_a_state_dir() {
        assert!(super::record_path(&Config::parse("MACHINE_NAME_PREFIX=devtest\n")).is_none());
        assert!(super::record_path(&Config::parse("ARENA_STATE_DIR=/tmp/x\n")).is_some());
    }
}
