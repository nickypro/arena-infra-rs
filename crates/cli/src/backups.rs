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
                    e.aside = true;
                    (pod, e)
                });
                write(p, |r| r.add_history(events.collect()));
            }
        }
    }
}

/// Pods a run of `job` left out because another backup held their lock: their results — and, for
/// a fleet-wide run (`named` false), the run's failures too ([`record::Record::amend`]).
pub(crate) fn note_left_out(cfg: &Config, job: Job, events: Vec<(String, Event)>, named: bool) {
    if events.is_empty() {
        return;
    }
    if let Some(Ok(p)) = record_path(cfg) {
        if named {
            write(&p, |r| r.add_events(events));
        } else {
            write(&p, |r| r.amend(job, std::process::id(), events));
        }
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

/// The machines `arena backups` is about: the fleet's listing (read-only, bounded) — each live
/// machine, as the dashboard's `b` has them — and this cohort's configured names that have none,
/// for one line. A fleet that can't be built or a provider that didn't answer: every configured
/// name instead (a `?` row for one with no machine), and a note saying why.
pub(crate) async fn backup_pods(cfg: &Config, fleet: std::result::Result<&dyn arena_core::Provider, String>) -> (Vec<backups::PodIn>, Vec<String>, Option<String>) {
    let configured = |why: String| (backups::cohort_pods(cfg), Vec::new(), Some(why));
    let provider = match fleet {
        Ok(p) => p,
        Err(e) => return configured(format!("the fleet couldn't be listed ({e}): every configured name is shown, `?` for one with no machine")),
    };
    let listing = match arena_core::ratelimit::bounded(arena_core::provider::LIST_TIMEOUT, provider.list_by_provider()).await {
        Ok(r) => arena_core::proxy::Listing::from_results(r),
        Err(_) => return configured("the fleet didn't list in time: every configured name is shown, `?` for one with no machine".into()),
    };
    let failed: Vec<String> = listing.errors().iter().map(|(p, e)| format!("{p}: {e}")).collect();
    if !failed.is_empty() {
        return configured(format!("not every provider listed ({}): every configured name is shown, `?` for one with no machine", failed.join("; ")));
    }
    let naming = arena_core::selector::Naming::from_config(cfg);
    let mut pods = listing.pods();
    pods.sort_by(|a, b| a.name.cmp(&b.name));
    let live = backups::listed_pods(&pods, &naming, |_| None);
    let without = backups::without_machine(cfg, &live);
    (live, without, None)
}

/// `arena backups [--json]`: list the fleet (read-only: which machines there are) and read what's
/// on this box (the crontab, the record, the backup dir, the archive's state, the locks, the
/// filesystems — nothing on a pod), judge it, print it. `crontab`: the `crontab -l` output
/// (`arena_core::backups::read_crontab` for real); `fleet`: the providers, or why there are
/// none ([`backup_pods`]). Exits 0: it's a look (the text says what's failing).
pub(crate) async fn handle_backups(
    cfg: &Config,
    config_path: &Path,
    json: bool,
    crontab: Result<String, String>,
    fleet: std::result::Result<&dyn arena_core::Provider, String>,
) -> Result<()> {
    let (pods, without, note) = backup_pods(cfg, fleet).await;
    let now = arena_core::snapshot::unix_now();
    let config = std::path::absolute(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    let inputs = backups::read(cfg, Some(&config.display().to_string()), crontab, pods, now);
    let mut status = backups::judge(&inputs);
    status.without_machine = without;
    status.notes.extend(note);
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
        super::handle_backups(&cfg, std::path::Path::new("/c"), false, Ok(tab.clone()), Err("no providers".into())).await.unwrap();
        super::handle_backups(&cfg, std::path::Path::new("/c"), true, Ok(tab), Err("no providers".into())).await.unwrap();
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
        // A size cap other than the configured one keeps other files: aside too (review r2 #1).
        let (r2, c2, s2) = (remote.clone(), cfg.clone(), rsync.clone());
        crate::pods::pull::handle_pull(&r2, &c2, None, &backups_dir, Some("1M".into()), None, false, false, &all, false, true, &s2).await.unwrap();
        let rec = load();
        assert!(rec.runs.is_empty() && rec.pods["devtest-apple"].copy.is_none());
        assert!(rec.pods["devtest-apple"].history[0].aside && rec.pods["devtest-apple"].history[0].detail.starts_with("not the configured backup (--max-size 1M)"));
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

    /// A pod another backup held the lock of is a failure of the fleet's git run, as the run's
    /// exit is (review r2 #3): never an OK run beside a pod that wasn't backed up.
    #[tokio::test]
    async fn a_pod_left_out_for_its_lock_fails_the_recorded_run() {
        let base = std::env::temp_dir().join(format!("arena-cli-backups-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let cfg = Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom)\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_STATE_DIR={}\n",
            base.join("state").display()
        ));
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22001", [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")]);
        let mut locks = crate::PodLocks { busy: vec!["devtest-bloom".into()], ..Default::default() };
        let sel = Selected::all(vec![pod("apple", 22001)]);
        crate::handle_backup(fake, &cfg, true, None, &sel, &mut locks, &mut Vec::new()).await.unwrap();
        let rec = Record::load(&base.join("state/devtest/backups.json")).unwrap().unwrap();
        let run = &rec.runs[&Job::Git];
        assert_eq!((run.ok, run.failed.as_slice(), run.summary.as_str()), (false, &["devtest-bloom".to_string()][..], "1 pushed, 0 no changes, 0 on main/master, 1 failed"));
        assert!(rec.pods["devtest-bloom"].git.as_ref().is_some_and(|e| e.outcome == Outcome::Failed && e.detail.starts_with("busy: another backup of devtest-bloom")));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Every pod left out for its lock: nothing ran, and the record says the run failed — not the
    /// previous good run (review r3 #2); under `--no-pull` the copy job is left alone.
    #[test]
    fn a_run_with_every_pod_left_out_is_a_failed_run() {
        let base = std::env::temp_dir().join(format!("arena-cli-backups-allbusy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR={}\n", base.join("state").display()));
        let busy = |job| vec![("devtest-bloom".to_string(), super::event(job, Outcome::Failed, "busy: another backup of devtest-bloom is running"))];
        // A good run before.
        super::Run::start(&cfg, Job::Git, 1, super::Scope::Fleet).finish(vec![("devtest-bloom".into(), super::event(Job::Git, Outcome::Pushed, ""))], "1 pushed".into());
        crate::pods::backup::record_all_left_out(&cfg, false, true, busy(Job::Git), busy(Job::Copy));
        let rec = Record::load(&base.join("state/devtest/backups.json")).unwrap().unwrap();
        let run = &rec.runs[&Job::Git];
        assert_eq!((run.ok, run.failed.as_slice(), run.summary.as_str()), (false, &["devtest-bloom".to_string()][..], "nothing done: 1 left out (another backup held its lock)"));
        assert!(!rec.runs.contains_key(&Job::Copy), "--no-pull: no copy run");
        crate::pods::backup::record_all_left_out(&cfg, false, false, busy(Job::Git), busy(Job::Copy));
        let rec = Record::load(&base.join("state/devtest/backups.json")).unwrap().unwrap();
        assert!(!rec.runs[&Job::Copy].ok);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Without a state dir named, this crate's tests never write the developer's own record.
    #[test]
    fn tests_write_no_record_without_a_state_dir() {
        assert!(super::record_path(&Config::parse("MACHINE_NAME_PREFIX=devtest\n")).is_none());
        assert!(super::record_path(&Config::parse("ARENA_STATE_DIR=/tmp/x\n")).is_some());
    }

    /// The listing `arena backups` reads: each provider's pods, or its error.
    struct Listed(Vec<(&'static str, Result<Vec<Pod>, &'static str>)>);

    #[async_trait::async_trait]
    impl arena_core::Provider for Listed {
        fn name(&self) -> &'static str {
            "hetzner"
        }
        fn describe(&self, _spec: &arena_core::PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
            unreachable!("backups lists by provider")
        }
        async fn list_by_provider(&self) -> Vec<(String, arena_core::Result<Vec<Pod>>)> {
            self.0.iter().map(|(n, r)| (n.to_string(), r.clone().map_err(arena_core::Error::provider))).collect()
        }
        async fn create_pod(&self, _spec: &arena_core::PodSpec) -> arena_core::Result<Pod> {
            panic!("backups must not create")
        }
        async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("backups must not stop")
        }
        async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("backups must not restart")
        }
        async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
            panic!("backups must not terminate")
        }
    }

    /// `arena backups` is about the live machines, as the dashboard's `b`: a configured name with
    /// no machine is counted on one line, not a `?` row at the top. Without a whole listing (no
    /// fleet, a provider down) it shows every configured name as before and says why.
    #[tokio::test]
    async fn arena_backups_shows_live_machines_and_counts_names_without_one() {
        let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cedar)\n");
        let hz = |name: &str| Pod { id: name.into(), name: format!("devtest-{name}"), provider: "hetzner".into(), status: "RUNNING".into(), ..Default::default() };
        let fleet = Listed(vec![("hetzner", Ok(vec![hz("bloom")])), ("runpod", Ok(vec![]))]);
        let (pods, without, note) = super::backup_pods(&cfg, Ok(&fleet)).await;
        assert_eq!(pods.iter().map(|p| (p.name.as_str(), p.up)).collect::<Vec<_>>(), [("devtest-bloom", Some(true))]);
        assert_eq!(without, ["devtest-apple", "devtest-cedar"]);
        assert_eq!(note, None);
        let mut s = backups::judge(&backups::read(&cfg, Some("/c"), Ok(String::new()), pods, arena_core::snapshot::unix_now()));
        s.without_machine = without;
        let text = backups::render::text(&s);
        assert!(text.contains("2 configured names have no machine now: devtest-apple, devtest-cedar"), "{text}");
        assert!(!text.contains("devtest-apple  ?"), "{text}");
        // A provider down: the configured names, as before, and why.
        let down = Listed(vec![("hetzner", Ok(vec![hz("bloom")])), ("runpod", Err("HTTP 503"))]);
        let (pods, without, note) = super::backup_pods(&cfg, Ok(&down)).await;
        assert_eq!(pods.len(), 3);
        assert!(without.is_empty());
        assert!(note.as_deref().is_some_and(|n| n.starts_with("not every provider listed (runpod: provider error: HTTP 503)")), "{note:?}");
        let (pods, _, note) = super::backup_pods(&cfg, Err("no provider key set".into())).await;
        assert_eq!(pods.len(), 3);
        assert!(note.as_deref().is_some_and(|n| n.contains("no provider key set")), "{note:?}");
    }
}
