//! `arena teardown --check`: the end-of-program audit — is anything still billing, or still
//! scheduled to recreate something? (Ops playbook §5 "End-of-program teardown": destroy pods
//! on every provider, delete volumes, verify empty, confirm no scheduled jobs remain; §9
//! "Count API keys as spend".)
//!
//! Everything here is pure: the CLI gathers the inputs ([`Inputs`]: each provider's own pod
//! listing, the RunPod volume listing, the crontab's arena lines, `atq` + `at -c`, the
//! OpenRouter key listing, the local proxy file) and [`build`] turns them into a checklist
//! ([`Report`]) with the exact commands that would clean each item up. Nothing in this module
//! — or in its CLI caller — deletes anything: it only reads and prints.
//!
//! The rule behind every check: **a source that couldn't be read is UNKNOWN, never empty.**
//! A teardown check that reads a provider's 429 as "no pods" is how a forgotten pod bills for
//! a month, so an unknown fails the check (non-zero exit) just like something remaining. Only
//! a source that doesn't apply here at all — a provider with no key, `at` not installed — is
//! skipped, and the output says it wasn't checked.

use serde::Serialize;

use crate::fleet::{currency_symbol, fmt_money};
use crate::naming::qualify;
use crate::openrouter::{key_name, KeyInfo};
use crate::pod::Pod;
use crate::provider::runpod::NetworkVolume;
use crate::proxy::parse_nginx;
use crate::selector::Naming;
use crate::status::bills_hourly;

/// ~$ per GB per month for a RunPod network volume — the ops playbook's figure (§5: volumes
/// "charge ~$0.07/GB/month until explicitly deleted"). Always shown as an estimate: RunPod's
/// real rate depends on the tier and size, and this check never reads a bill.
pub const VOLUME_USD_PER_GB_MONTH: f64 = 0.07;

/// Every provider a fleet can span, with the config key that turns it on — so a provider
/// that isn't configured is listed as "not checked" instead of silently missing.
pub const PROVIDER_KEYS: [(&str, &str); 3] = [("runpod", "RUNPOD_API_KEY"), ("vast", "VAST_API_KEY"), ("hetzner", "HETZNER_API_KEY")];

/// Commands of the legacy arena-infra scripts. The playbook schedules `destroy_pods --yes
/// <name>` with `at` at create time, so a pending job running one of these is arena's even
/// though "arena" appears nowhere in it — and one left over would act on the next cohort's
/// machine of the same name.
const LEGACY_FLEET_COMMANDS: [&str; 7] =
    ["create_pods", "destroy_pods", "ready_pods", "check_pods", "list_pods", "update_proxy", "deploy_keys"];

/// Longest command text shown for one cron line / `at` job (the whole text is still matched).
const MAX_SHOWN: usize = 200;

/// What one source said.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe<T> {
    /// It answered.
    Got(T),
    /// It couldn't be read (an error, a timeout, an unexpected shape) — UNKNOWN: fails the check.
    Failed(String),
    /// It doesn't apply here (not configured, tool not installed), with why. Doesn't fail the
    /// check; the output says it wasn't checked.
    Skipped(String),
}

/// The crontab's arena lines, as the CLI found them with its `CRON_BEGIN`/`CRON_END` block
/// markers: inside the managed block, and job lines outside it that mention arena anyway
/// ([`cron_line_is_arena_job`]) — added by hand, so `arena cron remove` won't touch them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CronLines {
    pub managed: Vec<String>,
    pub unmanaged: Vec<String>,
}

/// One `atq` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtqLine {
    pub id: String,
    pub when: String,
    /// The queue letter; `=` means the job is running right now.
    pub queue: String,
    pub user: String,
}

/// One pending `at` job: its `atq` line and its script (`at -c <id>`), or why that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtJob {
    pub job: AtqLine,
    pub script: Result<String, String>,
}

/// The local proxy config file and its text (empty when it doesn't exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyFile {
    pub path: String,
    pub text: String,
}

/// Everything [`build`] judges, gathered by the CLI.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// Each *configured* provider's own listing (`Provider::list_by_provider`), errors as text.
    pub listings: Vec<(String, Result<Vec<Pod>, String>)>,
    pub volumes: Probe<Vec<NetworkVolume>>,
    pub keys: Probe<Vec<KeyInfo>>,
    pub cron: Probe<CronLines>,
    pub at: Probe<Vec<AtJob>>,
    pub proxy: Probe<ProxyFile>,
    /// Whose crontab and `at` queue were read.
    pub user: String,
}

/// One checklist line's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Checked: nothing left.
    Clear,
    /// Checked: something is still there.
    Remaining,
    /// Couldn't check — not known to be empty.
    Unknown,
    /// Doesn't apply here (not configured / not installed): not checked.
    Skipped,
}

impl Verdict {
    pub fn glyph(self) -> &'static str {
        match self {
            Verdict::Clear => "✓",
            Verdict::Remaining => "✗",
            Verdict::Unknown => "?",
            Verdict::Skipped => "–",
        }
    }
}

/// What a checklist item covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Area {
    Pods,
    Volumes,
    Keys,
    Cron,
    At,
    Proxy,
}

impl Area {
    fn label(self) -> &'static str {
        match self {
            Area::Pods => "pods",
            Area::Volumes => "network volumes",
            Area::Keys => "OpenRouter keys",
            Area::Cron => "cron",
            Area::At => "at jobs",
            Area::Proxy => "proxy forwards",
        }
    }
}

/// One thing still there (or, for `at`, a job that couldn't be read).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    Pod {
        id: String,
        name: String,
        provider: String,
        status: String,
        /// Costing its hourly rate now ([`bills_hourly`]); a stopped pod still bills its disk.
        billing: bool,
        cost_per_hr: Option<f64>,
        /// Named like this cohort's machines (`{prefix}-…` or on `MACHINE_NAME_LIST`).
        cohort: bool,
    },
    Volume {
        id: String,
        name: String,
        size_gb: Option<u32>,
        data_center: Option<String>,
        tier: Option<String>,
        /// `size_gb` × [`VOLUME_USD_PER_GB_MONTH`] — an estimate.
        est_usd_per_month: Option<f64>,
    },
    Key {
        name: String,
        usage_usd: Option<f64>,
        limit_usd: Option<f64>,
    },
    CronLine {
        /// Secret-looking values redacted ([`redact`]).
        line: String,
        /// Inside arena's managed block (`arena cron remove` clears it) or added by hand.
        managed: bool,
    },
    AtJob {
        id: String,
        when: String,
        queue: String,
        /// The job's command (redacted, clipped); `None` when it couldn't be shown.
        command: Option<String>,
        /// Why the job couldn't be read (`at -c` failed) — then it's unknown, not arena's.
        unreadable: Option<String>,
    },
    Forward {
        name: String,
        public_port: u16,
        target: String,
        provider: Option<String>,
    },
}

/// One checklist line, with what's still there and how to clean it up.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub area: Area,
    /// The provider (pods/volumes), user (cron/at) or file (proxy) covered; empty for keys.
    pub scope: String,
    pub verdict: Verdict,
    pub summary: String,
    pub entries: Vec<Entry>,
    /// What this item did not cover, and caveats on the fix.
    pub notes: Vec<String>,
    /// The exact commands that clean this item up — printed for the operator, never run.
    pub fix: Vec<String>,
}

