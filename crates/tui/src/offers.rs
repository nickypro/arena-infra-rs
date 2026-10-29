//! The add-pod form's view of `arena offers`: each provider's catalog for its type field
//! (RunPod's GPU types — B300s, the MIG slices, everything it sells — Hetzner's server types) and,
//! for the choice the form is on, what `up` would try there, in order (the availability line).
//! Both come from the CLI as a child (`arena offers … --json`, `runs::capture`), never computed
//! here: the same planner `create`/`up` run, near the proxy included.
//!
//! Nothing here waits: a fetch runs in the background and lands through `Shared` (the loop hands
//! it to [`State::landed`]). A provider's catalogue is read the first time the form shows it
//! (`…` until it lands; the presets if it fails), again after [`CATALOG_FRESH`] — a dashboard
//! runs for weeks, prices move — and after a failure once [`CATALOG_RETRY`] has passed; the
//! last good one is shown meanwhile. A choice is checked once the form has rested on it for
//! [`DEBOUNCE`] — arrowing through twenty GPU types makes one request, not twenty: RunPod
//! rate-limits its API — and its answer is kept for [`FRESH`].
//!
//! [`args`] is the one place that knows the CLI's shape: when `offers` changes, change it there.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::newpod::{Choice, NewPodForm};

/// How long the form must rest on a choice before it's checked.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(400);

/// How long a choice's availability is kept before it's checked again.
pub(crate) const FRESH: Duration = Duration::from_secs(180);

/// How long one `arena offers` may take.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(45);

/// How long a catalogue is shown before it's read again (the form shows the old one meanwhile).
pub(crate) const CATALOG_FRESH: Duration = Duration::from_secs(30 * 60);

/// How long after a failed catalogue read the form asks again (the presets meanwhile).
pub(crate) const CATALOG_RETRY: Duration = Duration::from_secs(120);

/// What the form asks `offers` for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Query {
    /// Everything `provider` sells (the type field's choices).
    Catalog(String),
    /// What `up` would try for this choice.
    Plan(Key),
}

/// A choice to check: the provider, the GPU type (or Hetzner's server type), the RunPod tier(s)
/// and the GPUs per pod — `up`'s flags, as the form would pass them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub provider: String,
    pub kind: String,
    pub cloud: Option<String>,
    pub gpus: Option<u32>,
}

/// The choice `form` is on, as a [`Key`] (`None`: its type field has nothing to choose).
pub(crate) fn key_of(form: &NewPodForm) -> Option<Key> {
    let kind = form.kind()?.id.clone();
    let hetzner = form.is_hetzner();
    Some(Key {
        provider: form.provider_name().to_string(),
        kind,
        cloud: form.cloud().map(String::from),
        gpus: (!hetzner).then_some(form.gpu_count),
    })
}

/// The provider and `arena` arguments (after the global `--config`/`--provider`) for `q` — the
/// one place that knows `offers`' command line: `offers <provider> --json` for everything a
/// provider sells, `offers --json` with `up`'s flags for what it would try.
pub(crate) fn args(q: &Query) -> (String, Vec<String>) {
    match q {
        Query::Catalog(p) => (p.clone(), vec!["offers".into(), p.clone(), "--json".into()]),
        Query::Plan(k) => {
            let mut a = vec!["offers".to_string(), "--json".into()];
            if k.provider == "hetzner" {
                a.extend(["--server-type".into(), k.kind.clone()]);
            } else {
                a.extend(["--gpu".into(), k.kind.clone()]);
                if let Some(c) = &k.cloud {
                    a.extend(["--cloud".into(), c.clone()]);
                }
                if let Some(n) = k.gpus {
                    a.extend(["--gpus".into(), n.to_string()]);
                }
            }
            (k.provider.clone(), a)
        }
    }
}

/// Which providers have a catalogue for the type field to read — RunPod's GPU types, Vast's
/// market now, Hetzner's server types. The rest (Shadeform) choose from the presets.
pub(crate) fn has_catalog(provider: &str) -> bool {
    matches!(provider, "runpod" | "vast" | "hetzner")
}

// What `offers <provider> --json` prints, as far as the form reads it.
#[derive(Deserialize)]
struct CatalogsJson {
    #[serde(default)]
    catalogs: Vec<CatalogJson>,
    #[serde(default)]
    skipped: Vec<SkippedJson>,
    #[serde(default)]
    failed: Vec<SkippedJson>,
}

#[derive(Deserialize)]
struct CatalogJson {
    provider: String,
    #[serde(default)]
    rows: Vec<RowJson>,
    #[serde(default)]
    server_types: Option<Vec<ServerTypeJson>>,
    #[serde(default)]
    notes: Vec<String>,
}

