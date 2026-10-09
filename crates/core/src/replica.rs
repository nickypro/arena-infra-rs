//! Replication bookkeeping for `pods replace` / `pods migrate` — the copy of a participant's
//! home from the pod they use (`<name>`) onto its replacement (`<name>-new`).
//!
//! The copy is a **replica**, not a backup: a file the participant deleted on the original
//! must not come back on the new pod (live finding #20), so it mirrors deletions
//! ([`crate::pull::Mirror::Replica`]). But the new pod can hold work of its own — a
//! cutover, a day's work on it, then a revert and a re-copy overwrote that work in place
//! (live finding #7, data loss). So nothing on the receiver is ever destroyed by the
//! transfer itself: whatever it overwrites or deletes there is *moved* into a per-sync folder
//! (`~/arena-sync-kept/<stamp>/`, rsync `--backup-dir`), and afterwards this module's
//! [`settle_command`] sorts that folder on the pod. What it may drop is decided by what the
//! receiver itself recorded, never guessed from a file's own times (which `tar`, `unzip`,
//! `cp -p`, `mv` and `wget` all set to the past):
//!
//! - before each sync, [`receiver_prepare_command`] lists every file on the receiver whose
//!   **ctime** is newer than its last-sync stamp (`~/.arena-last-sync`) — created, edited,
//!   extracted, copied or renamed there since: a ctime can't be set by a user tool;
//! - after a COMPLETE sync ([`SettleMode::Advance`]), a moved file goes only when it is on
//!   neither side of that line — not on the list and not modified (mtime) since the stamp:
//!   what an earlier sync (or the image / setup, before the first one) put there, which the
//!   original deliberately deleted or has since changed. Everything else is KEPT where it
//!   was moved, counted and named in the report, for a human to reconcile;
//! - a sync that didn't complete ([`SettleMode::Keep`]) deletes nothing and leaves the
//!   baseline where it was. A receiver without a stamp (made by an older version of this
//!   tool) keeps everything that's moved.
//!
//! The kept folder is a **visible** folder in the home (not a dot-dir): the backup tiers
//! (`pods pull`) and a later replication of that pod carry it like any other work, and a
//! mirroring sync never deletes it (rsync protect rules, [`crate::pull::Mirror::Replica`]).
//! The repo's own moved files land beside its real directory first (on its volume, so rsync
//! moves them with a rename) and the tidy-up then brings them into the home's folder, under
//! the repo's place there.
//!
//! A mirror is only safe while the ORIGINAL still holds what the last sync copied from it.
//! After a container reset (RunPod resets them on its own; a stop discards the disk; a
//! started pod comes back as the image) the original's home is the bare image again, and a
//! mirror would delete every file the participant had on the new pod (review finding: the
//! data loss this module exists to prevent). So the source keeps a record of each completed
//! sync, by receiver, in a dot-dir (`~/.arena-sync/sent-to`: never backed up, restored or
//! replicated, and gone with the home on a reset); [`mirror_refusal`] refuses a re-sync onto
//! a receiver with a stamp unless that record is there and the source's container
//! ([`crate::volume::CONTAINER_MARK`]) is older than the last sync.
//!
//! Every step on the receiver — the prepare, the transfer (`--rsync-path`) and the tidy-up —
//! first checks, in the same SSH session, that the endpoint answers as the expected pod
//! ([`identity_condition`] / [`guarded`]): an endpoint can be reassigned to another pod at
//! any time, and a mirror onto the wrong one would move a stranger's home aside.
//!
//! Also here: the local-staging checks of the via-local copy (the fallback that pulls the
//! home onto the control machine first) — how much it would stage and whether the disk has
//! room for it, so it never fills a box it shares with a live cohort (live finding #13).

use crate::pull::Mirror;

/// The receiver's last-sync stamp (a file in its home; its mtime is the baseline).
pub const LAST_SYNC: &str = ".arena-last-sync";

/// Where a sync moves what it overwrites or deletes on the receiver, per sync:
/// `~/<dir>/<stamp>/` — a VISIBLE folder, so the backups and a later replication carry what
/// is kept there; a mirroring sync is told never to delete it ([`crate::pull::Mirror`]).
pub const KEPT_DIR: &str = "arena-sync-kept";

/// The bookkeeping dot-dir in the home (on both sides): the receiver's per-sync list of what
/// changed on it, the source's record of its syncs. A dot-dir, so no transfer, backup or
/// restore ever carries it — a record that came back from a backup would hide a reset.
pub const BOOK_DIR: &str = ".arena-sync";

/// On the source, in [`BOOK_DIR`]: one receiver pod id per line, each a completed sync.
pub const SENT_TO: &str = "sent-to";

/// The exit status of a step refused because the endpoint isn't the expected pod.
pub const WRONG_POD_EXIT: i32 = 97;

/// Where a RunPod container's own id is readable: PID 1's environment (`RUNPOD_POD_ID` isn't
/// in a non-interactive SSH session's env).
pub const POD_ID_ENVIRON: &str = "/proc/1/environ";

/// How long the receiver's prepare and tidy-up may take: each walks the home (and the repo,
/// on a slow network volume perhaps), and the tidy-up may move the repo's kept files off it.
pub const BOOKKEEPING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// A sync's folder name: its UTC start time, sortable (`20261008T110203Z`).
pub fn sync_stamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let (y, m, d) = crate::schedule::civil_from_days(days);
    let s = unix_secs % 86_400;
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", s / 3600, s % 3600 / 60, s % 60)
}

/// The mirror for the home transfer of the sync `stamp`: moved files go to
/// `~/arena-sync-kept/<stamp>/` (rsync's relative `--backup-dir` is the receiving dir's).
pub fn home_mirror(stamp: &str) -> Mirror {
    Mirror::Replica { backup_dir: format!("{KEPT_DIR}/{stamp}") }
}

/// The `--backup-dir` for a transfer into `<rel>/` (the repo's tree, carried on its own —
/// [`crate::volume::pod_to_pod_copy`]) when the home transfer's is `home_backup_dir`: beside
/// the repo's REAL directory (`../`), under its name — on the same filesystem as the repo
/// (its `/workspace` volume, when it's linked there), so rsync moves the files with a rename.
/// [`settle_command`] then brings what it keeps into the home's folder. `None` for an
/// unusable `rel`.
pub fn tree_backup_dir(home_backup_dir: &str, rel: &str) -> Option<String> {
    let name = rel_name(rel)?;
    Some(format!("../{home_backup_dir}/{name}"))
}