impl Item {
    fn new(area: Area, scope: impl Into<String>, verdict: Verdict, summary: impl Into<String>) -> Self {
        Item { area, scope: scope.into(), verdict, summary: summary.into(), entries: Vec::new(), notes: Vec::new(), fix: Vec::new() }
    }

    fn title(&self) -> String {
        if self.scope.is_empty() {
            self.area.label().to_string()
        } else {
            format!("{} ({})", self.area.label(), self.scope)
        }
    }
}

/// The whole checklist. `clear` only when nothing remains *and* nothing was unknown.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub cohort: String,
    pub clear: bool,
    /// Items with something still there.
    pub remaining: usize,
    /// Items that couldn't be checked.
    pub unknown: usize,
    pub items: Vec<Item>,
}

/// Judge every input. Items come in a fixed order — pods per provider (every known provider:
/// a missing one is "not configured"), volumes, keys, cron, at, proxy.
pub fn build(inputs: &Inputs, naming: &Naming) -> Report {
    let mut items: Vec<Item> = Vec::new();
    for (provider, key) in PROVIDER_KEYS {
        match inputs.listings.iter().find(|(p, _)| p == provider) {
            Some((_, listing)) => items.push(pods_item(provider, listing.as_ref().map(Vec::as_slice), naming)),
            None => items.push(Item::new(Area::Pods, provider, Verdict::Skipped, format!("not configured (no {key}) — not checked"))),
        }
    }
    // A backend outside the known three (only fakes today) is still reported, never dropped.
    for (provider, listing) in inputs.listings.iter().filter(|(p, _)| !PROVIDER_KEYS.iter().any(|(k, _)| *k == p.as_str())) {
        items.push(pods_item(provider, listing.as_ref().map(Vec::as_slice), naming));
    }
    items.push(volumes_item(&inputs.volumes));
    items.push(keys_item(&inputs.keys, naming));
    items.push(cron_item(&inputs.cron, &inputs.user));
    items.push(at_item(&inputs.at, &inputs.user));
    let configured: Vec<&str> = inputs.listings.iter().map(|(p, _)| p.as_str()).collect();
    items.push(proxy_item(&inputs.proxy, &configured));
    let count = |v: Verdict| items.iter().filter(|i| i.verdict == v).count();
    let (remaining, unknown) = (count(Verdict::Remaining), count(Verdict::Unknown));
    Report { cohort: naming.prefix.to_string(), clear: remaining == 0 && unknown == 0, remaining, unknown, items }
}

/// Is `name` one of this cohort's machines: `{prefix}-…` (parked `-old`/`-new` twins
/// included), or an absolute (`@name`) entry on the list. Only labels pods — every pod on the
/// account counts as remaining, since the account pays for all of them.
fn is_cohort_pod(naming: &Naming, name: &str) -> bool {
    name.starts_with(&format!("{}-", naming.prefix)) || naming.list.iter().any(|e| qualify(naming.prefix, e) == name)
}

/// Is `name` a key `keys gen` would have minted for this cohort — its own canonical key name
/// (`{prefix}-…`, or an absolute list entry's bare name). The rule `keys revoke` targets by,
/// so the printed fix revokes exactly these.
fn is_cohort_key(naming: &Naming, name: &str) -> bool {
    key_name(naming.prefix, naming.list, name) == name
}

/// One provider's pods: every pod in any state counts — a stopped RunPod pod keeps its name
/// and still bills its disk; a powered-off Hetzner server bills in full. Only a pod the
/// provider itself reports as `TERMINATED` (deleted, billing nothing) is left out, with a
/// note. A failed listing is UNKNOWN: never "no pods".
pub fn pods_item(provider: &str, listing: Result<&[Pod], &String>, naming: &Naming) -> Item {
    let pods = match listing {
        Err(e) => {
            let mut item = Item::new(Area::Pods, provider, Verdict::Unknown, format!("couldn't list ({e}) — NOT known to be empty"));
            item.notes.push(format!("re-run the check (a rate limit or outage passes); if it keeps failing, look in the {provider} console"));
            return item;
        }
        Ok(pods) => pods,
    };
    let (gone, mut left): (Vec<&Pod>, Vec<&Pod>) = pods.iter().partition(|p| p.status.trim().eq_ignore_ascii_case("TERMINATED"));
    left.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
    let mut item = if left.is_empty() {
        Item::new(Area::Pods, provider, Verdict::Clear, "none")
    } else {
        let billing: Vec<&&Pod> = left.iter().filter(|p| bills_hourly(provider, &p.status)).collect();
        let stopped = left.len() - billing.len();
        let mut summary = remain(left.len());
        if stopped == 0 {
            summary.push_str(", all billing");
        } else {
            summary.push_str(&format!(
                " — {} billing, {stopped} stopped (a stopped pod keeps its name and still bills its disk)",
                billing.len()
            ));
        }
        let rate = total(billing.iter().filter_map(|p| p.cost_per_hr));
        if billing.iter().any(|p| p.cost_per_hr.is_some()) {
            summary.push_str(&format!("; burning {}/h", fmt_money(currency_symbol(provider), rate)));
        }
        let mut item = Item::new(Area::Pods, provider, Verdict::Remaining, summary);
        item.entries = left
            .iter()
            .map(|p| Entry::Pod {
                id: p.id.clone(),
                name: p.name.clone(),
                provider: provider.to_string(),
                status: p.status.clone(),
                billing: bills_hourly(provider, &p.status),
                cost_per_hr: p.cost_per_hr,
                cohort: is_cohort_pod(naming, &p.name),
            })
            .collect();
        item.fix.push(
            "arena pods terminate --all  # every pod on every configured provider, stopped ones included (--dry-run lists them first)"
                .into(),
        );
        let others: Vec<&str> = left.iter().filter(|p| !is_cohort_pod(naming, &p.name)).map(|p| p.name.as_str()).collect();
        if !others.is_empty() {
            item.notes.push(format!(
                "{} not named like this cohort's machines ({}) — `--all` terminates them too; to keep one, terminate the others one at a time: `arena pods terminate <id>`",
                others.len(),
                others.join(", ")
            ));
        }
        item
    };
    if !gone.is_empty() {
        let names: Vec<&str> = gone.iter().map(|p| p.name.as_str()).collect();
        item.notes.push(format!("{} listed as TERMINATED — already deleted, not counted: {}", gone.len(), names.join(", ")));
    }
    item
}

