//! Where arena's cron lines log and lock, wherever they're rendered: the CLI's `cron install`
//! lines (backup, proxy, snapshot, archive, health) and the reaper's (`reap::render::cron_line`). One
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
    Health,
}

impl Job {
    pub const ALL: [Job; 6] = [Job::Backup, Job::Proxy, Job::Snapshot, Job::Archive, Job::Reap, Job::Health];

    /// The name its files go by in [`DIR`]: `backup.log`, `backup.lock`, ….
    pub fn name(self) -> &'static str {
        match self {
            Job::Backup => "backup",
            Job::Proxy => "proxy",
            Job::Snapshot => "snapshot",
            Job::Archive => "archive",
            Job::Reap => "reap",
            Job::Health => "health",
        }
    }

    /// The log its line appended to before any folder, directly in the home (026c308 and
    /// earlier) — moved into [`DIR`] when `cron install` rewrites such a line. `None`: a job
    /// newer than both earlier layouts (the health check), whose line has only ever logged in
    /// [`DIR`] or where someone chose.
    pub fn legacy_log(self) -> Option<&'static str> {
        match self {
            Job::Backup => Some("arena-cron.log"),
            Job::Proxy => Some("arena-proxy-cron.log"),
            Job::Snapshot => Some("arena-snapshot-cron.log"),
            Job::Archive => Some("arena-archive-cron.log"),
            Job::Reap => Some("arena-reap-cron.log"),
            Job::Health => None,
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
            Job::Health => None,
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
/// name alone: `/root/logs/cron/arena-cron.log` is someone's choice, not the old default. A job
/// no earlier layout had ([`Job::legacy_log`] `None`) has no old default: its file in
/// [`PREV_DIR`] would be someone's, so it is kept, never moved.
pub fn log_place(home: &str, job: Job, path: &str) -> LogPlace {
    let old = |legacy: &str| path == format!("{home}/{legacy}") || path == format!("{home}/{PREV_DIR}/{}.log", job.name());
    if path == log(home, job) {
        LogPlace::Default
    } else if job.legacy_log().is_some_and(old) {
        LogPlace::OldDefault
    } else {
        LogPlace::Chosen
    }
}

/// Whether `path`, an installed `job` line's lock, is one an earlier default used in `home`
/// (`~/.arena-backup-cron.lock`, `~/.local/state/arena/cron/backup.lock`) — never for a job no
/// earlier layout had, as [`log_place`].
pub fn is_old_lock(home: &str, job: Job, path: &str) -> bool {
    job.legacy_lock().is_some_and(|l| path == format!("{home}/{l}"))
        || (job.legacy_log().is_some() && path == format!("{home}/{PREV_DIR}/{}.lock", job.name()))
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

// ---------------------------------------------------------------------------------------
// Reading a crontab's arena lines (`cron install`/`remove`/`show`, `teardown --check`, and the
// backups' status: `crate::backups`)
// ---------------------------------------------------------------------------------------

/// The first line of the block `cron install` manages in a crontab.
pub const BEGIN: &str = "# >>> arena-infra-rs >>>";
/// Its last line.
pub const END: &str = "# <<< arena-infra-rs <<<";

/// The crontab's arena lines: every line inside arena's managed block(s) — what `cron remove`
/// clears — and every job line outside them that mentions arena anyway (hand-added, so `cron
/// remove` won't touch it). Pure.
pub fn arena_lines(crontab: &str) -> crate::teardown::CronLines {
    let mut out = crate::teardown::CronLines::default();
    let mut in_block = false;
    for line in crontab.lines() {
        match line.trim() {
            BEGIN => in_block = true,
            END => in_block = false,
            "" => {}
            _ if in_block => out.managed.push(line.to_string()),
            _ if crate::teardown::cron_line_is_arena_job(line) => out.unmanaged.push(line.to_string()),
            _ => {}
        }
    }
    out
}

/// Which arena-managed crontab line a line is. Each has a stable identity, so `cron install`
/// replaces only the lines its flags name and `cron remove --<kind>` drops only those — and the
/// backups' status says which of the backup lines are there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// `pods backup` (git, and without `--no-pull` the rsync file backup).
    Backup,
    /// `proxy apply --yes`, the proxy re-sync.
    Proxy,
    /// `snapshot --public --out …`, the dashboard JSON.
    Snapshot,
    /// `archive sync --tier snap`, the archive's snapshot push.
    ArchiveSnap,
    /// `archive sync --tier big`, the archive's all-files push.
    ArchiveBig,
    /// `pods reap run`, the opt-in auto-reaper.
    Reap,
    /// `pods test --deep`, the daily health check.
    Health,
}

impl Kind {
    /// The line's name in `cron show` and the notes.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Backup => "backup",
            Kind::Proxy => "proxy",
            Kind::Snapshot => "snapshot",
            Kind::ArchiveSnap => "archive-snap",
            Kind::ArchiveBig => "archive-big",
            Kind::Reap => "reap",
            Kind::Health => "health",
        }
    }

    /// The job whose files in the cron folder the line writes (both archive lines: the archive's).
    pub fn job(self) -> Job {
        match self {
            Kind::Backup => Job::Backup,
            Kind::Proxy => Job::Proxy,
            Kind::Snapshot => Job::Snapshot,
            Kind::ArchiveSnap | Kind::ArchiveBig => Job::Archive,
            Kind::Reap => Job::Reap,
            Kind::Health => Job::Health,
        }
    }

    /// The kind of a crontab line, by the arena subcommand it runs — whole words, so the
    /// lock/log file names (`cron/proxy.lock`, an older `.arena-proxy-cron.lock`) and the
    /// leading `mkdir -p <dir> &&` never count: `pods backup`, `proxy apply`, `snapshot`. Read
    /// from the command itself rather than from a tag we add, so the lines every earlier `cron
    /// install` wrote are recognized too. A line that matches none — or more than one (a
    /// hand-edited `a && b`) — is `None`: kept as it is by a targeted install/remove, never
    /// guessed at. So is any comment: a paused (commented-out) backup line isn't a backup, and
    /// an operator's note that mentions a snapshot isn't the snapshot line — neither is
    /// replaced or removed by a targeted install/remove. Pure.
    pub fn of(line: &str) -> Option<Kind> {
        if line.trim_start().starts_with('#') {
            return None;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let runs = |a: &str, b: Option<&str>| {
            words.iter().enumerate().any(|(i, w)| *w == a && b.is_none_or(|b| words.get(i + 1) == Some(&b)))
        };
        let found: Vec<Kind> = [
            (runs("pods", Some("backup")), Kind::Backup),
            (runs("proxy", Some("apply")), Kind::Proxy),
            (runs("snapshot", None), Kind::Snapshot),
            (runs("archive", Some("sync")) && runs("--tier", Some("snap")), Kind::ArchiveSnap),
            (runs("archive", Some("sync")) && runs("--tier", Some("big")), Kind::ArchiveBig),
            (runs("reap", Some("run")), Kind::Reap),
            (runs("pods", Some("test")) && runs("--deep", None), Kind::Health),
        ]
        .into_iter()
        .filter_map(|(hit, k)| hit.then_some(k))
        .collect();
        match found.as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    }
}