/// The last component of a home-relative `rel` (`work/ARENA_materials/` → `ARENA_materials`).
fn rel_name(rel: &str) -> Option<&str> {
    rel.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty() && *n != "." && *n != "..")
}

/// Single-quote for a POSIX shell.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `script` as one `sh -c` command: the login shell may be zsh (setup can make it so), whose
/// unmatched globs abort a command — these scripts are POSIX sh.
fn posix_sh(script: &str) -> String {
    format!("sh -c {}", sq(script))
}

/// A shell condition that holds only on the RunPod pod `pod_id`: its `RUNPOD_POD_ID`, read
/// from `environ` ([`POD_ID_ENVIRON`] on a pod; a temp file in tests). An unreadable or
/// empty id fails it: couldn't confirm = not the pod.
pub fn identity_condition(environ: &str, pod_id: &str) -> String {
    format!(
        "[ \"$(tr '\\0' '\\n' < {} 2>/dev/null | sed -n 's/^RUNPOD_POD_ID=//p')\" = {} ]",
        sq(environ),
        sq(pod_id)
    )
}

/// `script` preceded by `cond` (when there is one): on any other pod it says so on stderr
/// and exits [`WRONG_POD_EXIT`] having done nothing. Checked in the same session as the
/// work, so a reassigned endpoint can't slip in between a check and the step.
pub fn guarded(cond: Option<&str>, script: &str) -> String {
    match cond {
        None => script.to_string(),
        Some(c) => format!(
            "if ! {c}; then echo 'arena: this endpoint is not the expected pod (its ip:port was reassigned?) - refusing' >&2; \
             exit {WRONG_POD_EXIT}; fi\n{script}"
        ),
    }
}

/// rsync's `--rsync-path` for a transfer whose remote side must be the pod `cond` names:
/// the remote rsync only starts when the condition holds there, so a transfer that reaches a
/// reassigned endpoint fails having written (or read) nothing.
pub fn guarded_rsync_path(cond: &str) -> String {
    format!(
        "{{ {cond} || {{ echo 'arena: the remote end is not the expected pod (its ip:port was reassigned?) - refusing' >&2; \
         exit {WRONG_POD_EXIT}; }}; }} && rsync"
    )
}

/// What the source said before a sync ([`source_prepare_command`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceState {
    /// Its record holds a completed sync to this receiver (since its home was last reset).
    pub synced_to_dest: bool,
    /// When its container (its disk) was created — its last reset; `None` = unknown (a VM,
    /// or unreadable).
    pub container_created: Option<u64>,
}

/// Run on the SOURCE before a sync to `dest_id` (as one `sh -c`, behind `cond`): plant the
/// delivery marker (`marker` — a path like `$HOME/.arena_replace_marker`, expanded there —
/// holding `token`), and say whether its record holds a completed sync to `dest_id` and when
/// its container was created (`container_mark`: [`crate::volume::CONTAINER_MARK`]).
pub fn source_prepare_command(cond: Option<&str>, marker: &str, token: &str, dest_id: &str, container_mark: &str) -> String {
    let script = format!(
        "printf %s {token} > \"{marker}\" || exit 1\n\
         if grep -qxF -- {dest} \"$HOME/{BOOK_DIR}/{SENT_TO}\" 2>/dev/null; then echo arena-replica-sent=yes; else echo arena-replica-sent=no; fi\n\
         if [ -e {mark} ]; then t=$(date -r {mark} +%s 2>/dev/null) && echo \"arena-replica-container=$t\"; fi\n\
         true\n",
        token = sq(token),
        dest = sq(dest_id),
        mark = sq(container_mark),
    );
    posix_sh(&guarded(cond, &script))
}

/// [`source_prepare_command`]'s answer; `None` when it didn't run (no `sent` line).
pub fn parse_source_state(stdout: &str) -> Option<SourceState> {
    let mut out = SourceState::default();
    let mut seen = false;
    for line in stdout.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("arena-replica-sent=") {
            out.synced_to_dest = v == "yes";
            seen = true;
        } else if let Some(v) = line.strip_prefix("arena-replica-container=") {
            out.container_created = v.parse().ok();
        }
    }
    seen.then_some(out)
}

/// Run on the SOURCE after a sync to `dest_id` (behind `cond`): remove the delivery marker
/// and — when the sync `landed` — record it ([`SENT_TO`]), which the next sync's
/// [`mirror_refusal`] asks for.
pub fn source_finish_command(cond: Option<&str>, marker: &str, dest_id: &str, landed: bool) -> String {
    let mut script = format!("rm -f \"{marker}\"\n");
    if landed {
        script.push_str(&format!(
            "mkdir -p \"$HOME/{BOOK_DIR}\" && {{ grep -qxF -- {d} \"$HOME/{BOOK_DIR}/{SENT_TO}\" 2>/dev/null || echo {d} >> \"$HOME/{BOOK_DIR}/{SENT_TO}\"; }}\n",
            d = sq(dest_id)
        ));
    }
    posix_sh(&guarded(cond, &script))
}

/// Run on the RECEIVER before the sync `stamp` (as one `sh -c`, behind `cond`): on a pod the
/// copy has just created (`fresh`) start its baseline — everything on it so far is image +
/// setup; then, when it has a stamp, say when (`arena-last-sync=`) and list (into
/// `~/.arena-sync/changed-<stamp>`) every file whose ctime is newer than it — in the home,
/// and through `rel` (the repo's place, perhaps a link onto the volume) — for
/// [`settle_command`]. Ends with `arena-replica-ready` ([`parse_prepared`]).
pub fn receiver_prepare_command(cond: Option<&str>, stamp: &str, rel: Option<&str>, fresh: bool) -> String {
    let baseline = if fresh { "touch \"$s\" || exit 1\n" } else { "" };
    let tree = match rel.filter(|r| rel_name(r).is_some()).map(|r| r.trim_end_matches('/')) {
        Some(rel) => format!(
            "  (cd \"$HOME\"/{r}/ 2>/dev/null && find . -type f -cnewer \"$s\" -print | P={p} awk '{{ sub(/^\\.\\//, \"\"); print ENVIRON[\"P\"] $0 }}')\n",
            r = sq(rel),
            p = sq(&format!("{rel}/")),
        ),
        None => String::new(),
    };
    let script = format!(
        "mkdir -p \"$HOME/{BOOK_DIR}\" || exit 1\n\
         s=\"$HOME/{LAST_SYNC}\"\n\
         {baseline}\
         if [ -f \"$s\" ]; then\n\
         \x20 echo \"arena-last-sync=$(stat -c %Y \"$s\")\"\n\
         \x20 {{ (cd \"$HOME\" && find . \\( -path ./{BOOK_DIR} -o -path ./{KEPT_DIR} \\) -prune -o -type f -cnewer \"$s\" -print)\n\
         {tree}\
         \x20 }} | sed 's|^\\./||' > \"$HOME/{BOOK_DIR}/changed-{stamp}\" || exit 1\n\
         fi\n\
         echo arena-replica-ready\n"
    );
    posix_sh(&guarded(cond, &script))
}

