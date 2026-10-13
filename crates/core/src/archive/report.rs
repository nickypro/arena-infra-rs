//! Reading what the VM says (`setup.sh`'s report, `probe.sh`'s facts) and judging the archive's
//! health for `archive status` — pure, table-tested. The rule, as everywhere: **unknown is not
//! OK** — a probe that didn't finish, a listing that failed, is `UNKNOWN` (non-zero exit), never
//! a pass.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::cloud::{Inventory, Prices};
use super::render::render_push_key_line;
use super::state::{ArchiveHost, BigState, SnapState};
use super::sync::GIVE_UP_AFTER;
use super::{key_blob, ArchiveNames, Tier, SETUP_VERSION};

/// `key=value` lines (the last value of a key wins; anything else is ignored — a login banner,
/// a shell's noise).
pub fn parse_kv(stdout: &str) -> BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .map(|(k, v)| (k.to_string(), v.trim_end().to_string()))
        .collect()
}

/// What `setup.sh` reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupReport {
    pub values: BTreeMap<String, String>,
}

impl SetupReport {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.values.get(k).map(String::as_str)
    }

    /// It ran to the end (`setup_done=1`): a report without it was cut short.
    pub fn finished(&self) -> bool {
        self.get("setup_done") == Some("1")
    }

    /// Every failed step (`ok_<step>=0`) with its reason.
    pub fn failures(&self) -> Vec<(String, String)> {
        self.values
            .iter()
            .filter(|(k, v)| k.starts_with("ok_") && v.as_str() == "0")
            .map(|(k, _)| {
                let step = k.trim_start_matches("ok_").to_string();
                let why = self.get(&format!("why_{step}")).unwrap_or("no reason given").to_string();
                (step, why)
            })
            .collect()
    }

    /// The sshd drop-in failed its checks — and was therefore not installed (S14).
    pub fn sshd_failed(&self) -> bool {
        self.get("ok_sshd") == Some("0")
    }

    /// Where the VM's rrsync is.
    pub fn rrsync(&self) -> Option<&str> {
        self.get("rrsync").filter(|s| !s.is_empty())
    }
}

pub fn parse_setup_report(stdout: &str) -> SetupReport {
    SetupReport { values: parse_kv(stdout) }
}

/// One volume, as the probe saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TierProbe {
    /// Its device path resolved on the VM.
    pub dev: String,
    pub mounted: bool,
    pub source: String,
    pub fstype: String,
    pub owner: String,
    pub bytes_total: Option<u64>,
    pub bytes_used: Option<u64>,
    pub inodes_total: Option<u64>,
    pub inodes_used: Option<u64>,
    pub bind: bool,
    pub bind_source: String,
    pub bind_ro: bool,
}

/// Everything `probe.sh` reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProbeReport {
    pub snap: TierProbe,
    pub big: TierProbe,
    /// The three authorized-keys files, decoded.
    pub root_keys: String,
    pub push_keys: String,
    pub backups_keys: String,
    pub private_keys: u32,
    pub reboot_required: bool,
    pub ssh_service: String,
    pub ssh_socket: String,
    pub unattended_upgrades: String,
    pub rsync_version: String,
    pub rrsync: String,
    pub setup_version: Option<u32>,
    pub sshd_dropin: bool,
    pub root_avail: Option<u64>,
}

impl ProbeReport {
    pub fn tier(&self, t: Tier) -> &TierProbe {
        match t {
            Tier::Snap => &self.snap,
            Tier::Big => &self.big,
        }
    }
}

/// Parse `probe.sh`'s output — `Err` unless it finished (`probe_done=1`) and its base64 decodes.
pub fn parse_probe(stdout: &str) -> Result<ProbeReport, String> {
    let kv = parse_kv(stdout);
    if kv.get("probe_done").map(String::as_str) != Some("1") {
        return Err("the probe didn't finish".into());
    }
    let s = |k: &str| kv.get(k).cloned().unwrap_or_default();
    let n = |k: &str| kv.get(k).and_then(|v| v.parse::<u64>().ok());
    let flag = |k: &str| kv.get(k).map(String::as_str) == Some("1");
    let text = |k: &str| -> Result<String, String> {
        let raw = s(k);
        if raw.is_empty() {
            return Ok(String::new());
        }
        let bytes = crate::base64::decode(&raw).map_err(|e| format!("{k}: {e}"))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    let tier = |t: &str| TierProbe {
        dev: s(&format!("dev_{t}")),
        mounted: flag(&format!("mounted_{t}")),
        source: s(&format!("source_{t}")),
        fstype: s(&format!("fstype_{t}")),
        owner: s(&format!("owner_{t}")),
        bytes_total: n(&format!("bytes_total_{t}")),
        bytes_used: n(&format!("bytes_used_{t}")),
        inodes_total: n(&format!("inodes_total_{t}")),
        inodes_used: n(&format!("inodes_used_{t}")),
        bind: flag(&format!("bind_{t}")),
        bind_source: s(&format!("bind_source_{t}")),
        bind_ro: flag(&format!("bind_ro_{t}")),
    };
    Ok(ProbeReport {
        snap: tier("snap"),
        big: tier("big"),
        root_keys: text("root_keys_b64")?,
        push_keys: text("push_keys_b64")?,
        backups_keys: text("backups_keys_b64")?,
        private_keys: n("private_keys").map_or(0, |v| v as u32),
        reboot_required: flag("reboot_required"),
        ssh_service: s("ssh_service"),
        ssh_socket: s("ssh_socket"),
        unattended_upgrades: s("unattended_upgrades"),
        rsync_version: s("rsync_version"),
        rrsync: s("rrsync"),
        setup_version: n("setup_version").map(|v| v as u32),
        sshd_dropin: flag("sshd_dropin"),
        root_avail: n("root_avail"),
    })
}

/// How one status line reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
    Unknown,
}

impl Level {
    pub fn glyph(self) -> &'static str {
        match self {
            Level::Ok => "✓",
            Level::Warn => "⚠",
            Level::Fail => "✗",
            Level::Unknown => "?",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusItem {
    pub name: String,
    pub level: Level,
    pub msg: String,
}

/// `archive status`'s checklist.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    pub items: Vec<StatusItem>,
}

impl StatusReport {
    fn add(&mut self, name: impl Into<String>, level: Level, msg: impl Into<String>) {
        self.items.push(StatusItem { name: name.into(), level, msg: msg.into() });
    }

    /// Exit 0 only without a FAIL or an UNKNOWN.
    pub fn ok(&self) -> bool {
        !self.items.iter().any(|i| matches!(i.level, Level::Fail | Level::Unknown))
    }

    pub fn count(&self, l: Level) -> usize {
        self.items.iter().filter(|i| i.level == l).count()
    }