/// A crontab line's schedule (five fields, or one `@…` macro) and its command's words — after
/// the `mkdir -p <dir>… &&` arena's lines start with (their folders, recreated), so the inline
/// env assignments before the command (`ARENA_START_DATE=…`) are read the same from every
/// version's line. `None`: no schedule, or nothing after it. Pure.
pub fn line_parts(line: &str) -> Option<(String, Vec<&str>)> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let n = if words.first()?.starts_with('@') { 1 } else { 5 };
    let command = words.get(n..)?;
    let command = match command {
        [] => return None,
        ["mkdir", "-p", dirs @ ..] => match dirs.iter().position(|w| *w == "&&") {
            Some(i) if i > 0 => &dirs[i + 1..],
            _ => command,
        },
        _ => command,
    };
    Some((words[..n].join(" "), command.to_vec()))
}

/// A crontab line's log: `PATH` when the line ends in an append of its output to one file —
/// `>> PATH 2>&1` as arena's lines do, or a hand edit's `>>PATH 2>&1`, `>> PATH` or `>>PATH`.
/// `None` for any other ending — none, `> PATH`, `2>>ERR`, a comment after it, a quoted path
/// with a space. Pure.
pub fn log_target(line: &str) -> Option<&str> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let words = words.strip_suffix(&["2>&1"]).unwrap_or(&words);
    let path = match words {
        [.., ">>", path] => *path,
        [.., last] => last.strip_prefix(">>")?,
        [] => return None,
    };
    Some(path).filter(|p| !p.is_empty() && !p.starts_with(['>', '&']))
}