/// What [`receiver_prepare_command`] said.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Prepared {
    /// The receiver's last-sync time (UNIX secs); `None` = no stamp.
    pub last_sync: Option<u64>,
}

/// [`receiver_prepare_command`]'s answer; `None` unless it got to the end.
pub fn parse_prepared(stdout: &str) -> Option<Prepared> {
    let lines: Vec<&str> = stdout.lines().map(str::trim).collect();
    lines.contains(&"arena-replica-ready").then(|| Prepared { last_sync: parse_last_sync(stdout) })
}

/// Why a mirroring sync onto a receiver must not run (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorRefusal {
    /// The source's container was created (reset) after the receiver's last sync.
    SourceReset { container: u64, last_sync: u64 },
    /// The source holds no record of a completed sync to this receiver: its home was reset
    /// or rebuilt since (the record goes with it), or the receiver was synced by an older
    /// version of this tool.
    NoRecord { last_sync: u64 },
}

/// Whether a mirroring sync onto the receiver may run (pure). A receiver the copy has just
/// created (`fresh`: image + setup only) or one without a stamp (its tidy-up deletes nothing)
/// can't lose work to it. One with a stamp holds a copy of the source as it was at that sync,
/// plus perhaps work of its own; a mirror would delete whatever of it the source no longer
/// has — right when the participant deleted it, a disaster when the source was RESET since. So
/// the source must show it still is what was synced: its container older than the stamp (when
/// known), and its record of that sync there.
pub fn mirror_refusal(fresh: bool, last_sync: Option<u64>, src: &SourceState) -> Option<MirrorRefusal> {
    let last_sync = last_sync.filter(|_| !fresh)?;
    // Not older = not known to predate it (the same second is a reset right after it).
    if let Some(container) = src.container_created.filter(|&c| c >= last_sync) {
        return Some(MirrorRefusal::SourceReset { container, last_sync });
    }
    (!src.synced_to_dest).then_some(MirrorRefusal::NoRecord { last_sync })
}

/// Whether [`settle_command`] may drop moved files and move the baseline on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleMode {
    /// The sync completed and was verified: the receiver now matches the original — drop
    /// the moved files nobody changed there (see the module doc), new baseline.
    Advance,
    /// The sync failed part-way (or wasn't verified): keep EVERYTHING it moved, and keep the
    /// old baseline (files it didn't reach keep their place and must not look old next time).
    Keep,
}

/// The tidy-up + report run on the receiver after the sync `stamp` (one `sh -c`, behind
/// `cond`; see the module doc). `rel` = the repo's place in the home when its tree was
/// carried on its own (its moved files sit beside its real directory, and are brought into
/// `~/arena-sync-kept/<stamp>/<rel>/`, as are any an earlier tidy-up didn't get to). Prints
/// `arena-replica-*` lines ([`parse_settle`]).
pub fn settle_command(cond: Option<&str>, stamp: &str, rel: Option<&str>, mode: SettleMode) -> String {
    let tree = match rel.and_then(|r| rel_name(r).map(|n| (r.trim_end_matches('/'), n))) {
        // The repo's real directory (through a link); unless its kept folder IS the home's
        // (a plain directory at the home's top), tidy this sync's beside it, then move every
        // one there into the home's folder.
        Some((rel, name)) => format!(
            "rel={r}; name={n}\n\
             p=$(cd \"$HOME\"/\"$rel\" 2>/dev/null && pwd -P)\n\
             if [ -n \"$p\" ] && [ \"${{p%/*}}\" != \"$h\" ]; then\n\
             \x20 R=\"${{p%/*}}/{KEPT_DIR}\"\n\
             \x20 tidy \"$R/{stamp}/$name\" \"$rel/\"\n\
             \x20 for t in \"$R\"/*/\"$name\"; do\n\
             \x20   [ -d \"$t\" ] || continue\n\
             \x20   st=\"${{t%/*}}\"; st=\"${{st##*/}}\"\n\
             \x20   d=\"$h/{KEPT_DIR}/$st/$rel\"\n\
             \x20   [ -e \"$d\" ] && d=\"$d.moved-$$\"\n\
             \x20   if mkdir -p \"${{d%/*}}\" && mv \"$t\" \"$d\"; then\n\
             \x20     rmdir \"${{t%/*}}\" 2>/dev/null\n\
             \x20     [ \"$st\" = {stamp} ] || set -- \"$@\" \"$d\"\n\
             \x20   else\n\
             \x20     set -- \"$@\" \"$t\"\n\
             \x20   fi\n\
             \x20 done\n\
             \x20 rmdir \"$R\" 2>/dev/null\n\
             fi\n",
            r = sq(rel),
            n = sq(name),
        ),
        None => String::new(),
    };
    let (del, advance) = match mode {
        SettleMode::Advance => (1, "touch \"$s\"\n"),
        SettleMode::Keep => (0, ""),
    };
    let script = format!(
        "s=\"$HOME/{LAST_SYNC}\"\n\
         L=\"$HOME/{BOOK_DIR}/changed-{stamp}\"\n\
         h=$(cd \"$HOME\" && pwd -P) || exit 1\n\
         K=\"$h/{KEPT_DIR}/{stamp}\"\n\
         del={del}\n\
         tidy() {{\n\
         \x20 [ -d \"$1\" ] || return 0\n\
         \x20 if [ \"$del\" = 1 ] && [ -f \"$s\" ] && [ -f \"$L\" ]; then\n\
         \x20   (cd \"$1\" && find . -type f ! -newer \"$s\" -print) | P=\"$2\" L=\"$L\" awk 'BEGIN {{ while ((getline k < ENVIRON[\"L\"]) > 0) keep[k] = 1 }} {{ sub(/^\\.\\//, \"\"); if (!((ENVIRON[\"P\"] $0) in keep)) print }}' |\n\
         \x20     while IFS= read -r f; do rm -f -- \"$1/$f\"; done\n\
         \x20 fi\n\
         \x20 find \"$1\" -depth -type d -empty -exec rmdir {{}} \\; 2>/dev/null\n\
         \x20 return 0\n\
         }}\n\
         tidy \"$K\" \"\"\n\
         set -- \"$K\"\n\
         {tree}\
         rm -f \"$HOME/{BOOK_DIR}\"/changed-*\n\
         {advance}\
         rmdir \"$h/{KEPT_DIR}\" 2>/dev/null\n\
         n=0\n\
         for d in \"$@\"; do [ -d \"$d\" ] && n=$((n + $(find \"$d\" -type f | wc -l))); done\n\
         echo \"arena-replica-kept=$n\"\n\
         for d in \"$@\"; do\n\
         \x20 [ -d \"$d\" ] || continue\n\
         \x20 echo \"arena-replica-dir=$d $(du -sh \"$d\" 2>/dev/null | cut -f1)\"\n\
         \x20 find \"$d\" -type f ! -path '*/.git/*' | head -n 5 | while IFS= read -r f; do echo \"arena-replica-file=${{f#\"$d\"/}}\"; done\n\
         done\n\
         true\n"
    );
    posix_sh(&guarded(cond, &script))
}

