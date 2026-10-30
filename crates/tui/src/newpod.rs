//! The add-pod form (`n`) and its confirm: what to make — and on `y`, `arena pods up` with the
//! form's choices as its flags, run in the background (`runs.rs`), its progress in the runs panel.
//!
//! The form opens at once and **never waits on the network**. Switching provider used to await
//! a whole-fleet listing in the key loop — about a second per keypress on the sandbox, the
//! dashboard frozen meanwhile (the human: "i switch between runpod / vast / hetzner and its
//! laggy"). Now:
//! - the free names come from the last refresh's whole-fleet listing, already on screen: names
//!   are free fleet-wide, so a provider switch doesn't change them. They're a preview — `up` lists
//!   the whole fleet again (refusing if a provider doesn't answer) and picks free names from that;
//! - each provider's type field — RunPod's live GPU catalog (every type it sells: B300, the MIG
//!   slices, the unlisted types seen on the account…), the GPUs on Vast's market now, Hetzner's
//!   server types (vCPU, RAM, €/h) — comes from `arena offers <provider>` in the background
//!   (`offers.rs`), read when first shown and again when old: the presets meanwhile (marked
//!   `…`), and if it can't be read;
//! - the availability line — what `up` would try for the choice, in order, with price, stock and
//!   near/anywhere — likewise, debounced and cached per choice (`o` turns it off).
//!
//! The form model is pure (fields, choices, the flags it maps to); `key`, `tick` and `render`
//! are the dashboard's side.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use arena_core::provider::hetzner::HetznerOpts;
use arena_core::{naming, Config, Pod, PodSpec};

use crate::offers::{self, Fetch, Shown};
use crate::{centered_rect, runs, Mode, Shared, Ui};

/// One provider choice in the add-pod form, with whether its API key is configured.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProviderOpt {
    pub name: String,
    pub available: bool,
}

/// The editable fields of the add-pod form, in display order. Which apply depends on the
/// provider: a cloud tier only on RunPod; on Hetzner (a CPU VM sized by its server type) the type
/// field is the server type, and no GPU count, disk, volume or deep check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NpField {
    Provider,
    CloudType,
    /// The GPU type — or, on Hetzner, the server type.
    Kind,
    GpuCount,
    Disk,
    Volume,
    /// `up --check`: deep-check each pod, replacing one on a bad host.
    Check,
    /// `up --retry-mins N`: when there's no capacity, try again for up to N minutes ([`RETRIES`]).
    Retry,
    Pods,
}

/// A choice in the type field: a GPU type (`--gpu`), or a Hetzner server type (`--server-type`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Choice {
    /// What `up`'s flag takes: RunPod's / Vast's type id, Hetzner's server type name.
    pub id: String,
    /// How it reads: `RTX A4000 · 16GB`, `B300 · 288GB`, `cx33`.
    pub label: String,
    /// VRAM per GPU in GB — or a server type's vCPUs: what the list is sorted by after the default.
    pub size: u32,
    /// $/h per GPU on RunPod's COMMUNITY and SECURE tiers (`None`: not sold there, or unknown).
    pub community: Option<f64>,
    pub secure: Option<f64>,
    /// The prices are the presets' estimates (`~`), not the provider's.
    pub estimate: bool,
    /// The tiers' prices say where it's sold (RunPod's live catalog); else unknown.
    pub tiers_known: bool,
    /// A server type's size (`4 vCPU · 8 GB`) and €/h.
    pub server: Option<(String, Option<f64>)>,
    /// Vast: $/h per GPU of the cheapest offer a create would rent now (it searches again).
    pub market: Option<f64>,
    /// A RunPod type its public catalog lacks (`Some`), with what a pod of it bills per GPU
    /// when one on the account says: `--gpu` takes it, RunPod's create decides where.
    pub unlisted: Option<Option<f64>>,
}

impl Choice {
    /// A preset GPU (`arena_core::gpu`), or just its id when the presets don't know it.
    pub(crate) fn preset_gpu(id: &str) -> Choice {
        let g = arena_core::gpu::find(id);
        Choice {
            id: id.to_string(),
            label: arena_core::gpu::label(id),
            size: g.map_or(0, |g| g.vram_gb),
            community: g.map(|g| g.community),
            secure: g.map(|g| g.secure),
            estimate: true,
            tiers_known: false,
            server: None,
            market: None,
            unlisted: None,
        }
    }

    /// A server type known only by name (the catalog not read yet, or not readable).
    pub(crate) fn server_type(name: &str) -> Choice {
        Choice { id: name.into(), label: name.into(), size: 0, community: None, secure: None, estimate: false, tiers_known: false, server: None, market: None, unlisted: None }
    }

    /// Whether it's sold on `tier` — `None` when that isn't known.
    pub(crate) fn sold_on(&self, tier: &str) -> Option<bool> {
        if !self.tiers_known {
            return None;
        }
        Some(if tier.eq_ignore_ascii_case("SECURE") { self.secure.is_some() } else { self.community.is_some() })
    }

    /// The price to show for `cloud` (RunPod's tier, or both: `COMMUNITY,SECURE`) on `provider`.
    pub(crate) fn price(&self, provider: &str, cloud: Option<&str>) -> String {
        if let Some((_, eur)) = &self.server {
            return eur.map_or_else(|| "€ ?".into(), |e| format!("€{e:.4}/h"));
        }
        if let Some(pod) = self.unlisted {
            return pod.map_or_else(|| "unlisted".into(), |p| format!("unlisted · ${p:.2}/h a pod bills"));
        }
        if let Some(p) = self.market {
            return format!("from ${p:.2}/h now");
        }
        let t = if self.estimate { "~" } else { "" };
        let one = |p: Option<f64>| p.map_or_else(|| "-".to_string(), |p| format!("{t}${p:.2}"));
        match (provider, cloud) {
            ("runpod", Some(c)) if c.contains(',') => format!("{} / {} /h", one(self.community), one(self.secure)),
            ("runpod", Some(c)) if c.eq_ignore_ascii_case("SECURE") => format!("{}/h", one(self.secure)),
            ("runpod", _) => format!("{}/h", one(self.community)),
            ("vast", _) => match (self.community, self.secure) {
                (Some(a), Some(b)) => format!("~${a:.2}–{b:.2}/h (varies)"),
                _ => String::new(),
            },
            _ => String::new(),
        }
    }

