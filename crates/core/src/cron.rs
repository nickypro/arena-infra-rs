//! Where arena's cron lines log and lock, wherever they're rendered: the CLI's `cron install`
//! lines (backup, proxy, snapshot, archive) and the reaper's (`reap::render::cron_line`). One
//! place for the folder, the names and the earlier versions' names, so the lines, `cron
//! install`'s tidy-up of the old files and the docs can't drift apart.
//!
//! arena doesn't rotate these logs: an operator who wants that adds a logrotate rule for
//! whichever folder the lines log to (`copytruncate`: each line holds its log open with `>>`).

/// The folder arena's cron lines lock in — and log in, unless a line already logs somewhere
/// someone chose ([`LogPlace::Chosen`], kept on a re-install) — relative to the home the lines
/// run in: a hidden folder under `~/.local/state`, so the cron files add no entry to the home.
/// Not under arena's own state root (`~/.local/state/arena/<prefix>/`), where a fleet whose
/// prefix is `cron` would share it. Never `ARENA_STATE_DIR`/`XDG_STATE_HOME`: a fixed path keeps
/// the lines simple.
pub const DIR: &str = ".local/state/arena-cron";

/// The folder the lines of a663f65 used, inside arena's state root: still read as arena's own
/// default, so a re-install moves those lines (and their files) to [`DIR`].
pub const PREV_DIR: &str = ".local/state/arena/cron";

/// [`DIR`] in `home`.
pub fn dir(home: &str) -> String {
    format!("{home}/{DIR}")
}

/// An arena cron job, by the files it writes in [`DIR`] (the archive's two lines share one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Backup,
    Proxy,
    Snapshot,
    Archive,
    Reap,
}

impl Job {
    pub const ALL: [Job; 5] = [Job::Backup, Job::Proxy, Job::Snapshot, Job::Archive, Job::Reap];

    /// The name its files go by in [`DIR`]: `backup.log`, `backup.lock`, ….
    pub fn name(self) -> &'static str {
        match self {
            Job::Backup => "backup",
            Job::Proxy => "proxy",
            Job::Snapshot => "snapshot",
            Job::Archive => "archive",
            Job::Reap => "reap",
        }
    }

    /// The log its line appended to before any folder, directly in the home (026c308 and
    /// earlier) — moved into [`DIR`] when `cron install` rewrites such a line.
    pub fn legacy_log(self) -> &'static str {
        match self {
            Job::Backup => "arena-cron.log",
            Job::Proxy => "arena-proxy-cron.log",
            Job::Snapshot => "arena-snapshot-cron.log",
            Job::Archive => "arena-archive-cron.log",
            Job::Reap => "arena-reap-cron.log",
        }
    }

    /// The lock its line took then, directly in the home (the archive's lines take none: each
    /// tier locks in-process).
    pub fn legacy_lock(self) -> Option<&'static str> {
        match self {
            Job::Backup => Some(".arena-backup-cron.lock"),
            Job::Proxy => Some(".arena-proxy-cron.lock"),
            Job::Snapshot => Some(".arena-snapshot-cron.lock"),
            Job::Archive => None,
            Job::Reap => Some(".arena-reap-cron.lock"),
        }
    }
}

/// `job`'s default log in `home`'s [`DIR`].
pub fn log(home: &str, job: Job) -> String {
    format!("{}/{}.log", dir(home), job.name())
}

/// `job`'s `flock` file in `home`'s [`DIR`] — always there, wherever the line logs.
pub fn lock(home: &str, job: Job) -> String {
    format!("{}/{}.lock", dir(home), job.name())
}

/// Where an installed line's log is, against arena's defaults for `home`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogPlace {
    /// Today's default ([`log`]).
    Default,
    /// A default an earlier version wrote — `~/arena-cron.log`, … (026c308) or
    /// `~/.local/state/arena/cron/<job>.log` (a663f65): a re-install moves the line to
    /// [`Default`](LogPlace::Default) and the old file into [`DIR`].
    OldDefault,
    /// Anything else: someone chose it (a folder logrotate rotates, say). Kept as it is.
    Chosen,
}

/// Where `path`, an installed `job` line's `>>` target, is — by the exact path, never by a
/// name alone: `/root/logs/cron/arena-cron.log` is someone's choice, not the old default.
pub fn log_place(home: &str, job: Job, path: &str) -> LogPlace {
    if path == log(home, job) {
        LogPlace::Default
    } else if path == format!("{home}/{}", job.legacy_log()) || path == format!("{home}/{PREV_DIR}/{}.log", job.name()) {
        LogPlace::OldDefault
    } else {
        LogPlace::Chosen
    }
}

/// Whether `path`, an installed `job` line's lock, is one an earlier default used in `home`
/// (`~/.arena-backup-cron.lock`, `~/.local/state/arena/cron/backup.lock`).
pub fn is_old_lock(home: &str, job: Job, path: &str) -> bool {
    job.legacy_lock().is_some_and(|l| path == format!("{home}/{l}")) || path == format!("{home}/{PREV_DIR}/{}.lock", job.name())
}