/// RunPod network volumes, each with its size and an estimated $/month — the bill that
/// outlives the program. Deleting one is irreversible, so the fix says so.
pub fn volumes_item(volumes: &Probe<Vec<NetworkVolume>>) -> Item {
    const SCOPE: &str = "runpod";
    let vols = match volumes {
        Probe::Skipped(why) => return Item::new(Area::Volumes, SCOPE, Verdict::Skipped, why.clone()),
        Probe::Failed(e) => {
            let mut item = Item::new(Area::Volumes, SCOPE, Verdict::Unknown, format!("couldn't check ({e}) — NOT known to be empty"));
            item.notes.push("look in the RunPod console (Storage) before calling the teardown done".into());
            return item;
        }
        Probe::Got(v) if v.is_empty() => return Item::new(Area::Volumes, SCOPE, Verdict::Clear, "none"),
        Probe::Got(v) => v,
    };
    let est = |gb: Option<u32>| gb.map(|g| f64::from(g) * VOLUME_USD_PER_GB_MONTH);
    let total_gb: u32 = vols.iter().filter_map(|v| v.size_gb).sum();
    let monthly = total(vols.iter().filter_map(|v| est(v.size_gb)));
    let no_size = vols.iter().filter(|v| v.size_gb.is_none()).count();
    let mut summary = format!(
        "{}, {total_gb} GB — ~${monthly:.2}/month (~${:.2}/day), estimated at ${VOLUME_USD_PER_GB_MONTH:.2}/GB/month",
        remain(vols.len()),
        monthly * 12.0 / 365.0
    );
    if no_size > 0 {
        summary.push_str(&format!(" ({no_size} of unknown size not included)"));
    }
    let mut item = Item::new(Area::Volumes, SCOPE, Verdict::Remaining, summary);
    let mut sorted: Vec<&NetworkVolume> = vols.iter().collect();
    sorted.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
    for v in sorted {
        item.entries.push(Entry::Volume {
            id: v.id.clone(),
            name: v.name.clone(),
            size_gb: v.size_gb,
            data_center: v.data_center.clone(),
            tier: v.tier.clone(),
            est_usd_per_month: est(v.size_gb),
        });
        item.fix.push(format!(
            "curl -X DELETE -H \"Authorization: Bearer $RUNPOD_API_KEY\" https://api.runpod.io/v2/network-volumes/{}",
            sh_word(&v.id)
        ));
    }
    item.notes.push(
        "deleting a volume is irreversible and destroys its data — confirm the groups are finished first (export RUNPOD_API_KEY \
         for the commands, or delete it in the RunPod console)"
            .into(),
    );
    item
}

/// OpenRouter keys still enabled for this cohort, with what they've spent — API keys are
/// spend too (§9), and an enabled key keeps working after its pod is gone.
pub fn keys_item(keys: &Probe<Vec<KeyInfo>>, naming: &Naming) -> Item {
    let all = match keys {
        Probe::Skipped(why) => return Item::new(Area::Keys, "", Verdict::Skipped, why.clone()),
        Probe::Failed(e) => return Item::new(Area::Keys, "", Verdict::Unknown, format!("couldn't list ({e}) — NOT known to be empty")),
        Probe::Got(k) => k,
    };
    let cohort: Vec<&KeyInfo> = all.iter().filter(|k| k.name.as_deref().is_some_and(|n| is_cohort_key(naming, n))).collect();
    let mut enabled: Vec<&KeyInfo> = cohort.iter().copied().filter(|k| !k.disabled).collect();
    enabled.sort_by(|a, b| a.name.cmp(&b.name));
    let disabled = cohort.len() - enabled.len();
    let mut item = if enabled.is_empty() {
        Item::new(Area::Keys, "", Verdict::Clear, format!("no enabled `{}` keys", naming.prefix))
    } else {
        let mut summary = format!("{} enabled `{}` key(s)", enabled.len(), naming.prefix);
        if enabled.iter().any(|k| k.usage.is_some()) {
            summary.push_str(&format!(", ${:.2} spent so far", total(enabled.iter().filter_map(|k| k.usage))));
        }
        if enabled.iter().any(|k| k.limit.is_some()) {
            summary.push_str(&format!(" (limits total ${:.2})", total(enabled.iter().filter_map(|k| k.limit))));
        }
        let mut item = Item::new(Area::Keys, "", Verdict::Remaining, summary);
        item.entries = enabled
            .iter()
            .map(|k| Entry::Key { name: k.name.clone().unwrap_or_default(), usage_usd: k.usage, limit_usd: k.limit })
            .collect();
        let names: Vec<String> = enabled.iter().filter_map(|k| k.name.as_deref()).map(sh_word).collect();
        item.fix.push(format!(
            "arena keys revoke {}  # by name: `keys revoke --all` only reaches machines that still have a pod",
            names.join(" ")
        ));
        item
    };
    if disabled > 0 {
        item.notes.push(format!("{disabled} disabled `{}` key(s) can't spend — not counted", naming.prefix));
    }
    item
}

/// Whether a crontab line outside arena's block is an arena job: a job line (not blank, a
/// comment, or a `NAME=value` setting) whose text mentions arena ([`is_arena_command`]).
pub fn cron_line_is_arena_job(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return false;
    }
    if t.split_once('=').is_some_and(|(name, _)| is_identifier(name.trim_end())) {
        return false; // an environment setting (`MAILTO=…`, `PATH=…`), not a job
    }
    is_arena_command(t)
}

/// The crontab's arena lines: the managed block (`arena cron remove` clears it) and any job
/// line outside it that mentions arena (to be removed by hand).
pub fn cron_item(cron: &Probe<CronLines>, user: &str) -> Item {
    let lines = match cron {
        Probe::Skipped(why) => return Item::new(Area::Cron, user, Verdict::Skipped, why.clone()),
        Probe::Failed(e) => return Item::new(Area::Cron, user, Verdict::Unknown, format!("couldn't read the crontab ({e})")),
        Probe::Got(l) => l,
    };
    let mut item = if lines.managed.is_empty() && lines.unmanaged.is_empty() {
        Item::new(Area::Cron, user, Verdict::Clear, format!("no arena lines in {user}'s crontab"))
    } else {
        let n = lines.managed.len() + lines.unmanaged.len();
        let mut item = Item::new(Area::Cron, user, Verdict::Remaining, format!("{n} arena line(s) still scheduled"));
        for (set, managed) in [(&lines.managed, true), (&lines.unmanaged, false)] {
            item.entries.extend(set.iter().map(|l| Entry::CronLine { line: clip(&redact(l.trim())), managed }));
        }
        if !lines.managed.is_empty() {
            item.fix.push("arena cron remove".into());
        }
        if !lines.unmanaged.is_empty() {
            item.fix.push(
                "crontab -e  # delete the lines marked `added by hand` yourself: `arena cron remove` only removes its own block".into(),
            );
        }
        item
    };
    item.notes.push(format!("only {user}'s crontab is read — not root's, and not /etc/cron.d"));
    item
}

