//! `arena cron`: the crontab block arena installs — its lines (backup, pull, proxy, snapshot,
//! health, reap, archive), how they are read back, merged and tidied, and their logs.

use std::path::PathBuf;

use anyhow::{Context, Result};
use arena_core::Config;
use clap::Subcommand;

use crate::{archive, confirm, shell_quote};

#[derive(Subcommand)]
pub(crate) enum CronCmd {
    /// Add or replace arena cron lines (shows the diff, confirms)
    ///
    /// Each flag names one line (backup, proxy, snapshot, archive, reap, health); only those
    /// are (re)written and the rest are kept. No flag = the backup line. A rewritten backup line
    /// keeps the installed one's --pull, start date and schedule unless you change them.
    /// Locks live in ~/.local/state/arena-cron/, logs too (backup.log, …) unless the installed
    /// line logs elsewhere already: that path is kept. Old default logs (~/arena-cron.log) move.
    #[command(verbatim_doc_comment)]
    Install {
        /// The backup line: `pods backup` every 15 min (git only unless --pull)
        #[arg(long)]
        backup: bool,
        /// The backup line's cron schedule (default: the installed one's, else */15)
        #[arg(long)]
        schedule: Option<String>,
        /// Bake ARENA_START_DATE (YYYY-MM-DD) into the backup line (cron lacks your env)
        #[arg(long)]
        start_date: Option<String>,
        /// Drop the backup line's ARENA_START_DATE (e.g. once it's in config.env)
        #[arg(long, conflicts_with = "start_date")]
        no_start_date: bool,
        /// The backup line also runs `pods pull` (rsync file backup) after the git push
        #[arg(long)]
        pull: bool,
        /// Make the backup line git-only, even if the installed one pulls
        #[arg(long, conflicts_with = "pull")]
        no_pull: bool,
        /// The proxy line: `proxy apply --yes` every 5 min (catches outside changes)
        #[arg(long)]
        proxy: bool,
        /// The snapshot line: write DIR/fleet.json for web/fleet.html every 2 min
        #[arg(long, value_name = "DIR")]
        snapshot: Option<PathBuf>,
        /// The archive push lines (run `archive up` first; needs a start date)
        #[arg(long)]
        archive: bool,
        /// The reaper line: `observe` (logs only; start here) or `apply` (acts)
        ///
        /// `apply` terminates pods only with REAP_ENABLED=1 in the config file.
        #[arg(long, value_name = "MODE", value_parser = ["observe", "apply"])]
        reap: Option<String>,
        /// The health line: `pods test --deep` daily at HH:MM, this box's time zone
        #[arg(long, value_name = "HH:MM")]
        health: Option<String>,
        /// Preview only: print the diff, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Remove arena cron lines (the flagged ones, else all)
    Remove {
        /// Remove the backup line.
        #[arg(long)]
        backup: bool,
        /// Remove the proxy re-sync line.
        #[arg(long)]
        proxy: bool,
        /// Remove the public snapshot line.
        #[arg(long)]
        snapshot: bool,
        /// Remove the backup archive's two push lines.
        #[arg(long)]
        archive: bool,
        /// Remove the auto-reaper line.
        #[arg(long)]
        reap: bool,
        /// Remove the daily health check line.
        #[arg(long)]
        health: bool,
        /// Preview only: print the diff, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Show the installed arena cron lines
    Show,
}

/// Resolve the iteration (week, day): explicit `--week/--day` win; otherwise compute
/// from `ARENA_START_DATE` (the start date is w0d1) and today's date.
pub(crate) fn resolve_week_day(cfg: &Config, week: Option<u32>, day: Option<u32>) -> Result<(u32, u32)> {
    if let (Some(w), Some(d)) = (week, day) {
        return Ok((w, d));
    }
    let start = cfg
        .get("ARENA_START_DATE")
        .and_then(arena_core::schedule::parse_ymd)
        .context(
            "computing week/day needs ARENA_START_DATE=YYYY-MM-DD in config (or pass \
             --week and --day)",
        )?;
    let start_days = arena_core::schedule::days_from_civil(start.0, start.1, start.2);
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let today = arena_core::schedule::days_from_unix(now_secs);
    let (w, d) = arena_core::schedule::week_day(start_days, today);
    // Allow overriding just one of the two.
    Ok((week.unwrap_or(w), day.unwrap_or(d)))
}

pub(crate) use arena_core::cron::{BEGIN as CRON_BEGIN, END as CRON_END};

/// Return `existing` crontab text with any arena-managed block removed.
fn strip_arena_block(existing: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in existing.lines() {
        match line.trim() {
            CRON_BEGIN => in_block = true,
            CRON_END => in_block = false,
            _ if !in_block => out.push(line),
            _ => {}
        }
    }
    let mut s = out.join("\n");
    while s.ends_with('\n') {
        s.pop();
    }
    s
}

/// The crontab's arena lines, for `cron show` and `teardown --check`: every line inside
/// arena's managed block(s) — what `cron remove` clears — and every job line outside them that
/// mentions arena anyway (hand-added, so `cron remove` won't touch it).
pub(crate) fn arena_cron_lines(crontab: &str) -> arena_core::teardown::CronLines {
    arena_core::cron::arena_lines(crontab)
}

/// Build new crontab text = existing (minus old arena block) + the new arena block
/// (empty `lines` => just remove). Other entries are preserved untouched.
fn with_arena_block(existing: &str, lines: &[String]) -> String {
    let base = strip_arena_block(existing);
    if lines.is_empty() {
        return if base.is_empty() { String::new() } else { format!("{base}\n") };
    }
    let mut s = String::new();
    if !base.is_empty() {
        s.push_str(&base);
        s.push('\n');
    }
    s.push_str(CRON_BEGIN);
    s.push('\n');
    for l in lines {
        s.push_str(l);
        s.push('\n');
    }
    s.push_str(CRON_END);
    s.push('\n');
    s
}

/// The `crontab` binary `cron` reads and writes with (from PATH).
pub(crate) const CRONTAB: &str = "crontab";

/// `arena cron …`. `crontab` is the program that reads (`-l`) and replaces (`-`) the user's
/// crontab: [`CRONTAB`] for real, a stub over a file in tests. `home` is `$HOME` (a temp dir in
/// tests): the home the lines run in, which `install`/`remove` check first ([`cron_home`]).
pub(crate) async fn handle_cron(cmd: CronCmd, cfg: &Config, config_path: &std::path::Path, yes: bool, crontab: &str, home: Option<&str>) -> Result<()> {
    use tokio::process::Command;

    // Read the current crontab (no crontab installed => empty, not an error).
    let read = Command::new(crontab).arg("-l").output().await.context("running `crontab -l`")?;
    let current = if read.status.success() {
        String::from_utf8_lossy(&read.stdout).into_owned()
    } else {
        String::new()
    };
    let before = arena_cron_lines(&current).managed;

    match cmd {
        CronCmd::Show => {
            if before.is_empty() {
                println!("(no arena-managed cron lines)");
            } else {
                for l in &before {
                    let kind = CronKind::of(l);
                    println!("[{}] {l}", kind.map_or("other", CronKind::label));
                    // The health line's time is cron's, this box's zone: said, not left to guess.
                    if let Some(at) = HealthAt::of_line(l).filter(|_| kind == Some(CronKind::Health)) {
                        println!("  ↳ {}", health_when(at, box_zone().as_deref(), cron_tz(&current)));
                    }
                }
            }
            Ok(())
        }
        CronCmd::Remove { backup, proxy, snapshot, archive, reap, health, dry_run } => {
            cron_home(home, effective_uid()?)?;
            let mut kinds = selected_kinds(backup, proxy, snapshot, archive);
            if reap {
                kinds.push(CronKind::Reap);
            }
            if health {
                kinds.push(CronKind::Health);
            }
            // No flag: the whole block, hand edits inside it included (as always).
            let after = if kinds.is_empty() { Vec::new() } else { merge_cron_block(&before, &kinds, &[]) };
            apply_cron_change(&current, &before, &after, &[], dry_run, yes, crontab).await.map(|_| ())
        }
        CronCmd::Install { backup, schedule, start_date, no_start_date, pull, no_pull, proxy, snapshot, archive, reap, health, dry_run } => {
            // Once, before anything is written: the tidy after the write uses it too, so nothing
            // can fail once the crontab is in (review ai-review claude #10).
            let euid = effective_uid()?;
            let home = &cron_home(home, euid)?;
            let exe = std::env::current_exe().context("finding the arena executable path")?;
            let cfg_abs = std::fs::canonicalize(config_path)
                .unwrap_or_else(|_| config_path.to_path_buf());
            // Every path the lines carry unquoted: refused before anything is shown or written.
            crontab_word("HOME", home)?;
            crontab_word("the arena binary", &exe.display().to_string())?;
            crontab_word("the config path", &cfg_abs.display().to_string())?;
            // The logs the installed lines chose themselves: kept (arena never moves those).
            let kept_logs = kept_logs(&before, home);
            let kept_log = |k: CronKind| kept_logs.iter().find(|(x, _)| *x == k).map(|(_, l)| l.as_str());
            let start_date = start_date.map(|d| d.trim().to_string());
            if let Some(d) = &start_date {
                arena_core::schedule::parse_ymd(d).context("--start-date must be YYYY-MM-DD")?;
            }
            let health = health.as_deref().map(HealthAt::parse).transpose()?;
            let snapshot_dir = snapshot.as_deref().map(snapshot_cron_dir).transpose()?;
            // `--reap` and `--health` name their own lines: like `--proxy`, neither brings a
            // backup line along.
            let backup = install_backup_line(
                backup,
                schedule.is_some(),
                start_date.is_some() || no_start_date,
                pull || no_pull,
                proxy || reap.is_some() || health.is_some(),
                snapshot_dir.is_some() || archive,
            );
            let reap_line = reap
                .as_deref()
                .map(|mode| reap_cron_line(mode == "apply", config_path, &exe.display().to_string(), &cfg_abs.display().to_string(), home, kept_log(CronKind::Reap)))
                .transpose()?;
            // Its verdicts go to the health cache this shell resolves, as the archive's lines
            // carry their state dir (AD10): the one this shell's dashboard and `snapshot` read.
            let health_line = health
                .map(|at| -> Result<String> {
                    let root = archive::cron_state_root(cfg).map_err(|e| anyhow::anyhow!("--health: {e}"))?;
                    let (exe, cfg_abs) = (exe.display().to_string(), cfg_abs.display().to_string());
                    Ok(health_cron_line(at, &exe, &cfg_abs, home, &root, kept_log(CronKind::Health)))
                })
                .transpose()?;
            let mut notes = Vec::new();
            // The backup line's options: each flag, else the installed line's (so a
            // re-install — e.g. to pick up the flock — never silently drops its --pull or
            // start date), else the default.
            let installed = before.iter().find(|l| CronKind::of(l) == Some(CronKind::Backup));
            let read_back = installed.and_then(|l| BackupOpts::of_line(l));
            let flags = BackupFlags {
                schedule: schedule.as_deref(),
                start_date: start_date.as_deref(),
                no_start_date,
                pull,
                no_pull,
            };
            notes.extend(backup_read_note(backup, installed.map(String::as_str), read_back.as_ref(), &flags));
            let (opts, kept) = resolve_backup_opts(read_back.as_ref(), &flags);
            if backup && !kept.is_empty() {
                notes.push(format!(
                    "note: keeping the installed backup line's {} (--schedule / --no-pull / --no-start-date change them)",
                    kept.join(", ")
                ));
            }
            // The archive's lines need this user's archive state, an absolute backup dir and a
            // start date — checked before anything is shown or written (§7.1). They're also
            // re-rendered whenever the backup line is, so both always carry the same start date
            // (AD3): a stale one would seal the pull's label two hours into its own day.
            let installed_archive: Vec<&String> =
                before.iter().filter(|l| matches!(CronKind::of(l), Some(CronKind::ArchiveSnap) | Some(CronKind::ArchiveBig))).collect();
            let rerender_archive = !archive && backup && !installed_archive.is_empty();
            if archive {
                let refusals = archive::cron_refusals(cfg, opts.start_date.is_some());
                if !refusals.is_empty() {
                    anyhow::bail!("cron install --archive refused:\n  - {}", refusals.join("\n  - "));
                }
            }
            if rerender_archive && opts.start_date.is_none() && cfg.get("ARENA_START_DATE").and_then(arena_core::schedule::parse_ymd).is_none() {
                anyhow::bail!(
                    "the archive's cron lines need a start date too: keep one on the backup line, set ARENA_START_DATE in config, \
                     or `arena cron remove --archive` first"
                );
            }
            // The state dir the archive lines use, in the lines themselves (AD10): what this shell
            // resolves (env included) is what cron must resolve — or, re-rendering, what they had.
            let archive_state_dir = if archive || rerender_archive {
                let installed = installed_archive.iter().find_map(|l| env_assignment(l, "ARENA_STATE_DIR"));
                match (archive, installed, archive::cron_state_root(cfg)) {
                    (false, Some(dir), _) => Some(dir),
                    (_, _, Ok(dir)) => Some(dir),
                    (_, _, Err(e)) => anyhow::bail!("the archive's cron lines: {e}"),
                }
            } else {
                None
            };
            // Optional inline env (cron runs the line via sh, so `VAR=val cmd` works).
            let env_prefix = opts.start_date.as_ref().map(|d| format!("ARENA_START_DATE={d} ")).unwrap_or_default();
            let lines = cron_lines(&CronJob {
                schedule: &opts.schedule,
                env_prefix: &env_prefix,
                exe: &exe.display().to_string(),
                config: &cfg_abs.display().to_string(),
                home,
                backup,
                pull: opts.pull,
                proxy,
                snapshot_dir: snapshot_dir.as_deref(),
                archive: archive || rerender_archive,
                archive_state_dir: archive_state_dir.as_deref(),
                kept_logs: &kept_logs,
            });
            let mut lines = lines;
            if let Some(l) = reap_line {
                lines.push((CronKind::Reap, l));
            }
            if let Some(l) = health_line {
                lines.push((CronKind::Health, l));
            }
            let kinds: Vec<CronKind> = lines.iter().map(|(k, _)| *k).collect();
            let after = merge_cron_block(&before, &kinds, &lines);
            let kept: Vec<String> =
                kinds.iter().filter_map(|&k| kept_log(k).map(|l| format!("{l} ({})", k.label()))).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
            if !kept.is_empty() {
                notes.push(format!(
                    "note: keeping each installed line's own log: {} (arena's default, for a new line: ~/{}/)",
                    kept.join(", "),
                    arena_core::cron::DIR
                ));
            }
            notes.extend(unread_logs_note(&before, &kinds));
            // The archive pushes what the backup line's `--pull` writes: without it there is
            // nothing new to push.
            if archive && !after.iter().any(|l| BackupOpts::of_line(l).is_some_and(|o| o.pull)) {
                notes.push(
                    "warning: no backup line runs the rsync file backup (`--pull`) — the archive only pushes what that writes \
                     (`arena cron install --backup --pull`)"
                        .to_string(),
                );
            }
            // `--proxy`/`--snapshot` alone no longer bring a backup line along: say when
            // there will be none, so a fresh box doesn't silently go without backups.
            if !after.iter().any(|l| CronKind::of(l) == Some(CronKind::Backup)) {
                notes.push(
                    "note: no backup line — the scheduled backup is `arena cron install --backup` (add --pull for \
                     the rsync file backup too)"
                        .to_string(),
                );
            }
            if let Some(at) = health {
                notes.push(format!("note: the health check runs {}", health_when(at, box_zone().as_deref(), cron_tz(&current))));
                notes.extend(health_minute_warning(at));
            }
            if apply_cron_change(&current, &before, &after, &notes, dry_run, yes, crontab).await? {
                // Only once it's written: until then, the old lines still use their old files.
                let plan = tidy_plan(&kinds, &before, &after, home, &with_arena_block(&current, &after));
                if let Some(line) = tidy_line(&tidy_old_cron_files(&plan, home, euid)) {
                    println!("{line}");
                }
            }
            Ok(())
        }
    }
}

/// Show what a `cron install`/`remove` changes in arena's block — the [`cron_diff`], then
/// the `notes` — then confirm and write the whole crontab back with only that block
/// changed. Nothing to change, or a dry run: say so and write nothing. Returns whether the
/// crontab was written.
async fn apply_cron_change(
    current: &str,
    before: &[String],
    after: &[String],
    notes: &[String],
    dry_run: bool,
    yes: bool,
    crontab: &str,
) -> Result<bool> {
    if before == after {
        println!("The arena cron lines are already as asked — nothing to change.");
        for n in notes {
            println!("{n}");
        }
        return Ok(false);
    }
    println!("arena cron lines, before → after (- removed, + added):");
    for l in cron_diff(before, after) {
        println!("  {l}");
    }
    for n in notes {
        println!("{n}");
    }
    if dry_run {
        println!("\nPreview only — run without --dry-run to change the crontab.");
        return Ok(false);
    }
    if !confirm(yes, "\nWrite this crontab change?")? {
        println!("aborted.");
        return Ok(false);
    }
    write_crontab(&with_arena_block(current, after), crontab).await?;
    println!("Crontab updated (view with `arena cron show`).");
    Ok(true)
}

/// The home `cron install`/`remove` writes lines for: `$HOME`, checked — set, absolute, free
/// of whitespace, `%` and newlines (the lines carry it unquoted, and crontab turns a `%` into a
/// newline), and a directory the effective user (`euid`) owns. Under `sudo -E` it would be
/// another user's home: root's lines would create folders there, and the tidy would act on
/// that user's files. (`install` also holds it to [`crontab_word`]'s alphabet: `remove` writes
/// no path.)
fn cron_home(home: Option<&str>, euid: u32) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    const AS_USER: &str = "run as the user whose crontab this is (sudo -i / sudo -H)";
    let home = match home {
        None | Some("") => anyhow::bail!("HOME isn't set — {AS_USER}"),
        Some(h) if !h.starts_with('/') => anyhow::bail!("HOME={h} isn't an absolute path — {AS_USER}"),
        Some(h) => h,
    };
    if let Some(c) = home.chars().find(|c| c.is_whitespace() || *c == '%') {
        anyhow::bail!("HOME={home:?} holds {}, which a crontab line can't carry", char_named(c));
    }
    let meta = std::fs::metadata(home).map_err(|e| anyhow::anyhow!("HOME={home}: {e} — {AS_USER}"))?;
    if !meta.is_dir() {
        anyhow::bail!("HOME={home} isn't a directory — {AS_USER}");
    }
    if meta.uid() != euid {
        anyhow::bail!("HOME={home} isn't this user's home (owner uid {}, running as {euid}) — {AS_USER}, not sudo -E", meta.uid());
    }
    Ok(home.to_string())
}

/// This process's effective uid: the owner of a file it has just created in the temp dir (and
/// removed), else of `/proc/self`, else `id -u`. Why the file first (review fb2 F10): a new
/// file is owned by the uid this process makes files as — what the HOME check and the tidy
/// compare with — whereas `/proc/<pid>` is made root's for a process that isn't dumpable (a
/// setuid or capability wrapper, `fs.suid_dumpable=0` after a credential change), which would
/// read a user as uid 0. No libc call: no new crate.
fn effective_uid() -> Result<u32> {
    use std::os::unix::fs::MetadataExt;
    #[cfg(test)]
    if let Some(left) = EUID_ANSWERS.with(std::cell::Cell::get) {
        anyhow::ensure!(left > 0, "the effective uid can't be read (a test's)");
        EUID_ANSWERS.with(|n| n.set(Some(left - 1)));
    }
    if let Some(uid) = created_file_owner(&std::env::temp_dir()) {
        return Ok(uid);
    }
    if let Ok(m) = std::fs::metadata("/proc/self") {
        return Ok(m.uid());
    }
    let out = std::process::Command::new("id").arg("-u").output().context("running `id -u`")?;
    String::from_utf8_lossy(&out.stdout).trim().parse().context("reading the effective uid from `id -u`")
}

#[cfg(test)]
thread_local! {
    /// Under test: how many more times [`effective_uid`] answers before it fails (`None`: it always
    /// answers) — so a test can make any read after the first fail. A `#[tokio::test]` runs the
    /// command on its own thread, so this is that test's.
    static EUID_ANSWERS: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// The owner of a file created new in `dir` (`create_new`, so never someone else's file), read
/// from the open file and removed at once. `None` when none could be made.
fn created_file_owner(dir: &std::path::Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    (0..4).find_map(|i| {
        let path = dir.join(format!(".arena-euid-{}-{nanos}-{i}", std::process::id()));
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).ok()?;
        let uid = file.metadata().ok().map(|m| m.uid());
        let _ = std::fs::remove_file(&path);
        uid
    })
}

/// `value` (`what`: HOME, the arena binary, the config path), an absolute path `cron install`
/// writes into its lines as one plain, unquoted shell word — refused, in one line naming the
/// first character at fault, unless it is absolute and every character is in
/// [`CRON_PATH_CHARS`]. Whitespace or `%` (crontab turns it into a newline) silently cut the
/// command short, but so does more (review pr4): a valid, owned HOME `/srv/o'neil` or a config
/// `/srv/course(2026)/config.env` is a shell syntax error on every tick, so arena never runs;
/// `&`, `;`, `$`, a backtick change what the line runs. Refused rather than quoted, so reading an
/// installed line back stays splitting it into words.
pub(crate) fn crontab_word(what: &str, value: &str) -> Result<()> {
    if !value.starts_with('/') {
        anyhow::bail!("{what} ({value:?}) can't go into a crontab line: it isn't an absolute path");
    }
    if let Some(c) = cron_unsafe_char(value) {
        anyhow::bail!(
            "{what} ({value:?}) can't go into a crontab line: it holds {} — the line carries it unquoted, so only letters, \
             digits and `{CRON_PATH_CHARS}` may be in it; use a path without it",
            char_named(c)
        );
    }
    Ok(())
}

/// The characters besides ASCII letters and digits a path may hold to go into a crontab line
/// unquoted ([`crontab_word`]): none of them means anything to `sh` inside a word that starts with
/// `/` (no `~`, `$`, quotes, globs, `#`, separators or redirections), nor to crontab (no `%`).
pub(crate) const CRON_PATH_CHARS: &str = "./_+@:,=-";

/// The first character of `path` outside ASCII letters, digits and [`CRON_PATH_CHARS`], if any.
/// Pure.
pub(crate) fn cron_unsafe_char(path: &str) -> Option<char> {
    path.chars().find(|c| !c.is_ascii_alphanumeric() && !CRON_PATH_CHARS.contains(*c))
}

/// A character as a refusal names it: `' (U+0027)`, `a space (U+0020)` — whitespace and control
/// characters by name, so the line stays one line. Pure.
pub(crate) fn char_named(c: char) -> String {
    let name = match c {
        ' ' => "a space".to_string(),
        '\t' => "a tab".to_string(),
        '\n' => "a newline".to_string(),
        c if c.is_whitespace() || c.is_control() => "a control or space character".to_string(),
        c => c.to_string(),
    };
    format!("{name} (U+{:04X})", u32::from(c))
}

/// A crontab line's log ([`arena_core::cron::log_target`]): `PATH` when the line ends in an
/// append of its output to one file (review fb2 F8: a hand edit's `>>PATH 2>&1`, `>> PATH` or
/// `>>PATH` moved to the default log, said only by the diff); `None` for any other ending, which
/// [`unread_log`] tells apart from a line that logs nowhere. Pure.
fn cron_log_target(line: &str) -> Option<&str> {
    arena_core::cron::log_target(line)
}

/// The output redirection of a crontab line whose log [`cron_log_target`] can't read (from its
/// first word with a `>` other than `2>&1`: `> /x.log 2>&1`, `>> /x.log 2>>/err`, …) — a re-install
/// logs it to arena's default instead, which the install's note says. `None`: its log was read,
/// or it has none. Pure.
fn unread_log(line: &str) -> Option<String> {
    if cron_log_target(line).is_some() {
        return None;
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    let from = words.iter().position(|w| w.contains('>') && *w != "2>&1")?;
    Some(words[from..].join(" "))
}

/// The note `cron install` adds when an installed line of a kind it rewrites (`kinds`; the first
/// of each, the one replaced in place: [`kept_logs`]) has a log it couldn't read
/// ([`unread_log`]): those lines now log to arena's default, and how to keep one. Pure.
fn unread_logs_note(before: &[String], kinds: &[CronKind]) -> Option<String> {
    let mut seen = Vec::new();
    let mut unread = Vec::new();
    for line in before {
        let Some(kind) = CronKind::of(line).filter(|k| kinds.contains(k) && !seen.contains(k)) else { continue };
        seen.push(kind);
        if let Some(redirect) = unread_log(line) {
            unread.push(format!("{} (`{redirect}`)", kind.label()));
        }
    }
    (!unread.is_empty()).then(|| {
        format!(
            "note: couldn't read where these installed lines log — their new lines log to arena's default (~/{}/) instead; end \
             a line with `>> PATH 2>&1` for a re-install to keep its log; check the diff: {}",
            arena_core::cron::DIR,
            unread.join(", ")
        )
    })
}

/// A crontab line's `flock` file: the word after `flock` and its `-…` options.
fn cron_lock_target(line: &str) -> Option<&str> {
    arena_core::cron::lock_target(line)
}

/// The logs the installed lines chose themselves ([`arena_core::cron::LogPlace::Chosen`]), by
/// kind: a re-install keeps each, so arena never moves a log path someone picked (a folder
/// logrotate rotates, say). The first line of a kind decides — the one a re-install replaces in
/// place. Pure.
fn kept_logs(before: &[String], home: &str) -> Vec<(CronKind, String)> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for line in before {
        let Some(kind) = CronKind::of(line).filter(|k| !seen.contains(k)) else { continue };
        seen.push(kind);
        if let Some(log) = cron_log_target(line).filter(|l| arena_core::cron::log_place(home, kind.job(), l) == arena_core::cron::LogPlace::Chosen) {
            out.push((kind, log.to_string()));
        }
    }
    out
}

/// An old file of a line `cron install` replaced, tidied once the crontab is written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OldCronFile {
    /// An earlier default log: moved to the new line's log.
    Log { old: String, new: String },
    /// An earlier default lock: deleted.
    Lock(String),
}