/// A GPU type on one tier (RunPod's `COMMUNITY`/`SECURE`, Vast's host classes).
#[derive(Deserialize)]
struct RowJson {
    gpu: String,
    #[serde(default)]
    name: String,
    vram_gb: Option<u32>,
    cloud: Option<String>,
    price_per_gpu: Option<f64>,
    price_source: Option<String>,
    /// `unlisted`, `not sold now`, `v1: not creatable`, `--cloud unverified`.
    note: Option<String>,
}

#[derive(Deserialize)]
struct ServerTypeJson {
    name: String,
    cores: Option<u32>,
    memory_gb: Option<f64>,
    eur_per_hour: Option<f64>,
    #[serde(default)]
    architecture: Option<String>,
}

/// `{name} · {vram}GB`, or the name alone when the VRAM isn't known.
fn gpu_label(name: &str, id: &str, vram: Option<u32>) -> String {
    let name = if name.is_empty() { id } else { name };
    vram.map_or_else(|| name.to_string(), |v| format!("{name} · {v}GB"))
}

/// `provider`'s choices from `offers <provider> --json`. RunPod: one per GPU type, both tiers'
/// prices — a type its create rejects (v1) or that's sold on no tier now left out; a type its
/// public catalog lacks (`unlisted`: B300 MIG slices seen on the account) kept, priced as a pod
/// of it bills when one does. Vast: one per card a create would rent as configured, at the
/// cheapest offer now. Hetzner: each server type with its size and €/h, in the catalogue's
/// order (by family, then size). A failed or empty catalogue is an error — the form keeps the
/// presets. Pure.
pub(crate) fn parse_catalog(provider: &str, json: &str) -> Result<Vec<Choice>, String> {
    let c: CatalogsJson = serde_json::from_str(json).map_err(|e| format!("unreadable offers output: {e}"))?;
    let theirs = |v: &[SkippedJson]| v.iter().find(|s| s.provider == provider).map(|s| s.why.clone());
    let Some(cat) = c.catalogs.iter().find(|k| k.provider == provider) else {
        return Err(theirs(&c.failed).or_else(|| theirs(&c.skipped)).unwrap_or_else(|| format!("no {provider} catalogue in offers' answer")));
    };
    let why = || cat.notes.first().cloned().unwrap_or_else(|| format!("{provider}'s catalogue is empty"));
    let mut out: Vec<Choice> = Vec::new();
    match provider {
        "runpod" => {
            for r in &cat.rows {
                if matches!(r.note.as_deref(), Some("v1: not creatable" | "not sold now")) {
                    continue;
                }
                let at = match out.iter().position(|c| c.id == r.gpu) {
                    Some(i) => i,
                    None => {
                        let unlisted = r.note.as_deref() == Some("unlisted");
                        out.push(Choice {
                            id: r.gpu.clone(),
                            label: gpu_label(&r.name, &r.gpu, r.vram_gb),
                            size: r.vram_gb.unwrap_or(0),
                            community: None,
                            secure: None,
                            estimate: r.price_source.as_deref() == Some("estimate"),
                            tiers_known: !unlisted,
                            server: None,
                            market: None,
                            unlisted: unlisted.then_some(r.price_per_gpu),
                        });
                        out.len() - 1
                    }
                };
                match r.cloud.as_deref() {
                    Some("COMMUNITY") => out[at].community = r.price_per_gpu,
                    Some("SECURE") => out[at].secure = r.price_per_gpu,
                    _ => {}
                }
            }
        }
        "vast" => {
            // A card on a host class the configured create wouldn't rent (`--cloud unverified`)
            // isn't one the form can make: it has no tier field for Vast.
            for r in cat.rows.iter().filter(|r| r.note.is_none()) {
                match out.iter_mut().find(|c| c.id == r.gpu) {
                    Some(c) => c.market = [c.market, r.price_per_gpu].into_iter().flatten().reduce(f64::min),
                    None => out.push(Choice {
                        id: r.gpu.clone(),
                        label: gpu_label(&r.name, &r.gpu, r.vram_gb),
                        size: r.vram_gb.unwrap_or(0),
                        community: None,
                        secure: None,
                        estimate: false,
                        tiers_known: false,
                        server: None,
                        market: r.price_per_gpu,
                        unlisted: None,
                    }),
                }
            }
        }
        "hetzner" => {
            for t in cat.server_types.as_deref().unwrap_or_default() {
                let mut size = Vec::new();
                size.extend(t.cores.map(|c| format!("{c} vCPU")));
                size.extend(t.memory_gb.map(|m| format!("{} GB", fmt_gb(m))));
                if t.architecture.as_deref() == Some("arm") {
                    size.push("arm".into());
                }
                out.push(Choice { size: t.cores.unwrap_or(0), server: Some((size.join(" · "), t.eur_per_hour)), ..Choice::server_type(&t.name) });
            }
        }
        other => return Err(format!("{other} has no catalogue")),
    }
    if out.is_empty() {
        return Err(why());
    }
    Ok(out)
}