    /// The cheapest price it's sold at, for the sort.
    fn sort_price(&self) -> f64 {
        let p = [self.community, self.secure, self.market, self.unlisted.flatten(), self.server.as_ref().and_then(|s| s.1)].into_iter().flatten().fold(f64::INFINITY, f64::min);
        if p.is_finite() {
            p
        } else {
            f64::MAX
        }
    }
}

/// Sort `choices` as the type field lists them: `default` first, then GPUs by VRAM — an unknown
/// one last (the presets' newest cards, B300 and the MIG slices, have none) — price and name;
/// server types keep the catalogue's order (by family, then size: `offers hetzner`'s), which a
/// sort by vCPUs would interleave (cx23, cpx22 and cax11 all have 2). Pure.
pub(crate) fn sort_choices(choices: &mut [Choice], default: &str) {
    choices.sort_by(|a, b| {
        let first = (b.id == default).cmp(&(a.id == default));
        if a.server.is_some() || b.server.is_some() {
            return first;
        }
        first
            .then((a.size == 0).cmp(&(b.size == 0)))
            .then(a.size.cmp(&b.size))
            .then(a.sort_price().total_cmp(&b.sort_price()))
            .then(a.label.cmp(&b.label))
    });
}

/// Where the type field's choices came from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Source {
    /// The provider's catalogue, as `arena offers <provider>` reads it.
    Live,
    /// The presets, while the catalog is read.
    Loading,
    /// The presets, and why (no catalog to read, or it couldn't be read).
    Presets(String),
}

/// The type field's defaults: config `GPU_TYPE` (`PodSpec`'s), and Hetzner's server type —
/// `HETZNER_SERVER_TYPE`, else core's default (cx33).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Defaults {
    pub gpu: String,
    pub server: String,
}

/// RunPod's cloud tier choices: one tier, or community with secure to fall back on (`up`'s
/// comma list).
pub(crate) const CLOUDS: [&str; 3] = ["COMMUNITY", "SECURE", "COMMUNITY,SECURE"];

/// The retry field's choices (the human, 2026-10-30: "for create pod on tui, can u have an
/// auto-retry option?"): how long `up` keeps trying while there's no capacity — `(label,
/// minutes)`, `--retry-mins` (0: once, `up`'s default). A round every [`RETRY_EVERY_SECS`] (up's
/// `--retry-secs` default), each round every option of the choice (near the proxy first, then
/// the fallbacks), the confirmed names only, never past the window; Ctrl+C (`[s]` in the runs
/// panel) stops it between rounds, keeping what was made. Not `--keep-trying`: that gives up
/// after an hour whatever it says (120 tries 30 s apart), waits on one GPU type and tier only (a
/// fallback list is refused) and drops the near-the-proxy preference — and its 30 s waits don't
/// listen for Ctrl+C. The longest is a day — "until it works", bounded.
pub(crate) const RETRIES: [(&str, u64); 5] = [("off", 0), ("10 min", 10), ("30 min", 30), ("2 h", 120), ("24 h", 1440)];

/// How often a retrying `up` tries again: its `--retry-secs` default, which the form leaves as it
/// is.
pub(crate) const RETRY_EVERY_SECS: u64 = 60;

/// The add-pod form: `↑↓` moves between [`NpField`]s, `←→` changes the field's value. Pure.
#[derive(Debug, Clone)]
pub(crate) struct NewPodForm {
    pub providers: Vec<ProviderOpt>,
    pub provider_idx: usize,
    pub cloud_idx: usize,
    /// The type field's choices for the current provider.
    pub kinds: Vec<Choice>,
    pub kind_idx: usize,
    pub source: Source,
    pub defaults: Defaults,
    /// The type each provider's field was last on: a switch away and back keeps it.
    pub picked: HashMap<String, String>,
    pub gpu_count: u32,
    /// Container disk in GB (50 GB steps).
    pub disk_gb: u32,
    /// Persistent volume in GB (0 = none; 100 GB steps).
    pub volume_gb: u32,
    pub check: bool,
    /// Index into [`RETRIES`] (0: off).
    pub retry_idx: usize,
    pub count: usize,
    /// The free machine names as of the last refresh (fleet-wide: a name is free only if no
    /// provider holds it).
    pub free: Vec<String>,
    /// The last refresh had listed the fleet when the form opened (else the names are a guess).
    pub listed: bool,
    /// Index into [`Self::fields`].
    pub field: usize,
    /// A change the form made by itself, said once (the tier switched to where a type is sold).
    pub note: Option<String>,
}

fn wrap(idx: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (idx as i32 + delta).rem_euclid(len as i32) as usize
    }
}

impl NewPodForm {
    pub(crate) fn provider_name(&self) -> &str {
        self.providers.get(self.provider_idx).map(|p| p.name.as_str()).unwrap_or("runpod")
    }

    fn is_runpod(&self) -> bool {
        self.provider_name() == "runpod"
    }

    pub(crate) fn is_hetzner(&self) -> bool {
        self.provider_name() == "hetzner"
    }

    /// The fields that apply to the current provider, in display order.
    pub(crate) fn fields(&self) -> Vec<NpField> {
        let mut f = vec![NpField::Provider];
        if self.is_runpod() {
            f.push(NpField::CloudType);
        }
        f.push(NpField::Kind);
        if !self.is_hetzner() {
            f.extend([NpField::GpuCount, NpField::Disk, NpField::Volume, NpField::Check]);
        }
        f.extend([NpField::Retry, NpField::Pods]);
        f
    }