/// What to tidy after an install: for each of the installed `kinds` whose lines it changed and
/// whose new line logs to arena's default (a kept log means no tidy for that kind), the exact
/// paths each replaced line named — its log and its lock — when that path is an earlier arena
/// default for `home` ([`arena_core::cron::log_place`], [`arena_core::cron::is_old_lock`]) and
/// the `written` crontab doesn't still name it (by its path under the home, so a hand-added
/// line's `~/arena-cron.log` counts). Never `$HOME` plus a name: only paths the replaced lines
/// themselves used. Pure.
fn tidy_plan(kinds: &[CronKind], before: &[String], after: &[String], home: &str, written: &str) -> Vec<OldCronFile> {
    use arena_core::cron::{self, LogPlace};
    let of = |lines: &[String], k: CronKind| lines.iter().filter(|l| CronKind::of(l) == Some(k)).cloned().collect::<Vec<_>>();
    let still_named = |path: &str| written.contains(path.strip_prefix(&format!("{home}/")).unwrap_or(path));
    let mut plan = Vec::new();
    for &k in kinds {
        let (old_lines, new_lines) = (of(before, k), of(after, k));
        let Some(new) = new_lines.first().and_then(|l| cron_log_target(l)).filter(|l| *l == cron::log(home, k.job())) else { continue };
        if old_lines == new_lines {
            continue;
        }
        for line in &old_lines {
            let log = cron_log_target(line).filter(|l| cron::log_place(home, k.job(), l) == LogPlace::OldDefault && !still_named(l));
            let lock = cron_lock_target(line).filter(|l| cron::is_old_lock(home, k.job(), l) && !still_named(l));
            for file in [log.map(|old| OldCronFile::Log { old: old.into(), new: new.into() }), lock.map(|l| OldCronFile::Lock(l.into()))].into_iter().flatten() {
                if !plan.contains(&file) {
                    plan.push(file);
                }
            }
        }
    }
    plan
}