/// What [`settle_command`] reported: files kept, where (`dir size` per folder), and up to
/// five per folder by name (outside `.git`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settled {
    pub kept: u64,
    pub dirs: Vec<String>,
    pub files: Vec<String>,
}

/// Read [`settle_command`]'s output; `None` when it didn't run (no `kept` line).
pub fn parse_settle(stdout: &str) -> Option<Settled> {
    let mut out = Settled::default();
    let mut seen = false;
    for line in stdout.lines() {
        if let Some(n) = line.strip_prefix("arena-replica-kept=") {
            out.kept = n.trim().parse().ok()?;
            seen = true;
        } else if let Some(d) = line.strip_prefix("arena-replica-dir=") {
            out.dirs.push(d.trim().to_string());
        } else if let Some(f) = line.strip_prefix("arena-replica-file=") {
            out.files.push(f.to_string());
        }
    }
    seen.then_some(out)
}

/// The report lines for `pod` (the receiver), pure. After a complete sync: nothing when
/// nothing was kept; else how many files it held that the sync replaced or deleted and that
/// had changed on it since its last sync — kept, where, and a few by name. After one that
/// didn't complete ([`SettleMode::Keep`]): that nothing it moved was removed, and where.
pub fn settle_lines(pod: &str, s: &Settled, mode: SettleMode) -> Vec<String> {
    if s.kept == 0 {
        return Vec::new();
    }
    let mut out = vec![match mode {
        SettleMode::Advance => format!(
            "⚠ {} file(s) on {pod} had changed there since its last sync and were replaced or deleted by this \
             one — KEPT, moved to: {}",
            s.kept,
            s.dirs.join(", ")
        ),
        SettleMode::Keep => format!(
            "⚠ the sync didn't complete, so nothing it replaced or deleted on {pod} was removed: {} file(s) KEPT in {} \
             (older copies of the original's files among them)",
            s.kept,
            s.dirs.join(", ")
        ),
    }];
    for f in &s.files {
        out.push(format!("    {f}"));
    }
    out.push(format!(
        "  (a visible folder in {pod}'s home, backed up and carried like the rest of it. Compare and restore \
         what's wanted — the copy now in place is the original's — then delete the folder.)"
    ));
    out
}

/// Prints the receiver's last-sync time (`arena-last-sync=<unix secs>`), or nothing.
pub fn last_sync_command() -> String {
    format!("[ -f \"$HOME/{LAST_SYNC}\" ] && echo \"arena-last-sync=$(stat -c %Y \"$HOME/{LAST_SYNC}\")\"; true")
}

/// [`last_sync_command`]'s answer.
pub fn parse_last_sync(stdout: &str) -> Option<u64> {
    stdout.lines().find_map(|l| l.trim().strip_prefix("arena-last-sync=")).and_then(|v| v.trim().parse().ok())
}

/// The bytes a dry-run `rsync --stats` would transfer (`Total transferred file size:`),
/// with or without `--human-readable` units (`3.10G`, `1,234,567`, `812.33K`; rsync's
/// units are powers of 1000). `None` when the line is missing or unreadable.
pub fn transfer_bytes(stats: &str) -> Option<u64> {
    let rest = stats.lines().find_map(|l| l.trim().strip_prefix("Total transferred file size:"))?;
    let token = rest.split_whitespace().next()?;
    let (num, mult) = match token.chars().last()? {
        c if c.is_ascii_digit() => (token, 1u64),
        'K' | 'k' => (&token[..token.len() - 1], 1_000),
        'M' => (&token[..token.len() - 1], 1_000_000),
        'G' => (&token[..token.len() - 1], 1_000_000_000),
        'T' => (&token[..token.len() - 1], 1_000_000_000_000),
        'P' => (&token[..token.len() - 1], 1_000_000_000_000_000),
        _ => return None,
    };
    if mult == 1 {
        // Plain (level 1): digits with thousands separators, either ',' or '.'.
        return num.chars().filter(char::is_ascii_digit).collect::<String>().parse().ok();
    }
    let v: f64 = num.replace(',', ".").parse().ok()?;
    (v.is_finite() && v >= 0.0).then(|| (v * mult as f64).round() as u64)
}

/// What the local staging must leave free on its disk: a tenth of it, at least 5 GB — room
/// for everything else on a machine the live cohort's proxy and backups share.
pub fn staging_reserve(total: u64) -> u64 {
    (total / 10).max(5_000_000_000)
}

/// Whether staging `needed` bytes fits on a disk with `free` of `total` bytes, keeping
/// [`staging_reserve`] free. `Err` says why not (in GB). Pure.
pub fn staging_room(needed: u64, free: u64, total: u64) -> Result<(), String> {
    let reserve = staging_reserve(total);
    let gb = |b: u64| format!("{:.1} GB", b as f64 / 1e9);
    if needed.saturating_add(reserve) <= free {
        return Ok(());
    }
    Err(format!(
        "staging needs ~{} but its disk has {} free and {} must stay free",
        gb(needed),
        gb(free),
        gb(reserve)
    ))
}

