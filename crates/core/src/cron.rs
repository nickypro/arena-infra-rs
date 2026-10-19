//! Where arena's cron lines log and lock, wherever they're rendered: the CLI's `cron install`
//! lines (backup, proxy, snapshot, archive) and the reaper's (`reap::render::cron_line`). One
//! place for the folder and the names, so the lines, `cron install`'s tidy-up of the old files
//! and the docs can't drift apart.

/// The folder every arena cron line logs and locks in, relative to the home: `cron/` under
/// arena's default state root (`~/.local/state/arena`, [`crate::snapshot::state_dir`]'s
/// fallback), which already exists wherever arena has run — so the cron files add no entry to
/// the home. Always under the home the lines run in, never `ARENA_STATE_DIR`/`XDG_STATE_HOME`:
/// a fixed path keeps the lines simple.
pub const DIR: &str = ".local/state/arena/cron";

/// [`DIR`] in `home`.
pub fn dir(home: &str) -> String {
    format!("{home}/{DIR}")
}

/// What every line runs first, right after its schedule: `mkdir -p <dir> && `. cron's `sh`
/// doesn't run a command whose `>>` (or flock's lock file) points into a missing directory —
/// without this, deleting the folder would silently stop the backups.
pub fn mkdir(home: &str) -> String {
    format!("mkdir -p {} && ", dir(home))
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

    /// The log its line appended to before [`DIR`], directly in the home — the visible files
    /// `cron install` moves into the folder when it rewrites an old line.
    pub fn legacy_log(self) -> &'static str {
        match self {
            Job::Backup => "arena-cron.log",
            Job::Proxy => "arena-proxy-cron.log",
            Job::Snapshot => "arena-snapshot-cron.log",
            Job::Archive => "arena-archive-cron.log",
            Job::Reap => "arena-reap-cron.log",
        }
    }

    /// The lock its line took before [`DIR`], directly in the home (the archive's lines take
    /// none: each tier locks in-process).
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

/// `job`'s log in `home`'s [`DIR`].
pub fn log(home: &str, job: Job) -> String {
    format!("{}/{}.log", dir(home), job.name())
}

/// `job`'s `flock` file in `home`'s [`DIR`].
pub fn lock(home: &str, job: Job) -> String {
    format!("{}/{}.lock", dir(home), job.name())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every file is in the one folder under the state root — nothing new directly in the home.
    #[test]
    fn every_file_is_in_the_cron_folder() {
        assert_eq!(dir("/root"), "/root/.local/state/arena/cron");
        assert_eq!(mkdir("/root"), "mkdir -p /root/.local/state/arena/cron && ");
        let cases = [
            (Job::Backup, "/root/.local/state/arena/cron/backup.log", "/root/.local/state/arena/cron/backup.lock"),
            (Job::Proxy, "/root/.local/state/arena/cron/proxy.log", "/root/.local/state/arena/cron/proxy.lock"),
            (Job::Snapshot, "/root/.local/state/arena/cron/snapshot.log", "/root/.local/state/arena/cron/snapshot.lock"),
            (Job::Archive, "/root/.local/state/arena/cron/archive.log", "/root/.local/state/arena/cron/archive.lock"),
            (Job::Reap, "/root/.local/state/arena/cron/reap.log", "/root/.local/state/arena/cron/reap.lock"),
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
}