/// What [`tidy_old_cron_files`] did, by old path.
#[derive(Debug, Default, PartialEq, Eq)]
struct CronTidy {
    /// Old logs moved to the new ones (appended when a new log was already there).
    moved: Vec<String>,
    /// Old logs left where they are, with why: not a regular file of this user's (a symlink is
    /// never followed), or a new log that isn't a regular file.
    left: Vec<String>,
    /// Old files that couldn't be moved or removed, with why.
    failed: Vec<String>,
}

/// Carry out a [`tidy_plan`] once the crontab is written: each old log moves to its new log,
/// each old lock is deleted (nothing uses that path any more; a run still holding it keeps its
/// lock on the inode). Only regular files the effective user (`euid`) owns: a symlink is left
/// alone, never followed, and so is a new log that isn't a regular file.
fn tidy_old_cron_files(plan: &[OldCronFile], home: &str, euid: u32) -> CronTidy {
    use std::os::unix::fs::MetadataExt;
    let mut out = CronTidy::default();
    let shown = |p: &str| p.strip_prefix(home).filter(|r| r.starts_with('/')).map_or_else(|| p.to_string(), |r| format!("~{r}"));
    // Ok(true): a regular file of this user's; Ok(false): something else, left alone.
    let ours = |p: &str| std::fs::symlink_metadata(p).map(|m| m.file_type().is_file() && m.uid() == euid);
    for file in plan {
        match file {
            OldCronFile::Log { old, new } => match ours(old) {
                Ok(true) => match move_cron_log(std::path::Path::new(old), std::path::Path::new(new)) {
                    Ok(true) => out.moved.push(shown(old)),
                    Ok(false) => out.left.push(format!("{} ({} isn't a regular file)", shown(old), shown(new))),
                    Err(e) => out.failed.push(format!("{}: {e}", shown(old))),
                },
                Ok(false) => out.left.push(format!("{} (not a regular file of this user's)", shown(old))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => out.failed.push(format!("{}: {e}", shown(old))),
            },
            OldCronFile::Lock(lock) => {
                if ours(lock).unwrap_or(false) {
                    if let Err(e) = std::fs::remove_file(lock) {
                        out.failed.push(format!("{}: {e}", shown(lock)));
                    }
                }
            }
        }
    }
    out
}

/// Move the old log `old` (a regular file) to `new`; `Ok(false)`, touching nothing, when `new`
/// exists and isn't a regular file (a symlink, a directory). Hard-linked, then unlinked, when
/// `new` doesn't exist yet — the same file, so a run still appending to it goes on logging into
/// the new place, and never over a `new` a tick created meanwhile (as a rename would) — else
/// (`new` already there, or on another filesystem) appended to `new`, then removed. `new`
/// already being `old` (a tidy cut short between its link and its unlink) is just unlinked:
/// appending a file to itself would never end. And only `old`'s length as it was is copied, so
/// a run still writing to it can't keep the copy going either.
///
/// The append path's known rough edges, accepted as rare (it needs a tick that already wrote
/// the new log, or `~/.local` on another filesystem): what a still-running old-line job writes
/// after the copy is lost with the unlinked file; on a full disk a partial copy stays in the new
/// log (and the old file too, for a later retry by hand); the old lines land after the new log's
/// first ones; a new log created here gets the default mode, not the old one's.
fn move_cron_log(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<bool> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    if let Some(dir) = new.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::hard_link(old, new).is_err() {
        let was = std::fs::symlink_metadata(old)?;
        match std::fs::symlink_metadata(new) {
            Ok(m) if !m.file_type().is_file() => return Ok(false),
            Ok(m) if (m.dev(), m.ino()) == (was.dev(), was.ino()) => {}
            _ => {
                let from = std::fs::File::open(old)?;
                let mut to = std::fs::OpenOptions::new().append(true).create(true).open(new)?;
                std::io::copy(&mut from.take(was.len()), &mut to)?;
            }
        }
    }
    std::fs::remove_file(old)?;
    Ok(true)
}

/// The one line `cron install` prints after [`tidy_old_cron_files`]; `None` when it found
/// no old log.
fn tidy_line(t: &CronTidy) -> Option<String> {
    let mut parts = Vec::new();
    if !t.moved.is_empty() {
        parts.push(format!("moved: {}", t.moved.join(", ")));
    }
    if !t.left.is_empty() {
        parts.push(format!("left alone: {}", t.left.join(", ")));
    }
    if !t.failed.is_empty() {
        parts.push(format!("couldn't tidy: {}", t.failed.join("; ")));
    }
    (!parts.is_empty()).then(|| format!("· cron logs now in ~/{}/ ({})", arena_core::cron::DIR, parts.join("; ")))
}

/// Which arena-managed crontab line a line is ([`arena_core::cron::Kind`]): each has a stable
/// identity, so `cron install` replaces only the lines its flags name and `cron remove --<kind>`
/// drops only those — a later `cron install --proxy` no longer wipes an earlier `--pull`,
/// `--start-date` or `--snapshot` line.
pub(crate) use arena_core::cron::Kind as CronKind;

/// The kinds named by `cron remove`'s flags, in a fixed order (`--archive` is both of its lines).
fn selected_kinds(backup: bool, proxy: bool, snapshot: bool, archive: bool) -> Vec<CronKind> {
    [
        (backup, CronKind::Backup),
        (proxy, CronKind::Proxy),
        (snapshot, CronKind::Snapshot),
        (archive, CronKind::ArchiveSnap),
        (archive, CronKind::ArchiveBig),
    ]
    .into_iter()
    .filter_map(|(on, k)| on.then_some(k))
    .collect()
}

/// Whether `cron install` (re)writes the backup line: `--backup`, or any of the backup
/// line's own options (`--schedule`, `--start-date`/`--no-start-date`, `--pull`/`--no-pull`
/// — the caller folds each pair into one), or no line named at all — a bare `cron install`
/// installs the backup, as it always has. `--proxy`/`--snapshot` alone leave it alone.
fn install_backup_line(backup: bool, schedule: bool, start_date: bool, pull: bool, proxy: bool, others: bool) -> bool {
    backup || schedule || start_date || pull || !(proxy || others)
}

/// The options a backup line is rendered with — and, read back from an installed line
/// ([`BackupOpts::of_line`]), the ones a re-install keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BackupOpts {
    schedule: String,
    /// `ARENA_START_DATE=…` baked into the line.
    start_date: Option<String>,
    /// The line also runs the rsync file backup (no `--no-pull`).
    pull: bool,
}