    pub fn render(&self) -> String {
        let w = self.items.iter().map(|i| i.name.chars().count()).max().unwrap_or(0);
        let mut s = String::new();
        for i in &self.items {
            s.push_str(&format!("{} {:<w$}  {}\n", i.level.glyph(), i.name, i.msg));
        }
        s.push_str(&format!(
            "\n{}: {} fail, {} unknown, {} warning(s).\n",
            if self.ok() { "OK" } else { "NOT OK" },
            self.count(Level::Fail),
            self.count(Level::Unknown),
            self.count(Level::Warn)
        ));
        s
    }
}

/// Space thresholds (bytes and inodes alike).
pub const WARN_USED: f64 = 0.80;
pub const FAIL_USED: f64 = 0.95;
/// How stale a tier's last OK push may get (seconds): snap is pushed every 15 min, big hourly.
pub const SNAP_WARN_AGE: u64 = 3600;
pub const SNAP_FAIL_AGE: u64 = 6 * 3600;
pub const BIG_WARN_AGE: u64 = 3 * 3600;
pub const BIG_FAIL_AGE: u64 = 24 * 3600;
/// An archive `up` made this long ago that was never pushed to is a FAIL, not a warning.
pub const NEVER_PUSHED_FAIL_AGE: u64 = 3600;
/// ext4 on a Hetzner volume shows ~97% of the device in `df`; under 90% the filesystem wasn't grown.
pub const FS_SMALLER_FRACTION: f64 = 0.90;

/// What `status` judges.
#[derive(Debug, Clone)]
pub struct StatusInputs<'a> {
    pub names: &'a ArchiveNames,
    /// The labelled inventory, or why it couldn't be listed (`None` = no token: not checked).
    pub inventory: Option<Result<&'a Inventory, String>>,
    pub host: Option<&'a ArchiveHost>,
    /// The probe over admin SSH (`None` = not run: no server to reach).
    pub probe: Option<Result<ProbeReport, String>>,
    /// The local admin, push and expected `backups` contents.
    pub admin_pub: Option<&'a str>,
    pub push_pub: Option<&'a str>,
    pub expected_backups: Option<&'a str>,
    pub snap: &'a SnapState,
    pub big: &'a BigState,
    pub unsealed: &'a [String],
    pub custom_labels: &'a [String],
    pub control_box_only: &'a [String],
    /// Pod dirs named like this cohort's but not on `MACHINE_NAME_LIST` (never pushed).
    pub not_on_list: &'a [String],
    /// Pod dirs still pushed only because they once were on the list (`allowed_ever`, A2-3).
    pub off_list: &'a [String],
    /// Pod dirs `archive forget` revoked (never pushed again).
    pub forgotten: &'a [String],
    /// Dirs no longer pushed whose earlier copies are still on the archive (A3-7): forgotten ones
    /// not deleted (`--delete-archived`), and staff boxes once on the list.
    pub still_archived: &'a [String],
    /// How long ago `archive.json` was written (s): an archive that has never been pushed to an
    /// hour after `up` means the cron isn't pushing (AD10).
    pub host_written_age: Option<u64>,
    /// What a full snap push costs at least (the newest label and its baseline, counted once) and
    /// what `big/` holds — compared with the volumes (AD8).
    pub snap_need: Option<u64>,
    pub big_need: Option<u64>,
    pub now: u64,
    pub keep_until_days: Option<i64>,
    pub today_days: i64,
    /// The fleet-token fallback warning, if it applies.
    pub token_warning: Option<&'a str>,
    pub prices: Option<Prices>,
}

fn pct(used: Option<u64>, total: Option<u64>) -> Option<f64> {
    match (used, total) {
        (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64),
        _ => None,
    }
}

fn age(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    if s < 3600 {
        format!("{} min ago", s / 60)
    } else if s < 2 * 86_400 {
        format!("{:.1} h ago", s as f64 / 3600.0)
    } else {
        format!("{} days ago", s / 86_400)
    }
}

fn blobs(text: &str) -> BTreeSet<String> {
    text.lines().filter_map(key_blob).map(String::from).collect()
}