/// A crontab line's `flock` file: the word after `flock` and its `-…` options. Pure.
pub fn lock_target(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace().skip_while(|w| *w != "flock").skip(1);
    words.find(|w| !w.starts_with('-'))
}

/// The config a line runs arena with: the word after `--config`. Pure.
pub fn config_target(line: &str) -> Option<&str> {
    let words: Vec<&str> = line.split_whitespace().collect();
    words.iter().position(|w| *w == "--config").and_then(|i| words.get(i + 1).copied())
}

// ---------------------------------------------------------------------------------------
// Schedules: when a line runs next, and how often
// ---------------------------------------------------------------------------------------

/// A crontab schedule — five fields (minute, hour, day of month, month, day of week) or an `@…`
/// macro — as cron reads it: `*`, `*/n`, `a`, `a-b`, `a-b/n`, `a/n` (from `a` to the field's
/// end), lists of those, month and day names (`jan`, `mon`), Sunday as `0` or `7`; with both day
/// fields restricted, a day matching either runs (Vixie cron's rule). Only what the backups'
/// status needs: when it runs next ([`Schedule::next_after`]), its longest gap
/// ([`Schedule::longest_gap`]) and how often, in words ([`Schedule::describe`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    /// As written.
    pub text: String,
    minutes: u64,
    hours: u32,
    doms: u32,
    months: u16,
    dows: u8,
    dom_any: bool,
    dow_any: bool,
    /// `@reboot`: at boot only — no next time.
    reboot: bool,
}

/// One field's bits, read from its text (`lo..=hi` the field's range; `names` its words).
fn field_bits(text: &str, lo: u32, hi: u32, names: &[&str]) -> Result<u64, String> {
    let value = |s: &str| -> Result<u32, String> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Ok(i as u32 + if lo == 1 { 1 } else { 0 });
        }
        s.parse::<u32>().map_err(|_| format!("`{s}` isn't a number"))
    };
    let mut bits = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0).ok_or_else(|| format!("bad step in `{item}`"))?),
            None => (item, 1),
        };
        let (a, b) = match range {
            "*" => (lo, hi),
            r => match r.split_once('-') {
                Some((a, b)) => (value(a)?, value(b)?),
                // `a/n`: from `a` to the field's end.
                None if item.contains('/') => (value(r)?, hi),
                None => (value(r)?, value(r)?),
            },
        };
        // Sunday may be 7 (the day-of-week field's own range is 0..=7).
        if a < lo || b > hi || a > b {
            return Err(format!("`{item}` is out of {lo}-{hi}"));
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok(bits)
}