impl BackupOpts {
    /// Read an installed backup line's options back — any version's: the schedule (five
    /// fields, or one `@…` macro), an `ARENA_START_DATE=` among the inline env assignments
    /// before the command (only a valid date; after the `mkdir -p … &&` this version's line
    /// starts with: [`cron_line_parts`]), and whether it pulls (`pods backup` does
    /// unless `--no-pull`). `None` for a line that isn't a backup line or has no readable
    /// schedule. A start date that can't be read that way (behind a `cd … &&`, invalid, in a
    /// comment) is left out and the rest kept — [`unread_start_date`] says so, for the install's
    /// note (review fb2 F7: the whole line used to read as unreadable, so a re-install dropped its
    /// schedule and `--pull` with the date). Pure.
    fn of_line(line: &str) -> Option<BackupOpts> {
        if CronKind::of(line) != Some(CronKind::Backup) {
            return None;
        }
        let (schedule, words) = cron_line_parts(line)?;
        let is_assignment = |w: &str| {
            w.split_once('=').is_some_and(|(name, _)| {
                name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        };
        let start_date = words
            .iter()
            .copied()
            .take_while(|w| is_assignment(w))
            .filter_map(|w| w.strip_prefix("ARENA_START_DATE="))
            .find(|d| arena_core::schedule::parse_ymd(d).is_some())
            .map(String::from);
        Some(BackupOpts { schedule, start_date, pull: !words.contains(&"--no-pull") })
    }
}

/// Whether an installed backup line names an `ARENA_START_DATE=` that [`BackupOpts::of_line`]
/// couldn't read (behind a `cd … &&`, an invalid date, in a comment …): its re-install keeps the
/// line's schedule and `--pull` and drops only the date, which [`backup_read_note`] says. Pure.
fn unread_start_date(line: &str) -> bool {
    line.contains("ARENA_START_DATE=") && BackupOpts::of_line(line).is_some_and(|o| o.start_date.is_none())
}

/// What `cron install` says about the installed backup line it rewrites (`backup`; `installed`,
/// and `read`: what [`BackupOpts::of_line`] read of it) — that it couldn't read the line's
/// options at all, so the flags alone decide; or only its start date, dropped while the rest is
/// kept, unless `--start-date` / `--no-start-date` decides that too. `None`: nothing to say. Pure.
fn backup_read_note(backup: bool, installed: Option<&str>, read: Option<&BackupOpts>, f: &BackupFlags) -> Option<String> {
    let line = installed.filter(|_| backup)?;
    match read {
        None => Some(
            "note: couldn't read the installed backup line's options (hand-edited?) — it is rewritten from these flags alone; \
             check the diff"
                .to_string(),
        ),
        Some(_) if f.start_date.is_none() && !f.no_start_date && unread_start_date(line) => Some(
            "note: couldn't read the installed backup line's ARENA_START_DATE (hand-edited?) — the new line has none \
             (--start-date YYYY-MM-DD sets one); its schedule and --pull are kept; check the diff"
                .to_string(),
        ),
        Some(_) => None,
    }
}

/// `cron install`'s backup-line flags, as given.
struct BackupFlags<'a> {
    schedule: Option<&'a str>,
    /// Already trimmed and validated.
    start_date: Option<&'a str>,
    no_start_date: bool,
    pull: bool,
    no_pull: bool,
}

/// The backup line to write: each option from its flag, else from the `installed` line, else
/// the default (every 15 minutes, no start date, git-only). Also returns what was kept from
/// the installed line that differs from the default — for a note, so the operator sees why
/// the new line still pulls or still carries a start date. The fix for a re-install (`cron
/// install --backup` to pick up the flock) silently dropping `ARENA_START_DATE` — every
/// cron tick then failing to label its pull — or the `--pull`. Pure.
fn resolve_backup_opts(installed: Option<&BackupOpts>, f: &BackupFlags) -> (BackupOpts, Vec<String>) {
    let mut kept = Vec::new();
    let schedule = match (f.schedule, installed) {
        (Some(s), _) => s.to_string(),
        (None, Some(i)) => {
            if i.schedule != BACKUP_CRON_SCHEDULE {
                kept.push(format!("schedule `{}`", i.schedule));
            }
            i.schedule.clone()
        }
        (None, None) => BACKUP_CRON_SCHEDULE.to_string(),
    };
    let start_date = match (f.start_date, f.no_start_date, installed) {
        (Some(d), _, _) => Some(d.to_string()),
        (None, true, _) => None,
        (None, false, Some(i)) => {
            if let Some(d) = &i.start_date {
                kept.push(format!("ARENA_START_DATE={d}"));
            }
            i.start_date.clone()
        }
        (None, false, None) => None,
    };
    let pull = match (f.pull, f.no_pull, installed) {
        (true, _, _) => true,
        (false, true, _) => false,
        (false, false, Some(i)) => {
            if i.pull {
                kept.push("--pull".to_string());
            }
            i.pull
        }
        (false, false, None) => false,
    };
    (BackupOpts { schedule, start_date, pull }, kept)
}

/// Arena's block after an install or a targeted remove. Every line of a kind in `touched`
/// goes; each of `add`'s lines takes the place of the first line of its kind (else is
/// appended), so the block keeps its order and a line nobody asked about stays byte for
/// byte — the backup line's `--pull`/`ARENA_START_DATE`, a hand edit. Pure.
fn merge_cron_block(current: &[String], touched: &[CronKind], add: &[(CronKind, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut placed: Vec<CronKind> = Vec::new();
    for line in current {
        match CronKind::of(line) {
            Some(k) if touched.contains(&k) => {
                if !placed.contains(&k) {
                    if let Some((_, new)) = add.iter().find(|(ak, _)| *ak == k) {
                        out.push(new.clone());
                        placed.push(k);
                    }
                }
            }
            _ => out.push(line.clone()),
        }
    }
    for (k, new) in add {
        if !placed.contains(k) {
            out.push(new.clone());
            placed.push(*k);
        }
    }
    out
}

/// The before → after preview of arena's block: `- line` for each line that goes, then the
/// new block with `+ line` for each line that's new and `  line` for each that stays.
/// Counted per copy, not by membership: when duplicates collapse into one (the merge does
/// that), each copy that goes is shown going — the preview never hides a line the write
/// removes. Pure.
fn cron_diff(before: &[String], after: &[String]) -> Vec<String> {
    use std::collections::HashMap;
    let count = |lines: &[String]| {
        let mut m: HashMap<String, usize> = HashMap::new();
        for l in lines {
            *m.entry(l.clone()).or_default() += 1;
        }
        m
    };
    // A copy in `before` stays if `after` still has an unclaimed copy of it.
    let mut in_after = count(after);
    let mut out = Vec::new();
    for l in before {
        match in_after.get_mut(l) {
            Some(n) if *n > 0 => *n -= 1,
            _ => out.push(format!("- {l}")),
        }
    }
    let mut stays = count(before);
    for l in after {
        match stays.get_mut(l) {
            Some(n) if *n > 0 => {
                *n -= 1;
                out.push(format!("  {l}"));
            }
            _ => out.push(format!("+ {l}")),
        }
    }
    out
}

/// What `cron install` schedules (the paths already resolved to absolute ones).
struct CronJob<'a> {
    /// The backup line's schedule.
    schedule: &'a str,
    /// Inline env for the backup line (e.g. `ARENA_START_DATE=… `), already validated.
    env_prefix: &'a str,
    exe: &'a str,
    config: &'a str,
    home: &'a str,
    /// Render the backup line ([`install_backup_line`]).
    backup: bool,
    /// The backup line also runs the rsync file backup.
    pull: bool,
    proxy: bool,
    /// `--snapshot`: the (absolute, validated) directory `fleet.json` is written to.
    snapshot_dir: Option<&'a str>,
    /// `--archive`: the backup archive's two push lines.
    archive: bool,
    /// The state root (`ARENA_STATE_DIR`) those lines run with.
    archive_state_dir: Option<&'a str>,
    /// The logs the installed lines chose themselves, kept ([`kept_logs`]); any other line logs
    /// to arena's default.
    kept_logs: &'a [(CronKind, String)],
}

/// The backup line's default schedule: every 15 minutes.
const BACKUP_CRON_SCHEDULE: &str = "*/15 * * * *";

/// How often the optional public snapshot is rewritten: often enough that the page's
/// "updated N min ago" stays small, cheap (one list call per provider + one details query +
/// one ≤ 3 s TCP probe per pod, all at once — no SSH session), and well inside the page's
/// 10-minute stale banner, so one slow tick never trips it.
const SNAPSHOT_CRON_SCHEDULE: &str = "*/2 * * * *";

/// `cron install --snapshot DIR`'s directory as it goes into the crontab: absolute (cron
/// runs in `$HOME`), an existing directory, and free of `%` and newlines — crontab turns a
/// `%` into a newline, which would silently cut the command short.
fn snapshot_cron_dir(dir: &std::path::Path) -> Result<String> {
    let abs = std::fs::canonicalize(dir).with_context(|| format!("--snapshot {}", dir.display()))?;
    if !abs.is_dir() {
        anyhow::bail!("--snapshot {}: not a directory", dir.display());
    }
    let s = abs.to_str().context("--snapshot: the directory path isn't UTF-8")?.to_string();
    if s.contains('%') || s.contains('\n') {
        anyhow::bail!("--snapshot {s}: a crontab line can't hold `%` or a newline — pick another directory");
    }
    Ok(s)
}

/// How often the optional proxy re-sync runs. Each tick is one list call per provider plus
/// a no-op when nothing changed (the config is only rewritten — and nginx only reloaded —
/// on a real change), so 5 minutes is cheap and bounds how long a moved endpoint stays
/// unrouted.
const PROXY_CRON_SCHEDULE: &str = "*/5 * * * *";

/// cron runs jobs with `PATH=/usr/bin:/bin`, but Debian/Ubuntu install nginx in
/// `/usr/sbin`: without this the default reload (`nginx -t && nginx -s reload`) fails
/// with "nginx: not found" on every tick.
const PROXY_CRON_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The arena-managed crontab lines `cron install` writes for `job`, each with its
/// [`CronKind`] — pure, so the rendering is tested without touching a real crontab.
///
/// - **backup** (when `job.backup`): `pods backup --yes` on `job.schedule`, git-only
///   (`--no-pull`) unless `pull`. `flock -n` like the others: a tick that finds the previous
///   backup still running exits at once instead of stacking a second fleet-wide backup on
///   top of it (each pod's rsync is bounded by `BACKUP_TIMEOUT_SECS`, so a wedged pod
///   delays the next backup by at most that, never forever). `-o` too, unlike the others:
///   the lock is then held by flock itself for exactly as long as arena runs, instead of by
///   an inherited fd — rsync (unlike ssh) doesn't close it, so a stray rsync outliving an
///   arena that was itself killed would otherwise keep every later tick skipping. The
///   inline env (`ARENA_START_DATE=…`) goes before `flock`, which hands it on to arena.
/// - **proxy** (`proxy`): `proxy apply --yes` every 5 minutes. That's safe unattended
///   because the merge is sticky: a provider that fails to list never drops a forward, and
///   if none answers nothing is written. It has its own log so its every-5-minutes chatter
///   doesn't bury the backup's output. `flock -n` makes a tick that finds the previous one
///   still running exit at once instead of piling up (list calls are bounded by
///   `LIST_TIMEOUT`, but an SSH to a remote proxy isn't). Deliberately no `timeout` kill:
///   one landing between the write and the reload would leave a file nginx never loaded,
///   which later ticks would read as up to date.
/// - **snapshot** (`snapshot_dir`): the public dashboard JSON every 2 minutes.
///
/// Each line's lock is in one hidden folder ([`arena_core::cron::DIR`]), and so is its log
/// unless the installed line already logged somewhere else (`kept_logs`: arena never moves a
/// log path someone chose), so they add nothing to the home. Each line recreates those folders
/// before anything else ([`arena_core::cron::mkdir`]): cron's `sh` skips a command whose `>>`
/// or lock points into a missing directory, so a deleted folder would otherwise silently stop
/// the backups.
fn cron_lines(job: &CronJob) -> Vec<(CronKind, String)> {
    use arena_core::cron;
    let CronJob { schedule, env_prefix, exe, config, home, backup, pull, proxy, snapshot_dir, archive, archive_state_dir, kept_logs } = job;
    // A line's `mkdir -p … && `, lock and log: the kept log, else the default. Every line
    // recreates its folders first: cron's `sh` skips a line whose `>>`/lock dir is gone.
    let files = |k: CronKind| {
        let log = kept_logs.iter().find(|(x, _)| *x == k).map_or_else(|| cron::log(home, k.job()), |(_, l)| l.clone());
        (cron::mkdir(home, &log), cron::lock(home, k.job()), log)
    };
    let mut lines = Vec::new();
    if *backup {
        // `pods backup` also rsyncs the home (the file backup); default the cron to git-only
        // (`--no-pull`) since it runs frequently, and let `--pull` opt into the full backup
        // each tick (rsync is incremental, so repeats only move deltas).
        let no_pull = if *pull { "" } else { " --no-pull" };
        let (mkdir, lock, log) = files(CronKind::Backup);
        lines.push((
            CronKind::Backup,
            format!("{schedule} {mkdir}{env_prefix}flock -n -o {lock} {exe} --config {config} pods backup{no_pull} --yes >> {log} 2>&1"),
        ));
    }
    if *proxy {
        let (mkdir, lock, log) = files(CronKind::Proxy);
        lines.push((
            CronKind::Proxy,
            format!("{PROXY_CRON_SCHEDULE} {mkdir}{PROXY_CRON_PATH} flock -n {lock} {exe} --config {config} proxy apply --yes >> {log} 2>&1"),
        ));
    }
    // The public dashboard JSON. Read-only (lists pods, TCP-probes their SSH ports, reads
    // local files), so safe unattended; `flock -n` as above so a tick stuck on a slow
    // provider never piles up, and the write is atomic, so the page always reads a whole
    // file (a killed tick just leaves the previous one, which the page then shows as stale).
    if let Some(dir) = snapshot_dir {
        let out = shell_quote(&format!("{}/fleet.json", dir.trim_end_matches('/')));
        let (mkdir, lock, log) = files(CronKind::Snapshot);
        lines.push((
            CronKind::Snapshot,
            format!("{SNAPSHOT_CRON_SCHEDULE} {mkdir}flock -n {lock} {exe} --config {config} snapshot --public --out {out} >> {log} 2>&1"),
        ));
    }
    // The backup archive's pushes (`archive sync`): snap 7 minutes after each */15 backup tick
    // (the newest work is on the archive ≤ ~30 min after it was written), big hourly. No flock:
    // each tier takes its own lock in-process, so a tick that finds its tier running skips that
    // tier only (and a manual sync is just as safe). `nice`: the control box is also the
    // participants' SSH proxy. Never in the backup line's path: an archive outage can't delay a
    // backup.
    if *archive {
        let state = archive_state_dir.map(|d| format!("ARENA_STATE_DIR={d} ")).unwrap_or_default();
        for (kind, sched, tier) in [(CronKind::ArchiveSnap, ARCHIVE_SNAP_CRON_SCHEDULE, "snap"), (CronKind::ArchiveBig, ARCHIVE_BIG_CRON_SCHEDULE, "big")] {
            let (mkdir, _, log) = files(kind);
            lines.push((
                kind,
                format!(
                    "{sched} {mkdir}{env_prefix}{state}nice -n 10 {exe} --config {config} archive sync --tier {tier} >> {log} 2>&1"
                ),
            ));
        }
    }
    lines
}

/// The value of an inline env assignment (`NAME=value`) among a cron line's leading words
/// (after its schedule and any `mkdir -p <dir> &&`: [`cron_line_parts`]).
pub(crate) fn env_assignment(line: &str, name: &str) -> Option<String> {
    let (_, words) = cron_line_parts(line)?;
    words.iter().take_while(|w| w.contains('=') && !w.starts_with('-')).find_map(|w| w.strip_prefix(&format!("{name}="))).map(String::from)
}

/// A crontab line's schedule and its command's words, after the `mkdir -p <dir>… &&` arena's
/// lines start with ([`arena_core::cron::line_parts`]). Pure.
fn cron_line_parts(line: &str) -> Option<(String, Vec<&str>)> {
    arena_core::cron::line_parts(line)
}

/// When the archive's snap push runs: 7 minutes after each */15 backup tick.
const ARCHIVE_SNAP_CRON_SCHEDULE: &str = "7,22,37,52 * * * *";
/// When the archive's big push runs: hourly — its cadence is this line.
const ARCHIVE_BIG_CRON_SCHEDULE: &str = "40 * * * *";

/// The auto-reaper's cron line for `cron install --reap observe|apply`
/// (`arena_core::reap::render::cron_line`). Refuses `apply` unless the config FILE says
/// `REAP_ENABLED=1` (observe first, review the log, then switch it on); needs valid REAP_*
/// settings and an explicit, absolute ARENA_STATE_DIR in the config FILE (the reaper reads the
/// file's only — RS3 — so a hold set from any shell lands where the cron looks).
/// ARENA_START_DATE is written into the line when it comes from the environment rather than the
/// file (cron doesn't inherit a shell's environment). `log`: the log the installed reap line
/// chose, kept ([`kept_logs`]).
pub(crate) fn reap_cron_line(apply: bool, config_path: &std::path::Path, exe: &str, cfg_abs: &str, home: &str, log: Option<&str>) -> Result<String> {
    let text = std::fs::read_to_string(config_path).with_context(|| format!("reading {}", config_path.display()))?;
    let file = Config::parse(&text);
    let cfg = Config::load(config_path)?;
    if apply && file.get("REAP_ENABLED").map(str::trim) != Some("1") {
        anyhow::bail!(
            "refusing `--reap apply`: REAP_ENABLED isn't 1 in {cfg_abs} — install `--reap observe` first, review \
             `arena pods reap log` (its would_reap lines), then set REAP_ENABLED=1 in the config file"
        );
    }
    arena_core::reap::ReapSettings::from_config(&cfg).map_err(|e| anyhow::anyhow!("--reap: {e}"))?;
    arena_core::snapshot::state_dir_explicit(&cfg).map_err(|e| anyhow::anyhow!("--reap: {e}"))?;
    let mut env = Vec::new();
    let key = "ARENA_START_DATE";
    if let (None, Some(v)) = (file.get(key), cfg.get(key).map(str::trim).filter(|v| !v.is_empty())) {
        if arena_core::schedule::parse_ymd(v).is_none() {
            anyhow::bail!("--reap: ARENA_START_DATE must be YYYY-MM-DD (got {v})");
        }
        if v.chars().any(|c| c.is_whitespace() || c == '%' || c == '\'' || c == '"') {
            anyhow::bail!("--reap: {key} can't hold whitespace, quotes or `%` in a crontab line ({v})");
        }
        env.push((key.to_string(), v.to_string()));
    }
    for (what, v) in [("the arena binary", exe), ("the config path", cfg_abs), ("HOME", home)] {
        if v.contains('%') || v.contains('\n') {
            anyhow::bail!("--reap: {what} ({v}) can't hold `%` or a newline in a crontab line");
        }
    }
    Ok(arena_core::reap::render::cron_line(apply, exe, cfg_abs, home, log, &env))
}

/// When the daily health check runs (`cron install --health HH:MM`): cron's time, this box's
/// zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HealthAt {
    hour: u32,
    minute: u32,
}

impl HealthAt {
    /// `--health`'s `HH:MM` (or `H:MM`): an hour 0–23 and two-digit minutes 00–59 — `07:05` and
    /// `7:05`, never `7:5` (07:05 or 07:50?), `25:00` or `07:60`. Pure.
    fn parse(s: &str) -> Result<HealthAt> {
        let bad = || anyhow::anyhow!("--health takes HH:MM in this box's time zone, e.g. 07:30 (got {s:?})");
        let (h, m) = s.trim().split_once(':').ok_or_else(bad)?;
        let digits = |x: &str, len: std::ops::RangeInclusive<usize>| len.contains(&x.len()) && x.bytes().all(|b| b.is_ascii_digit());
        if !digits(h, 1..=2) || !digits(m, 2..=2) {
            return Err(bad());
        }
        let (hour, minute): (u32, u32) = (h.parse()?, m.parse()?);
        if hour > 23 || minute > 59 {
            return Err(bad());
        }
        Ok(HealthAt { hour, minute })
    }