/// `4` for 4.0, `0.5` for a half.
fn fmt_gb(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x:.0}")
    } else {
        format!("{x}")
    }
}

// What `offers --json` prints for a plan, as far as the availability line reads it.
#[derive(Deserialize)]
struct PlanJson {
    #[serde(default)]
    providers: Vec<ProviderPlanJson>,
    #[serde(default)]
    failed: Vec<SkippedJson>,
    #[serde(default)]
    skipped: Vec<SkippedJson>,
}

#[derive(Deserialize)]
struct SkippedJson {
    provider: String,
    why: String,
}

#[derive(Deserialize)]
struct ProviderPlanJson {
    provider: String,
    #[serde(default)]
    options: Vec<OptionJson>,
    #[serde(default)]
    dropped: Vec<serde_json::Value>,
    #[serde(default)]
    notes: Vec<String>,
    /// Near the proxy's plan (`{"region": …}`) when it's on for this provider.
    #[serde(default)]
    near: Option<serde_json::Value>,
    #[serde(default)]
    server_type: Option<ServerTypePlanJson>,
}

#[derive(Deserialize)]
struct OptionJson {
    label: String,
    cloud: Option<String>,
    price_per_pod: Option<f64>,
    price_source: Option<String>,
    stock: Option<String>,
    /// The region, on a near-the-proxy option.
    #[serde(default)]
    near: Option<String>,
}

#[derive(Deserialize)]
struct ServerTypePlanJson {
    name: String,
    cores: Option<u32>,
    memory_gb: Option<f64>,
    eur_per_hour: Option<f64>,
    #[serde(default)]
    locations: Vec<String>,
}

/// What `up` would try, from `offers --json`, one line per option in its order: `1. 1×RTX A4000
/// COMMUNITY  $0.17/h  stock High  near europe` — on Hetzner the server type, its size, €/h and
/// the locations a create walks — then what was dropped and the first note. Pure.
pub(crate) fn parse_plan(json: &str) -> Result<Vec<String>, String> {
    let p: PlanJson = serde_json::from_str(json).map_err(|e| format!("unreadable offers output: {e}"))?;
    if let Some(f) = p.failed.first() {
        return Err(format!("{}: {}", f.provider, f.why));
    }
    let Some(prov) = p.providers.first() else {
        return Err(p.skipped.first().map_or_else(|| "offers had nothing to say".into(), |s| format!("{}: {}", s.provider, s.why)));
    };
    let mut lines = Vec::new();
    if let Some(t) = &prov.server_type {
        let mut parts = vec![t.name.clone()];
        parts.extend(t.cores.map(|c| format!("{c} vCPU")));
        parts.extend(t.memory_gb.map(|m| format!("{} GB", fmt_gb(m))));
        parts.push(t.eur_per_hour.map_or_else(|| "€ unknown".into(), |e| format!("€{e:.4}/h")));
        if !t.locations.is_empty() {
            parts.push(t.locations.join(" → "));
        }
        lines.push(format!("1. {}", parts.join(" · ")));
    } else {
        for (i, o) in prov.options.iter().enumerate() {
            let what = match &o.cloud {
                Some(c) => format!("{} {c}", o.label),
                None => o.label.clone(),
            };
            let tilde = if o.price_source.as_deref() == Some("estimate") { "~" } else { "" };
            let price = o.price_per_pod.map_or_else(|| "price unknown".into(), |p| format!("{tilde}${p:.2}/h"));
            let mut line = format!("{}. {what}  {price}", i + 1);
            if let Some(s) = &o.stock {
                line.push_str(&format!("  stock {s}"));
            }
            match (&o.near, &prov.near) {
                (Some(region), _) => line.push_str(&format!("  near {region}")),
                (None, Some(_)) => line.push_str("  anywhere"),
                _ => {}
            }
            lines.push(line);
        }
        if lines.is_empty() {
            lines.push(format!("nothing {} would try{}", prov.provider, prov.notes.first().map(|n| format!(" — {n}")).unwrap_or_default()));
            return Ok(lines);
        }
    }
    if !prov.dropped.is_empty() {
        lines.push(format!("({} more dropped by its rules — `arena offers` says which)", prov.dropped.len()));
    }
    if let Some(n) = prov.notes.first() {
        lines.push(format!("note: {n}"));
    }
    Ok(lines)
}

/// A fetch: out, or what it came to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Fetch<T> {
    Out,
    Ready(T),
    Failed(String),
}