    pub(crate) fn selected(&self) -> NpField {
        let f = self.fields();
        f[self.field.min(f.len() - 1)]
    }

    pub(crate) fn move_field(&mut self, delta: i32) {
        self.field = wrap(self.field, delta, self.fields().len());
    }

    /// The type field's current choice.
    pub(crate) fn kind(&self) -> Option<&Choice> {
        self.kinds.get(self.kind_idx)
    }

    /// The RunPod tier(s) — `COMMUNITY`, `SECURE` or `COMMUNITY,SECURE`; `None` elsewhere.
    pub(crate) fn cloud(&self) -> Option<&str> {
        self.is_runpod().then(|| CLOUDS[self.cloud_idx.min(CLOUDS.len() - 1)])
    }

    /// Change the selected field's value by `delta` (−1 left / +1 right). `true`: the provider
    /// changed — the caller puts its choices in ([`Self::set_kinds`]), from what's cached.
    pub(crate) fn change(&mut self, delta: i32) -> bool {
        self.note = None;
        match self.selected() {
            NpField::Provider => {
                let before = self.provider_idx;
                self.cycle_provider(delta);
                // The field set may have changed (→ Hetzner); stay on the provider field.
                self.field = 0;
                return self.provider_idx != before;
            }
            NpField::CloudType => self.cloud_idx = wrap(self.cloud_idx, delta, CLOUDS.len()),
            NpField::Kind => {
                self.kind_idx = wrap(self.kind_idx, delta, self.kinds.len());
                if let Some(k) = self.kind() {
                    self.picked.insert(self.provider_name().to_string(), k.id.clone());
                }
                self.follow_tier();
            }
            NpField::GpuCount => self.gpu_count = (self.gpu_count as i32 + delta).clamp(1, 8) as u32,
            NpField::Disk => self.disk_gb = (self.disk_gb as i32 + delta * 50).clamp(20, 2000) as u32,
            NpField::Volume => self.volume_gb = (self.volume_gb as i32 + delta * 100).clamp(0, 2000) as u32,
            NpField::Check => self.check = !self.check,
            NpField::Retry => self.retry_idx = wrap(self.retry_idx, delta, RETRIES.len()),
            NpField::Pods => {
                let max = self.free.len().max(1) as i32;
                self.count = (self.count as i32 + delta).clamp(1, max) as usize;
            }
        }
        false
    }

    /// A type RunPod sells only on the other tier (a B300, the MIG slices: SECURE only): the tier
    /// follows it, and the form says so — never a choice that would plan nothing.
    fn follow_tier(&mut self) {
        let (Some(k), Some(cloud)) = (self.kind(), self.cloud()) else { return };
        if cloud.contains(',') || k.sold_on(cloud) != Some(false) {
            return;
        }
        let other = if cloud == "SECURE" { "COMMUNITY" } else { "SECURE" };
        if k.sold_on(other) == Some(true) {
            let label = k.label.clone();
            self.cloud_idx = CLOUDS.iter().position(|c| *c == other).unwrap_or(0);
            self.note = Some(format!("cloud → {other}: {label} is sold only there"));
        }
    }

    /// Step to the next provider with an API key, in `delta`'s direction.
    fn cycle_provider(&mut self, delta: i32) {
        let n = self.providers.len() as i32;
        if n == 0 {
            return;
        }
        let step = if delta < 0 { -1 } else { 1 };
        let mut idx = self.provider_idx as i32;
        for _ in 0..n {
            idx = (idx + step).rem_euclid(n);
            if self.providers[idx as usize].available {
                self.provider_idx = idx as usize;
                return;
            }
        }
    }

    /// Put the current provider's choices in, sorted ([`sort_choices`]), on the one picked for it
    /// before, else the default, else the first.
    pub(crate) fn set_kinds(&mut self, mut kinds: Vec<Choice>, source: Source) {
        let default = if self.is_hetzner() { self.defaults.server.clone() } else { self.defaults.gpu.clone() };
        sort_choices(&mut kinds, &default);
        let want = self.picked.get(self.provider_name()).cloned().or_else(|| self.kind().map(|k| k.id.clone()));
        let at = |id: &str| kinds.iter().position(|k| k.id == id);
        self.kind_idx = want.as_deref().and_then(at).or_else(|| at(&default)).unwrap_or(0);
        self.kinds = kinds;
        self.source = source;
    }

    /// How long `up` retries while there's no capacity, in minutes (0: it doesn't).
    pub(crate) fn retry_mins(&self) -> u64 {
        RETRIES[self.retry_idx.min(RETRIES.len() - 1)].1
    }

    /// The names the create would get as of the last refresh.
    pub(crate) fn planned_names(&self) -> Vec<String> {
        self.free.iter().take(self.count).cloned().collect()
    }

    /// `arena pods up`'s arguments for this form (the provider goes in `--provider`, the
    /// runner's): `--add N` — `up` picks the names from its own whole-fleet listing — then the
    /// type, and what the form shows for this provider: on Hetzner the server type alone; on a
    /// GPU provider the GPU type, GPUs per pod, RunPod's tier(s), disk, volume and `--check`;
    /// then `--retry-mins N` when it retries. `--yes`: the dashboard's confirm has asked. Pure.
    pub(crate) fn up_args(&self) -> Vec<String> {
        let mut a: Vec<String> = vec!["pods".into(), "up".into(), "--add".into(), self.count.to_string()];
        let kind = self.kind().map(|k| k.id.clone()).unwrap_or_default();
        if self.is_hetzner() {
            a.extend(["--server-type".into(), kind]);
        } else {
            a.extend(["--gpu".into(), kind, "--gpus".into(), self.gpu_count.to_string()]);
            if let Some(c) = self.cloud() {
                a.extend(["--cloud".into(), c.to_string()]);
            }
            a.extend(["--disk".into(), self.disk_gb.to_string(), "--volume".into(), self.volume_gb.to_string()]);
            if self.check {
                a.push("--check".into());
            }
        }
        if self.retry_mins() > 0 {
            a.extend(["--retry-mins".into(), self.retry_mins().to_string()]);
        }
        a.push("--yes".into());
        a
    }
}