    /// The time an installed health line runs at, read back from its schedule — `M H * * *`, as
    /// [`health_cron_line`] writes it; `None` for any other (a hand edit). Pure.
    fn of_line(line: &str) -> Option<HealthAt> {
        let (schedule, _) = cron_line_parts(line)?;
        match schedule.split(' ').collect::<Vec<_>>().as_slice() {
            [m, h, "*", "*", "*"] => HealthAt::parse(&format!("{h}:{m:0>2}")).ok(),
            _ => None,
        }
    }
}

impl std::fmt::Display for HealthAt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

/// The daily health check's line (`cron install --health HH:MM`): `pods test --deep` once a day
/// at `at`, in this box's time zone (cron's). Read-only, so no `--yes`; and it never reaches
/// outside the cohort — a deep check that names no pod takes this cohort's ([`cohort_only`]), so
/// a staff box running someone's job never gets its CUDA, NCCL and download checks. Under `flock
/// -n -o` like the backup line: a check still running never gets a second one beside it, and no
/// ssh it started can hold the lock. `ARENA_STATE_DIR` goes into the line, as the archive's does
/// (AD10): the verdicts land in the health cache this shell's dashboard and `snapshot` read,
/// whatever cron's environment lacks. It exits non-zero when a pod FAILs — the log has the table.
/// `log`: the log the installed health line chose, kept ([`kept_logs`]). Pure.
fn health_cron_line(at: HealthAt, exe: &str, config: &str, home: &str, state_root: &str, log: Option<&str>) -> String {
    use arena_core::cron::{self, Job};
    let log = log.map_or_else(|| cron::log(home, Job::Health), String::from);
    format!(
        "{} {} * * * {}ARENA_STATE_DIR={state_root} flock -n -o {} {exe} --config {config} pods test --deep >> {log} 2>&1",
        at.minute,
        at.hour,
        cron::mkdir(home, &log),
        cron::lock(home, Job::Health)
    )
}

/// How a health line's time reads (`cron install`'s note, `cron show`): `daily at 07:20, this
/// box's time zone (BST +0100)`, the zone as [`box_zone`] read it — and, when the crontab sets a
/// zone of its own (`set`: [`cron_tz`]), that a cron honouring it runs the line there instead
/// (cronie reads `CRON_TZ`; Debian's cron doesn't), so the time said is never silently wrong. Pure.
fn health_when(at: HealthAt, zone: Option<&str>, set: Option<(&str, &str)>) -> String {
    let mut out = match zone {
        Some(z) => format!("daily at {at}, this box's time zone ({z})"),
        None => format!("daily at {at}, this box's time zone"),
    };
    if let Some((name, value)) = set {
        out.push_str(&format!(" — this crontab sets {name}={value}: a cron that reads it runs the line at {at} there instead"));
    }
    out
}

/// The warning for a health line on one of the reaper's minutes
/// ([`arena_core::reap::render::CRON_SCHEDULE`]): its look then lands during the deep check every
/// day, and its window checks (CPU, network, GPU) read the check as use — measured live, the check
/// is otherwise invisible to it (no file in the home changes after a pod's first CUDA use, and its
/// CPU and download stay under the cumulative thresholds). A pod idle for a day or more would
/// then never be reaped. `None` on any other minute. Pure.
fn health_minute_warning(at: HealthAt) -> Option<String> {
    let reap = arena_core::reap::render::CRON_SCHEDULE.split(' ').next().unwrap_or_default();
    let minutes: Vec<u32> = reap.split(',').filter_map(|m| m.parse().ok()).collect();
    minutes.contains(&at.minute).then(|| {
        let shown = minutes.iter().map(|m| format!(":{m:02}")).collect::<Vec<_>>().join("/");
        format!(
            "warning: the reaper looks at {shown}: a deep check at :{:02} reads as use to it every day, so a pod idle for a \
             day or more is never reaped — pick another minute",
            at.minute
        )
    })
}

/// The time zone a crontab sets for the health line, if any: a setting on a line of its own
/// (not a comment, not inside a job's command), quotes dropped, from the lines **before** the
/// health line (a setting applies to the lines after it; with no health line yet — `install`
/// appends it — the whole crontab). `CRON_TZ` (cronie's scheduling zone) wins over `TZ` (the
/// commands' environment), each the last one before the line. Pure.
fn cron_tz(crontab: &str) -> Option<(&'static str, &str)> {
    let before: Vec<&str> = crontab.lines().take_while(|l| CronKind::of(l) != Some(CronKind::Health)).collect();
    let last = |want: &'static str| {
        before.iter().rev().find_map(|l| {
            let (name, value) = l.trim_start().split_once('=')?;
            (name.trim_end() == want).then_some(())?;
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            (!value.is_empty() && !value.contains(char::is_whitespace)).then_some((want, value))
        })
    };
    last("CRON_TZ").or_else(|| last("TZ"))
}

/// This box's time zone as cron reads it — the system's, so `date` without this shell's `TZ` —
/// as `BST +0100`. `None` when `date` can't say.
fn box_zone() -> Option<String> {
    let out = std::process::Command::new("date").env_remove("TZ").arg("+%Z %z").stdin(std::process::Stdio::null()).output().ok()?;
    let zone = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !zone.is_empty()).then_some(zone)
}

/// Whether this user's crontab has the reap line — `config check` then shows the reaper's
/// readiness in full (observe mode needs it too). Unreadable (no crontab, no `crontab`): no.
pub(crate) fn reap_cron_installed() -> bool {
    std::process::Command::new(CRONTAB)
        .arg("-l")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| CronKind::of(l) == Some(CronKind::Reap)))
}

/// Replace the crontab with `content` via `crontab -` (reads from stdin).
async fn write_crontab(content: &str, crontab: &str) -> Result<()> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut child = Command::new(crontab)
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .context("spawning `crontab -`")?;
    child
        .stdin
        .as_mut()
        .context("opening crontab stdin")?
        .write_all(content.as_bytes())
        .await
        .context("writing crontab")?;
    let status = child.wait().await.context("waiting for crontab")?;
    if !status.success() {
        anyhow::bail!("`crontab -` exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