/// Judge everything (§14.9).
pub fn judge_status(i: &StatusInputs) -> StatusReport {
    let mut r = StatusReport::default();
    let names = i.names;
    if let Some(w) = i.token_warning {
        r.add("credentials", Level::Warn, w.trim_start_matches("warning: "));
    }

    // --- the API's view -----------------------------------------------------------------
    let mut gb_total = 0u32;
    let mut server_seen = false;
    match &i.inventory {
        None => r.add("hetzner", Level::Unknown, "no ARCHIVE_HETZNER_API_KEY / HETZNER_API_KEY — the server and volumes weren't checked"),
        Some(Err(e)) => r.add("hetzner", Level::Unknown, format!("couldn't list the archive's resources ({e}) — NOT known to be gone")),
        Some(Ok(inv)) => {
            let ours = inv.owned_by(names);
            match ours.servers.as_slice() {
                [] => r.add("server", Level::Fail, format!("no {} — `arena archive up` (its volumes, if any, are kept)", names.server())),
                [s] => {
                    server_seen = true;
                    let (level, mut msg) = match (s.delete_protected, s.status.as_str()) {
                        (false, _) => (Level::Fail, "delete protection OFF — `arena archive up` turns it on".to_string()),
                        (true, "running") => (Level::Ok, "running, protected".to_string()),
                        (true, st) => (Level::Fail, format!("{st} (not running), protected")),
                    };
                    msg = format!("{} ({}, {}, {}) — {msg}", s.name, s.server_type, s.location, s.ipv4.as_deref().unwrap_or("no IPv4"));
                    if let Some(h) = i.host.filter(|h| h.is_up() && Some(h.ipv4.as_str()) != s.ipv4.as_deref()) {
                        r.add("server", Level::Fail, format!("{msg}; but archive.json points at {} — re-run `arena archive up`", h.ipv4));
                    } else {
                        r.add("server", level, msg);
                    }
                }
                many => r.add("server", Level::Fail, format!("{} servers carry the label — sort it out in the console", many.len())),
            }
            for t in Tier::ALL {
                let name = format!("volume {t}");
                match inv.volume(names, t) {
                    None => r.add(name, Level::Fail, format!("no {} — `arena archive up`", names.volume(t))),
                    Some(v) => {
                        gb_total += v.size_gb;
                        let attached = v.server.is_some() && ours.servers.first().is_some_and(|s| Some(s.id) == v.server);
                        let level = if v.delete_protected && attached { Level::Ok } else { Level::Fail };
                        let mut msg = format!("{} GB, {}", v.size_gb, if attached { "attached" } else { "NOT attached to the archive" });
                        msg.push_str(if v.delete_protected { ", protected" } else { ", delete protection OFF" });
                        r.add(name, level, msg);
                    }
                }
            }
        }
    }

    // --- the local state ------------------------------------------------------------------
    match i.host {
        None => r.add("state", Level::Fail, "no archive.json for this user — run `arena archive up` as this user (or share ARENA_STATE_DIR)"),
        Some(h) if !h.is_up() => r.add("state", Level::Fail, "archive.json says the server is down (`arena archive down`) — `arena archive up` brings it back"),
        Some(h) => {
            let access = if h.access_on { "participants can read (access on)" } else { "participant access OFF (`arena archive access on`)" };
            r.add("access", Level::Ok, access);
            if h.setup_version < SETUP_VERSION {
                r.add("setup", Level::Warn, format!("set up by an older arena (v{} < v{SETUP_VERSION}) — re-run `arena archive up`", h.setup_version));
            }
        }
    }

    // --- the VM's own view ------------------------------------------------------------------
    match &i.probe {
        None => {
            if server_seen {
                r.add("probe", Level::Unknown, "the server wasn't probed (no archive.json for this user)");
            }
        }
        Some(Err(e)) => r.add("probe", Level::Unknown, format!("couldn't probe the archive over SSH ({e})")),
        Some(Ok(p)) => judge_probe(&mut r, p, i),
    }

    // --- the pushes ---------------------------------------------------------------------------
    let unpushed_too_long = i.host_written_age.is_some_and(|a| a > NEVER_PUSHED_FAIL_AGE);
    let stale = |last: Option<u64>, warn: u64, fail: u64, what: &str| -> (Level, String) {
        match last {
            None if unpushed_too_long => (
                Level::Fail,
                format!(
                    "{what}: never pushed, over an hour after `archive up` — is the cron installed for the user (and \
                     ARENA_STATE_DIR) that ran `up`? (`arena cron show`, `arena archive sync`)"
                ),
            ),
            None => (Level::Warn, format!("{what}: never pushed — `arena archive sync` / `arena cron install --archive`")),
            Some(t) => {
                let a = i.now.saturating_sub(t);
                let level = if a > fail { Level::Fail } else if a > warn { Level::Warn } else { Level::Ok };
                (level, format!("{what}: last OK push {}", age(i.now, t)))
            }
        }
    };
    let (mut level, mut msg) = stale(i.snap.last_ok(), SNAP_WARN_AGE, SNAP_FAIL_AGE, "snap");
    if let Some(e) = &i.snap.last_error {
        if i.snap.last_ok().is_none_or(|ok| e.at > ok) {
            msg.push_str(&format!("; last error {} ({}{}: {})", age(i.now, e.at), e.class, e.label.as_deref().map(|l| format!(" {l}")).unwrap_or_default(), e.message));
            level = level.max(outage_level(e.class, Tier::Snap, i, &mut msg));
        }
    }
    if i.snap.last_run.is_some_and(|r| r.all_failed) {
        level = Level::Fail;
        msg.push_str(" — every push of the last run failed: the archive, not a pod (`arena archive sync` says why)");
    }
    r.add("sync snap", level, msg);
    // The sync state describes one volume (AD2): another one holds none of what it says.
    if let Some(h) = i.host {
        for (tier, state_volume) in [(Tier::Snap, i.snap.volume.as_deref()), (Tier::Big, i.big.volume.as_deref())] {
            if let Some(v) = state_volume.filter(|v| *v != h.device(tier)) {
                r.add(
                    format!("state {tier}"),
                    Level::Warn,
                    format!(
                        "the {tier} sync state describes another volume ({v}, now {}) — the next sync starts over and pushes \
                         everything again; nothing above about {tier} pushes is on this volume",
                        h.device(tier)
                    ),
                );
            }
        }
    }
    let (mut level, mut msg) = stale(i.big.last_any_ok(), BIG_WARN_AGE, BIG_FAIL_AGE, "big");
    if let Some(e) = &i.big.last_error {
        if i.big.last_any_ok().is_none_or(|ok| e.at > ok) {
            msg.push_str(&format!("; last error {} ({}: {})", age(i.now, e.at), e.class, e.message));
            level = level.max(outage_level(e.class, Tier::Big, i, &mut msg));
        }
    }
    if i.big.last_run.is_some_and(|r| r.all_failed) {
        level = Level::Fail;
        msg.push_str(" — every push of the last run failed: the archive, not a pod (`arena archive sync` says why)");
    }
    r.add("sync big", level, msg);
    // Big goes pod by pod (A3-1): one pod failing is that pod's, named, never the tier's.
    let big_failing: Vec<String> = i
        .big
        .pods_failing
        .iter()
        .filter(|(p, _)| !i.forgotten.contains(p))
        .map(|(p, why)| {
            let ok = i.big.pods_ok_at.get(p).map_or("never".to_string(), |t| format!("last OK {}", age(i.now, *t)));
            format!("{p} ({ok}; {why})")
        })
        .collect();
    if !big_failing.is_empty() {
        r.add("big failing", Level::Warn, format!("big pushes failing: {}", big_failing.join(", ")));
    }
    let mut labels = format!("{} label(s) pushed", i.snap.labels.len());
    if !i.unsealed.is_empty() {
        labels.push_str(&format!("; not final yet: {}", i.unsealed.join(", ")));
    }
    if !i.custom_labels.is_empty() {
        labels.push_str(&format!("; custom labels on the control box only (`arena archive sync --label L`): {}", i.custom_labels.join(", ")));
    }
    r.add("labels", Level::Ok, labels);
    if !i.control_box_only.is_empty() {
        r.add("kept local", Level::Ok, format!("kept on the control box only (staff boxes, other cohorts): {}", i.control_box_only.join(", ")));
    }
    if !i.not_on_list.is_empty() {
        r.add(
            "not on the list",
            Level::Warn,
            format!(
                "named like this cohort's pods but not on MACHINE_NAME_LIST, so never pushed: {} — add them to the list to archive them",
                i.not_on_list.join(", ")
            ),
        );
    }
    if !i.off_list.is_empty() {
        r.add(
            "off the list",
            Level::Ok,
            format!(
                "off MACHINE_NAME_LIST, still archived (they were on it): {} — `arena archive forget <pod>` stops that",
                i.off_list.join(", ")
            ),
        );
    }
    if !i.forgotten.is_empty() {
        r.add("forgotten", Level::Ok, format!("never pushed again (`arena archive forget`): {}", i.forgotten.join(", ")));
    }
    if !i.still_archived.is_empty() {
        let access_on = i.host.is_some_and(|h| h.access_on);
        let (level, who) =
            if access_on { (Level::Warn, "READABLE BY EVERY PARTICIPANT (access is on)") } else { (Level::Ok, "not readable now (access is off)") };
        r.add(
            "still archived",
            level,
            format!(
                "no longer pushed but their earlier copies are still on the archive, {who}: {} — `arena archive forget <pod> \
                 --delete-archived` deletes one",
                i.still_archived.join(", ")
            ),
        );
    }
    // One stuck pod never holds the rest up (A2-1): who keeps failing, and who was settled as is.
    let retrying: Vec<String> = i
        .snap
        .labels
        .iter()
        .flat_map(|(l, r)| r.fails.iter().filter(|(p, _)| !r.given_up.contains(*p)).map(move |(p, n)| (l, p, n)))
        .filter(|(_, p, _)| !i.forgotten.contains(p))
        .map(|(l, p, n)| format!("{l} {p} ({n} of {GIVE_UP_AFTER})"))
        .collect();
    if !retrying.is_empty() {
        r.add(
            "failing",
            Level::Warn,
            format!("pushes of a past label that keep failing (failed in a row): {} — `arena archive sync` says why", retrying.join(", ")),
        );
    }
    let partial: Vec<(String, String)> = i.snap.partial().into_iter().filter(|(_, p)| !i.forgotten.contains(p)).collect();
    if !partial.is_empty() {
        let list: Vec<String> = partial.iter().map(|(l, p)| format!("{l} {p}")).collect();
        r.add(
            "partial",
            Level::Warn,
            format!(
                "sealed partial — kept as they are on the archive after {GIVE_UP_AFTER} failed pushes in a row: {} — `arena archive sync \
                 --label <L>` retries one",
                list.join(", ")
            ),
        );
    }
    if i.snap.baseline_fails > 0 {
        let n = i.snap.baseline_fails;
        let what = if super::sync::baseline_given_up(i.snap) {
            "labels now seal without it (each pod keeps its own copy of the image's files)"
        } else {
            "labels wait for it to seal"
        };
        r.add("baseline", Level::Warn, format!("the fleet baseline's push failed {n} time(s) in a row — {what}; `arena archive sync` says why"));
    }
    // Room for what's coming (AD8): a volume under twice what a full push needs fills on the seed.
    for (tier, need) in [(Tier::Snap, i.snap_need), (Tier::Big, i.big_need)] {
        let gb = volume_gb(i, tier);
        if let (Some(need), Some(gb)) = (need, gb) {
            let have = u64::from(gb) << 30;
            let what = match tier {
                Tier::Snap => "the newest snapshot label with its baseline (shared files once)",
                Tier::Big => "big/",
            };
            let (level, verdict) = if have < 2 * need { (Level::Warn, format!(" — under 2×: `arena archive resize {} N`", tier.gb_flag())) } else { (Level::Ok, String::new()) };
            r.add(format!("size {tier}"), level, format!("{what} holds {} on the control box; the {tier} volume is {gb} GB{verdict}", crate::pull::human_bytes(need)));
        }
    }

    // --- retention and cost -------------------------------------------------------------------
    match i.keep_until_days {
        None => r.add("retention", Level::Ok, "no retention date set (ARCHIVE_KEEP_UNTIL) — nothing is ever deleted automatically"),
        Some(d) if i.today_days > d => r.add(
            "retention",
            Level::Warn,
            format!(
                "past ARCHIVE_KEEP_UNTIL ({}) — `arena archive access off`, then `arena archive down --delete-volumes --confirm {}`",
                crate::schedule::ymd_string(d),
                names.server()
            ),
        ),
        Some(d) => r.add("retention", Level::Ok, format!("kept until {}", crate::schedule::ymd_string(d))),
    }
    if let Some(p) = i.prices {
        r.add("cost", Level::Ok, format!("≈ €{:.2}/month ({gb_total} GB of volumes{})", p.monthly(server_seen, gb_total), if server_seen { " + the server" } else { "" }));
    }
    r
}