const MONTHS: &[&str] = &["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const DAYS: &[&str] = &["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

impl Schedule {
    /// Read a schedule: five fields or an `@…` macro. `Err` says what it couldn't read. Pure.
    pub fn parse(text: &str) -> Result<Schedule, String> {
        let text = text.trim();
        let expanded = match text.to_ascii_lowercase().as_str() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            "@reboot" => {
                return Ok(Schedule { text: text.into(), minutes: 0, hours: 0, doms: 0, months: 0, dows: 0, dom_any: true, dow_any: true, reboot: true })
            }
            m if m.starts_with('@') => return Err(format!("unknown schedule `{text}`")),
            _ => text,
        };
        let f: Vec<&str> = expanded.split_whitespace().collect();
        let [mi, h, dom, mo, dow] = f.as_slice() else { return Err(format!("`{text}` isn't five fields")) };
        let dows = field_bits(dow, 0, 7, DAYS)?;
        Ok(Schedule {
            text: text.into(),
            minutes: field_bits(mi, 0, 59, &[])?,
            hours: field_bits(h, 0, 23, &[])? as u32,
            doms: field_bits(dom, 1, 31, &[])? as u32,
            months: field_bits(mo, 1, 12, MONTHS)? as u16,
            // Sunday as 7 is Sunday as 0.
            dows: ((dows | (dows >> 7)) & 0x7f) as u8,
            dom_any: dom.starts_with('*'),
            dow_any: dow.starts_with('*'),
            reboot: false,
        })
    }

    /// Whether it runs on day `days` (since the epoch, local).
    fn runs_on(&self, days: i64) -> bool {
        let (_, m, d) = crate::schedule::civil_from_days(days);
        let wd = (days + 4).rem_euclid(7) as u32; // 1970-01-01 was a Thursday
        if self.months & (1 << m) == 0 {
            return false;
        }
        let (dom, dow) = (self.doms & (1 << d) != 0, self.dows & (1 << wd) != 0);
        match (self.dom_any, self.dow_any) {
            (false, false) => dom || dow,
            _ => dom && dow,
        }
    }

    /// The first time after `t` (UNIX seconds) it runs, with the box's clock `offset` seconds
    /// ahead of UTC (cron reads the schedule in the box's own time zone) — within the next four
    /// years; `None` for `@reboot` or a date that never comes (`0 0 31 2 *`). Pure.
    pub fn next_after(&self, t: u64, offset: i64) -> Option<u64> {
        if self.reboot {
            return None;
        }
        let local = t as i64 + offset;
        let first = local.div_euclid(60) * 60 + 60;
        let mut day = first.div_euclid(86_400);
        let mut from = first.rem_euclid(86_400);
        for _ in 0..(4 * 366) {
            if self.runs_on(day) {
                for h in (from / 3600) as u32..24 {
                    if self.hours & (1 << h) == 0 {
                        continue;
                    }
                    let start = if h as i64 == from / 3600 { (from % 3600) / 60 } else { 0 };
                    if let Some(m) = (start as u32..60).find(|m| self.minutes & (1 << m) != 0) {
                        let at = day * 86_400 + h as i64 * 3600 + m as i64 * 60 - offset;
                        return u64::try_from(at).ok();
                    }
                }
            }
            day += 1;
            from = 0;
        }
        None
    }

    /// The longest wait between two runs, from `t` over the next eight days (so a line that
    /// skips the weekend counts it) or 400 runs — how late a last run may be before it's
    /// overdue. `None` for `@reboot` or a schedule that runs less than twice in that time. Pure.
    pub fn longest_gap(&self, t: u64, offset: i64) -> Option<u64> {
        let mut at = self.next_after(t, offset)?;
        let until = at + 8 * 86_400;
        let mut gap = None;
        for _ in 0..400 {
            let next = self.next_after(at, offset)?;
            if next > until && gap.is_some() {
                break;
            }
            gap = Some(gap.unwrap_or(0).max(next - at));
            at = next;
        }
        gap
    }

    /// How often, in words: `every 15 min`, `hourly at :40`, `hourly at :07 :22 :37 :52`,
    /// `daily at 07:20`, `at boot`; anything else as written. Pure.
    pub fn describe(&self) -> String {
        if self.reboot {
            return "at boot".into();
        }
        let set = |bits: u64, lo: u32, hi: u32| (lo..=hi).filter(|v| bits & (1 << v) != 0).collect::<Vec<u32>>();
        let minutes = set(self.minutes, 0, 59);
        let hours = set(self.hours as u64, 0, 23);
        let every_day = self.doms & 0xffff_fffe == 0xffff_fffe && self.dows == 0x7f && self.months & 0x1ffe == 0x1ffe;
        let all_hours = hours.len() == 24;
        // Evenly spaced from :00 (`*/15`): every N min.
        let step = minutes.get(1).map(|m| m - minutes[0]);
        let even = minutes.first() == Some(&0) && step.is_some_and(|s| 60 % s == 0 && minutes.windows(2).all(|w| w[1] - w[0] == s) && minutes.len() as u32 * s == 60);
        match (every_day, all_hours, minutes.as_slice(), hours.as_slice()) {
            (true, true, _, _) if minutes.len() == 60 => "every minute".into(),
            (true, true, _, _) if even => format!("every {} min", step.unwrap_or(1)),
            (true, true, ms, _) => format!("hourly at {}", ms.iter().map(|m| format!(":{m:02}")).collect::<Vec<_>>().join(" ")),
            (true, false, [m], [h]) => format!("daily at {h:02}:{m:02}"),
            _ => format!("`{}`", self.text),
        }
    }
}

