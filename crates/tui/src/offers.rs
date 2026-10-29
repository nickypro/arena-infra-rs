//! The add-pod form's view of `arena offers`: each provider's catalog for its type field
//! (RunPod's GPU types — B300s, the MIG slices, everything it sells — Hetzner's server types) and,
//! for the choice the form is on, what `up` would try there, in order (the availability line).
//! Both come from the CLI as a child (`arena offers … --json`, `runs::capture`), never computed
//! here: the same planner `create`/`up` run, near the proxy included.
//!
//! Nothing here waits: a fetch runs in the background and lands through `Shared` (the loop hands
//! it to [`State::landed`]). A catalog is read once per provider for the dashboard's life (`…`
//! until it lands; the presets if it fails). A choice is checked once the form has rested on it
//! for [`DEBOUNCE`] — arrowing through twenty GPU types makes one request, not twenty: RunPod
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
/// one place that knows `offers`' command line.
pub(crate) fn args(q: &Query) -> (String, Vec<String>) {
    match q {
        Query::Catalog(p) => (p.clone(), vec!["offers".into(), "--all".into(), "--json".into()]),
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

/// Which providers have a catalog for the type field to read: the rest (Vast, Shadeform:
/// marketplaces, priced per search) choose from the presets.
pub(crate) fn has_catalog(provider: &str) -> bool {
    matches!(provider, "runpod" | "hetzner")
}

// What `offers --all --json` prints, as far as the form reads it.
#[derive(Deserialize)]
struct CatalogsJson {
    #[serde(default)]
    runpod: Option<Vec<GpuRowJson>>,
    #[serde(default)]
    hetzner: Option<Vec<ServerTypeJson>>,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Deserialize)]
struct GpuRowJson {
    id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    memory_gb: u32,
    community_price: Option<f64>,
    secure_price: Option<f64>,
    #[serde(default)]
    creatable: Option<bool>,
    #[serde(default)]
    price_source: Option<String>,
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

/// `provider`'s catalog from `offers --all --json`: RunPod's GPU types (minus what its create
/// API rejects), Hetzner's server types. An empty one, or one the JSON doesn't carry (it
/// couldn't be read: a note says why), is an error — the form keeps the presets.
pub(crate) fn parse_catalog(provider: &str, json: &str) -> Result<Vec<Choice>, String> {
    let c: CatalogsJson = serde_json::from_str(json).map_err(|e| format!("unreadable offers output: {e}"))?;
    let why = || c.notes.first().cloned().unwrap_or_else(|| format!("no {provider} catalog in offers' answer"));
    let out: Vec<Choice> = match provider {
        "runpod" => c
            .runpod
            .as_ref()
            .ok_or_else(why)?
            .iter()
            .filter(|r| r.creatable != Some(false))
            .map(|r| Choice {
                id: r.id.clone(),
                label: format!("{} · {}GB", if r.display_name.is_empty() { &r.id } else { &r.display_name }, r.memory_gb),
                size: r.memory_gb,
                community: r.community_price,
                secure: r.secure_price,
                estimate: r.price_source.as_deref() != Some("live"),
                tiers_known: true,
                server: None,
            })
            .collect(),
        "hetzner" => c
            .hetzner
            .as_ref()
            .ok_or_else(why)?
            .iter()
            .map(|t| {
                let mut size = Vec::new();
                size.extend(t.cores.map(|c| format!("{c} vCPU")));
                size.extend(t.memory_gb.map(|m| format!("{} GB", fmt_gb(m))));
                if t.architecture.as_deref() == Some("arm") {
                    size.push("arm".into());
                }
                Choice {
                    id: t.name.clone(),
                    label: t.name.clone(),
                    size: t.cores.unwrap_or(0),
                    community: None,
                    secure: None,
                    estimate: false,
                    tiers_known: false,
                    server: Some((size.join(" · "), t.eur_per_hour)),
                }
            })
            .collect(),
        other => return Err(format!("{other} has no catalog")),
    };
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

/// The form's offers, kept for the dashboard's life ([`crate::Ui`] holds it, the UI thread only).
#[derive(Debug)]
pub(crate) struct State {
    /// The availability line is shown (and so fetched): `o` toggles it.
    pub show: bool,
    catalogs: HashMap<String, Fetch<Vec<Choice>>>,
    plans: HashMap<Key, (Instant, Fetch<Vec<String>>)>,
    /// The choice the form is on, and since when.
    wanted: Option<(Key, Instant)>,
}

impl State {
    pub(crate) fn new(show: bool) -> Self {
        State { show, catalogs: HashMap::new(), plans: HashMap::new(), wanted: None }
    }

    /// `provider`'s catalog to ask for now — once, if it has one — marked out.
    pub(crate) fn catalog_due(&mut self, provider: &str) -> Option<Query> {
        if !has_catalog(provider) || self.catalogs.contains_key(provider) {
            return None;
        }
        self.catalogs.insert(provider.to_string(), Fetch::Out);
        Some(Query::Catalog(provider.to_string()))
    }

    pub(crate) fn catalog(&self, provider: &str) -> Option<&Fetch<Vec<Choice>>> {
        self.catalogs.get(provider)
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
                self.catalogs.insert(p, got.map_or_else(Fetch::Failed, Fetch::Ready));
            }
            Query::Plan(k) => {
                let got = out.and_then(|j| parse_plan(&j));
                self.plans.insert(k, (now, got.map_or_else(Fetch::Failed, Fetch::Ready)));
            }
        }
    }

    /// Whether anything is out or waiting out its debounce: the loop then looks again soon.
    pub(crate) fn busy(&self, now: Instant) -> bool {
        let out = self.catalogs.values().any(|f| *f == Fetch::Out) || self.plans.values().any(|(_, f)| *f == Fetch::Out);
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

    /// `args` is the CLI's shape in one place: a catalog is `offers --all --json`; a plan is the
    /// form's choice as `up`'s flags (Hetzner: its server type only).
    #[test]
    fn offers_args_are_ups_flags() {
        assert_eq!(args(&Query::Catalog("runpod".into())), ("runpod".into(), vec!["offers".into(), "--all".into(), "--json".into()]));
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

    /// A provider's catalog is asked for once, for the providers that have one.
    #[test]
    fn a_catalog_is_asked_for_once_per_provider() {
        let mut s = State::new(true);
        assert_eq!(s.catalog_due("runpod"), Some(Query::Catalog("runpod".into())));
        assert_eq!(s.catalog_due("runpod"), None);
        assert_eq!(s.catalog("runpod"), Some(&Fetch::Out));
        assert_eq!(s.catalog_due("vast"), None, "a marketplace: the presets");
        s.landed(Query::Catalog("runpod".into()), Err("timed out".into()), Instant::now());
        assert_eq!(s.catalog("runpod"), Some(&Fetch::Failed("timed out".into())));
        assert_eq!(s.catalog_due("runpod"), None, "not again: the presets stand in");
    }

    /// RunPod's catalog: every type its create takes, B300 and the MIG slices included, with
    /// both tiers' prices (`None`: not sold there); Hetzner's: each server type's size and €/h.
    #[test]
    fn catalogs_parse_into_the_type_fields_choices() {
        let rp = r#"{"runpod":[
            {"id":"NVIDIA B300 SXM6 AC","display_name":"B300","memory_gb":288,"community_price":6.94,"secure_price":8.99,"creatable":true,"price_source":"live"},
            {"id":"NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb","display_name":"PRO 6000 MIG 24GB","memory_gb":24,"community_price":null,"secure_price":0.69,"creatable":true,"price_source":"live"},
            {"id":"NVIDIA Old","display_name":"Old","memory_gb":8,"community_price":0.1,"secure_price":null,"creatable":false,"price_source":"live"}
        ],"notes":[]}"#;
        let c = parse_catalog("runpod", rp).unwrap();
        assert_eq!(c.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["B300 · 288GB", "PRO 6000 MIG 24GB · 24GB"], "not creatable: left out");
        assert_eq!((c[1].community, c[1].secure, c[1].estimate), (None, Some(0.69), false));
        let hz = r#"{"hetzner":[{"name":"cx33","cores":4,"cpu_type":"shared","architecture":"x86","memory_gb":8.0,"disk_gb":80,"eur_per_hour":0.016,"locations":["fsn1"]},
                                {"name":"cax11","cores":2,"architecture":"arm","memory_gb":4.0,"eur_per_hour":0.0059,"locations":[]}],"notes":[]}"#;
        let c = parse_catalog("hetzner", hz).unwrap();
        assert_eq!(c[0].server, Some(("4 vCPU · 8 GB".to_string(), Some(0.016))));
        assert_eq!(c[1].server, Some(("2 vCPU · 4 GB · arm".to_string(), Some(0.0059))));
        assert_eq!(parse_catalog("hetzner", r#"{"notes":["hetzner: couldn't read the server types (401)"]}"#), Err("hetzner: couldn't read the server types (401)".into()));
        assert!(parse_catalog("runpod", "not json").unwrap_err().starts_with("unreadable"));
        assert!(parse_catalog("runpod", r#"{"runpod":[],"notes":[]}"#).is_err(), "empty: the presets stand in");
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
        let none = r#"{"providers":[{"provider":"runpod","options":[],"dropped":[],"notes":["no B300 on COMMUNITY"]}],"skipped":[],"failed":[]}"#;
        assert_eq!(parse_plan(none).unwrap(), ["nothing runpod would try — no B300 on COMMUNITY"]);
    }
}