/// `(free, total)` bytes of the filesystem holding `path` (`statvfs`; free = what an
/// unprivileged process may use). `None` when it can't be read.
#[cfg(unix)]
pub fn disk_space(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a properly sized out-param.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some((st.f_bavail as u64 * frsize, st.f_blocks as u64 * frsize))
}

#[cfg(not(unix))]
pub fn disk_space(_path: &std::path::Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    #[test]
    fn stamps_are_utc_and_sortable() {
        assert_eq!(sync_stamp(0), "19700101T000000Z");
        assert_eq!(sync_stamp(1_791_457_323), "20261008T110203Z");
        assert!(sync_stamp(1_791_457_323) < sync_stamp(1_791_457_324));
    }

    #[test]
    fn backup_dirs_for_the_home_and_the_tree() {
        assert_eq!(home_mirror("S"), Mirror::Replica { backup_dir: "arena-sync-kept/S".into() });
        assert_eq!(tree_backup_dir("arena-sync-kept/S", "ARENA_materials").as_deref(), Some("../arena-sync-kept/S/ARENA_materials"));
        assert_eq!(tree_backup_dir("b", "work/ARENA_materials/").as_deref(), Some("../b/ARENA_materials"));
        for bad in ["", "/", "..", "a/.."] {
            assert_eq!(tree_backup_dir("b", bad), None, "{bad:?}");
        }
    }

    #[test]
    fn transfer_bytes_reads_every_rsync_number_style() {
        let line = |v: &str| format!("Number of files: 9\nTotal transferred file size: {v} bytes\nLiteral data: 0\n");
        for (v, want) in [
            ("1,234,567", Some(1_234_567)),
            ("1.234.567", Some(1_234_567)),
            ("0", Some(0)),
            ("3.10G", Some(3_100_000_000)),
            ("184.32M", Some(184_320_000)),
            ("812.33K", Some(812_330)),
            ("1,5G", Some(1_500_000_000)),
            ("lots", None),
        ] {
            assert_eq!(transfer_bytes(&line(v)), want, "{v}");
        }
        assert_eq!(transfer_bytes("nothing here"), None);
    }

    #[test]
    fn staging_keeps_a_reserve_free() {
        const G: u64 = 1_000_000_000;
        // (needed, free, total, fits)
        for (needed, free, total, fits) in [
            (3 * G, 18 * G, 75 * G, true),   // the shared box: 18 GB free, 7.5 GB reserve
            (11 * G, 18 * G, 75 * G, false), // would leave < 7.5 GB
            (G, 5 * G, 20 * G, false),       // small disk: 5 GB floor
            (0, 5 * G, 20 * G, true),
        ] {
            assert_eq!(staging_room(needed, free, total).is_ok(), fits, "{needed} {free} {total}");
        }
        let e = staging_room(11 * G, 18 * G, 75 * G).unwrap_err();
        assert!(e.contains("~11.0 GB") && e.contains("18.0 GB free") && e.contains("7.5 GB must stay free"), "{e}");
    }

    #[test]
    fn settle_prepare_source_and_last_sync_output_parse() {
        let out = "arena-replica-kept=3\narena-replica-dir=/root/arena-sync-kept/S 12K\narena-replica-file=notes.txt\narena-replica-file=ARENA_materials/x.py\n";
        let s = parse_settle(out).unwrap();
        assert_eq!(s, Settled { kept: 3, dirs: vec!["/root/arena-sync-kept/S 12K".into()], files: vec!["notes.txt".into(), "ARENA_materials/x.py".into()] });
        let lines = settle_lines("devtest-a-new", &s, SettleMode::Advance);
        assert!(lines[0].contains("3 file(s) on devtest-a-new") && lines[0].contains("KEPT") && lines[0].contains("/root/arena-sync-kept/S"), "{lines:?}");
        assert_eq!(lines[1], "    notes.txt");
        assert!(lines.last().unwrap().contains("visible folder") && lines.last().unwrap().contains("backed up"), "{lines:?}");
        let partial = settle_lines("devtest-a-new", &s, SettleMode::Keep);
        assert!(partial[0].contains("didn't complete") && partial[0].contains("nothing it replaced or deleted") && partial[0].contains("3 file(s) KEPT"), "{partial:?}");
        assert!(settle_lines("p", &Settled::default(), SettleMode::Advance).is_empty());
        assert!(settle_lines("p", &Settled::default(), SettleMode::Keep).is_empty());
        assert_eq!(parse_settle("garbage"), None);
        assert_eq!(parse_last_sync("arena-last-sync=1791457323\n"), Some(1_791_457_323));
        assert_eq!(parse_last_sync(""), None);
        assert_eq!(parse_prepared("arena-last-sync=5\narena-replica-ready\n"), Some(Prepared { last_sync: Some(5) }));
        assert_eq!(parse_prepared("arena-replica-ready\n"), Some(Prepared { last_sync: None }));
        assert_eq!(parse_prepared("arena-last-sync=5\n"), None, "a prepare that didn't finish didn't happen");
        assert_eq!(
            parse_source_state("arena-replica-sent=yes\narena-replica-container=7\n"),
            Some(SourceState { synced_to_dest: true, container_created: Some(7) })
        );
        assert_eq!(parse_source_state("arena-replica-sent=no\n"), Some(SourceState { synced_to_dest: false, container_created: None }));
        assert_eq!(parse_source_state(""), None);
    }

    /// The reset guard (review finding: a reset original mirrored over the new pod deleted
    /// every file the participant had there), as a table.
    #[test]
    fn a_mirror_onto_a_synced_receiver_needs_the_source_unchanged_since() {
        let src = |synced_to_dest, container_created| SourceState { synced_to_dest, container_created };
        // (fresh, receiver's last sync, source, refusal)
        let cases = [
            (true, Some(100), src(false, Some(500)), None),                // a pod made for this copy: image + setup only
            (false, None, src(false, Some(500)), None),                    // no stamp: its tidy-up deletes nothing
            (false, Some(100), src(true, Some(50)), None),                 // the same container, the sync on record
            (false, Some(100), src(true, None), None),                     // a VM (no container): the record alone
            (false, Some(100), src(true, Some(500)), Some(MirrorRefusal::SourceReset { container: 500, last_sync: 100 })),
            (false, Some(100), src(true, Some(100)), Some(MirrorRefusal::SourceReset { container: 100, last_sync: 100 })),
            (false, Some(100), src(false, Some(50)), Some(MirrorRefusal::NoRecord { last_sync: 100 })),
            (false, Some(100), src(false, None), Some(MirrorRefusal::NoRecord { last_sync: 100 })),
        ];
        for (fresh, last, s, want) in cases {
            assert_eq!(mirror_refusal(fresh, last, &s), want, "{fresh} {last:?} {s:?}");
        }
    }

    /// A temp dir, removed when dropped.
    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("arena-replica-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
    fn put(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    /// Set a file's mtime `secs` before now (via `touch -d @…`) — as `tar x`, `unzip`,
    /// `cp -p` or `wget` leave a file they create.
    fn age(p: &Path, secs: u64) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() - secs;
        assert!(Command::new("touch").arg("-d").arg(format!("@{t}")).arg(p).status().unwrap().success());
    }
    /// The kernel stamps file times at a jiffy's granularity: let one pass between a sync's
    /// end and the work that follows it (on a pod, minutes or days pass).
    fn tick() {
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    fn sh(cmd: &str, home: &Path) -> String {
        let out = Command::new("sh").arg("-c").arg(cmd).env("HOME", home).output().unwrap();
        assert!(out.status.success(), "{cmd}\n{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    fn have_rsync() -> bool {
        Command::new("rsync").arg("--version").output().is_ok()
    }
    /// Every file under `dir`, relative and sorted.
    fn files(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let ft = e.file_type().unwrap();
                if ft.is_dir() {
                    stack.push(e.path());
                } else {
                    out.push(e.path().strip_prefix(dir).unwrap().display().to_string());
                }
            }
        }
        out.sort();
        out
    }

    /// One replica sync of `src/` onto `dst/` with the real rsync and the replication
    /// filters (a local receiver instead of `u@h:` — rsync's own transport isn't the point).
    fn sync(src: &Path, dst: &Path, stamp: &str) {
        let pc = crate::pull::PullConfig { mirror: home_mirror(stamp), ..crate::pull::PullConfig::replication() };
        let mut args: Vec<String> = crate::pull::push_rsync_args(
            &crate::ssh::SshTarget { user: "u".into(), host: "h".into(), port: 1, key_paths: vec![], connect_timeout_secs: 1 },
            &pc,
            &format!("{}/", src.display()),
        );
        let e = args.iter().position(|a| a == "-e").unwrap();
        args.drain(e..e + 2);
        *args.last_mut().unwrap() = format!("{}/", dst.display());
        let out = Command::new("rsync").args(&args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    /// A whole sync on the receiver `dst`: prepare, transfer, tidy-up (as the CLI runs them).
    fn full_sync(src: &Path, dst: &Path, stamp: &str, fresh: bool) -> Settled {
        let prepared = parse_prepared(&sh(&receiver_prepare_command(None, stamp, None, fresh), dst)).unwrap();
        assert!(prepared.last_sync.is_some());
        sync(src, dst, stamp);
        parse_settle(&sh(&settle_command(None, stamp, None, SettleMode::Advance), dst)).unwrap()
    }

    /// The whole contract, with the real rsync and the real scripts on a temp-dir "pod": a
    /// replica sync propagates the original's deletions, moves everything it replaces or
    /// deletes into the VISIBLE kept folder instead of destroying it, and the tidy-up drops
    /// only what an earlier sync put there and nobody touched since, KEEPING (and naming)
    /// what changed on the pod itself — the data loss of live finding #7 and the resurrection
    /// of #20 in one run. Including work whose files carry OLD times (review finding: the
    /// tidy-up judged by mtime alone, so an extracted archive and a renamed file — both
    /// "older than the stamp" — were deleted after a revert and a re-copy).
    #[cfg(unix)]
    #[test]
    fn a_replica_sync_mirrors_deletions_and_keeps_work_done_on_the_receiver() {
        if !have_rsync() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let t = tmp("sync");
        let (src, dst) = (t.0.join("src"), t.0.join("dst"));
        // First sync onto a fresh receiver: the original's files land.
        for f in ["work.py", "todelete.txt", "keep.txt", "notes.txt"] {
            put(&src.join(f), &format!("{f} v1\n"));
            age(&src.join(f), 200); // written on the original well before the first sync
        }
        std::fs::create_dir_all(&dst).unwrap();
        let first = full_sync(&src, &dst, "S1", true);
        assert_eq!(first.kept, 0, "a fresh receiver keeps nothing: {first:?}");
        tick();
        // Then: the participant deletes a file and edits one on the original; meanwhile work
        // was done on the receiver itself (after a cutover + revert): an edit, a new file, an
        // extracted archive's file (an old mtime) and a renamed one (`mv` keeps its mtime).
        std::fs::remove_file(src.join("todelete.txt")).unwrap();
        put(&src.join("work.py"), "v2 (original)\n");
        age(&src.join("work.py"), 60); // edited on the original BEFORE the receiver's edit
        put(&dst.join("work.py"), "v3 (on the new pod)\n");
        put(&dst.join("only_new.txt"), "made on the new pod\n");
        put(&dst.join("results/run1.csv"), "1,2,3\n");
        age(&dst.join("results/run1.csv"), 3600);
        std::fs::rename(dst.join("notes.txt"), dst.join("notes_final.txt")).unwrap();
        let s = full_sync(&src, &dst, "S2", false);
        // #20: the deleted file is gone from the receiver's tree; the original's copies are in place.
        assert!(!dst.join("todelete.txt").exists());
        assert_eq!(std::fs::read_to_string(dst.join("work.py")).unwrap(), "v2 (original)\n");
        assert_eq!(std::fs::read_to_string(dst.join("notes.txt")).unwrap(), "notes.txt v1\n");
        // #7: everything done on the receiver is kept and named; the stale replica copy dropped.
        let kept = dst.join(KEPT_DIR).join("S2");
        assert_eq!(std::fs::read_to_string(kept.join("work.py")).unwrap(), "v3 (on the new pod)\n");
        assert_eq!(files(&kept), ["notes_final.txt", "only_new.txt", "results/run1.csv", "work.py"], "old-dated work is work too");
        assert_eq!(s.kept, 4, "{s:?}");
        assert!(!dst.join(BOOK_DIR).join("changed-S2").exists(), "the list goes with its sync");
        // A third sync with nothing changed on the receiver: nothing kept, no folder for it,
        // and the earlier folder (the kept work) untouched — a mirror never deletes it.
        tick();
        put(&src.join("work.py"), "v4\n");
        let s3 = full_sync(&src, &dst, "S3", false);
        assert_eq!(s3.kept, 0, "{s3:?}");
        assert!(!dst.join(KEPT_DIR).join("S3").exists());
        assert_eq!(files(&kept).len(), 4, "kept work survives later syncs");
        assert_eq!(std::fs::read_to_string(dst.join("work.py")).unwrap(), "v4\n");
    }

    /// Review finding (critical): the original reset since the last sync (its home is the
    /// image again) and a re-sync mirrored that over the new pod, deleting the participant's
    /// files there. The source's record of the sync is gone with its home, and its container
    /// is newer than the receiver's stamp: the mirror is refused, and the receiver keeps
    /// everything. The record is written only once a sync landed, and per receiver.
    #[cfg(unix)]
    #[test]
    fn a_reset_original_is_never_mirrored_over_the_copy() {
        if !have_rsync() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let t = tmp("reset");
        let (src, dst, mark) = (t.0.join("src"), t.0.join("dst"), t.0.join("dockerenv"));
        put(&mark, "");
        age(&mark, 1000); // the original's container: long before any sync
        put(&src.join("ARENA_materials/my_solution.py"), "work\n");
        put(&src.join("notes.txt"), "notes\n");
        std::fs::create_dir_all(&dst).unwrap();
        let marker = "$HOME/.arena_replace_marker";
        let source = |src: &Path| {
            parse_source_state(&sh(&source_prepare_command(None, marker, "tok", "dest-1", &mark.display().to_string()), src)).unwrap()
        };
        let before = source(&src);
        assert_eq!(before, SourceState { synced_to_dest: false, container_created: before.container_created });
        assert!(before.container_created.is_some());
        assert_eq!(std::fs::read_to_string(src.join(".arena_replace_marker")).unwrap(), "tok");
        // The first sync onto a fresh receiver runs, lands, and is recorded on the source.
        let prepared = parse_prepared(&sh(&receiver_prepare_command(None, "S1", None, true), &dst)).unwrap();
        assert_eq!(mirror_refusal(true, prepared.last_sync, &before), None);
        sync(&src, &dst, "S1");
        sh(&settle_command(None, "S1", None, SettleMode::Advance), &dst);
        sh(&source_finish_command(None, marker, "dest-1", true), &src);
        assert!(!src.join(".arena_replace_marker").exists());
        tick();
        let prepared = parse_prepared(&sh(&receiver_prepare_command(None, "S2", None, false), &dst)).unwrap();
        assert_eq!(mirror_refusal(false, prepared.last_sync, &source(&src)), None, "unchanged since: a re-sync may mirror");
        assert!(source(&src).synced_to_dest, "the landed sync is on record");
        assert!(
            !parse_source_state(&sh(&source_prepare_command(None, marker, "tok", "dest-2", &mark.display().to_string()), &src))
                .unwrap()
                .synced_to_dest,
            "the record is per receiver"
        );
        // A failed sync records nothing.
        let other = t.0.join("other");
        std::fs::create_dir_all(&other).unwrap();
        sh(&source_finish_command(None, marker, "dest-3", false), &other);
        assert!(!other.join(BOOK_DIR).exists());

        // The original is reset: its home is the image again, its container new.
        std::fs::remove_dir_all(&src).unwrap();
        put(&src.join("ARENA_materials/ex.py"), "image\n");
        put(&mark, "");
        let reset = source(&src);
        assert!(!reset.synced_to_dest, "the record went with the home");
        let refusal = mirror_refusal(false, prepared.last_sync, &reset);
        assert!(matches!(refusal, Some(MirrorRefusal::SourceReset { .. })), "{refusal:?}");
        // Even where the container's time can't be read (a VM), the missing record refuses.
        let unknown = SourceState { container_created: None, ..reset };
        assert!(matches!(mirror_refusal(false, prepared.last_sync, &unknown), Some(MirrorRefusal::NoRecord { .. })));
        // Refused, so nothing ran: the receiver still has every file of the participant's.
        assert_eq!(files(&dst).into_iter().filter(|f| !f.starts_with('.')).collect::<Vec<_>>(), ["ARENA_materials/my_solution.py", "notes.txt"]);
    }

    /// Without a stamp (a receiver from an older version) nothing moved is deleted; a sync
    /// that didn't complete (`Keep`) deletes NOTHING it moved, even with a stamp and a list,
    /// and doesn't move the baseline; the repo's folder beside its real directory (through a
    /// link, on the "volume") is tidied, then brought into the home's kept folder under the
    /// repo's place — as is one an earlier tidy-up never got to.
    #[cfg(unix)]
    #[test]
    fn the_tidy_up_guesses_nothing() {
        let t = tmp("settle");
        let home = t.0.join("home");
        let ws = t.0.join("ws");
        std::fs::create_dir_all(ws.join("ARENA_materials")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(ws.join("ARENA_materials"), home.join("ARENA_materials")).unwrap();
        let rel = Some("ARENA_materials");
        // No stamp: everything moved stays (and the repo's is brought home).
        put(&home.join(KEPT_DIR).join("S/old.txt"), "x");
        age(&home.join(KEPT_DIR).join("S/old.txt"), 1000);
        put(&ws.join(KEPT_DIR).join("S/ARENA_materials/nb.ipynb"), "y");
        assert_eq!(parse_prepared(&sh(&receiver_prepare_command(None, "S", rel, false), &home)).unwrap().last_sync, None);
        let s = parse_settle(&sh(&settle_command(None, "S", rel, SettleMode::Advance), &home)).unwrap();
        assert_eq!(s.kept, 2, "{s:?}");
        assert_eq!(files(&home.join(KEPT_DIR)), ["S/ARENA_materials/nb.ipynb", "S/old.txt"]);
        assert!(!ws.join(KEPT_DIR).exists(), "moved off the volume into the home's folder");
        assert!(s.files.contains(&"ARENA_materials/nb.ipynb".to_string()), "{s:?}");
        assert!(home.join(LAST_SYNC).exists(), "a complete sync starts the baseline it lacked");
        std::fs::remove_dir_all(home.join(KEPT_DIR)).unwrap();

        // With a stamp and a list: a partial sync (Keep) deletes nothing, keeps the baseline.
        sh(&receiver_prepare_command(None, "P", rel, true), &home); // a fresh pod's baseline
        tick();
        let stamp_before = std::fs::metadata(home.join(LAST_SYNC)).unwrap().modified().unwrap();
        put(&home.join(KEPT_DIR).join("P/stale.txt"), "x");
        age(&home.join(KEPT_DIR).join("P/stale.txt"), 1000);
        let s = parse_settle(&sh(&settle_command(None, "P", rel, SettleMode::Keep), &home)).unwrap();
        assert_eq!(s.kept, 1, "{s:?}");
        assert_eq!(std::fs::metadata(home.join(LAST_SYNC)).unwrap().modified().unwrap(), stamp_before, "Keep keeps the baseline");
        assert!(!home.join(BOOK_DIR).join("changed-P").exists());

        // A complete one (Advance): the repo's stale copy goes, its changed file stays; a
        // folder an earlier tidy-up left beside the repo is brought home too.
        tick();
        put(&ws.join("ARENA_materials/changed.py"), "edited on the new pod\n");
        sh(&receiver_prepare_command(None, "Q", rel, false), &home);
        let listed = std::fs::read_to_string(home.join(BOOK_DIR).join("changed-Q")).unwrap();
        assert_eq!(listed.lines().collect::<Vec<_>>(), ["ARENA_materials/changed.py"], "found through the link");
        // (what the sync then did: moved the changed file and a stale copy beside the repo)
        std::fs::create_dir_all(ws.join(KEPT_DIR).join("Q/ARENA_materials")).unwrap();
        std::fs::rename(ws.join("ARENA_materials/changed.py"), ws.join(KEPT_DIR).join("Q/ARENA_materials/changed.py")).unwrap();
        put(&ws.join(KEPT_DIR).join("Q/ARENA_materials/stale.py"), "an earlier sync's copy\n");
        age(&ws.join(KEPT_DIR).join("Q/ARENA_materials/stale.py"), 1000);
        put(&ws.join(KEPT_DIR).join("OLD/ARENA_materials/left.py"), "never tidied\n");
        let s = parse_settle(&sh(&settle_command(None, "Q", rel, SettleMode::Advance), &home)).unwrap();
        assert_eq!(s.kept, 2, "{s:?}");
        assert_eq!(
            files(&home.join(KEPT_DIR)),
            ["OLD/ARENA_materials/left.py", "P/stale.txt", "Q/ARENA_materials/changed.py"],
            "the stale copy went; the rest is in the home's folder"
        );
        assert!(!ws.join(KEPT_DIR).exists(), "nothing left beside the repo");
        assert_eq!(s.dirs.len(), 2, "this sync's folder and the one brought home: {s:?}");
        let stamped = std::fs::metadata(home.join(LAST_SYNC)).unwrap().modified().unwrap();
        assert!(stamped > stamp_before, "Advance moves the baseline on");
        assert!(parse_last_sync(&sh(&last_sync_command(), &home)).is_some());
    }

    /// Every step on a receiver runs only on the pod it names (review finding: the mirror
    /// and the tidy-up ran on whatever answered at the listed endpoint): the guard reads
    /// `RUNPOD_POD_ID` from PID 1's environment (a file here) in the same session, and on any
    /// other pod exits 97 having done nothing — an exec'd step and an rsync alike.
    #[cfg(unix)]
    #[test]
    fn a_step_on_the_wrong_pod_does_nothing() {
        let t = tmp("guard");
        let environ = t.0.join("environ");
        std::fs::write(&environ, "PATH=/bin\0RUNPOD_POD_ID=pod-a\0HOME=/root\0").unwrap();
        let env = environ.display().to_string();
        let run = |cond: &str, cmd: &str| {
            Command::new("sh").arg("-c").arg(guarded(Some(cond), cmd)).env("HOME", &t.0).output().unwrap()
        };
        let ok = run(&identity_condition(&env, "pod-a"), "touch \"$HOME/ran\"");
        assert!(ok.status.success() && t.0.join("ran").exists());
        for (cond, why) in [
            (identity_condition(&env, "pod-b"), "another pod"),
            (identity_condition(&t.0.join("missing").display().to_string(), "pod-a"), "an unreadable id"),
            (identity_condition(&env, ""), "an empty id"),
        ] {
            let out = run(&cond, "touch \"$HOME/wrong\"");
            assert_eq!(out.status.code(), Some(WRONG_POD_EXIT), "{why}");
            assert!(String::from_utf8_lossy(&out.stderr).contains("not the expected pod"), "{why}");
            assert!(!t.0.join("wrong").exists(), "{why}");
        }
        assert_eq!(guarded(None, "x"), "x", "no condition (not RunPod): as before");
        // Through the prepare and the tidy-up: refused before either touches anything.
        let wrong = identity_condition(&env, "pod-b");
        let out = Command::new("sh").arg("-c").arg(receiver_prepare_command(Some(&wrong), "S", None, true)).env("HOME", &t.0).output().unwrap();
        assert_eq!(out.status.code(), Some(WRONG_POD_EXIT));
        assert!(!t.0.join(LAST_SYNC).exists() && !t.0.join(BOOK_DIR).exists(), "no baseline started on the wrong pod");
        put(&t.0.join(KEPT_DIR).join("S/theirs.txt"), "x");
        let out = Command::new("sh").arg("-c").arg(settle_command(Some(&wrong), "S", None, SettleMode::Advance)).env("HOME", &t.0).output().unwrap();
        assert_eq!(out.status.code(), Some(WRONG_POD_EXIT));
        assert!(t.0.join(KEPT_DIR).join("S/theirs.txt").exists());

        // An rsync whose remote end isn't the pod transfers nothing (the fake transport runs
        // the remote command in a shell, as sshd does).
        if !have_rsync() {
            return;
        }
        let (src, dst) = (t.0.join("src"), t.0.join("dst"));
        put(&src.join("a.txt"), "a\n");
        std::fs::create_dir_all(&dst).unwrap();
        let fake = crate::volume::fake_ssh::install(&t.0);
        let push = |pod: &str| {
            let mut a = vec!["-a".to_string(), format!("--rsync-path={}", guarded_rsync_path(&identity_condition(&env, pod)))];
            a.extend(["-e".to_string(), fake.display().to_string(), format!("{}/", src.display()), "u@h:".to_string()]);
            Command::new("rsync").args(&a).env("FAKE_HOME", &dst).output().unwrap()
        };
        let out = push("pod-b");
        assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("not the expected pod"), "{out:?}");
        assert!(!dst.join("a.txt").exists());
        assert!(push("pod-a").status.success());
        assert!(dst.join("a.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn disk_space_reads_the_temp_dir() {
        let (free, total) = disk_space(&std::env::temp_dir()).unwrap();
        assert!(total > 0 && free <= total);
        assert_eq!(disk_space(Path::new("/nonexistent/arena")), None);
    }
}