/// How bad a tier's last error is (A3-2): a failure of the archive itself (full, read-only,
/// failing, unmounted, unreachable, keys refused …) is a FAIL — nothing is pushed and no pod is to
/// blame — with what to do; a pod's own failure leaves the level to the staleness check.
fn outage_level(class: super::PushFailure, tier: Tier, i: &StatusInputs, msg: &mut String) -> Level {
    if !super::sync::archive_wide(class) {
        return Level::Ok;
    }
    let gb = i.host.map_or(0, |h| h.size_gb(tier));
    let hint = super::sync::failure_hint(class, tier, gb);
    if !hint.is_empty() {
        msg.push_str(&format!(" — {hint}"));
    }
    Level::Fail
}

/// A tier's volume size: the API's, else archive.json's.
fn volume_gb(i: &StatusInputs, tier: Tier) -> Option<u32> {
    let api = i.inventory.as_ref().and_then(|r| r.as_ref().ok()).and_then(|inv| inv.volume(i.names, tier)).map(|v| v.size_gb);
    api.or_else(|| i.host.map(|h| h.size_gb(tier)).filter(|g| *g > 0))
}

fn judge_probe(r: &mut StatusReport, p: &ProbeReport, i: &StatusInputs) {
    for t in Tier::ALL {
        let tp = p.tier(t);
        let dev = if tp.dev.is_empty() { "?" } else { tp.dev.as_str() };
        if !tp.mounted {
            r.add(format!("mount {t}"), Level::Fail, format!("{} NOT mounted (pushes fail NotMounted; the root disk is untouched) — `arena archive up` re-mounts it", t.mountpoint()));
        } else if tp.source != tp.dev || tp.fstype != "ext4" {
            r.add(format!("mount {t}"), Level::Fail, format!("{} has {} ({}) mounted, expected the volume {dev} (ext4)", t.mountpoint(), tp.source, tp.fstype));
        } else {
            r.add(format!("mount {t}"), Level::Ok, format!("{dev} ext4 at {}", t.mountpoint()));
        }
        if !tp.bind {
            r.add(format!("view {t}"), Level::Fail, format!("{} not bound — participants see nothing; `arena archive up`", t.view_mountpoint()));
        } else if !tp.bind_ro {
            r.add(format!("view {t}"), Level::Fail, format!("{} is bound READ-WRITE — `arena archive up` re-binds it read-only", t.view_mountpoint()));
        } else if tp.bind_source != tp.dev {
            r.add(format!("view {t}"), Level::Fail, format!("{} binds {} instead of the volume — `arena archive up`", t.view_mountpoint(), tp.bind_source));
        } else {
            r.add(format!("view {t}"), Level::Ok, "read-only bind");
        }
        if tp.mounted {
            let bytes = pct(tp.bytes_used, tp.bytes_total);
            let inodes = pct(tp.inodes_used, tp.inodes_total);
            let worst = bytes.into_iter().chain(inodes).fold(0.0_f64, f64::max);
            let level = if bytes.is_none() || inodes.is_none() {
                Level::Unknown
            } else if worst >= FAIL_USED {
                Level::Fail
            } else if worst >= WARN_USED {
                Level::Warn
            } else {
                Level::Ok
            };
            let show = |x: Option<f64>| x.map_or("?".to_string(), |v| format!("{:.0}%", v * 100.0));
            let mut msg = format!(
                "{} of {} used ({}), inodes {} of {} ({})",
                tp.bytes_used.map_or("?".into(), crate::pull::human_bytes),
                tp.bytes_total.map_or("?".into(), crate::pull::human_bytes),
                show(bytes),
                tp.inodes_used.map_or("?".into(), |v| v.to_string()),
                tp.inodes_total.map_or("?".into(), |v| v.to_string()),
                show(inodes)
            );
            if level >= Level::Warn {
                let gb = i.host.map_or(0, |h| h.size_gb(t));
                msg.push_str(&format!(" — {}", super::sync::failure_hint(super::PushFailure::NoSpace, t, gb)));
            }
            r.add(format!("space {t}"), level, msg);
            // A resize whose filesystem step failed (AD6): the volume grew, ext4 didn't.
            if let (Some(gb), Some(total)) = (volume_gb(i, t), tp.bytes_total) {
                if (total as f64) < FS_SMALLER_FRACTION * ((u64::from(gb) << 30) as f64) {
                    r.add(
                        format!("filesystem {t}"),
                        Level::Warn,
                        format!(
                            "the filesystem holds {} but the volume is {gb} GB — finish the grow: `arena archive resize {} {gb}`",
                            crate::pull::human_bytes(total),
                            t.gb_flag()
                        ),
                    );
                }
            }
        }
    }

    // Keys (S5): root = {admin}; backups = what access says; push = the forced line; no private keys.
    let root = blobs(&p.root_keys);
    match i.admin_pub.and_then(key_blob) {
        Some(admin) if root.len() == 1 && root.contains(admin) => r.add("keys root", Level::Ok, "only the archive admin key"),
        Some(_) => r.add("keys root", Level::Fail, format!("root's authorized_keys holds {} key(s), not only the archive admin key", root.len())),
        None => r.add("keys root", Level::Unknown, "this state dir's admin_ed25519.pub is unreadable — can't compare"),
    }
    match i.expected_backups {
        Some(want) if blobs(want) == blobs(&p.backups_keys) => {
            r.add("keys backups", Level::Ok, if blobs(want).is_empty() { "none (access off)".to_string() } else { format!("{} key(s) (cohort + read)", blobs(want).len()) })
        }
        Some(_) => r.add("keys backups", Level::Fail, "the read user's keys aren't what `access` says — `arena archive access on|off` rewrites them"),
        None => r.add("keys backups", Level::Unknown, "the expected read keys couldn't be worked out (cohort key unreadable?)"),
    }
    match i.push_pub.map(|pubk| render_push_key_line(&p.rrsync, pubk)) {
        Some(Ok(want)) if want.trim() == p.push_keys.trim() => r.add("keys push", Level::Ok, format!("forced into {} -munge -wo -no-del -no-lock", p.rrsync)),
        Some(Ok(_)) => r.add("keys push", Level::Fail, "the push key line isn't the forced rrsync line — `arena archive up` rewrites it"),
        _ => r.add("keys push", Level::Unknown, "this state dir's push_ed25519.pub is unreadable — can't compare"),
    }
    if p.private_keys > 0 {
        r.add("credentials on VM", Level::Fail, format!("{} private key file(s) under /root/.ssh, /home/*/.ssh or /etc/ssh/arena-keys — the archive must hold none", p.private_keys));
    }

    // The host.
    let ssh = p.ssh_service == "active" || p.ssh_socket == "active";
    let mut host = vec![format!("rsync {}", if p.rsync_version.is_empty() { "?" } else { &p.rsync_version })];
    let mut level = Level::Ok;
    if !ssh {
        level = Level::Warn;
        host.push("ssh not reported active".into());
    }
    if p.reboot_required {
        level = Level::Warn;
        host.push("a reboot is pending (`arena archive ssh -- reboot`; mounts come back)".into());
    }
    if p.unattended_upgrades != "active" {
        level = Level::Warn;
        host.push(format!("unattended-upgrades {}", if p.unattended_upgrades.is_empty() { "unknown" } else { &p.unattended_upgrades }));
    }
    if p.setup_version.unwrap_or(0) < SETUP_VERSION || !p.sshd_dropin {
        level = Level::Warn;
        host.push("setup is missing or older than this arena — re-run `arena archive up`".into());
    }
    r.add("host", level, host.join("; "));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::cloud::{CloudServer, CloudVolume};
    use crate::archive::state::{ErrRecord, LabelRecord, RunRecord, STATE_VERSION};
    use crate::archive::PushFailure;

    const ADMIN: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAdminAdminAdminAdminAdminAdminAdminAdmin1 admin";
    const PUSH: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPushPushPushPushPushPushPushPushPushPus1 push";
    const OTHER: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtherOtherOtherOtherOtherOtherOtherOther9 x";
    const NOW: u64 = 1_800_000_000;
    /// A 50 GB volume's filesystem.
    const TOTAL: u64 = 52_000_000_000;

    fn b64(s: &str) -> String {
        crate::base64::encode(s.as_bytes())
    }

    fn healthy_probe_text() -> String {
        let mut s = String::from("Welcome to Ubuntu\n");
        for t in ["snap", "big"] {
            let dev = if t == "snap" { "/dev/sdb" } else { "/dev/sdc" };
            s.push_str(&format!(
                "dev_{t}={dev}\nmounted_{t}=1\nsource_{t}={dev}\nfstype_{t}=ext4\nowner_{t}=arena-push\nbytes_total_{t}={TOTAL}\nbytes_used_{t}={}\n\
                 inodes_total_{t}=1000\ninodes_used_{t}=10\nbind_{t}=1\nbind_source_{t}={dev}\nbind_ro_{t}=1\n",
                TOTAL / 10
            ));
        }
        let push = render_push_key_line("/usr/bin/rrsync", PUSH).unwrap();
        s.push_str(&format!(
            "root_keys_b64={}\npush_keys_b64={}\nbackups_keys_b64=\nprivate_keys=0\nreboot_required=0\nssh_service=active\nssh_socket=inactive\n\
             unattended_upgrades=active\nrsync_version=3.2.7\nrrsync=/usr/bin/rrsync\nsetup_version=1\nsshd_dropin=1\nroot_avail=9\nprobe_done=1\n",
            b64(&format!("{ADMIN}\n")),
            b64(&push)
        ));
        s
    }

    fn host() -> ArchiveHost {
        ArchiveHost {
            version: STATE_VERSION,
            prefix: "arena9".into(),
            server_id: 1,
            ipv4: "192.0.2.1".into(),
            location: "nbg1".into(),
            snap_device: "/dev/disk/by-id/scsi-0HC_Volume_2".into(),
            big_device: "/dev/disk/by-id/scsi-0HC_Volume_3".into(),
            access_on: false,
            setup_version: SETUP_VERSION,
            snap_gb: 50,
            big_gb: 200,
            host_key_fingerprint: None,
        }
    }

    fn inv() -> Inventory {
        let n = ArchiveNames::new("arena9").unwrap();
        let vol = |id, t: Tier| CloudVolume {
            id,
            name: n.volume(t),
            size_gb: 50,
            location: "nbg1".into(),
            server: Some(1),
            format: Some("ext4".into()),
            linux_device: String::new(),
            labels: n.volume_labels(t),
            delete_protected: true,
        };
        Inventory {
            servers: vec![CloudServer {
                id: 1,
                name: n.server(),
                status: "running".into(),
                location: "nbg1".into(),
                ipv4: Some("192.0.2.1".into()),
                labels: n.labels(),
                delete_protected: true,
                server_type: "cx23".into(),
            }],
            volumes: vec![vol(2, Tier::Snap), vol(3, Tier::Big)],
            ssh_keys: vec![],
        }
    }

    fn snap_ok() -> SnapState {
        let mut s = SnapState::default();
        s.labels.insert("w1d1".into(), LabelRecord { ok_started: NOW - 700, ok_finished: NOW - 600, files: 1, bytes: 1, ..Default::default() });
        s
    }
    fn big_ok() -> BigState {
        BigState { last_ok: Some(RunRecord { started: NOW - 1000, finished: NOW - 900, ok: true, all_failed: false }), ..BigState::default() }
    }

    fn judge(edit: impl Fn(&mut ProbeReport), snap: &SnapState, big: &BigState, h: &ArchiveHost, inventory: Option<Result<&Inventory, String>>) -> StatusReport {
        let n = ArchiveNames::new("arena9").unwrap();
        let mut p = parse_probe(&healthy_probe_text()).unwrap();
        edit(&mut p);
        let i = StatusInputs {
            names: &n,
            inventory,
            host: Some(h),
            probe: Some(Ok(p)),
            admin_pub: Some(ADMIN),
            push_pub: Some(PUSH),
            expected_backups: Some(""),
            snap,
            big,
            unsealed: &[],
            custom_labels: &[],
            control_box_only: &[],
            not_on_list: &[],
            off_list: &[],
            forgotten: &[],
            still_archived: &[],
            host_written_age: None,
            snap_need: None,
            big_need: None,
            now: NOW,
            keep_until_days: None,
            today_days: 20_000,
            token_warning: None,
            prices: None,
        };
        judge_status(&i)
    }

    fn level_of(r: &StatusReport, name: &str) -> Level {
        r.items.iter().find(|i| i.name == name).unwrap_or_else(|| panic!("no {name} in {r:?}")).level
    }

    #[test]
    fn setup_reports_name_the_failed_steps() {
        let r = parse_setup_report("banner\nok_os=1\nok_fs_big=0\nwhy_fs_big=/dev/sdc is xfs\nok_sshd=0\nwhy_sshd=sshd -t: bad — config unchanged\nrrsync=/usr/bin/rrsync\nsetup_done=1\n");
        assert!(r.finished());
        assert!(r.sshd_failed());
        assert_eq!(r.rrsync(), Some("/usr/bin/rrsync"));
        assert_eq!(r.failures(), [("fs_big".to_string(), "/dev/sdc is xfs".to_string()), ("sshd".to_string(), "sshd -t: bad — config unchanged".to_string())]);
        assert!(!parse_setup_report("ok_os=1\n").finished());
    }

    #[test]
    fn probe_parsing_needs_a_finished_report() {
        let p = parse_probe(&healthy_probe_text()).unwrap();
        assert!(p.snap.mounted && p.big.bind_ro);
        assert_eq!(p.snap.bytes_total, Some(TOTAL));
        assert!(p.root_keys.contains("AdminAdmin"));
        assert_eq!(p.setup_version, Some(1));
        assert!(parse_probe("mounted_snap=1\n").unwrap_err().contains("didn't finish"));
        assert!(parse_probe("root_keys_b64=!!!\nprobe_done=1\n").is_err());
    }

    /// §14.9: healthy, then each failure the status must catch.
    #[test]
    fn status_judges_mounts_binds_space_keys_staleness_and_retention() {
        let h = host();
        let i = inv();
        let healthy = judge(|_| {}, &snap_ok(), &big_ok(), &h, Some(Ok(&i)));
        assert!(healthy.ok(), "{}", healthy.render());
        assert_eq!(healthy.count(Level::Warn), 0, "{}", healthy.render());

        let cases: Vec<(&str, Box<dyn Fn(&mut ProbeReport)>, &str, Level)> = vec![
            ("volume missing", Box::new(|p| p.big.mounted = false), "mount big", Level::Fail),
            ("wrong device", Box::new(|p| p.snap.source = "/dev/sda1".into()), "mount snap", Level::Fail),
            ("bind not ro", Box::new(|p| p.snap.bind_ro = false), "view snap", Level::Fail),
            ("bind missing", Box::new(|p| p.big.bind = false), "view big", Level::Fail),
            ("bind of the bare dir", Box::new(|p| p.big.bind_source = "/dev/sda1".into()), "view big", Level::Fail),
            ("80% bytes", Box::new(|p| p.big.bytes_used = Some(TOTAL / 100 * 80)), "space big", Level::Warn),
            ("95% bytes", Box::new(|p| p.big.bytes_used = Some(TOTAL / 100 * 95)), "space big", Level::Fail),
            ("80% inodes", Box::new(|p| p.snap.inodes_used = Some(850)), "space snap", Level::Warn),
            ("95% inodes", Box::new(|p| p.snap.inodes_used = Some(999)), "space snap", Level::Fail),
            ("unknown space", Box::new(|p| p.snap.inodes_total = None), "space snap", Level::Unknown),
            ("root keys ≠ admin", Box::new(|p| p.root_keys = format!("{ADMIN}\n{OTHER}\n")), "keys root", Level::Fail),
            ("root keys = another key", Box::new(|p| p.root_keys = format!("{OTHER}\n")), "keys root", Level::Fail),
            ("backups keys while access is off", Box::new(|p| p.backups_keys = format!("restrict {OTHER}\n")), "keys backups", Level::Fail),
            ("push line changed", Box::new(|p| p.push_keys = format!("{PUSH}\n")), "keys push", Level::Fail),
            ("a private key on the VM", Box::new(|p| p.private_keys = 1), "credentials on VM", Level::Fail),
            ("reboot pending", Box::new(|p| p.reboot_required = true), "host", Level::Warn),
        ];
        for (case, edit, item, want) in cases {
            let r = judge(edit, &snap_ok(), &big_ok(), &h, Some(Ok(&i)));
            assert_eq!(level_of(&r, item), want, "{case}: {}", r.render());
            assert_eq!(r.ok(), want < Level::Fail, "{case}");
        }
        // The resize hint names the volume's size.
        let r = judge(|p| p.big.bytes_used = Some(TOTAL / 100 * 96), &snap_ok(), &big_ok(), &h, Some(Ok(&i)));
        assert!(r.render().contains("arena archive resize --big-gb 300"), "{}", r.render());

        // Stale pushes.
        let mut stale_snap = snap_ok();
        stale_snap.labels.get_mut("w1d1").unwrap().ok_finished = NOW - 2 * 3600;
        assert_eq!(level_of(&judge(|_| {}, &stale_snap, &big_ok(), &h, Some(Ok(&i))), "sync snap"), Level::Warn);
        stale_snap.labels.get_mut("w1d1").unwrap().ok_finished = NOW - 7 * 3600;
        stale_snap.last_error = Some(ErrRecord { at: NOW - 60, label: Some("w1d2".into()), class: PushFailure::Unreachable, message: "refused".into() });
        let r = judge(|_| {}, &stale_snap, &big_ok(), &h, Some(Ok(&i)));
        assert_eq!(level_of(&r, "sync snap"), Level::Fail);
        assert!(r.render().contains("Unreachable w1d2"), "{}", r.render());
        let stale_big = BigState { last_ok: Some(RunRecord { started: 0, finished: NOW - 25 * 3600, ok: true, all_failed: false }), ..BigState::default() };
        assert_eq!(level_of(&judge(|_| {}, &snap_ok(), &stale_big, &h, Some(Ok(&i))), "sync big"), Level::Fail);
        assert_eq!(level_of(&judge(|_| {}, &SnapState::default(), &BigState::default(), &h, Some(Ok(&i))), "sync snap"), Level::Warn);

        // The API: protection off, a failed listing, a lost server.
        let mut unprotected = inv();
        unprotected.servers[0].delete_protected = false;
        assert_eq!(level_of(&judge(|_| {}, &snap_ok(), &big_ok(), &h, Some(Ok(&unprotected))), "server"), Level::Fail);
        unprotected.volumes[1].delete_protected = false;
        assert_eq!(level_of(&judge(|_| {}, &snap_ok(), &big_ok(), &h, Some(Ok(&unprotected))), "volume big"), Level::Fail);
        let r = judge(|_| {}, &snap_ok(), &big_ok(), &h, Some(Err("HTTP 503".into())));
        assert_eq!(level_of(&r, "hetzner"), Level::Unknown);
        assert!(!r.ok());
        let mut moved = inv();
        moved.servers[0].ipv4 = Some("192.0.2.99".into());
        assert!(judge(|_| {}, &snap_ok(), &big_ok(), &h, Some(Ok(&moved))).render().contains("archive.json points at 192.0.2.1"));
    }

    /// AD2, AD6, AD10: a sync state for another volume, a filesystem that didn't grow with its
    /// volume, and an archive nobody has pushed to an hour after `up`.
    #[test]
    fn status_flags_another_volume_an_ungrown_filesystem_and_a_cron_that_never_pushed() {
        let h = host();
        let i = inv();
        let mut moved = snap_ok();
        moved.volume = Some("/dev/disk/by-id/scsi-0HC_Volume_OLD".into());
        let r = judge(|_| {}, &moved, &big_ok(), &h, Some(Ok(&i)));
        assert_eq!(level_of(&r, "state snap"), Level::Warn);
        assert!(r.render().contains("starts over"), "{}", r.render());
        let same = SnapState { volume: Some(h.snap_device.clone()), ..snap_ok() };
        assert!(!judge(|_| {}, &same, &big_ok(), &h, Some(Ok(&i))).items.iter().any(|x| x.name == "state snap"));
        let r = judge(|p| p.big.bytes_total = Some(10 << 30), &snap_ok(), &big_ok(), &h, Some(Ok(&i)));
        assert_eq!(level_of(&r, "filesystem big"), Level::Warn);
        assert!(r.render().contains("arena archive resize --big-gb 50"), "{}", r.render());
        assert!(!judge(|_| {}, &snap_ok(), &big_ok(), &h, Some(Ok(&i))).items.iter().any(|x| x.name.starts_with("filesystem")));
        // Never pushed: a warning at first, a FAIL an hour after `up`.
        let n = ArchiveNames::new("arena9").unwrap();
        let p = parse_probe(&healthy_probe_text()).unwrap();
        let (never_s, never_b) = (SnapState::default(), BigState::default());
        let at = |age: u64| {
            judge_status(&StatusInputs {
                names: &n,
                inventory: Some(Ok(&i)),
                host: Some(&h),
                probe: Some(Ok(p.clone())),
                admin_pub: Some(ADMIN),
                push_pub: Some(PUSH),
                expected_backups: Some(""),
                snap: &never_s,
                big: &never_b,
                unsealed: &[],
                custom_labels: &[],
                control_box_only: &[],
                not_on_list: &[],
                off_list: &[],
                forgotten: &[],
                still_archived: &[],
                host_written_age: Some(age),
                snap_need: None,
                big_need: None,
                now: NOW,
                keep_until_days: None,
                today_days: 20_000,
                token_warning: None,
                prices: None,
            })
        };
        assert_eq!(level_of(&at(600), "sync snap"), Level::Warn);
        assert_eq!(level_of(&at(7200), "sync snap"), Level::Fail);
        assert_eq!(level_of(&at(7200), "sync big"), Level::Fail);
        assert!(!at(7200).ok());
    }

    /// A2-1: status names each pod that keeps failing a past label (and how close it is to being
    /// settled), each label sealed partial, and a failing baseline — whether labels still wait.
    #[test]
    fn status_names_failing_pods_partial_seals_and_a_failing_baseline() {
        let (h, i) = (host(), inv());
        let mut snap = snap_ok();
        let mut retrying = LabelRecord::default();
        retrying.fails.insert("arena9-bloom".into(), 2);
        snap.labels.insert("w0d4".into(), retrying);
        let mut settled = LabelRecord::default();
        settled.fails.insert("arena9-apple".into(), 3);
        settled.given_up.insert("arena9-apple".into());
        snap.labels.insert("w0d3".into(), settled);
        snap.baseline_fails = 1;
        let r = judge(|_| {}, &snap, &big_ok(), &h, Some(Ok(&i)));
        let text = r.render();
        assert_eq!(level_of(&r, "failing"), Level::Warn);
        assert_eq!(level_of(&r, "partial"), Level::Warn);
        assert!(text.contains("w0d4 arena9-bloom (2 of 3)") && !text.contains("w0d3 arena9-apple (3"), "{text}");
        assert!(text.contains("sealed partial") && text.contains("w0d3 arena9-apple") && text.contains("--label <L>"), "{text}");
        assert_eq!(level_of(&r, "baseline"), Level::Warn);
        assert!(text.contains("labels wait for it"), "{text}");
        snap.baseline_fails = GIVE_UP_AFTER;
        assert!(judge(|_| {}, &snap, &big_ok(), &h, Some(Ok(&i))).render().contains("labels now seal without it"));
        let quiet = judge(|_| {}, &snap_ok(), &big_ok(), &h, Some(Ok(&i)));
        assert!(!quiet.items.iter().any(|x| ["failing", "partial", "baseline"].contains(&x.name.as_str())), "{}", quiet.render());
    }

    /// A3-2: a failure of the archive itself (a read-only or failing volume …) or a run in which
    /// every push failed is a FAIL at once, with what to do; a pod's own failure isn't. A3-1: a
    /// big pod that keeps failing is named, never the tier. A3-7: forgotten pods drop out of the
    /// failing/partial lines, and copies still on the archive are named — a WARN with access on.
    #[test]
    fn status_fails_on_an_archive_failure_and_names_failing_big_pods() {
        let (h, i) = (host(), inv());
        let mut snap = snap_ok();
        snap.last_error = Some(ErrRecord { at: NOW - 60, label: Some("w1d2".into()), class: PushFailure::ArchiveIo, message: "Read-only file system (30)".into() });
        let r = judge(|_| {}, &snap, &big_ok(), &h, Some(Ok(&i)));
        assert_eq!(level_of(&r, "sync snap"), Level::Fail);
        assert!(r.render().contains("refuses writes"), "{}", r.render());
        snap.last_error.as_mut().unwrap().class = PushFailure::Other;
        assert_eq!(level_of(&judge(|_| {}, &snap, &big_ok(), &h, Some(Ok(&i))), "sync snap"), Level::Ok, "a pod's own failure");
        snap.last_run = Some(RunRecord { started: NOW - 100, finished: NOW - 50, ok: false, all_failed: true });
        let r = judge(|_| {}, &snap, &big_ok(), &h, Some(Ok(&i)));
        assert_eq!(level_of(&r, "sync snap"), Level::Fail);
        assert!(r.render().contains("every push of the last run failed"));
        let mut big = big_ok();
        big.pods_ok_at.insert("arena9-apple".into(), NOW - 100);
        big.pods_failing.insert("arena9-bloom".into(), "Other: exit 23".into());
        big.pods_failing.insert("arena9-gone".into(), "Other: exit 23".into());
        let r = judge(|_| {}, &snap_ok(), &big, &h, Some(Ok(&i)));
        assert_eq!(level_of(&r, "sync big"), Level::Ok, "one pod failing is that pod's");
        assert_eq!(level_of(&r, "big failing"), Level::Warn);
        assert!(r.render().contains("arena9-bloom (never; Other: exit 23)"), "{}", r.render());
        // A3-7, through the full inputs: forgotten pods leave the failing/partial lines.
        let n = ArchiveNames::new("arena9").unwrap();
        let mut s2 = snap_ok();
        let mut rec = LabelRecord::default();
        rec.fails.insert("arena9-gone".into(), 2);
        rec.given_up.insert("arena9-gone2".into());
        s2.labels.insert("w0d3".into(), rec);
        let forgotten = ["arena9-gone".to_string(), "arena9-gone2".to_string()];
        let mut on = h.clone();
        on.access_on = true;
        let still = ["arena9-gone".to_string()];
        let r = judge_status(&StatusInputs {
            names: &n,
            inventory: Some(Ok(&i)),
            host: Some(&on),
            probe: None,
            admin_pub: Some(ADMIN),
            push_pub: Some(PUSH),
            expected_backups: Some(""),
            snap: &s2,
            big: &big,
            unsealed: &[],
            custom_labels: &[],
            control_box_only: &[],
            not_on_list: &[],
            off_list: &[],
            forgotten: &forgotten,
            still_archived: &still,
            host_written_age: None,
            snap_need: None,
            big_need: None,
            now: NOW,
            keep_until_days: None,
            today_days: 20_000,
            token_warning: None,
            prices: None,
        });
        let text = r.render();
        assert!(!r.items.iter().any(|x| x.name == "failing" || x.name == "partial"), "{text}");
        assert!(!text.contains("arena9-gone (never"), "a forgotten pod's big failure isn't listed: {text}");
        assert_eq!(level_of(&r, "still archived"), Level::Warn);
        assert!(text.contains("READABLE BY EVERY PARTICIPANT") && text.contains("--delete-archived"), "{text}");
    }

    #[test]
    fn status_says_unknown_probe_and_retention() {
        let n = ArchiveNames::new("arena9").unwrap();
        let i = inv();
        let h = host();
        let (snap, big) = (snap_ok(), big_ok());
        let mut inputs = StatusInputs {
            names: &n,
            inventory: Some(Ok(&i)),
            host: Some(&h),
            probe: Some(Err("ssh: connect to host 192.0.2.1 port 22: Connection refused".into())),
            admin_pub: Some(ADMIN),
            push_pub: Some(PUSH),
            expected_backups: Some(""),
            snap: &snap,
            big: &big,
            unsealed: &["w1d2".to_string()],
            custom_labels: &["mylabel".to_string()],
            control_box_only: &["james-gpu".to_string()],
            not_on_list: &["arena9-gone".to_string()],
            off_list: &["arena9-left".to_string()],
            forgotten: &["arena9-withdrew".to_string()],
            still_archived: &["arena9-withdrew".to_string()],
            host_written_age: Some(7200),
            snap_need: Some(30 << 30),
            big_need: Some(1 << 30),
            now: NOW,
            keep_until_days: Some(20_000),
            today_days: 20_001,
            token_warning: Some(crate::archive::FLEET_TOKEN_WARNING),
            prices: Some(Prices { volume_gb_month: 0.05, server_month: Some(5.0), ipv4_month: Some(0.5) }),
        };
        let r = judge_status(&inputs);
        assert_eq!(level_of(&r, "probe"), Level::Unknown);
        assert!(!r.ok());
        assert_eq!(level_of(&r, "retention"), Level::Warn);
        assert_eq!(level_of(&r, "credentials"), Level::Warn);
        let text = r.render();
        assert_eq!(level_of(&r, "not on the list"), Level::Warn);
        assert_eq!(level_of(&r, "size snap"), Level::Warn, "a 50 GB volume for a 30 GiB seed");
        assert_eq!(level_of(&r, "size big"), Level::Ok);
        for needle in ["not final yet: w1d2", "mylabel", "james-gpu", "past ARCHIVE_KEEP_UNTIL", "≈ €10.50/month", "arena9-gone"] {
            assert!(text.contains(needle), "{needle}: {text}");
        }
        // A2-3: what is still archived only because it once was on the list, and what was forgotten.
        assert_eq!(level_of(&r, "off the list"), Level::Ok);
        assert!(text.contains("still archived (they were on it): arena9-left") && text.contains("forget"), "{text}");
        assert_eq!(level_of(&r, "forgotten"), Level::Ok);
        assert!(text.contains("never pushed again (`arena archive forget`): arena9-withdrew"), "{text}");
        inputs.keep_until_days = None;
        inputs.probe = None;
        let r = judge_status(&inputs);
        assert!(r.render().contains("no retention date set"));
        assert_eq!(level_of(&r, "probe"), Level::Unknown, "a server that wasn't probed isn't OK");
    }
}