/// The type field's choices before (or without) a catalog: the GPU presets — config `GPU_TYPE`
/// first — or Hetzner's configured server type.
pub(crate) fn preset_kinds(provider: &str, d: &Defaults) -> Vec<Choice> {
    if provider == "hetzner" {
        return vec![Choice::server_type(&d.server)];
    }
    let mut ids = Vec::new();
    if !d.gpu.is_empty() {
        ids.push(d.gpu.clone());
    }
    for api in arena_core::gpu::preset_apis() {
        if !ids.contains(&api) {
            ids.push(api);
        }
    }
    ids.iter().map(|id| Choice::preset_gpu(id)).collect()
}

/// Put the form's provider's choices in from what's cached: its catalog when read, the presets
/// while it's out (`Loading`) or when it failed or there is none.
pub(crate) fn apply_kinds(form: &mut NewPodForm, offers: &offers::State) {
    let p = form.provider_name().to_string();
    let presets = preset_kinds(&p, &form.defaults);
    match offers.catalog(&p) {
        Some(Fetch::Ready(kinds)) => form.set_kinds(kinds.to_vec(), Source::Live),
        Some(Fetch::Failed(why)) => form.set_kinds(presets, Source::Presets(format!("the catalogue couldn't be read: {why}"))),
        Some(Fetch::Out) => form.set_kinds(presets, Source::Loading),
        None if offers::has_catalog(&p) => form.set_kinds(presets, Source::Loading),
        None => form.set_kinds(presets, Source::Presets(format!("{p} has no list to read: availability shows what up would try"))),
    }
}

/// Seed the add-pod form: the providers with an API key (the dashboard's own first), the type
/// field's defaults, and the free names as of the last refresh (`existing`, `listed`: whether
/// it had listed the fleet yet). Pure.
pub(crate) fn build_form(cfg: &Config, launch_provider: &str, existing: &[Pod], listed: bool) -> NewPodForm {
    let key_for = |p: &str| match p {
        "runpod" => "RUNPOD_API_KEY",
        "vast" => "VAST_API_KEY",
        "hetzner" => "HETZNER_API_KEY",
        "shadeform" => "SHADEFORM_API_KEY",
        _ => "",
    };
    let providers: Vec<ProviderOpt> = ["runpod", "vast", "hetzner", "shadeform"]
        .iter()
        .map(|p| ProviderOpt { name: p.to_string(), available: cfg.get(key_for(p)).is_some_and(|v| !v.is_empty()) })
        .collect();
    let provider_idx = providers
        .iter()
        .position(|o| o.name == launch_provider && o.available)
        .or_else(|| providers.iter().position(|o| o.available))
        .unwrap_or(0);
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let free = naming::next_free_names(prefix, &cfg.machine_names, existing, usize::MAX);
    let spec = PodSpec::from_config(cfg);
    let cloud_idx = CLOUDS.iter().position(|c| c.eq_ignore_ascii_case(spec.cloud_type.trim())).unwrap_or(0);
    let defaults = Defaults { gpu: spec.gpu_type.clone(), server: HetznerOpts::from_config(cfg).server_type };
    let mut form = NewPodForm {
        providers,
        provider_idx,
        cloud_idx,
        kinds: Vec::new(),
        kind_idx: 0,
        source: Source::Loading,
        defaults,
        picked: HashMap::new(),
        gpu_count: spec.gpu_count.max(1),
        disk_gb: spec.disk_gb.max(20),
        volume_gb: spec.volume_gb,
        check: false,
        retry_idx: 0,
        count: 1,
        free,
        listed,
        field: 0,
        note: None,
    };
    let p = form.provider_name().to_string();
    form.set_kinds(preset_kinds(&p, &form.defaults), Source::Loading);
    form
}

/// `n`: the form, at once, from the snapshot on screen.
pub(crate) fn open(ui: &mut Ui, shared: &Arc<Mutex<Shared>>) {
    let (pods, listed) = {
        let s = shared.lock().unwrap();
        (s.pods.clone(), s.last_refresh.is_some())
    };
    let mut form = build_form(&ui.cfg, &ui.provider_name, &pods, listed);
    apply_kinds(&mut form, &ui.offers);
    ui.mode = Mode::NewPod(form);
}

/// The form's background work, each pass of the loop: its provider's catalogue asked for when
/// due ([`offers::State::catalog_due`]), put in when it lands, and the availability of its
/// choice (debounced). Starts fetches; never waits on one. `true`: the form changed.
pub(crate) fn tick(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, now: Instant) -> bool {
    let Ui { mode, offers, cli, .. } = ui;
    let Mode::NewPod(form) = mode else {
        offers.want(None, now);
        return false;
    };
    let mut changed = false;
    if let Some(q) = offers.catalog_due(form.provider_name(), now) {
        runs::fetch_offers(shared, cli, q, offers, now);
    }
    // Its catalogue landed (the first time, or a refresh): in, on the same choice.
    if offers.take_new(form.provider_name()) {
        apply_kinds(form, offers);
        changed = true;
    }
    offers.want(offers::key_of(form), now);
    if let Some(q) = offers.plan_due(now) {
        runs::fetch_offers(shared, cli, q, offers, now);
        changed = true;
    }
    changed
}