/// Pending `at` jobs that mention arena (or run a legacy fleet script). A job whose script
/// couldn't be read is UNKNOWN; other users' jobs aren't visible unless run as root.
pub fn at_item(at: &Probe<Vec<AtJob>>, user: &str) -> Item {
    let jobs = match at {
        Probe::Skipped(why) => return Item::new(Area::At, user, Verdict::Skipped, why.clone()),
        Probe::Failed(e) => return Item::new(Area::At, user, Verdict::Unknown, format!("couldn't list the queue ({e})")),
        Probe::Got(j) => j,
    };
    let mut arena: Vec<Entry> = Vec::new();
    let mut unreadable: Vec<Entry> = Vec::new();
    let mut other: Vec<&str> = Vec::new();
    for j in jobs {
        let AtqLine { id, when, queue, .. } = &j.job;
        let entry = |command: Option<String>, unreadable: Option<String>| Entry::AtJob {
            id: id.clone(),
            when: when.clone(),
            queue: queue.clone(),
            command,
            unreadable,
        };
        match &j.script {
            Err(e) => unreadable.push(entry(None, Some(e.clone()))),
            Ok(script) => match at_command(script) {
                Some(cmd) if is_arena_command(&cmd) => arena.push(entry(Some(clip(&redact(&cmd))), None)),
                Some(_) => other.push(id),
                // A script in a shape we don't know: matched whole (over-flagging beats
                // missing a job) and never shown — its preamble is the environment, secrets
                // included.
                None if is_arena_command(script) => arena.push(entry(None, None)),
                None => other.push(id),
            },
        }
    }
    let ids = |es: &[Entry]| -> Vec<String> {
        es.iter().filter_map(|e| if let Entry::AtJob { id, .. } = e { Some(id.clone()) } else { None }).collect()
    };
    let mut item = if !arena.is_empty() {
        let mut item = Item::new(Area::At, user, Verdict::Remaining, format!("{} pending job(s) mention arena", arena.len()));
        item.fix.push(format!("atrm {}", ids(&arena).join(" ")));
        item
    } else if !unreadable.is_empty() {
        Item::new(Area::At, user, Verdict::Unknown, format!("couldn't read {} pending job(s) — NOT known to be arena-free", unreadable.len()))
    } else if other.is_empty() {
        Item::new(Area::At, user, Verdict::Clear, "no pending jobs")
    } else {
        Item::new(Area::At, user, Verdict::Clear, "no pending job mentions arena")
    };
    if !unreadable.is_empty() {
        item.notes.push(format!("read each with `at -c <id>` and `atrm` it if it's arena's: {}", ids(&unreadable).join(" ")));
    }
    if !other.is_empty() {
        item.notes.push(format!("{} other pending job(s) don't mention arena — not counted: {}", other.len(), other.join(" ")));
    }
    item.entries = arena.into_iter().chain(unreadable).collect();
    item.notes.push(format!("atq shows only {user}'s jobs unless run as root"));
    item
}

/// Forwards still in the local proxy config. `arena proxy apply` drops each one once its
/// provider lists without its pod; a forward owned by a provider that isn't configured here
/// is never confirmed gone, so `apply` keeps it — that one goes by hand.
pub fn proxy_item(proxy: &Probe<ProxyFile>, configured: &[&str]) -> Item {
    let file = match proxy {
        Probe::Skipped(why) => return Item::new(Area::Proxy, "", Verdict::Skipped, why.clone()),
        Probe::Failed(e) => return Item::new(Area::Proxy, "", Verdict::Unknown, e.clone()),
        Probe::Got(f) => f,
    };
    let mut forwards = parse_nginx(&file.text);
    forwards.sort_by_key(|f| f.public_port);
    if forwards.is_empty() {
        return Item::new(Area::Proxy, file.path.clone(), Verdict::Clear, "none");
    }
    let mut item = Item::new(Area::Proxy, file.path.clone(), Verdict::Remaining, format!("{} still in the config", forwards.len()));
    item.fix.push(
        "arena proxy apply  # after the pods are gone: drops each forward once its provider lists without its pod".into(),
    );
    let orphaned: Vec<&str> = forwards
        .iter()
        .filter(|f| f.provider.as_deref().is_some_and(|p| !configured.contains(&p)))
        .map(|f| f.name.as_str())
        .collect();
    if !orphaned.is_empty() {
        item.notes.push(format!(
            "owned by a provider that isn't configured here, so `proxy apply` keeps them — delete their blocks from {} by hand: {}",
            file.path,
            orphaned.join(", ")
        ));
    }
    item.entries = forwards
        .iter()
        .map(|f| Entry::Forward { name: f.name.clone(), public_port: f.public_port, target: f.target(), provider: f.provider.clone() })
        .collect();
    item
}

/// Does a scheduled command look like arena's: it mentions arena (any case — the binary, the
/// repo, a config path) or runs a legacy fleet script ([`LEGACY_FLEET_COMMANDS`]).
pub fn is_arena_command(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    lower.contains("arena") || LEGACY_FLEET_COMMANDS.iter().any(|c| lower.contains(c))
}

/// Parse `atq`: `<id>\t<date> <queue> <user>` per job, e.g. `12\tThu Oct  9 14:00:00 2026 a
/// dev`. A line that doesn't fit is an error — never skipped, since a skipped line is a job
/// the check didn't see.
pub fn parse_atq(text: &str) -> Result<Vec<AtqLine>, String> {
    let bad = |line: &str| format!("unexpected atq line `{}`", clip(line.trim()));
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (id, rest) = line.trim().split_once(char::is_whitespace).ok_or_else(|| bad(line))?;
        let words: Vec<&str> = rest.split_whitespace().collect();
        if !id.chars().all(|c| c.is_ascii_digit()) || words.len() < 3 {
            return Err(bad(line));
        }
        let n = words.len();
        out.push(AtqLine {
            id: id.to_string(),
            when: words[..n - 2].join(" "),
            queue: words[n - 2].to_string(),
            user: words[n - 1].to_string(),
        });
    }
    Ok(out)
}