/// What every line runs first, right after its schedule: `mkdir -p <DIR> && `, plus the
/// folder of the line's `log` when that's elsewhere (a kept path) and a plain path with a
/// folder. cron's `sh` doesn't run a command whose `>>` (or flock's lock file) points into a
/// missing directory: without this, deleting either folder would silently stop the job.
pub fn mkdir(home: &str, log: &str) -> String {
    let dir = dir(home);
    let plain = log.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+@,:=~${}".contains(c));
    match log.rsplit_once('/').map(|(parent, _)| parent) {
        Some(parent) if plain && !parent.is_empty() && parent != dir => format!("mkdir -p {dir} {parent} && "),
        _ => format!("mkdir -p {dir} && "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The locks and the default logs are in the one hidden folder — nothing new directly in
    /// the home, and outside arena's per-prefix state root.
    #[test]
    fn every_default_file_is_in_the_cron_folder() {
        assert_eq!(dir("/root"), "/root/.local/state/arena-cron");
        let cases = [
            (Job::Backup, "/root/.local/state/arena-cron/backup.log", "/root/.local/state/arena-cron/backup.lock"),
            (Job::Proxy, "/root/.local/state/arena-cron/proxy.log", "/root/.local/state/arena-cron/proxy.lock"),
            (Job::Snapshot, "/root/.local/state/arena-cron/snapshot.log", "/root/.local/state/arena-cron/snapshot.lock"),
            (Job::Archive, "/root/.local/state/arena-cron/archive.log", "/root/.local/state/arena-cron/archive.lock"),
            (Job::Reap, "/root/.local/state/arena-cron/reap.log", "/root/.local/state/arena-cron/reap.lock"),
        ];
        assert_eq!(cases.map(|(j, _, _)| j), Job::ALL);
        for (job, want_log, want_lock) in cases {
            assert_eq!((log("/root", job).as_str(), lock("/root", job).as_str()), (want_log, want_lock), "{job:?}");
        }
        // The old names, directly in the home: visible logs, hidden locks (none for the archive).
        let legacy: Vec<(&str, Option<&str>)> = Job::ALL.iter().map(|j| (j.legacy_log(), j.legacy_lock())).collect();
        assert_eq!(
            legacy,
            [
                ("arena-cron.log", Some(".arena-backup-cron.lock")),
                ("arena-proxy-cron.log", Some(".arena-proxy-cron.lock")),
                ("arena-snapshot-cron.log", Some(".arena-snapshot-cron.lock")),
                ("arena-archive-cron.log", None),
                ("arena-reap-cron.log", Some(".arena-reap-cron.lock")),
            ]
        );
    }

    /// An installed line's log: today's default, an earlier default (exact paths only), or
    /// someone's choice — a folder of their own, another home, or merely the old name.
    #[test]
    fn a_log_path_is_an_old_default_only_exactly() {
        use LogPlace::*;
        let cases = [
            (Job::Backup, "/root/.local/state/arena-cron/backup.log", Default),
            (Job::Backup, "/root/arena-cron.log", OldDefault),
            (Job::Proxy, "/root/arena-proxy-cron.log", OldDefault),
            (Job::Reap, "/root/.local/state/arena/cron/reap.log", OldDefault),
            (Job::Backup, "/root/logs/cron/arena-cron.log", Chosen),
            (Job::Backup, "/home/dev/arena-cron.log", Chosen),
            (Job::Backup, "/root/arena-proxy-cron.log", Chosen),
            (Job::Backup, "~/arena-cron.log", Chosen),
            (Job::Archive, "/root/.local/state/arena-cron/backup.log", Chosen),
        ];
        for (job, path, want) in cases {
            assert_eq!(log_place("/root", job, path), want, "{job:?} {path}");
        }
        assert!(is_old_lock("/root", Job::Backup, "/root/.arena-backup-cron.lock"));
        assert!(is_old_lock("/root", Job::Snapshot, "/root/.local/state/arena/cron/snapshot.lock"));
        assert!(!is_old_lock("/root", Job::Backup, "/root/.arena-proxy-cron.lock"));
        assert!(!is_old_lock("/root", Job::Backup, "/root/.local/state/arena-cron/backup.lock"));
        assert!(!is_old_lock("/root", Job::Backup, "/srv/.arena-backup-cron.lock"));
    }

    /// The mkdir makes the cron folder and, for a kept log elsewhere, that log's folder too —
    /// only when it can be written as a plain word.
    #[test]
    fn the_mkdir_makes_every_folder_the_line_writes_into() {
        let cases = [
            ("/root/.local/state/arena-cron/backup.log", "mkdir -p /root/.local/state/arena-cron && "),
            ("/root/logs/cron/arena-cron.log", "mkdir -p /root/.local/state/arena-cron /root/logs/cron && "),
            ("~/logs/arena-cron.log", "mkdir -p /root/.local/state/arena-cron ~/logs && "),
            ("arena-cron.log", "mkdir -p /root/.local/state/arena-cron && "),
            ("/arena-cron.log", "mkdir -p /root/.local/state/arena-cron && "),
            ("'/srv/logs/x.log'", "mkdir -p /root/.local/state/arena-cron && "),
        ];
        for (log, want) in cases {
            assert_eq!(mkdir("/root", log), want, "{log}");
        }
    }
}