/// The form's keys, and its confirm's. `false`: not ours.
pub(crate) fn key(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, code: KeyCode) -> bool {
    match ui.mode.clone() {
        Mode::NewPod(mut form) => {
            match code {
                KeyCode::Esc => {
                    ui.mode = Mode::List;
                    return true;
                }
                KeyCode::Up | KeyCode::Char('k') => form.move_field(-1),
                KeyCode::Down | KeyCode::Char('j') => form.move_field(1),
                KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l') => {
                    let delta = if matches!(code, KeyCode::Left | KeyCode::Char('h')) { -1 } else { 1 };
                    if form.change(delta) {
                        // From the cache: a catalog not read yet is asked for by the next `tick`.
                        apply_kinds(&mut form, &ui.offers);
                    }
                }
                KeyCode::Char('o') => ui.offers.show = !ui.offers.show,
                KeyCode::Enter => {
                    ui.mode = if form.kind().is_none() {
                        Mode::Result("✗ nothing to create: no type to choose".into())
                    } else {
                        Mode::UpConfirm(form)
                    };
                    return true;
                }
                _ => {}
            }
            ui.mode = Mode::NewPod(form);
            true
        }
        Mode::UpConfirm(form) => {
            match code {
                KeyCode::Char('y') => {
                    let args = form.up_args();
                    let title = up_title(&form);
                    match runs::start(ui, shared, runs::Kind::Up, form.provider_name(), args, title) {
                        Ok(id) => ui.mode = Mode::Runs { id, scroll: None, stop: false },
                        Err(e) => ui.mode = Mode::Result(format!("✗ {e}")),
                    }
                }
                KeyCode::Char('n') | KeyCode::Esc => ui.mode = Mode::NewPod(form),
                _ => {}
            }
            true
        }
        _ => false,
    }
}

/// `pods up: 2 × RTX A4000 · 16GB on runpod` — the run's title (`, retrying up to 30 min`).
fn up_title(form: &NewPodForm) -> String {
    let kind = form.kind().map_or("?", |k| k.label.as_str());
    let retry = if form.retry_mins() > 0 { format!(", retrying up to {}", retry_span(form.retry_mins())) } else { String::new() };
    format!("pods up: {} × {kind} on {}{retry}", form.count, form.provider_name())
}

/// `10 min`, `2 h`, `24 h` — a retry window as the form names it.
pub(crate) fn retry_span(mins: u64) -> String {
    RETRIES.iter().find(|(_, m)| *m == mins).map(|(l, _)| l.to_string()).unwrap_or_else(|| format!("{mins} min"))
}

/// The retry field's value: `off`, or `30 min if there's no capacity`.
fn retry_words(mins: u64) -> String {
    if mins == 0 {
        "off — once; no capacity ends it".into()
    } else {
        format!("{} while there's no capacity", retry_span(mins))
    }
}

/// The command line the confirm shows: what will run, flags and all.
pub(crate) fn command_line(form: &NewPodForm) -> String {
    let quote = |a: &str| if a.contains(' ') || a.contains(',') { format!("'{a}'") } else { a.to_string() };
    let args: Vec<String> = form.up_args().iter().map(|a| quote(a)).collect();
    format!("arena --provider {} {}", form.provider_name(), args.join(" "))
}

/// The availability lines for `form`'s choice, as the form and its confirm show them.
fn availability(form: &NewPodForm, offers: &offers::State) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let key = offers::key_of(form);
    let head = |s: &str| Line::styled(s.to_string(), Style::default().add_modifier(Modifier::BOLD));
    match offers.shown(key.as_ref()) {
        Shown::Off => vec![Line::styled("availability: off ([o] shows what up would try)", dim)],
        Shown::Waiting | Shown::Checking => vec![head("what up would try, in order:"), Line::styled("  checking…", Style::default().fg(Color::Cyan))],
        Shown::Failed(why) => vec![head("what up would try, in order:"), Line::styled(format!("  ✗ {why}"), Style::default().fg(Color::Red))],
        Shown::Ready(lines) => {
            let mut out = vec![head("what up would try, in order:")];
            out.extend(lines.iter().map(|l| if l.starts_with("note:") || l.starts_with('(') { Line::styled(format!("  {l}"), dim) } else { Line::raw(format!("  {l}")) }));
            out
        }
    }
}