/// The command an `at -c <id>` script runs, without at's preamble — which is the whole
/// environment of the shell that scheduled it (keys included), so it is never matched on or
/// shown. The preamble ends with at's `cd <dir> || { … }` block; at ≥ 3.1.14 then wraps the
/// commands in a heredoc for the user's shell (`${SHELL:-/bin/sh} << 'marcinDELIMITER…'`),
/// which is unwrapped. Lines are joined with `; `. `None` when the script isn't in that shape.
pub fn at_command(script: &str) -> Option<String> {
    let lines: Vec<&str> = script.lines().collect();
    let cd = lines.iter().position(|l| {
        let t = l.trim();
        t.starts_with("cd ") && t.ends_with("|| {")
    })?;
    let close = lines[cd..].iter().position(|l| l.trim() == "}")?;
    let mut body: Vec<&str> = lines[cd + close + 1..].iter().copied().filter(|l| !l.trim().is_empty()).collect();
    if let Some(first) = body.first() {
        let delim = first
            .trim()
            .strip_prefix("${SHELL:-/bin/sh}")
            .and_then(|r| r.trim_start().strip_prefix("<<"))
            .map(|d| d.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
            .filter(|d| !d.is_empty());
        if let Some(d) = delim {
            body = body[1..].iter().copied().take_while(|l| l.trim() != d).collect();
        }
    }
    Some(body.iter().map(|l| l.trim()).collect::<Vec<_>>().join("; "))
}

/// Hide what looks like a secret in a command we print (a crontab line, an `at` job): the
/// value of a `NAME=value` word whose NAME says key/token/secret/password, and words shaped
/// like provider keys (`sk-…`, `rpa_…`). A command is shown to say *what* is scheduled.
pub fn redact(cmd: &str) -> String {
    const SECRET_NAMES: [&str; 5] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD"];
    cmd.split(' ')
        .map(|w| {
            if let Some((name, _)) = w.split_once('=') {
                let bare = name.trim_start_matches(|c| c == '\'' || c == '"');
                if is_identifier(bare) && SECRET_NAMES.iter().any(|s| bare.to_ascii_uppercase().contains(s)) {
                    return format!("{name}=…");
                }
            }
            let v = w.trim_matches(|c| c == '\'' || c == '"');
            if (v.starts_with("sk-") || v.starts_with("rpa_")) && v.len() > 8 {
                return "…".to_string();
            }
            w.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `N remain`, or `1 remains`.
fn remain(n: usize) -> String {
    if n == 1 {
        "1 remains".into()
    } else {
        format!("{n} remain")
    }
}

/// The sum of some amounts, `0.0` when there are none (`Iterator::sum` of no `f64`s is
/// `-0.0`, which prints as `$-0.00`).
fn total(amounts: impl Iterator<Item = f64>) -> f64 {
    amounts.fold(0.0, |a, b| a + b)
}

/// A shell identifier (`[A-Za-z_][A-Za-z0-9_]*`).
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One word of a printed command: as-is when it's plainly safe, else single-quoted — names
/// and ids come from provider APIs, and a fix line gets pasted into a shell.
fn sh_word(s: &str) -> String {
    let plain = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@:/+=,".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// At most [`MAX_SHOWN`] chars (char-boundary safe), marked when cut.
fn clip(s: &str) -> String {
    if s.chars().count() > MAX_SHOWN {
        format!("{}…", s.chars().take(MAX_SHOWN).collect::<String>())
    } else {
        s.to_string()
    }
}

/// The lines under an item, one per entry, the name/id column aligned within the item.
fn entry_lines(entries: &[Entry]) -> Vec<String> {
    let key = |e: &Entry| match e {
        Entry::Pod { name, .. } | Entry::Key { name, .. } | Entry::Forward { name, .. } => name.clone(),
        Entry::Volume { id, .. } => id.clone(),
        Entry::AtJob { id, .. } => format!("#{id}"),
        Entry::CronLine { .. } => String::new(),
    };
    let w = entries.iter().map(|e| key(e).chars().count()).max().unwrap_or(0);
    entries
        .iter()
        .map(|e| {
            let k = format!("{:<w$}", key(e));
            let line = match e {
                Entry::Pod { id, provider, status, billing, cost_per_hr, cohort, .. } => {
                    let cost = match (billing, cost_per_hr) {
                        (true, Some(c)) => format!("billing {}/h", fmt_money(currency_symbol(provider), *c)),
                        (true, None) => "billing".to_string(),
                        (false, _) => "stopped — still bills its disk".to_string(),
                    };
                    let other = if *cohort { "" } else { "  (not this cohort's)" };
                    format!("{k}  id={id}  {status}  {cost}{other}")
                }
                Entry::Volume { name, size_gb, data_center, tier, est_usd_per_month, .. } => {
                    let size = size_gb.map_or("? GB".to_string(), |g| format!("{g} GB"));
                    let est = est_usd_per_month.map_or("~$?/month".to_string(), |m| format!("~${m:.2}/month"));
                    let place = [data_center.as_deref(), tier.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" ");
                    let mut parts = vec![k, format!("\"{name}\""), size];
                    if !place.is_empty() {
                        parts.push(place);
                    }
                    parts.push(est);
                    parts.join("  ")
                }
                Entry::Key { usage_usd, limit_usd, .. } => {
                    let used = usage_usd.map_or("$?".to_string(), |u| format!("${u:.2}"));
                    let limit = limit_usd.map_or("no limit".to_string(), |l| format!("limit ${l:.2}"));
                    format!("{k}  {used} used, {limit}")
                }
                Entry::CronLine { line, managed } => {
                    if *managed {
                        line.clone()
                    } else {
                        format!("{line}  (added by hand)")
                    }
                }
                Entry::AtJob { when, queue, command, unreadable, .. } => {
                    let running = if queue == "=" { " (running now)" } else { "" };
                    let what = match (command, unreadable) {
                        (_, Some(e)) => format!("couldn't read it: {e}"),
                        (Some(c), None) => c.clone(),
                        (None, None) => "(script in an unrecognised format — mentions arena; see `at -c`)".to_string(),
                    };
                    format!("{k}  {when}{running}  {what}")
                }
                Entry::Forward { public_port, target, provider, .. } => {
                    let owner = provider.as_deref().map(|p| format!("  ({p})")).unwrap_or_default();
                    format!("{k}  :{public_port} → {target}{owner}")
                }
            };
            line.trim_end().to_string()
        })
        .collect()
}

impl Report {
    /// The checklist as text: one `✓ ✗ ? –` line per item, what's still there under it, its
    /// notes and fix commands, then the verdict.
    pub fn render(&self) -> String {
        let mut out = format!("Teardown check for cohort `{}` (read-only: nothing is changed)\n\n", self.cohort);
        for item in &self.items {
            out.push_str(&format!("{} {}: {}\n", item.verdict.glyph(), item.title(), item.summary));
            for l in entry_lines(&item.entries) {
                out.push_str(&format!("      {l}\n"));
            }
            for n in &item.notes {
                out.push_str(&format!("    note: {n}\n"));
            }
            for f in &item.fix {
                out.push_str(&format!("    fix:  {f}\n"));
            }
        }
        out.push('\n');
        out.push_str(&self.verdict_line());
        out.push('\n');
        if !self.clear {
            out.push_str(
                "Suggested order: cron and at first (so nothing recreates a pod), then keys, pods, volumes, \
                 and `arena proxy apply` last — then re-run `arena teardown --check`.\n",
            );
        }
        out
    }

    /// The bottom line, also the error a non-clear check exits with.
    pub fn verdict_line(&self) -> String {
        if self.clear {
            let skipped = self.items.iter().filter(|i| i.verdict == Verdict::Skipped).count();
            let note = if skipped > 0 { format!(" ({skipped} not applicable here — marked –)") } else { String::new() };
            format!("ALL CLEAR: nothing left billing or scheduled{note}.")
        } else {
            format!(
                "NOT CLEAR: {} item(s) still remain, {} couldn't be checked.",
                self.remaining, self.unknown
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Vec<String> {
        ["apple", "bloom", "cloud", "@james-gpu"].iter().map(|s| s.to_string()).collect()
    }

    fn pod(name: &str, status: &str, cost: Option<f64>) -> Pod {
        Pod {
            id: format!("id-{name}"),
            name: name.into(),
            provider: "runpod".into(),
            status: status.into(),
            cost_per_hr: cost,
            ..Default::default()
        }
    }

    fn key(name: &str, disabled: bool, usage: Option<f64>, limit: Option<f64>) -> KeyInfo {
        KeyInfo { hash: format!("h-{name}"), name: Some(name.into()), label: None, disabled, limit, usage }
    }

    /// An `at -c` script as at 3.2 prints it: the environment (a key in it), the `cd` block,
    /// then the commands in a heredoc for the user's shell.
    fn at_script(cmd: &str) -> String {
        format!(
            "#!/bin/sh\n# atrun uid=1000 gid=1000\n# mail dev 0\numask 2\n\
             RUNPOD_API_KEY=rpa_SECRETSECRET; export RUNPOD_API_KEY\nPWD=/home/dev/arena-infra-rs; export PWD\n\
             cd /home/dev/arena-infra-rs || {{\n\t echo 'Execution directory inaccessible' >&2\n\t exit 1\n}}\n\
             ${{SHELL:-/bin/sh}} << 'marcinDELIMITER2f0a6b1c'\n{cmd}\n\nmarcinDELIMITER2f0a6b1c\n"
        )
    }

    fn inputs() -> Inputs {
        Inputs {
            listings: vec![("runpod".into(), Ok(vec![])), ("hetzner".into(), Ok(vec![]))],
            volumes: Probe::Got(vec![]),
            keys: Probe::Got(vec![key("arena7-apple", false, Some(3.0), Some(5.0))]),
            cron: Probe::Got(CronLines::default()),
            at: Probe::Skipped("`at` isn't installed here".into()),
            proxy: Probe::Got(ProxyFile { path: "/srv/proxy.conf".into(), text: String::new() }),
            user: "dev".into(),
        }
    }

    #[test]
    fn all_clear_when_everything_answered_empty() {
        let names = list();
        let naming = Naming { prefix: "arena8", list: &names };
        let r = build(&inputs(), &naming);
        assert!(r.clear, "{}", r.render());
        assert_eq!((r.remaining, r.unknown), (0, 0));
        let v: Vec<(Area, &str, Verdict)> = r.items.iter().map(|i| (i.area, i.scope.as_str(), i.verdict)).collect();
        assert_eq!(
            v,
            [
                (Area::Pods, "runpod", Verdict::Clear),
                (Area::Pods, "vast", Verdict::Skipped), // not configured: said, not failed
                (Area::Pods, "hetzner", Verdict::Clear),
                (Area::Volumes, "runpod", Verdict::Clear),
                (Area::Keys, "", Verdict::Clear), // another cohort's key isn't ours
                (Area::Cron, "dev", Verdict::Clear),
                (Area::At, "dev", Verdict::Skipped),
                (Area::Proxy, "/srv/proxy.conf", Verdict::Clear),
            ]
        );
        let text = r.render();
        assert!(text.contains("– pods (vast): not configured (no VAST_API_KEY) — not checked"), "{text}");
        assert!(text.contains("– at jobs (dev): `at` isn't installed here"), "{text}");
        assert!(text.contains("ALL CLEAR: nothing left billing or scheduled (2 not applicable here — marked –)."), "{text}");
        assert!(!text.contains("fix:") && !text.contains("Suggested order"), "{text}");
    }

    /// A provider that failed to list is UNKNOWN — never "empty" — and fails the check even
    /// when everything else is clear.
    #[test]
    fn a_provider_that_failed_to_list_is_unknown_and_not_clear() {
        let names = list();
        let naming = Naming { prefix: "arena8", list: &names };
        let mut i = inputs();
        i.listings.push(("vast".into(), Err("provider error: vast list HTTP 429 Too Many Requests".into())));
        let r = build(&i, &naming);
        assert!(!r.clear);
        assert_eq!((r.remaining, r.unknown), (0, 1));
        let vast = r.items.iter().find(|it| it.scope == "vast").unwrap();
        assert_eq!(vast.verdict, Verdict::Unknown);
        assert!(vast.summary.contains("HTTP 429") && vast.summary.contains("NOT known to be empty"), "{}", vast.summary);
        assert!(r.render().contains("? pods (vast): couldn't list"), "{}", r.render());
        assert_eq!(r.verdict_line(), "NOT CLEAR: 0 item(s) still remain, 1 couldn't be checked.");
    }

    /// Every pod in any state counts — a stopped pod keeps its name and bills its disk; only a
    /// pod reported TERMINATED (deleted) is left out, with a note. Off-cohort pods count too
    /// (the account pays for them) but are named, since `--all` would take them as well.
    #[test]
    fn stopped_pods_count_as_remaining() {
        let names = list();
        let naming = Naming { prefix: "arena8", list: &names };
        let pods = vec![
            pod("arena8-bloom", "EXITED", Some(0.17)),
            pod("arena8-apple", "RUNNING", Some(0.17)),
            pod("arena8-apple-old", "EXITED", None),
            pod("james-gpu", "RUNNING", Some(0.44)),
            pod("someone-else", "RUNNING", None),
            pod("arena8-cloud", "TERMINATED", None),
        ];
        let item = pods_item("runpod", Ok(&pods), &naming);
        assert_eq!(item.verdict, Verdict::Remaining);
        assert_eq!(
            item.summary,
            "5 remain — 3 billing, 2 stopped (a stopped pod keeps its name and still bills its disk); burning $0.61/h"
        );
        let lines = entry_lines(&item.entries);
        assert_eq!(
            lines,
            [
                "arena8-apple      id=id-arena8-apple  RUNNING  billing $0.17/h",
                "arena8-apple-old  id=id-arena8-apple-old  EXITED  stopped — still bills its disk",
                "arena8-bloom      id=id-arena8-bloom  EXITED  stopped — still bills its disk",
                "james-gpu         id=id-james-gpu  RUNNING  billing $0.44/h",
                "someone-else      id=id-someone-else  RUNNING  billing  (not this cohort's)",
            ]
        );
        assert_eq!(item.fix.len(), 1);
        assert!(item.fix[0].starts_with("arena pods terminate --all  #"), "{:?}", item.fix);
        assert!(item.notes[0].starts_with("1 not named like this cohort's machines (someone-else)"), "{:?}", item.notes);
        assert_eq!(item.notes[1], "1 listed as TERMINATED — already deleted, not counted: arena8-cloud");
        // Only stopped pods: still remaining — the whole point of the check.
        let stopped = [pod("arena8-bloom", "EXITED", Some(0.17))];
        let item = pods_item("runpod", Ok(&stopped), &naming);
        assert_eq!(item.verdict, Verdict::Remaining);
        assert!(item.summary.starts_with("1 remains — 0 billing, 1 stopped"), "{}", item.summary);
        assert!(!item.summary.contains("burning"), "a stopped pod's rate isn't burning: {}", item.summary);
        // A powered-off Hetzner server bills in full, in €.
        let mut off = pod("arena8-vm", "off", Some(0.006));
        off.provider = "hetzner".into();
        let item = pods_item("hetzner", Ok(std::slice::from_ref(&off)), &naming);
        assert_eq!(item.summary, "1 remains, all billing; burning €0.006/h");
        // Only TERMINATED left: clear, with the note.
        let gone = [pod("arena8-cloud", "TERMINATED", None)];
        let item = pods_item("runpod", Ok(&gone), &naming);
        assert_eq!((item.verdict, item.summary.as_str()), (Verdict::Clear, "none"));
        assert_eq!(item.notes.len(), 1);
    }

    #[test]
    fn volume_cost_is_an_estimate_per_gb_month_with_a_delete_command_each() {
        let vols = vec![
            NetworkVolume { id: "vol2".into(), name: "group-b".into(), size_gb: Some(2048), data_center: Some("EU-RO-1".into()), tier: Some("STANDARD".into()) },
            NetworkVolume { id: "vol1".into(), name: "group-a".into(), size_gb: Some(100), data_center: None, tier: None },
            NetworkVolume { id: "odd id".into(), name: "x".into(), size_gb: None, data_center: None, tier: None },
        ];
        let item = volumes_item(&Probe::Got(vols));
        assert_eq!(item.verdict, Verdict::Remaining);
        // 2148 GB × $0.07 = $150.36/month; the playbook's "2 TB ≈ $4.80/day" scale.
        assert_eq!(
            item.summary,
            "3 remain, 2148 GB — ~$150.36/month (~$4.94/day), estimated at $0.07/GB/month (1 of unknown size not included)"
        );
        let est: Vec<Option<f64>> = item
            .entries
            .iter()
            .map(|e| if let Entry::Volume { est_usd_per_month, .. } = e { *est_usd_per_month } else { None })
            .collect();
        assert_eq!(est.len(), 3);
        assert!((est[0].unwrap() - 7.0).abs() < 1e-9 && (est[1].unwrap() - 143.36).abs() < 1e-9 && est[2].is_none(), "{est:?}");
        assert_eq!(
            entry_lines(&item.entries),
            [
                "vol1    \"group-a\"  100 GB  ~$7.00/month",
                "vol2    \"group-b\"  2048 GB  EU-RO-1 STANDARD  ~$143.36/month",
                "odd id  \"x\"  ? GB  ~$?/month",
            ]
        );
        assert_eq!(
            item.fix[0],
            "curl -X DELETE -H \"Authorization: Bearer $RUNPOD_API_KEY\" https://api.runpod.io/v2/network-volumes/vol1"
        );
        assert!(item.fix[2].ends_with("/network-volumes/'odd id'"), "a pasted id is quoted: {:?}", item.fix);
        assert!(item.notes[0].contains("irreversible"), "{:?}", item.notes);
        // Couldn't check → unknown, with where to look; not configured → skipped.
        let failed = volumes_item(&Probe::Failed("list network volumes HTTP 500".into()));
        assert_eq!(failed.verdict, Verdict::Unknown);
        assert!(failed.summary.contains("couldn't check (list network volumes HTTP 500)") && failed.notes[0].contains("RunPod console"));
        assert_eq!(volumes_item(&Probe::Skipped("RunPod isn't configured".into())).verdict, Verdict::Skipped);
        assert_eq!(volumes_item(&Probe::Got(vec![])).verdict, Verdict::Clear);
        let one = NetworkVolume { id: "v".into(), name: String::new(), size_gb: None, data_center: None, tier: None };
        let item = volumes_item(&Probe::Got(vec![one]));
        assert!(item.summary.starts_with("1 remains, 0 GB — ~$0.00/month (~$0.00/day)"), "{}", item.summary);
    }

    /// Enabled keys of this cohort remain, with usage; disabled ones and other cohorts' don't
    /// count. The fix names them (`--all` would only reach machines that still have a pod).
    #[test]
    fn enabled_cohort_keys_remain_with_usage() {
        let names = list();
        let naming = Naming { prefix: "arena8", list: &names };
        let keys = vec![
            key("arena8-bloom", false, Some(1.25), Some(5.0)),
            key("arena8-apple", false, Some(0.5), None),
            key("arena8-cloud", true, Some(4.0), Some(5.0)),
            key("james-gpu", false, None, Some(10.0)),
            key("arena7-apple", false, Some(3.0), Some(5.0)),
            key("admin", false, Some(99.0), None),
        ];
        let item = keys_item(&Probe::Got(keys), &naming);
        assert_eq!(item.verdict, Verdict::Remaining);
        assert_eq!(item.summary, "3 enabled `arena8` key(s), $1.75 spent so far (limits total $15.00)");
        assert_eq!(
            entry_lines(&item.entries),
            ["arena8-apple  $0.50 used, no limit", "arena8-bloom  $1.25 used, limit $5.00", "james-gpu     $? used, limit $10.00"]
        );
        assert!(item.fix[0].starts_with("arena keys revoke arena8-apple arena8-bloom james-gpu  #"), "{:?}", item.fix);
        assert_eq!(item.notes, ["1 disabled `arena8` key(s) can't spend — not counted"]);
        // No usage or limit reported: said by omission, never as `$0.00` (or `$-0.00`).
        let bare = keys_item(&Probe::Got(vec![key("arena8-apple", false, None, None)]), &naming);
        assert_eq!(bare.summary, "1 enabled `arena8` key(s)");
        let none = keys_item(&Probe::Got(vec![key("arena8-x", true, None, None)]), &naming);
        assert_eq!((none.verdict, none.summary.as_str()), (Verdict::Clear, "no enabled `arena8` keys"));
        assert_eq!(keys_item(&Probe::Failed("HTTP 401".into()), &naming).verdict, Verdict::Unknown);
        assert_eq!(keys_item(&Probe::Skipped("no provisioning key".into()), &naming).verdict, Verdict::Skipped);
        // A key name that isn't shell-plain is quoted in the fix.
        let odd = keys_item(&Probe::Got(vec![key("arena8-a b;rm", false, None, None)]), &naming);
        assert!(odd.fix[0].starts_with("arena keys revoke 'arena8-a b;rm'  #"), "{:?}", odd.fix);
    }

    #[test]
    fn crontab_lines_outside_the_block_are_jobs_that_mention_arena() {
        for (line, arena) in [
            ("*/15 * * * * /usr/local/bin/arena --config /x pods backup --yes", true),
            ("0 3 * * * /home/dev/ARENA_materials/sync.sh", true),
            ("0 * * * * destroy_pods --yes apple", true),
            ("0 3 * * * /usr/bin/certbot renew", false),
            ("# arena backup, disabled", false),
            ("ARENA_START_DATE=2026-10-01", false),
            ("PATH=/home/dev/arena/bin:/usr/bin", false),
            ("   ", false),
        ] {
            assert_eq!(cron_line_is_arena_job(line), arena, "{line}");
        }
    }

    #[test]
    fn cron_item_lists_managed_and_hand_added_lines_with_their_fixes() {
        let lines = CronLines {
            managed: vec!["*/15 * * * * /bin/arena --config /c pods backup --no-pull --yes >> /h/arena-cron.log 2>&1".into()],
            unmanaged: vec!["0 9 * * * HF_TOKEN=hf_SECRET /bin/arena pods copy-keys --yes".into()],
        };
        let item = cron_item(&Probe::Got(lines), "dev");
        assert_eq!((item.verdict, item.summary.as_str()), (Verdict::Remaining, "2 arena line(s) still scheduled"));
        let shown = entry_lines(&item.entries);
        assert!(shown[0].starts_with("*/15 * * * * /bin/arena") && !shown[0].contains("added by hand"), "{shown:?}");
        assert_eq!(shown[1], "0 9 * * * HF_TOKEN=… /bin/arena pods copy-keys --yes  (added by hand)");
        assert_eq!(item.fix[0], "arena cron remove");
        assert!(item.fix[1].starts_with("crontab -e  #"), "{:?}", item.fix);
        assert_eq!(item.notes, ["only dev's crontab is read — not root's, and not /etc/cron.d"]);
        let clean = cron_item(&Probe::Got(CronLines::default()), "dev");
        assert_eq!((clean.verdict, clean.summary.as_str()), (Verdict::Clear, "no arena lines in dev's crontab"));
        assert_eq!(cron_item(&Probe::Failed("`crontab -l` timed out".into()), "dev").verdict, Verdict::Unknown);
        assert_eq!(cron_item(&Probe::Skipped("crontab isn't installed".into()), "dev").verdict, Verdict::Skipped);
    }

    #[test]
    fn atq_parses_ids_dates_queues_and_users_and_rejects_junk() {
        let out = "12\tThu Oct  9 14:00:00 2026 a dev\n7\tWed Oct  8 23:30:00 2026 = root\n\n";
        assert_eq!(
            parse_atq(out).unwrap(),
            [
                AtqLine { id: "12".into(), when: "Thu Oct 9 14:00:00 2026".into(), queue: "a".into(), user: "dev".into() },
                AtqLine { id: "7".into(), when: "Wed Oct 8 23:30:00 2026".into(), queue: "=".into(), user: "root".into() },
            ]
        );
        assert!(parse_atq("").unwrap().is_empty());
        for junk in ["Cannot open lockfile", "12", "x1\tThu Oct 9 14:00:00 2026 a dev", "3\ta dev"] {
            assert!(parse_atq(junk).unwrap_err().contains("unexpected atq line"), "{junk}");
        }
    }

    /// `at -c`: the command only — never the environment preamble (which holds keys and an
    /// `arena` PWD that would match everything) — heredoc-wrapped (at ≥ 3.1.14) or not.
    #[test]
    fn at_c_yields_only_the_jobs_command() {
        assert_eq!(at_command(&at_script("destroy_pods --yes apple")).as_deref(), Some("destroy_pods --yes apple"));
        assert_eq!(at_command(&at_script("echo hi\nmake backup")).as_deref(), Some("echo hi; make backup"));
        let unwrapped = "#!/bin/sh\nHOME=/home/dev; export HOME\ncd /tmp || {\n\t echo 'Execution directory inaccessible' >&2\n\t exit 1\n}\n/usr/bin/arena pods terminate apple --yes\n";
        assert_eq!(at_command(unwrapped).as_deref(), Some("/usr/bin/arena pods terminate apple --yes"));
        assert_eq!(at_command("#!/bin/sh\necho no cd block\n"), None);
        // The environment mentions arena; the command doesn't — not arena's.
        let script = at_script("/usr/bin/certbot renew");
        assert!(is_arena_command(&script) && !is_arena_command(&at_command(&script).unwrap()));
    }

    #[test]
    fn at_item_counts_arena_jobs_and_treats_unreadable_ones_as_unknown() {
        let job = |id: &str, queue: &str, script: Result<String, String>| AtJob {
            job: AtqLine { id: id.into(), when: "Thu Oct 9 14:00:00 2026".into(), queue: queue.into(), user: "dev".into() },
            script,
        };
        let jobs = vec![
            job("12", "a", Ok(at_script("destroy_pods --yes apple"))),
            job("13", "=", Ok(at_script("RUNPOD_API_KEY=rpa_SECRETSECRET /usr/bin/arena pods up -n 3 --yes"))),
            job("14", "a", Ok(at_script("/usr/bin/certbot renew"))),
            job("15", "a", Err("Cannot find jobid 15".into())),
            job("16", "a", Ok("#!/bin/sh\nPWD=/home/dev/arena; export PWD\nweird format\n".into())),
        ];
        let item = at_item(&Probe::Got(jobs), "dev");
        assert_eq!((item.verdict, item.summary.as_str()), (Verdict::Remaining, "3 pending job(s) mention arena"));
        assert_eq!(item.fix, ["atrm 12 13 16"]);
        let shown = entry_lines(&item.entries);
        assert_eq!(
            shown,
            [
                "#12  Thu Oct 9 14:00:00 2026  destroy_pods --yes apple",
                "#13  Thu Oct 9 14:00:00 2026 (running now)  RUNPOD_API_KEY=… /usr/bin/arena pods up -n 3 --yes",
                "#16  Thu Oct 9 14:00:00 2026  (script in an unrecognised format — mentions arena; see `at -c`)",
                "#15  Thu Oct 9 14:00:00 2026  couldn't read it: Cannot find jobid 15",
            ]
        );
        assert!(shown.iter().all(|l| !l.contains("SECRET")), "{shown:?}");
        assert_eq!(item.notes[0], "read each with `at -c <id>` and `atrm` it if it's arena's: 15");
        assert_eq!(item.notes[1], "1 other pending job(s) don't mention arena — not counted: 14");
        // Only an unreadable job: unknown, not clear.
        let item = at_item(&Probe::Got(vec![job("15", "a", Err("permission denied".into()))]), "dev");
        assert_eq!(item.verdict, Verdict::Unknown);
        // Only unrelated jobs: clear, and said so.
        let item = at_item(&Probe::Got(vec![job("14", "a", Ok(at_script("/usr/bin/certbot renew")))]), "dev");
        assert_eq!((item.verdict, item.summary.as_str()), (Verdict::Clear, "no pending job mentions arena"));
        assert_eq!(at_item(&Probe::Got(vec![]), "dev").summary, "no pending jobs");
        assert_eq!(at_item(&Probe::Failed("atq: permission denied".into()), "dev").verdict, Verdict::Unknown);
    }

    #[test]
    fn proxy_forwards_still_present_remain_and_orphans_are_named() {
        use crate::proxy::{render_nginx, Forward};
        let fwd = |name: &str, port: u16, provider: &str| Forward {
            name: name.into(),
            public_port: port,
            target_ip: "203.0.113.7".into(),
            target_port: 22001,
            provider: Some(provider.into()),
            pod_id: None,
        };
        let text = render_nginx(&[fwd("arena8-bloom", 9501, "runpod"), fwd("arena8-apple", 9500, "vast")]);
        let item = proxy_item(&Probe::Got(ProxyFile { path: "/srv/proxy.conf".into(), text }), &["runpod"]);
        assert_eq!((item.verdict, item.summary.as_str()), (Verdict::Remaining, "2 still in the config"));
        assert_eq!(
            entry_lines(&item.entries),
            ["arena8-apple  :9500 → 203.0.113.7:22001  (vast)", "arena8-bloom  :9501 → 203.0.113.7:22001  (runpod)"]
        );
        assert!(item.fix[0].starts_with("arena proxy apply  #"), "{:?}", item.fix);
        assert!(item.notes[0].contains("by hand: arena8-apple"), "{:?}", item.notes);
        let empty = proxy_item(&Probe::Got(ProxyFile { path: "/srv/proxy.conf".into(), text: String::new() }), &["runpod"]);
        assert_eq!((empty.verdict, empty.scope.as_str()), (Verdict::Clear, "/srv/proxy.conf"));
        assert_eq!(proxy_item(&Probe::Failed("remote proxy".into()), &[]).verdict, Verdict::Unknown);
        assert_eq!(proxy_item(&Probe::Skipped("no proxy".into()), &[]).verdict, Verdict::Skipped);
    }

    #[test]
    fn redact_hides_secret_looking_values_only() {
        assert_eq!(redact("HF_TOKEN=hf_x RUNPOD_API_KEY='rpa' arena pods up"), "HF_TOKEN=… RUNPOD_API_KEY=… arena pods up");
        assert_eq!(redact("curl -H 'Bearer sk-or-v1-abcdef' x"), "curl -H 'Bearer … x");
        assert_eq!(redact("ARENA_START_DATE=2026-10-01 arena  pods backup"), "ARENA_START_DATE=2026-10-01 arena  pods backup");
    }

    /// The JSON scripts read: verdict per item, typed entries, the counts.
    #[test]
    fn report_serialises_for_scripts() {
        let names = list();
        let naming = Naming { prefix: "arena8", list: &names };
        let mut i = inputs();
        i.listings[0].1 = Ok(vec![pod("arena8-bloom", "EXITED", None)]);
        let r = build(&i, &naming);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["clear"], false);
        assert_eq!(v["remaining"], 1);
        assert_eq!(v["items"][0]["area"], "pods");
        assert_eq!(v["items"][0]["verdict"], "remaining");
        assert_eq!(v["items"][0]["entries"][0]["kind"], "pod");
        assert_eq!(v["items"][0]["entries"][0]["billing"], false);
        assert_eq!(v["items"][1]["verdict"], "skipped");
        let text = r.render();
        assert!(text.contains("✗ pods (runpod): 1 remains") && text.contains("Suggested order"), "{text}");
        assert_eq!(r.verdict_line(), "NOT CLEAR: 1 item(s) still remain, 0 couldn't be checked.");
    }
}