/// The box's UTC offset at `t` (seconds east of UTC), as this process's local time reads it —
/// cron's own zone unless this shell sets `TZ` differently (then the next run said can be off by
/// that difference; `cron show` names the box's zone). `0` where it can't be read.
#[cfg(unix)]
pub fn local_offset(t: u64) -> i64 {
    let tt = t as libc::time_t;
    // SAFETY: localtime_r(3) reads `tt` and fills the zeroed struct we own; no shared state.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&tt, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}

/// Off unix: UTC.
#[cfg(not(unix))]
pub fn local_offset(_t: u64) -> i64 {
    0
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
            (Job::Health, "/root/.local/state/arena-cron/health.log", "/root/.local/state/arena-cron/health.lock"),
        ];
        assert_eq!(cases.map(|(j, _, _)| j), Job::ALL);
        for (job, want_log, want_lock) in cases {
            assert_eq!((log("/root", job).as_str(), lock("/root", job).as_str()), (want_log, want_lock), "{job:?}");
        }
        // The old names, directly in the home: visible logs, hidden locks (none for the archive);
        // the health check never had any.
        let legacy: Vec<(Option<&str>, Option<&str>)> = Job::ALL.iter().map(|j| (j.legacy_log(), j.legacy_lock())).collect();
        assert_eq!(
            legacy,
            [
                (Some("arena-cron.log"), Some(".arena-backup-cron.lock")),
                (Some("arena-proxy-cron.log"), Some(".arena-proxy-cron.lock")),
                (Some("arena-snapshot-cron.log"), Some(".arena-snapshot-cron.lock")),
                (Some("arena-archive-cron.log"), None),
                (Some("arena-reap-cron.log"), Some(".arena-reap-cron.lock")),
                (None, None),
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
            (Job::Health, "/root/.local/state/arena-cron/health.log", Default),
            // No earlier layout had a health line: a log in the a663f65 folder is someone's.
            (Job::Health, "/root/.local/state/arena/cron/health.log", Chosen),
            (Job::Health, "/root/arena-health-cron.log", Chosen),
        ];
        for (job, path, want) in cases {
            assert_eq!(log_place("/root", job, path), want, "{job:?} {path}");
        }
        assert!(is_old_lock("/root", Job::Backup, "/root/.arena-backup-cron.lock"));
        assert!(is_old_lock("/root", Job::Snapshot, "/root/.local/state/arena/cron/snapshot.lock"));
        assert!(!is_old_lock("/root", Job::Backup, "/root/.arena-proxy-cron.lock"));
        assert!(!is_old_lock("/root", Job::Backup, "/root/.local/state/arena-cron/backup.lock"));
        assert!(!is_old_lock("/root", Job::Backup, "/srv/.arena-backup-cron.lock"));
        assert!(!is_old_lock("/root", Job::Health, "/root/.local/state/arena/cron/health.lock"));
    }

    /// A schedule as cron reads it: when it runs next (in the box's zone), its longest gap and
    /// how often in words — every way arena's own lines and a hand edit write one.
    #[test]
    fn a_schedule_says_when_it_runs_next_and_how_often() {
        // 2026-10-30 21:34:57 UTC, a Friday.
        let t = 1_793_396_097;
        let at = |h: u64, m: u64| 1_793_318_400 + h * 3600 + m * 60; // that day's 00:00 UTC + h:m
        let cases: &[(&str, Option<u64>, Option<u64>, &str)] = &[
            ("*/15 * * * *", Some(at(21, 45)), Some(900), "every 15 min"),
            ("*/2 * * * *", Some(at(21, 36)), Some(120), "every 2 min"),
            ("*/5 * * * *", Some(at(21, 35)), Some(300), "every 5 min"),
            ("7,22,37,52 * * * *", Some(at(21, 37)), Some(900), "hourly at :07 :22 :37 :52"),
            ("40 * * * *", Some(at(21, 40)), Some(3600), "hourly at :40"),
            ("20 7 * * *", Some(at(24 + 7, 20)), Some(86_400), "daily at 07:20"),
            ("@hourly", Some(at(22, 0)), Some(3600), "hourly at :00"),
            ("@daily", Some(at(24, 0)), Some(86_400), "daily at 00:00"),
            ("* * * * *", Some(at(21, 35)), Some(60), "every minute"),
            // Weekdays only: Saturday and Sunday are skipped, so the longest gap is the weekend.
            ("0 9 * * mon-fri", Some(at(3 * 24 + 9, 0)), Some(3 * 86_400), "`0 9 * * mon-fri`"),
            // Both day fields restricted: either one.
            ("0 0 1 * 6", Some(at(24, 0)), None, "`0 0 1 * 6`"),
            ("0 0 * * 7", Some(at(2 * 24, 0)), Some(7 * 86_400), "`0 0 * * 7`"),
            ("15 3 1 jan *", Some(1_798_773_300), None, "`15 3 1 jan *`"),
            ("@reboot", None, None, "at boot"),
            ("0 0 31 2 *", None, None, "`0 0 31 2 *`"),
        ];
        for (text, next, gap, words) in cases {
            let s = Schedule::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(s.next_after(t, 0), *next, "{text}");
            if gap.is_some() {
                assert_eq!(s.longest_gap(t, 0), *gap, "{text}");
            }
            assert_eq!(s.describe(), *words, "{text}");
        }
        // In the box's zone: 07:20 at UTC+1 is 06:20 UTC.
        assert_eq!(Schedule::parse("20 7 * * *").unwrap().next_after(t, 3600), Some(at(24 + 6, 20)));
        for bad in ["* * * *", "61 * * * *", "*/0 * * * *", "a * * * *", "@sometimes", "5-1 * * * *"] {
            assert!(Schedule::parse(bad).is_err(), "{bad}");
        }
    }

    /// The crontab's lines read back: which arena line each is, its schedule, log, lock and config.
    #[test]
    fn an_installed_line_reads_back() {
        let tab = "MAILTO=x\n0 5 * * 1 tar -zcf /var/backups/home.tgz /home/\n# >>> arena-infra-rs >>>\n\
            */15 * * * * mkdir -p /root/.local/state/arena-cron /root/logs/cron && flock -n -o /root/.local/state/arena-cron/backup.lock \
            /usr/local/bin/arena --config /etc/arena/config.env pods backup --yes >> /root/logs/cron/arena-cron.log 2>&1\n\
            # 7,22,37,52 * * * * arena archive sync --tier snap\n\
            # <<< arena-infra-rs <<<\n15 3 * * * /usr/local/bin/arena --config /c pods test --deep\n";
        let lines = arena_lines(tab);
        assert_eq!(lines.managed.len(), 2);
        assert_eq!(lines.unmanaged, ["15 3 * * * /usr/local/bin/arena --config /c pods test --deep"]);
        let backup = &lines.managed[0];
        assert_eq!(Kind::of(backup), Some(Kind::Backup));
        assert_eq!(Kind::of(&lines.managed[1]), None, "a paused line is no line");
        assert_eq!(line_parts(backup).map(|(s, _)| s).as_deref(), Some("*/15 * * * *"));
        assert_eq!(log_target(backup), Some("/root/logs/cron/arena-cron.log"));
        assert_eq!(lock_target(backup), Some("/root/.local/state/arena-cron/backup.lock"));
        assert_eq!(config_target(backup), Some("/etc/arena/config.env"));
        assert_eq!(Kind::of(&lines.unmanaged[0]), Some(Kind::Health));
        assert_eq!(Kind::Health.job(), Job::Health);
        assert_eq!(Kind::ArchiveBig.label(), "archive-big");
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