/// The add-pod form: one line per field (`▶ label ‹ value ›`), the choices of the field the
/// cursor is on, the availability of the choice, the keys.
pub(crate) fn render(f: &mut Frame, form: &NewPodForm, offers: &offers::State) {
    let sel = form.selected();
    let dim = Style::default().fg(Color::DarkGray);
    let cur = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let label = Style::default().add_modifier(Modifier::BOLD);
    let provider = form.provider_name();
    let loading = if form.source == Source::Loading { " …" } else { "" };

    let mut lines: Vec<Line> = Vec::new();
    for fld in form.fields() {
        let selected = fld == sel;
        let (name, value) = match fld {
            NpField::Provider => ("provider", provider.to_string()),
            NpField::CloudType => ("cloud", form.cloud().unwrap_or("-").replace(',', " → ")),
            NpField::Kind => {
                let name = if form.is_hetzner() { "server" } else { "gpu type" };
                let value = match form.kind() {
                    Some(k) => {
                        let size = k.server.as_ref().map(|(s, _)| format!(" · {s}")).unwrap_or_default();
                        let price = k.price(provider, form.cloud());
                        let price = if price.is_empty() { String::new() } else { format!("  {price}") };
                        format!("{}{size}{price}{loading}", k.label)
                    }
                    None => format!("-{loading}"),
                };
                (name, value)
            }
            NpField::GpuCount => ("gpus/pod", form.gpu_count.to_string()),
            NpField::Disk => ("disk", format!("{}GB", form.disk_gb)),
            NpField::Volume => ("volume", if form.volume_gb == 0 { "none".to_string() } else { format!("{}GB", form.volume_gb) }),
            NpField::Check => ("check", if form.check { "deep-check, replace a bad host".into() } else { "off".to_string() }),
            NpField::Retry => ("retry", retry_words(form.retry_mins())),
            NpField::Pods => ("pods", format!("{} of {} free", form.count, form.free.len())),
        };
        let marker = if selected { "▶ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{name:<9} "), if selected { cur } else { label }),
            Span::styled(format!("‹ {value} ›"), if selected { cur } else { Style::default() }),
        ]));
    }
    if let Some(n) = &form.note {
        lines.push(Line::styled(format!("  {n}"), Style::default().fg(Color::Yellow)));
    }
    lines.push(Line::from(""));
    match sel {
        NpField::Provider => {
            lines.push(Line::styled("providers:", label));
            for p in &form.providers {
                let mark = if p.name == provider { "●" } else { "○" };
                let text = if p.available { format!("  {mark} {}", p.name) } else { format!("  {mark} {} (no api key)", p.name) };
                lines.push(Line::styled(text, if !p.available { dim } else if p.name == provider { cur } else { Style::default() }));
            }
        }
        NpField::CloudType => {
            lines.push(Line::styled("cloud tiers (a list falls back in order):", label));
            for (i, c) in CLOUDS.iter().enumerate() {
                let mark = if i == form.cloud_idx { "●" } else { "○" };
                lines.push(Line::styled(format!("  {mark} {}", c.replace(',', " → ")), if i == form.cloud_idx { cur } else { Style::default() }));
            }
        }
        NpField::Kind => {
            let what = match &form.source {
                Source::Live => format!("{provider}'s catalog ({})", form.kinds.len()),
                Source::Loading => "presets — reading the catalog…".into(),
                Source::Presets(why) => format!("presets — {why}"),
            };
            let head = if form.is_hetzner() { "server types" } else { "gpu types" };
            lines.push(Line::styled(format!("{head}: {what}"), label));
            // A window around the choice: a catalog runs to dozens.
            const AROUND: usize = 5;
            let start = form.kind_idx.saturating_sub(AROUND);
            let end = (form.kind_idx + AROUND + 1).min(form.kinds.len());
            if start > 0 {
                lines.push(Line::styled(format!("  ↑ {} more", start), dim));
            }
            for (i, k) in form.kinds.iter().enumerate().take(end).skip(start) {
                let mark = if i == form.kind_idx { "●" } else { "○" };
                let size = k.server.as_ref().map(|(s, _)| format!("{s:<16} ")).unwrap_or_default();
                let tier = match form.cloud() {
                    Some(c) if !c.contains(',') && k.sold_on(c) == Some(false) => {
                        if c == "SECURE" { "  (community only)" } else { "  (secure only)" }
                    }
                    _ => "",
                };
                let text = format!("  {mark} {:<28}{size}{}{tier}", k.label, k.price(provider, form.cloud()));
                let style = if i == form.kind_idx { cur } else if tier.is_empty() { Style::default() } else { dim };
                lines.push(Line::styled(text, style));
            }
            if end < form.kinds.len() {
                lines.push(Line::styled(format!("  ↓ {} more", form.kinds.len() - end), dim));
            }
        }
        NpField::GpuCount => lines.push(Line::styled("GPUs per pod: 1–8", dim)),
        NpField::Disk => lines.push(Line::styled("container disk in 50GB steps — the main storage (wiped if the pod is destroyed).", dim)),
        NpField::Volume => lines.push(Line::styled("persistent volume in 100GB steps (0 = none, the default). Survives restarts.", dim)),
        NpField::Check => lines.push(Line::styled("`up --check`: deep-check each pod; one failing on its host is terminated and recreated (2 tries).", dim)),
        NpField::Retry => {
            lines.push(Line::styled(
                format!(
                    "when nothing is free (no capacity): try again every {RETRY_EVERY_SECS}s — every option, near the proxy first — for up to this long, then stop; nothing bills until a pod is made. [s] in the runs panel stops it early, keeping what was made. (`up --retry-mins N`)"
                ),
                dim,
            ));
            for (i, (label, _)) in RETRIES.iter().enumerate() {
                let mark = if i == form.retry_idx { "●" } else { "○" };
                lines.push(Line::styled(format!("  {mark} {label}"), if i == form.retry_idx { cur } else { Style::default() }));
            }
        }
        NpField::Pods => {
            lines.push(Line::styled(if form.listed { "will create (free as of the last refresh — up re-checks):" } else { "will create (the fleet isn't listed yet — up picks free names):" }, label));
            let planned = form.planned_names();
            if planned.is_empty() {
                lines.push(Line::styled("  (no free machine names left)", dim));
            }
            for n in planned.iter().take(8) {
                lines.push(Line::from(format!("  • {n}")));
            }
            if planned.len() > 8 {
                lines.push(Line::styled(format!("  … and {} more", planned.len() - 8), dim));
            }
        }
    }
    lines.push(Line::from(""));
    lines.extend(availability(form, offers));
    lines.push(Line::from(""));
    lines.push(Line::styled("[↑↓] field   [←→] change   [o] availability   [enter] review   [esc] cancel", dim));

    let area = centered_rect(70, 85, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)).title(" add pod ")),
        area,
    );
}