/// What the form shows for its choice's availability.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Shown<'a> {
    /// Turned off (`o`).
    Off,
    /// Nothing to check (no choice), or the form hasn't rested on it long enough yet.
    Waiting,
    /// Asked; not back yet.
    Checking,
    Ready(&'a [String]),
    Failed(&'a str),
}

/// One provider's catalogue as the form knows it: the last answer (and when it came) and
/// whether a read is out — a refresh keeps showing the last good list.
#[derive(Debug, Default)]
struct Catalogue {
    got: Option<(Instant, Result<Vec<Choice>, String>)>,
    /// When a refresh of a good list last failed (asked again [`CATALOG_RETRY`] after).
    refresh_failed: Option<Instant>,
    out: bool,
    /// An answer landed that the form hasn't taken in yet.
    new: bool,
}

/// The form's offers, kept for the dashboard's life ([`crate::Ui`] holds it, the UI thread only).
#[derive(Debug)]
pub(crate) struct State {
    /// The availability line is shown (and so fetched): `o` toggles it.
    pub show: bool,
    catalogs: HashMap<String, Catalogue>,
    plans: HashMap<Key, (Instant, Fetch<Vec<String>>)>,
    /// The choice the form is on, and since when.
    wanted: Option<(Key, Instant)>,
}

impl State {
    pub(crate) fn new(show: bool) -> Self {
        State { show, catalogs: HashMap::new(), plans: HashMap::new(), wanted: None }
    }

    /// `provider`'s catalogue to ask for now, if it has one — marked out: never read, or read
    /// [`CATALOG_FRESH`] ago, or failed [`CATALOG_RETRY`] ago; never while a read is out.
    pub(crate) fn catalog_due(&mut self, provider: &str, now: Instant) -> Option<Query> {
        if !has_catalog(provider) {
            return None;
        }
        let c = self.catalogs.entry(provider.to_string()).or_default();
        let due = match &c.got {
            _ if c.out => false,
            None => true,
            Some((at, Ok(_))) => {
                now.duration_since(*at) >= CATALOG_FRESH && c.refresh_failed.is_none_or(|f| now.duration_since(f) >= CATALOG_RETRY)
            }
            Some((at, Err(_))) => now.duration_since(*at) >= CATALOG_RETRY,
        };
        if !due {
            return None;
        }
        c.out = true;
        Some(Query::Catalog(provider.to_string()))
    }

    /// What the form shows for `provider`'s type field: the last good catalogue (even while a
    /// newer one is read), else the read out, else why it failed. `None`: never asked.
    pub(crate) fn catalog(&self, provider: &str) -> Option<Fetch<&[Choice]>> {
        let c = self.catalogs.get(provider)?;
        Some(match &c.got {
            Some((_, Ok(kinds))) => Fetch::Ready(kinds.as_slice()),
            _ if c.out => Fetch::Out,
            Some((_, Err(why))) => Fetch::Failed(why.clone()),
            None => return None,
        })
    }

    /// Whether `provider`'s catalogue changed since the form last took it in — and it's taken now.
    pub(crate) fn take_new(&mut self, provider: &str) -> bool {
        self.catalogs.get_mut(provider).is_some_and(|c| std::mem::take(&mut c.new))
    }

    /// The form is on `key` at `now` (`None`: nothing to check). A new choice restarts the
    /// debounce; the same one keeps its time.
    pub(crate) fn want(&mut self, key: Option<Key>, now: Instant) {
        match (key, &self.wanted) {
            (Some(k), Some((w, _))) if *w == k => {}
            (Some(k), _) => self.wanted = Some((k, now)),
            (None, _) => self.wanted = None,
        }
    }

    /// The plan to ask for now: the choice the form has rested on for [`DEBOUNCE`], unless one
    /// is out for it or a fresh answer is kept — marked out.
    pub(crate) fn plan_due(&mut self, now: Instant) -> Option<Query> {
        if !self.show {
            return None;
        }
        let (key, since) = self.wanted.as_ref()?;
        if now.duration_since(*since) < DEBOUNCE {
            return None;
        }
        match self.plans.get(key) {
            Some((_, Fetch::Out)) => return None,
            Some((at, _)) if now.duration_since(*at) < FRESH => return None,
            _ => {}
        }
        self.plans.insert(key.clone(), (now, Fetch::Out));
        Some(Query::Plan(key.clone()))
    }

    /// What to show for the choice the form is on.
    pub(crate) fn shown(&self, key: Option<&Key>) -> Shown<'_> {
        if !self.show {
            return Shown::Off;
        }
        match key.and_then(|k| self.plans.get(k)) {
            None => Shown::Waiting,
            Some((_, Fetch::Out)) => Shown::Checking,
            Some((_, Fetch::Ready(lines))) => Shown::Ready(lines),
            Some((_, Fetch::Failed(why))) => Shown::Failed(why),
        }
    }

    /// A fetch came back: `out` is `offers`' JSON, or why there's none.
    pub(crate) fn landed(&mut self, q: Query, out: Result<String, String>, now: Instant) {
        match q {
            Query::Catalog(p) => {
                let got = out.and_then(|j| parse_catalog(&p, &j));
                let c = self.catalogs.entry(p).or_default();
                c.out = false;
                c.new = true;
                // A failed refresh keeps the last good list, and is retried after CATALOG_RETRY.
                match (&c.got, got) {
                    (Some((_, Ok(_))), Err(_)) => c.refresh_failed = Some(now),
                    (_, got) => {
                        c.got = Some((now, got));
                        c.refresh_failed = None;
                    }
                }
            }
            Query::Plan(k) => {
                let got = out.and_then(|j| parse_plan(&j));
                self.plans.insert(k, (now, got.map_or_else(Fetch::Failed, Fetch::Ready)));
            }
        }
    }

    /// Whether anything is out or waiting out its debounce: the loop then looks again soon.
    pub(crate) fn busy(&self, now: Instant) -> bool {
        let out = self.catalogs.values().any(|c| c.out) || self.plans.values().any(|(_, f)| *f == Fetch::Out);
        let resting = self.show && self.wanted.as_ref().is_some_and(|(k, since)| now.duration_since(*since) < DEBOUNCE + Duration::from_millis(50) && !self.plans.contains_key(k));
        out || resting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(kind: &str) -> Key {
        Key { provider: "runpod".into(), kind: kind.into(), cloud: Some("COMMUNITY".into()), gpus: Some(1) }
    }

    /// `args` is the CLI's shape in one place: a catalogue is `offers <provider> --json`; a plan is the
    /// form's choice as `up`'s flags (Hetzner: its server type only).
    #[test]
    fn offers_args_are_ups_flags() {
        assert_eq!(args(&Query::Catalog("runpod".into())), ("runpod".into(), vec!["offers".into(), "runpod".into(), "--json".into()]));
        let (p, a) = args(&Query::Plan(Key { provider: "runpod".into(), kind: "NVIDIA RTX A4000".into(), cloud: Some("COMMUNITY,SECURE".into()), gpus: Some(2) }));
        assert_eq!(p, "runpod");
        assert_eq!(a, ["offers", "--json", "--gpu", "NVIDIA RTX A4000", "--cloud", "COMMUNITY,SECURE", "--gpus", "2"]);
        let (p, a) = args(&Query::Plan(Key { provider: "hetzner".into(), kind: "cx33".into(), cloud: None, gpus: None }));
        assert_eq!((p.as_str(), a), ("hetzner", vec!["offers".to_string(), "--json".into(), "--server-type".into(), "cx33".into()]));
        let (_, a) = args(&Query::Plan(Key { provider: "vast".into(), kind: "NVIDIA GeForce RTX 3090".into(), cloud: None, gpus: Some(1) }));
        assert_eq!(a, ["offers", "--json", "--gpu", "NVIDIA GeForce RTX 3090", "--gpus", "1"]);
    }

    /// A choice is checked once the form has rested on it for the debounce — arrowing past one
    /// asks nothing — then never again while it's out or fresh; a new choice restarts the wait.
    /// Off (`o`): nothing is asked, `Off` is shown.
    #[test]
    fn availability_waits_out_the_debounce_and_caches_per_choice() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut s = State::new(true);
        s.want(Some(key("a")), t0);
        assert_eq!(s.plan_due(t0 + ms(100)), None, "still resting");
        s.want(Some(key("b")), t0 + ms(200));
        assert_eq!(s.plan_due(t0 + ms(500)), None, "b restarted the wait");
        assert!(s.busy(t0 + ms(500)));
        assert_eq!(s.shown(Some(&key("b"))), Shown::Waiting);
        assert_eq!(s.plan_due(t0 + ms(600)), Some(Query::Plan(key("b"))), "rested 400 ms");
        assert_eq!(s.shown(Some(&key("b"))), Shown::Checking);
        assert_eq!(s.plan_due(t0 + ms(700)), None, "out already");
        let json = r#"{"providers":[{"provider":"runpod","options":[{"label":"1×RTX A4000","cloud":"COMMUNITY","price_per_pod":0.17,"price_source":"live","stock":"High"}]}],"skipped":[],"failed":[]}"#;
        s.landed(Query::Plan(key("b")), Ok(json.into()), t0 + ms(900));
        assert_eq!(s.shown(Some(&key("b"))), Shown::Ready(&["1. 1×RTX A4000 COMMUNITY  $0.17/h  stock High".to_string()]));
        assert!(!s.busy(t0 + ms(950)));
        // Back to b later: kept while fresh, asked again after.
        s.want(Some(key("a")), t0 + ms(1_000));
        s.want(Some(key("b")), t0 + ms(2_000));
        assert_eq!(s.plan_due(t0 + ms(3_000)), None, "fresh");
        assert_eq!(s.plan_due(t0 + ms(900) + FRESH), Some(Query::Plan(key("b"))), "stale: again");
        // A failure is shown, and kept as an answer (not retried every frame).
        s.landed(Query::Plan(key("b")), Err("runpod: 429".into()), t0 + FRESH + ms(1_000));
        assert_eq!(s.shown(Some(&key("b"))), Shown::Failed("runpod: 429"));
        assert_eq!(s.plan_due(t0 + FRESH + ms(2_000)), None);
        // Off: nothing asked.
        let mut off = State::new(false);
        off.want(Some(key("c")), t0);
        assert_eq!(off.plan_due(t0 + ms(5_000)), None);
        assert_eq!(off.shown(Some(&key("c"))), Shown::Off);
    }

    /// A provider's catalogue is asked for the first time the form shows it, never twice at
    /// once, again once it's [`CATALOG_FRESH`] old — the old list shown meanwhile — and after a
    /// failure once [`CATALOG_RETRY`] has passed (a failed refresh keeps the good list).
    /// Shadeform has none: the presets.
    #[test]
    fn a_catalogue_is_read_when_shown_refreshed_when_old_retried_after_a_failure() {
        let t0 = Instant::now();
        let mut s = State::new(true);
        let q = || Some(Query::Catalog("runpod".into()));
        let one = r#"{"catalogs":[{"provider":"runpod","rows":[{"gpu":"A","name":"A","vram_gb":16,"cloud":"COMMUNITY","price_per_gpu":0.2,"price_source":"live"}],"notes":[]}]}"#;
        let ids = |s: &State| match s.catalog("runpod") {
            Some(Fetch::Ready(k)) => k.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        assert_eq!(s.catalog("runpod"), None);
        assert_eq!(s.catalog_due("runpod", t0), q());
        assert_eq!(s.catalog_due("runpod", t0), None, "out already");
        assert_eq!(s.catalog("runpod"), Some(Fetch::Out));
        assert!(s.busy(t0));
        assert!(!s.take_new("runpod"));
        s.landed(Query::Catalog("runpod".into()), Ok(one.into()), t0);
        assert!(s.take_new("runpod") && !s.take_new("runpod"), "the form takes it in once");
        assert_eq!(ids(&s), ["A"]);
        assert_eq!(s.catalog_due("runpod", t0 + CATALOG_FRESH - Duration::from_secs(1)), None, "fresh");
        assert_eq!(s.catalog_due("runpod", t0 + CATALOG_FRESH), q(), "old: read again");
        assert_eq!(ids(&s), ["A"], "the old list while the new one is read");
        s.landed(Query::Catalog("runpod".into()), Err("429".into()), t0 + CATALOG_FRESH);
        assert_eq!(ids(&s), ["A"], "a failed refresh keeps the good list");
        assert_eq!(s.catalog_due("runpod", t0 + CATALOG_FRESH + Duration::from_secs(1)), None);
        assert_eq!(s.catalog_due("runpod", t0 + CATALOG_FRESH + CATALOG_RETRY), q(), "retried");
        // Never read well: the failure shows (the presets stand in), retried after CATALOG_RETRY.
        assert_eq!(s.catalog_due("hetzner", t0), Some(Query::Catalog("hetzner".into())));
        s.landed(Query::Catalog("hetzner".into()), Err("timed out".into()), t0);
        assert_eq!(s.catalog("hetzner"), Some(Fetch::Failed("timed out".into())));
        assert_eq!(s.catalog_due("hetzner", t0 + Duration::from_secs(10)), None);
        assert_eq!(s.catalog_due("hetzner", t0 + CATALOG_RETRY), Some(Query::Catalog("hetzner".into())));
        assert_eq!(s.catalog("hetzner"), Some(Fetch::Out), "retrying: nothing good to show");
        assert_eq!(s.catalog_due("shadeform", t0), None, "no list: the presets");
    }

    /// `offers <provider> --json`'s catalogue as the type field's choices. RunPod: a type once
    /// with both tiers' prices (`None`: not sold there), B300 and MIG slices included; a type
    /// its catalog lacks kept as `unlisted` with a pod's price; one not sold now, or that v1's
    /// create rejects, left out. Vast: the cards a create would rent as configured, at the
    /// cheapest class's price. Hetzner: each server type's size and €/h, in order. A failed or
    /// empty catalogue is an error: the presets stand in.
    #[test]
    fn catalogues_parse_into_the_type_fields_choices() {
        let rp = r#"{"catalogs":[{"provider":"runpod","gpu_count":1,"rows":[
            {"gpu":"NVIDIA RTX A4000","name":"RTX A4000","vram_gb":16,"cloud":"COMMUNITY","price_per_gpu":0.17,"price_source":"live","stock":"High","try_order":1},
            {"gpu":"NVIDIA RTX A4000","name":"RTX A4000","vram_gb":16,"cloud":"SECURE","price_per_gpu":0.25,"price_source":"live","stock":"Low"},
            {"gpu":"NVIDIA B300 SXM6 AC","name":"B300","vram_gb":288,"cloud":"SECURE","price_per_gpu":8.99,"price_source":"live"},
            {"gpu":"NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb","name":"PRO 6000 MIG 1g.24gb","vram_gb":24,"cloud":"SECURE","price_per_gpu":0.69,"price_source":"live"},
            {"gpu":"NVIDIA Old","name":"Old","vram_gb":8,"cloud":null,"price_per_gpu":null,"price_source":null,"note":"not sold now"},
            {"gpu":"NVIDIA V1Only","name":"V1Only","vram_gb":8,"cloud":"COMMUNITY","price_per_gpu":0.1,"price_source":"live","note":"v1: not creatable"},
            {"gpu":"NVIDIA B300 SXM6 AC MIG 1g.34gb","name":"B300 MIG 1g.34gb","vram_gb":34,"cloud":null,"price_per_gpu":1.1,"price_source":"pod","note":"unlisted"}
        ],"notes":["marked: what up would try"]}],"skipped":[],"failed":[]}"#;
        let c = parse_catalog("runpod", rp).unwrap();
        assert_eq!(
            c.iter().map(|c| (c.label.as_str(), c.community, c.secure)).collect::<Vec<_>>(),
            [
                ("RTX A4000 · 16GB", Some(0.17), Some(0.25)),
                ("B300 · 288GB", None, Some(8.99)),
                ("PRO 6000 MIG 1g.24gb · 24GB", None, Some(0.69)),
                ("B300 MIG 1g.34gb · 34GB", None, None),
            ]
        );
        assert_eq!((c[1].sold_on("COMMUNITY"), c[1].sold_on("SECURE")), (Some(false), Some(true)), "B300: SECURE only");
        assert_eq!((c[3].unlisted, c[3].sold_on("COMMUNITY")), (Some(Some(1.1)), None), "unlisted: where is RunPod's to say");
        assert_eq!(c[3].price("runpod", Some("COMMUNITY")), "unlisted · $1.10/h a pod bills");
        assert_eq!(c[3].id, "NVIDIA B300 SXM6 AC MIG 1g.34gb", "--gpu takes the full id");
        let vast = r#"{"catalogs":[{"provider":"vast","gpu_count":1,"rows":[
            {"gpu":"RTX 3090","name":"RTX 3090","vram_gb":24,"cloud":"DATACENTER","price_per_gpu":0.31,"price_source":"live","location":"Sweden, SE"},
            {"gpu":"RTX 3090","name":"RTX 3090","vram_gb":24,"cloud":"VERIFIED","price_per_gpu":0.22,"price_source":"live"},
            {"gpu":"RTX 3090","name":"RTX 3090","vram_gb":24,"cloud":"UNVERIFIED","price_per_gpu":0.12,"price_source":"live","note":"--cloud unverified"},
            {"gpu":"RTX 2080","name":"RTX 2080","vram_gb":8,"cloud":"UNVERIFIED","price_per_gpu":0.05,"price_source":"live","note":"--cloud unverified"}
        ],"notes":[]}],"skipped":[],"failed":[]}"#;
        let c = parse_catalog("vast", vast).unwrap();
        assert_eq!(c.iter().map(|c| (c.id.as_str(), c.market)).collect::<Vec<_>>(), [("RTX 3090", Some(0.22))], "unverified hosts: not rented as configured");
        assert_eq!(c[0].price("vast", None), "from $0.22/h now");
        let hz = r#"{"catalogs":[{"provider":"hetzner","gpu_count":1,"rows":[],"server_types":[
            {"name":"cx23","cores":2,"cpu_type":"shared","architecture":"x86","memory_gb":4.0,"disk_gb":40,"eur_per_hour":0.0104,"locations":["nbg1"]},
            {"name":"cax11","cores":2,"architecture":"arm","memory_gb":4.0,"eur_per_hour":0.0059,"locations":[]}],"notes":[]}],"skipped":[],"failed":[]}"#;
        let c = parse_catalog("hetzner", hz).unwrap();
        assert_eq!(c.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["cx23", "cax11"]);
        assert_eq!(c[0].server, Some(("2 vCPU · 4 GB".to_string(), Some(0.0104))));
        assert_eq!(c[1].server, Some(("2 vCPU · 4 GB · arm".to_string(), Some(0.0059))));
        let failed = r#"{"catalogs":[],"skipped":[],"failed":[{"provider":"hetzner","why":"couldn't read the server types (401)"}]}"#;
        assert_eq!(parse_catalog("hetzner", failed), Err("couldn't read the server types (401)".into()));
        assert_eq!(parse_catalog("vast", r#"{"catalogs":[{"provider":"vast","rows":[],"notes":["vast: searching verified hosts failed (429)"]}]}"#), Err("vast: searching verified hosts failed (429)".into()));
        assert!(parse_catalog("runpod", "not json").unwrap_err().starts_with("unreadable"));
        assert_eq!(parse_catalog("runpod", r#"{"catalogs":[]}"#), Err("no runpod catalogue in offers' answer".into()));
    }

    /// A plan reads as `up` would try it: in order, priced (`~` an estimate), stock, near the
    /// proxy or anywhere when near is on; Hetzner's as its server type; a failed provider is the
    /// error; nothing to try says why.
    #[test]
    fn a_plan_reads_as_up_would_try_it() {
        let rp = r#"{"providers":[{"provider":"runpod","near":{"region":"europe","max_extra_pct":25.0},"options":[
            {"label":"1×RTX A4000","cloud":"COMMUNITY","price_per_pod":0.17,"price_source":"live","stock":"Low","near":"europe"},
            {"label":"1×RTX A4000","cloud":"COMMUNITY","price_per_pod":0.17,"price_source":"live","stock":"Low"},
            {"label":"1×RTX A4000","cloud":"SECURE","price_per_pod":0.32,"price_source":"estimate","stock":null}],
            "dropped":[{"x":1}],"notes":["near the proxy (NEAR=europe): Europe first"]}],"skipped":[],"failed":[]}"#;
        assert_eq!(
            parse_plan(rp).unwrap(),
            [
                "1. 1×RTX A4000 COMMUNITY  $0.17/h  stock Low  near europe",
                "2. 1×RTX A4000 COMMUNITY  $0.17/h  stock Low  anywhere",
                "3. 1×RTX A4000 SECURE  ~$0.32/h  anywhere",
                "(1 more dropped by its rules — `arena offers` says which)",
                "note: near the proxy (NEAR=europe): Europe first",
            ]
        );
        let hz = r#"{"providers":[{"provider":"hetzner","options":[{"label":"CPU VM","cloud":null,"price_per_pod":null,"price_source":null,"stock":null}],"dropped":[],"notes":[],
            "server_type":{"name":"cx33","cores":4,"memory_gb":8.0,"eur_per_hour":0.016,"locations":["fsn1","nbg1","hel1"]}}],"skipped":[],"failed":[]}"#;
        assert_eq!(parse_plan(hz).unwrap(), ["1. cx33 · 4 vCPU · 8 GB · €0.0160/h · fsn1 → nbg1 → hel1"]);
        assert_eq!(parse_plan(r#"{"providers":[],"skipped":[],"failed":[{"provider":"runpod","why":"429 Too Many Requests"}]}"#), Err("runpod: 429 Too Many Requests".into()));
        // Vast's options carry their host class in `cloud` since offers-cli (2.1; it was null):
        // as `arena offers --provider vast --json --gpu "RTX 3090"` answered live, 2026-10-29.
        let vast = r#"{"providers":[{"provider":"vast","gpu_count":1,"near":{"region":"europe","max_extra_pct":25.0},"options":[
            {"label":"1×RTX 3090","cloud":"VERIFIED","price_per_pod":0.18777777777777777,"price_source":"live","stock":null,"near":"europe"},
            {"label":"1×RTX 3090","cloud":"VERIFIED","price_per_pod":0.21888888888888886,"price_source":"live","stock":null}],
            "dropped":[],"notes":[]}],"skipped":[],"failed":[]}"#;
        assert_eq!(parse_plan(vast).unwrap(), ["1. 1×RTX 3090 VERIFIED  $0.19/h  near europe", "2. 1×RTX 3090 VERIFIED  $0.22/h  anywhere"]);
        let none = r#"{"providers":[{"provider":"runpod","options":[],"dropped":[],"notes":["no B300 on COMMUNITY"]}],"skipped":[],"failed":[]}"#;
        assert_eq!(parse_plan(none).unwrap(), ["nothing runpod would try — no B300 on COMMUNITY"]);
    }
}