/// The confirm: the command that will run (`--yes`: this is the asking), the names, the
/// availability, `[y]`.
pub(crate) fn render_confirm(f: &mut Frame, form: &NewPodForm, offers: &offers::State) {
    let dim = Style::default().fg(Color::DarkGray);
    let names = form.planned_names();
    let mut lines = vec![
        Line::styled(format!("create {} pod(s) on {} and set each up, as `arena pods up` does:", form.count, form.provider_name()), Style::default().add_modifier(Modifier::BOLD)),
        Line::from(""),
        Line::styled(format!("  $ {}", command_line(form)), Style::default().fg(Color::Cyan)),
        Line::from(""),
        Line::from(format!("names (free as of the last refresh): {}", if names.is_empty() { "picked by up".to_string() } else { names.join(", ") })),
        Line::styled("up lists the whole fleet again first, and refuses if a provider doesn't answer.", dim),
        Line::from(""),
    ];
    if form.retry_mins() > 0 {
        let until = arena_core::snapshot::unix_now() + form.retry_mins() * 60;
        lines.push(Line::styled(
            format!(
                "retry: no capacity → tries again every {RETRY_EVERY_SECS}s until {} ({}), then stops; [s] in the runs panel stops it sooner",
                runs::clock(until),
                retry_span(form.retry_mins())
            ),
            Style::default().fg(Color::Yellow),
        ));
        lines.push(Line::from(""));
    }
    lines.extend(availability(form, offers));
    lines.push(Line::from(""));
    lines.push(Line::styled("it runs in the background: [p] shows its progress, the dashboard stays usable.", dim));
    lines.push(Line::from(""));
    lines.push(Line::styled("[y] create   [n/esc] back to the form", Style::default().add_modifier(Modifier::BOLD)));
    let area = centered_rect(76, 60, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Yellow)).title(" create? ")),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Defaults {
        Defaults { gpu: "NVIDIA RTX A4000".into(), server: "cx33".into() }
    }

    fn form() -> NewPodForm {
        let mut f = NewPodForm {
            providers: vec![
                ProviderOpt { name: "runpod".into(), available: true },
                ProviderOpt { name: "vast".into(), available: false },
                ProviderOpt { name: "hetzner".into(), available: true },
            ],
            provider_idx: 0,
            cloud_idx: 0,
            kinds: Vec::new(),
            kind_idx: 0,
            source: Source::Loading,
            defaults: defaults(),
            picked: HashMap::new(),
            gpu_count: 1,
            disk_gb: 100,
            volume_gb: 0,
            check: false,
            retry_idx: 0,
            count: 1,
            free: vec!["arena8-apple".into(), "arena8-autumn".into()],
            listed: true,
            field: 0,
        note: None,
        };
        f.set_kinds(preset_kinds("runpod", &defaults()), Source::Loading);
        f
    }

    fn live(id: &str, size: u32, community: Option<f64>, secure: Option<f64>) -> Choice {
        Choice { id: id.into(), label: id.into(), size, community, secure, estimate: false, tiers_known: true, server: None, market: None, unlisted: None }
    }

    fn hetzner_types() -> Vec<Choice> {
        let t = |name: &str, cores: u32, eur: f64| Choice { server: Some((format!("{cores} vCPU"), Some(eur))), size: cores, ..Choice::server_type(name) };
        // In the catalogue's order (`offers hetzner`: by family, then size).
        vec![t("cx23", 2, 0.0104), t("cx33", 4, 0.016), t("cx43", 8, 0.0296), t("cpx22", 2, 0.0089)]
    }

    #[test]
    fn fields_depend_on_provider() {
        let mut f = form();
        use NpField::*;
        assert_eq!(f.fields(), [Provider, CloudType, Kind, GpuCount, Disk, Volume, Check, Retry, Pods]);
        assert!(f.change(1), "runpod → hetzner (vast has no key)");
        assert_eq!(f.provider_name(), "hetzner");
        assert_eq!(f.fields(), [Provider, Kind, Retry, Pods], "a CPU VM: its server type, no GPU count, cloud, disk or check");
        assert_eq!(f.cloud(), None);
    }

    /// On Hetzner the type field is the server type: its choices are the server types (the
    /// configured one first — cx33 by default — then in the catalogue's order, by family and
    /// size), each with vCPU/RAM and €/h; back on RunPod, the GPU it was on.
    #[test]
    fn hetzner_switches_the_type_field_to_server_types() {
        let mut f = form();
        f.field = 2;
        f.change(1); // the next GPU preset
        let gpu = f.kind().unwrap().id.clone();
        f.field = 0;
        f.change(1);
        f.set_kinds(hetzner_types(), Source::Live);
        assert_eq!(f.kinds.iter().map(|k| k.id.as_str()).collect::<Vec<_>>(), ["cx33", "cx23", "cx43", "cpx22"], "the default first, then the catalogue's order");
        assert_eq!(f.kind().unwrap().id, "cx33");
        assert_eq!(f.kind().unwrap().price("hetzner", None), "€0.0160/h");
        f.field = 1;
        f.change(1);
        assert_eq!(f.kind().unwrap().id, "cx23");
        f.field = 0;
        f.change(-1);
        f.set_kinds(preset_kinds("runpod", &defaults()), Source::Loading);
        assert_eq!(f.kind().unwrap().id, gpu, "back on RunPod: the GPU picked there");
        f.change(1);
        f.set_kinds(hetzner_types(), Source::Live);
        assert_eq!(f.kind().unwrap().id, "cx23", "and Hetzner's server type picked there");
        // Before the catalog: the configured type alone, known by name.
        assert_eq!(preset_kinds("hetzner", &defaults()), [Choice::server_type("cx33")]);
    }

    /// The form's choices as `up`'s flags — on Hetzner the server type alone; on RunPod every
    /// GPU field, the tier(s) and `--check` when on; on Vast no tier; always `--add` and `--yes`.
    #[test]
    fn the_form_maps_to_ups_flags() {
        let mut f = form();
        assert_eq!(
            f.up_args(),
            ["pods", "up", "--add", "1", "--gpu", "NVIDIA RTX A4000", "--gpus", "1", "--cloud", "COMMUNITY", "--disk", "100", "--volume", "0", "--yes"]
        );
        f.cloud_idx = 2;
        f.gpu_count = 2;
        f.count = 2;
        f.check = true;
        f.volume_gb = 100;
        assert_eq!(
            f.up_args(),
            ["pods", "up", "--add", "2", "--gpu", "NVIDIA RTX A4000", "--gpus", "2", "--cloud", "COMMUNITY,SECURE", "--disk", "100", "--volume", "100", "--check", "--yes"]
        );
        assert_eq!(command_line(&f), "arena --provider runpod pods up --add 2 --gpu 'NVIDIA RTX A4000' --gpus 2 --cloud 'COMMUNITY,SECURE' --disk 100 --volume 100 --check --yes");
        f.field = 0;
        f.change(1);
        f.set_kinds(hetzner_types(), Source::Live);
        assert_eq!(f.up_args(), ["pods", "up", "--add", "2", "--server-type", "cx33", "--yes"]);
        f.providers[1].available = true;
        f.change(-1);
        assert_eq!(f.provider_name(), "vast");
        f.set_kinds(preset_kinds("vast", &defaults()), Source::Presets("x".into()));
        assert_eq!(
            f.up_args(),
            ["pods", "up", "--add", "2", "--gpu", "NVIDIA RTX A4000", "--gpus", "2", "--disk", "100", "--volume", "100", "--check", "--yes"]
        );
    }

    /// The human (2026-10-30): "for create pod on tui, can u have an auto-retry option?" The
    /// retry field — off by default, on every provider — maps to `up --retry-mins N`: off, 10
    /// min, 30 min, 2 h, a day; the title and the confirm say so.
    #[test]
    fn the_retry_field_maps_to_retry_mins() {
        let mut f = form();
        assert_eq!(f.retry_mins(), 0);
        assert!(!f.up_args().contains(&"--retry-mins".to_string()), "off: once, as before");
        f.field = f.fields().iter().position(|x| *x == NpField::Retry).unwrap();
        let mut seen = Vec::new();
        for _ in 0..RETRIES.len() {
            f.change(1);
            seen.push(f.retry_mins());
        }
        assert_eq!(seen, [10, 30, 120, 1440, 0], "←→ cycles through them");
        f.change(-1);
        assert_eq!(f.retry_mins(), 1440, "and back");
        f.retry_idx = 2;
        assert_eq!(
            f.up_args(),
            ["pods", "up", "--add", "1", "--gpu", "NVIDIA RTX A4000", "--gpus", "1", "--cloud", "COMMUNITY", "--disk", "100", "--volume", "0", "--retry-mins", "30", "--yes"]
        );
        assert!(up_title(&f).ends_with(" on runpod, retrying up to 30 min"), "{}", up_title(&f));
        // Hetzner too: a cx type can be out of stock in a location.
        f.field = 0;
        f.change(1);
        f.set_kinds(hetzner_types(), Source::Live);
        assert_eq!(f.up_args(), ["pods", "up", "--add", "1", "--server-type", "cx33", "--retry-mins", "30", "--yes"]);
        assert_eq!(retry_span(1440), "24 h");
    }

    /// RunPod's live catalog: sorted with the default first, then by VRAM, then price; a type
    /// sold only on the other tier is listed (never hidden) and choosing it moves the tier there,
    /// saying so.
    #[test]
    fn a_type_sold_only_on_the_other_tier_moves_the_tier() {
        let mut f = form();
        f.set_kinds(
            vec![
                live("NVIDIA B300 SXM6 AC", 288, None, Some(8.99)),
                live("MIG 1g.24gb", 24, None, Some(0.69)),
                live("NVIDIA GeForce RTX 3090", 24, Some(0.22), Some(0.5)),
                live("NVIDIA RTX A4000", 16, Some(0.17), Some(0.25)),
            ],
            Source::Live,
        );
        assert_eq!(f.kinds.iter().map(|k| k.id.as_str()).collect::<Vec<_>>(), ["NVIDIA RTX A4000", "NVIDIA GeForce RTX 3090", "MIG 1g.24gb", "NVIDIA B300 SXM6 AC"]);
        assert_eq!(f.cloud(), Some("COMMUNITY"));
        f.field = 2;
        f.change(1);
        assert_eq!((f.kind().unwrap().id.as_str(), f.cloud(), f.note.as_deref()), ("NVIDIA GeForce RTX 3090", Some("COMMUNITY"), None));
        f.change(1);
        assert_eq!((f.kind().unwrap().id.as_str(), f.cloud()), ("MIG 1g.24gb", Some("SECURE")));
        assert_eq!(f.note.as_deref(), Some("cloud → SECURE: MIG 1g.24gb is sold only there"));
        assert_eq!(f.kind().unwrap().price("runpod", f.cloud()), "$0.69/h");
        // Both tiers chosen: nothing to move, both prices shown.
        f.cloud_idx = 2;
        f.change(1);
        assert_eq!(f.cloud(), Some("COMMUNITY,SECURE"));
        assert_eq!(f.kind().unwrap().price("runpod", f.cloud()), "- / $8.99 /h");
        // The presets don't say where they're sold: nothing moves.
        assert_eq!(Choice::preset_gpu("NVIDIA RTX A4000").sold_on("SECURE"), None);
    }

    /// Without a catalogue (Shadeform, or before/without RunPod's) the presets stand in, the
    /// newest cards and MIG slices among them (offers-cli's picker extras) — after the ones whose
    /// VRAM is known, not before the 8 GB cards.
    #[test]
    fn the_presets_offer_b300_and_mig_slices_last() {
        let mut f = form();
        f.set_kinds(preset_kinds("shadeform", &defaults()), Source::Presets("x".into()));
        let ids: Vec<&str> = f.kinds.iter().map(|k| k.id.as_str()).collect();
        assert_eq!(ids[0], "NVIDIA RTX A4000", "the default first");
        for extra in ["NVIDIA B300 SXM6 AC", "NVIDIA B300 SXM6 AC MIG 1g.34gb", "NVIDIA B200"] {
            assert!(ids.contains(&extra), "{extra}: {ids:?}");
        }
        let first_unknown = f.kinds.iter().position(|k| k.size == 0).unwrap();
        assert!(f.kinds[first_unknown..].iter().all(|k| k.size == 0) && first_unknown > 1, "{ids:?}");
    }

    #[test]
    fn change_clamps_gpu_count_disk_volume_and_pods() {
        let mut f = form();
        f.field = 3; // GpuCount
        for _ in 0..20 {
            f.change(1);
        }
        assert_eq!(f.gpu_count, 8);
        f.field = 4; // Disk (50GB steps)
        f.change(1);
        assert_eq!(f.disk_gb, 150);
        f.field = 5; // Volume (100GB steps)
        f.change(1);
        f.change(1);
        assert_eq!(f.volume_gb, 200);
        f.field = 6; // Check toggles
        f.change(1);
        assert!(f.check);
        f.field = 8; // Pods
        for _ in 0..20 {
            f.change(1);
        }
        assert_eq!(f.count, 2, "capped at the free names");
    }
}
