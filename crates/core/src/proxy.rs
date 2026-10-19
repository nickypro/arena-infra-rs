//! Port-forwarding / proxy planning.
//!
//! Pods are accessed over SSH (the team uses VS Code Remote-SSH, not Jupyter). The
//! provider assigns each pod's SSH endpoint (IP + port) and *reassigns* it whenever
//! the pod restarts — so a raw provider endpoint is a moving target to put in a VS
//! Code config. The proxy host (`SSH_PROXY_HOST`) solves that by giving each machine
//! a **stable public address** — `cute.sus.cat:7000`, `:7001`, … — that nginx's
//! `stream` module forwards straight to the pod's *current* SSH endpoint as raw TCP.
//! No tunnel process: nginx is the whole mechanism (declarative, graceful reload).
//!
//! This module only *plans* — it renders the nginx config and parses the previous one
//! back; deploying is the CLI's job (`proxy apply`), because the proxy is shared
//! production infrastructure.
//!
//! ## Why the ports are stable
//!
//! A pod's public port is anchored to its machine name's index in the fixed
//! `MACHINE_NAME_LIST`, not to its position among the currently-running pods. So
//! `arena8-apple` is always `starting_port + 0`. Tearing down one pod never renumbers
//! the others, and when a machine restarts (or is recreated with the same name) it
//! reclaims its old port — only the `proxy_pass` target behind it changes.
//!
//! ## Why the plan is a *merge*, not a rebuild
//!
//! Participants' VS Code configs point at those stable ports, so wrongly dropping a
//! forward locks someone out mid-session. Rebuilding the config from "whatever the
//! providers listed just now" did exactly that whenever a pod was momentarily listed
//! without an endpoint, or a whole provider's list call failed (a Vast 429 made every
//! Vast forward vanish). So [`plan_forwards`] merges the *previous* forwards (parsed
//! back from the config we rendered last time, see [`parse_nginx`]) with a per-provider
//! [`Listing`], and a forward is only removed when its pod is **confirmed gone** — absent
//! from a provider whose listing *succeeded*:
//!
//! - **R1** pod listed with an endpoint → forward to it (added / changed / unchanged);
//! - **R2** pod listed without a usable endpoint → keep the previous forward (its recorded
//!   owner only changes hands once the old owner is confirmed gone, as in R3);
//! - **R3** previous forward absent from its provider's successful listing → removed
//!   (a legacy entry with no recorded owner: only if *every* provider listed OK);
//! - **R4** previous forward whose provider failed to list (or wasn't queried) → kept;
//! - **R5** no provider listed successfully → `abort`: callers must write nothing.

use std::collections::{HashMap, HashSet};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::pod::Pod;

/// What `proxy apply` runs after writing the config when `SSH_PROXY_RELOAD_CMD` is absent:
/// validate first so a bad config never replaces the running one, then reload gracefully.
pub const DEFAULT_RELOAD_CMD: &str = "nginx -t && nginx -s reload";

/// Proxy-host settings, read from the `SSH_PROXY_*` keys in `config.env`.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// User to SSH into the proxy host as (for manual deploy of the rendered config).
    pub proxy_user: String,
    /// The public proxy host (domain or IP) that holds the stable ports.
    pub proxy_host: String,
    /// Where the generated nginx config should live on the proxy host.
    pub nginx_path: String,
    /// First public port; machine index 0 gets this, index 1 gets +1, etc.
    pub starting_port: u16,
    /// Deploy nginx on THIS machine (write the config + reload locally) instead of over
    /// SSH to `proxy_host`. Defaults on: the control plane usually *is* the proxy host
    /// (e.g. `proxy_host` resolves back to this box), where SSHing to it is a needless
    /// hairpin. Set `PROXY_LOCAL=false` for a genuinely remote proxy.
    pub local: bool,
    /// Command run (locally or on the proxy host) after the config is written. Empty means
    /// **write-only**: the file is written and nginx is never reloaded — so a sandbox that
    /// shares a box with the production nginx can exercise `proxy apply` without ever
    /// touching it. See [`reload_cmd_from`].
    pub reload_cmd: String,
}

impl ProxyConfig {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let proxy_host = cfg
            .get("SSH_PROXY_HOST")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Config("missing SSH_PROXY_HOST".into()))?
            .to_string();
        // Local by default; only a literal false/0/no opts back into remote SSH deploy.
        // A loopback host name also forces local regardless.
        let opted_remote = matches!(
            cfg.get("PROXY_LOCAL").map(|s| s.trim().to_lowercase()).as_deref(),
            Some("false") | Some("0") | Some("no") | Some("off")
        );
        let loopback = matches!(proxy_host.as_str(), "localhost" | "127.0.0.1" | "::1");
        Ok(Self {
            proxy_user: cfg.get("SSH_PROXY_USER").unwrap_or("root").to_string(),
            proxy_host,
            nginx_path: cfg
                .get("SSH_PROXY_NGINX_CONFIG_PATH")
                .unwrap_or("~/proxy.conf")
                .to_string(),
            starting_port: cfg.get_parsed("SSH_PROXY_STARTING_PORT").unwrap_or(7000),
            local: loopback || !opted_remote,
            reload_cmd: reload_cmd_from(cfg),
        })
    }

    /// Write the config file but never reload nginx (`SSH_PROXY_RELOAD_CMD=""`).
    pub fn write_only(&self) -> bool {
        self.reload_cmd.trim().is_empty()
    }
}

/// `SSH_PROXY_RELOAD_CMD`: **absent** → [`DEFAULT_RELOAD_CMD`] (prod's behaviour, so an
/// existing config is unaffected); **present but empty** → `""` = write-only, never reload;
/// otherwise the given command (e.g. `sudo systemctl reload nginx`). Absent and empty are
/// deliberately different: an empty value is an explicit "don't touch nginx". It may also
/// be set (to empty) from the environment, even when the file lacks it — how a wrapper on a
/// box shared with a production nginx guarantees write-only.
pub fn reload_cmd_from(cfg: &Config) -> String {
    match cfg.get("SSH_PROXY_RELOAD_CMD") {
        None => DEFAULT_RELOAD_CMD.to_string(),
        Some(s) => s.trim().to_string(),
    }
}

/// One pod's forwarding: a stable public port on the proxy that streams to the pod's
/// current SSH endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub name: String,
    /// Stable public port on the proxy host (what a VS Code SSH config targets).
    pub public_port: u16,
    /// The pod's current SSH endpoint (hostname or IP) that nginx streams to.
    pub target_ip: String,
    pub target_port: u16,
    /// Which provider owns the pod behind this forward — whose *successful* listing is
    /// needed before the forward may be removed. `None` for entries parsed from a legacy
    /// config that didn't record it.
    pub provider: Option<String>,
    /// The owning pod's provider id (informational; `None` for legacy entries).
    pub pod_id: Option<String>,
}

impl Forward {
    /// `host:port` as nginx's `proxy_pass` wants it (IPv6 literals bracketed).
    pub fn target(&self) -> String {
        if self.target_ip.contains(':') {
            format!("[{}]:{}", self.target_ip, self.target_port)
        } else {
            format!("{}:{}", self.target_ip, self.target_port)
        }
    }
}

/// A pod that can't be wired up, with the reason — surfaced so the operator never
/// thinks coverage is complete when it isn't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// One provider's listing outcome. The error is kept as text: the planner only needs to
/// know *that* the listing failed (and say why), never to act on the error's type.
#[derive(Debug, Clone)]
pub struct ProviderListing {
    pub provider: String,
    pub pods: std::result::Result<Vec<Pod>, String>,
}

/// Per-provider listing outcomes for the whole fleet — the input that lets the planner
/// tell "listed OK without this pod" (it's gone) from "that provider didn't answer"
/// (we know nothing). Build it from [`crate::Provider::list_by_provider`].
#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub providers: Vec<ProviderListing>,
}

/// What a [`Listing`] says about one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingStatus<'a> {
    Ok,
    Failed(&'a str),
    NotQueried,
}

impl Listing {
    pub fn from_results(results: Vec<(String, Result<Vec<Pod>>)>) -> Self {
        Self {
            providers: results
                .into_iter()
                .map(|(provider, r)| ProviderListing { provider, pods: r.map_err(|e| e.to_string()) })
                .collect(),
        }
    }

    /// The outcome for `provider`. If it somehow appears twice, any failure wins — an
    /// uncertain listing must never count as a confirmation that a pod is gone.
    pub fn status(&self, provider: &str) -> ListingStatus<'_> {
        let mut status = ListingStatus::NotQueried;
        for pl in self.providers.iter().filter(|pl| pl.provider == provider) {
            match &pl.pods {
                Err(e) => return ListingStatus::Failed(e),
                Ok(_) => status = ListingStatus::Ok,
            }
        }
        status
    }

    /// At least one provider was queried and every one of them listed successfully.
    pub fn all_ok(&self) -> bool {
        !self.providers.is_empty() && self.providers.iter().all(|pl| pl.pods.is_ok())
    }

    pub fn any_ok(&self) -> bool {
        self.providers.iter().any(|pl| pl.pods.is_ok())
    }

    /// `(provider, error)` for every failed listing.
    pub fn errors(&self) -> Vec<(&str, &str)> {
        self.providers
            .iter()
            .filter_map(|pl| pl.pods.as_ref().err().map(|e| (pl.provider.as_str(), e.as_str())))
            .collect()
    }

    /// Every pod from the successful listings (e.g. for readiness polling).
    pub fn pods(&self) -> Vec<Pod> {
        self.providers.iter().filter_map(|pl| pl.pods.as_ref().ok()).flatten().cloned().collect()
    }
}

/// What the plan does to one machine's forward. `Added`, `Changed` and `Unchanged` come
/// only from R1, so they mean "routed to the endpoint the listing reports for this pod";
/// `Kept` never does (see [`ProxyPlan::routed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// New forward (no previous entry for this name).
    Added,
    /// Target (or port) differs from the previous entry, shown as `from`.
    Changed { from: Forward },
    /// Previous entry dropped — the pod is confirmed gone, or the entry is invalid.
    Removed { reason: String },
    /// Previous entry carried over unconfirmed (stale): its pod was listed without an
    /// endpoint, or its provider couldn't be listed. `moved_from` is its old public port
    /// when the `MACHINE_NAME_LIST` index moved it: a port move breaks participants' SSH
    /// configs even though the target is unchanged, so it's shown with `~` and counted as
    /// *changed*, never hidden among the quiet `=` lines.
    Kept { reason: String, moved_from: Option<u16> },
    /// Same routing as before.
    Unchanged,
}

/// One per-name line of a plan: the resulting forward (for `Removed`, the dropped one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub forward: Forward,
    pub kind: ChangeKind,
}

/// The result of planning: the forward set to render, what changed vs the previous
/// config, pods we had to skip, and whether writing must be refused altogether.
#[derive(Debug, Clone, Default)]
pub struct ProxyPlan {
    /// The full forward set to render, sorted by public port.
    pub forwards: Vec<Forward>,
    pub changes: Vec<Change>,
    pub skipped: Vec<Skipped>,
    /// Machines on the list that a provider listed with no usable SSH endpoint and no
    /// previous forward to keep — typically pods still booting right after `pods create`.
    /// They're also in `skipped` (with the precise reason); this list lets a post-lifecycle
    /// sync say "N not forwarded yet" instead of a bare `+0` that reads like it did nothing.
    pub pending: Vec<String>,
    /// Set when the listing can't support *any* decision (every provider failed, or none
    /// was queried). Callers must not write the config — a rebuild from nothing would
    /// drop every forward. `forwards` is then the previous set, untouched.
    pub abort: Option<String>,
}

/// Counts of each change kind, for the one-line summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeCounts {
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    pub kept: usize,
    pub unchanged: usize,
}

impl ChangeCounts {
    /// `+N ~N -N =N` (added, changed, removed, kept-stale) — the terse form for the
    /// one-line sync report after a lifecycle command. Unchanged entries are left out:
    /// they're the steady state, not news.
    pub fn compact(&self) -> String {
        format!("+{} ~{} -{} ={}", self.added, self.changed, self.removed, self.kept)
    }
}

impl ProxyPlan {
    pub fn counts(&self) -> ChangeCounts {
        let mut c = ChangeCounts::default();
        for ch in &self.changes {
            match ch.kind {
                ChangeKind::Added => c.added += 1,
                ChangeKind::Changed { .. } => c.changed += 1,
                ChangeKind::Removed { .. } => c.removed += 1,
                ChangeKind::Kept { moved_from: Some(_), .. } => c.changed += 1,
                ChangeKind::Kept { .. } => c.kept += 1,
                ChangeKind::Unchanged => c.unchanged += 1,
            }
        }
        c
    }

    /// The forwards this plan routes to an endpoint the listing *confirmed* (R1: added,
    /// changed or unchanged) — as opposed to kept-stale ones, whose target may be a pod
    /// that's already gone. A caller about to destroy the pod a forward used to point at
    /// (e.g. `pods replace` terminating the parked original) checks this first.
    pub fn routed(&self) -> Vec<Forward> {
        self.changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Added | ChangeKind::Changed { .. } | ChangeKind::Unchanged))
            .map(|c| c.forward.clone())
            .collect()
    }

    /// `+N added, ~N changed, -N removed, =N kept (stale), N unchanged` — the line
    /// `proxy plan`/`apply` print so a run is reviewable at a glance.
    pub fn summary(&self) -> String {
        let c = self.counts();
        format!(
            "+{} added, ~{} changed, -{} removed, ={} kept (stale), {} unchanged",
            c.added, c.changed, c.removed, c.kept, c.unchanged
        )
    }

    /// One line per change with a `+ ~ - =` marker (two spaces for unchanged, which are
    /// left out unless `include_unchanged`), e.g.
    /// `~ arena8-apple   cute.sus.cat:7000  -> 5.6.7.8:22  (was 1.2.3.4:22000)`.
    pub fn change_lines(&self, proxy_host: &str, include_unchanged: bool) -> Vec<String> {
        self.changes
            .iter()
            .filter(|c| include_unchanged || c.kind != ChangeKind::Unchanged)
            .map(|c| change_line(c, proxy_host))
            .collect()
    }
}

fn change_line(c: &Change, proxy_host: &str) -> String {
    let f = &c.forward;
    let owner = f.provider.as_deref().unwrap_or("owner unknown");
    let (marker, note) = match &c.kind {
        ChangeKind::Added => ('+', owner.to_string()),
        ChangeKind::Changed { from } if from.public_port != f.public_port => {
            ('~', format!("was :{} -> {}", from.public_port, from.target()))
        }
        ChangeKind::Changed { from } => ('~', format!("was {}", from.target())),
        ChangeKind::Removed { reason } => ('-', format!("removed: {reason}")),
        ChangeKind::Kept { reason, moved_from: Some(old) } => {
            ('~', format!("port moved :{old} -> :{}; kept: {reason}", f.public_port))
        }
        ChangeKind::Kept { reason, moved_from: None } => ('=', format!("kept: {reason}")),
        ChangeKind::Unchanged => (' ', owner.to_string()),
    };
    format!(
        "{marker} {:<24} {:<22} -> {:<22} ({note})",
        f.name,
        format!("{proxy_host}:{}", f.public_port),
        f.target()
    )
}

/// A name we'll put in an nginx comment: non-empty, and no whitespace, control
/// characters, or `# ; { }` — so nothing provider-supplied can end a comment line or
/// smuggle in a directive.
fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && !s.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '#' | ';' | '{' | '}'))
}

/// A `proxy_pass` host: a plain hostname or IP literal (`[A-Za-z0-9.:-]+`), nothing an
/// nginx parser could read as more than one token.
fn safe_host(s: &str) -> bool {
    !s.is_empty() && s.len() <= 253 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-'))
}

/// Why a listed pod has no forwardable endpoint.
enum EndpointIssue {
    Missing,
    EmptyHost,
    BadHost(String),
    ZeroPort,
}

impl EndpointIssue {
    /// Reason for a pod skipped outright (no previous forward to keep).
    fn skip_reason(&self) -> String {
        match self {
            Self::Missing => "no SSH endpoint yet (pod still starting?)".into(),
            Self::EmptyHost => "empty SSH host from provider".into(),
            Self::BadHost(h) => format!("rejected SSH host {h:?} (not a plain hostname/IP)"),
            Self::ZeroPort => "SSH port 0 from provider".into(),
        }
    }
    /// Short form for a kept entry ("listed by runpod without endpoint").
    fn short(&self) -> String {
        match self {
            Self::Missing => "without endpoint".into(),
            Self::EmptyHost => "with an empty SSH host".into(),
            Self::BadHost(h) => format!("with a rejected SSH host {h:?}"),
            Self::ZeroPort => "with SSH port 0".into(),
        }
    }
}

/// The pod's usable SSH endpoint, validated so it can go straight into `proxy_pass`.
fn endpoint(pod: &Pod) -> std::result::Result<(String, u16), EndpointIssue> {
    let (Some(ip), Some(port)) = (pod.ssh_ip.as_deref(), pod.ssh_port) else {
        return Err(EndpointIssue::Missing);
    };
    let ip = ip.trim();
    if ip.is_empty() {
        return Err(EndpointIssue::EmptyHost);
    }
    if !safe_host(ip) {
        return Err(EndpointIssue::BadHost(ip.to_string()));
    }
    if port == 0 {
        return Err(EndpointIssue::ZeroPort);
    }
    Ok((ip.to_string(), port))
}

/// Why the listing can't confirm anything about a forward owned by `owner` — `None` when
/// "absent" *would* be a confirmation: the owner's provider listed OK, or (a legacy entry
/// with no recorded owner) every provider did. Shared by R2 (may the forward change
/// hands?) and R3/R4 (may it be dropped?), so both ask the same question.
fn owner_unconfirmed(owner: Option<&str>, listing: &Listing) -> Option<String> {
    match owner {
        Some(o) => match listing.status(o) {
            ListingStatus::Ok => None,
            ListingStatus::Failed(e) => Some(format!("{o} listing failed: {e}")),
            ListingStatus::NotQueried => Some(format!("{o} was not queried")),
        },
        None if listing.all_ok() => None,
        None => {
            let failed: Vec<&str> = listing.errors().iter().map(|(prov, _)| *prov).collect();
            Some(format!("owner unknown (legacy entry) and {} failed to list", failed.join(", ")))
        }
    }
}

/// Build the forwarding plan by merging the previous forwards with a per-provider
/// listing (rules R1–R5 in the module docs). Pure (no I/O) so every rule is table-tested:
/// the caller passes the fixed machine candidate list, the forwards parsed from the
/// current config (`prev`, empty if there's none), and the fleet's [`Listing`].
///
/// The public port is always recomputed from the name's *current* `MACHINE_NAME_LIST`
/// index, so a kept entry moves with the list (and is removed once its name leaves it).
/// Listed pods whose name isn't in the list have no stable slot and are skipped.
pub fn plan_forwards(
    cfg: &ProxyConfig,
    prefix: &str,
    candidates: &[String],
    prev: &[Forward],
    listing: &Listing,
) -> ProxyPlan {
    let mut plan = ProxyPlan::default();

    // R5: with no successful listing we can't confirm anything — keep the previous
    // config byte-for-byte and tell the caller not to write.
    if !listing.any_ok() {
        let reason = if listing.providers.is_empty() {
            "no provider was listed".to_string()
        } else {
            let errs: Vec<String> = listing.errors().iter().map(|(p, e)| format!("{p}: {e}")).collect();
            format!("every provider failed to list ({})", errs.join("; "))
        };
        plan.forwards = prev.to_vec();
        plan.forwards.sort_by_key(|f| f.public_port);
        plan.changes = plan
            .forwards
            .iter()
            .map(|f| Change {
                forward: f.clone(),
                kind: ChangeKind::Kept { reason: "nothing written".into(), moved_from: None },
            })
            .collect();
        plan.abort = Some(reason);
        return plan;
    }

    // Stable port slot per qualified name (first occurrence wins on a duplicated entry).
    let mut slots: HashMap<String, usize> = HashMap::new();
    for (i, c) in candidates.iter().enumerate() {
        slots.entry(crate::naming::qualify(prefix, c)).or_insert(i);
    }
    let port_for = |name: &str| -> std::result::Result<u16, String> {
        let idx = slots
            .get(name)
            .ok_or_else(|| "name no longer in MACHINE_NAME_LIST (no stable port slot)".to_string())?;
        // u32 so the overflow is detectable rather than silently colliding two pods.
        let port = cfg.starting_port as u32 + *idx as u32;
        u16::try_from(port).map_err(|_| format!("public port {port} exceeds 65535 (starting_port + index too high)"))
    };

    // Previous entries by name; a name appearing twice in the old file keeps its first.
    let mut prev_by_name: HashMap<&str, &Forward> = HashMap::new();
    let mut prev_dupes: Vec<&Forward> = Vec::new();
    for f in prev {
        if prev_by_name.contains_key(f.name.as_str()) {
            prev_dupes.push(f);
        } else {
            prev_by_name.insert(f.name.as_str(), f);
        }
    }

    // Listed pods (successful providers only) grouped by name, in listing order.
    let rank: HashMap<&str, usize> =
        listing.providers.iter().enumerate().map(|(i, pl)| (pl.provider.as_str(), i)).collect();
    let mut listed: Vec<(String, Vec<(&str, &Pod)>)> = Vec::new();
    let mut listed_idx: HashMap<String, usize> = HashMap::new();
    for pl in &listing.providers {
        let Ok(pods) = &pl.pods else { continue };
        for pod in pods {
            // Names are echoed to the terminal, so show odd ones escaped.
            let shown = || pod.name.escape_debug().to_string();
            if !slots.contains_key(&pod.name) {
                plan.skipped.push(Skipped {
                    name: shown(),
                    reason: "name not in MACHINE_NAME_LIST (no stable port slot)".into(),
                });
                continue;
            }
            // In the list but unsafe to write (a config entry with odd characters).
            if !safe_name(&pod.name) {
                plan.skipped.push(Skipped {
                    name: shown(),
                    reason: "name contains whitespace/control/`#;{}` characters — never written to nginx".into(),
                });
                continue;
            }
            let i = *listed_idx.entry(pod.name.clone()).or_insert_with(|| {
                listed.push((pod.name.clone(), Vec::new()));
                listed.len() - 1
            });
            listed[i].1.push((pl.provider.as_str(), pod));
        }
    }

    let mut handled: HashSet<String> = HashSet::new();
    for (name, mut holders) in listed {
        let prev_f = prev_by_name.get(name.as_str()).copied();
        // Same name listed more than once (two providers, or a provider allowing duplicate
        // names): pick deterministically — a usable endpoint first, then the pod the
        // previous config already pointed at (no flapping between two), then provider
        // order, then id — and report the rest.
        if holders.len() > 1 {
            holders.sort_by_key(|(prov, pod)| {
                let is_prev_owner = prev_f.is_some_and(|f| {
                    f.provider.as_deref() == Some(*prov) && f.pod_id.as_deref() == Some(pod.id.as_str())
                });
                (endpoint(pod).is_err(), !is_prev_owner, rank.get(prov).copied().unwrap_or(usize::MAX), pod.id.clone())
            });
            let (cprov, cpod) = holders[0];
            for (prov, pod) in &holders[1..] {
                plan.skipped.push(Skipped {
                    name: name.clone(),
                    reason: format!(
                        "duplicate name: also listed by {prov} (id {}); using {cprov} (id {})",
                        pod.id, cpod.id
                    ),
                });
            }
        }
        let (prov, pod) = holders[0];
        handled.insert(name.clone());

        let public_port = match port_for(&name) {
            Ok(p) => p,
            Err(why) => {
                plan.skipped.push(Skipped { name: name.clone(), reason: why.clone() });
                if let Some(pf) = prev_f {
                    plan.changes.push(Change { forward: pf.clone(), kind: ChangeKind::Removed { reason: why } });
                }
                continue;
            }
        };
        let pod_id = safe_name(&pod.id).then(|| pod.id.clone());
        match endpoint(pod) {
            // R1: listed with an endpoint → forward to it.
            Ok((host, tport)) => {
                let f = Forward {
                    name: name.clone(),
                    public_port,
                    target_ip: host,
                    target_port: tport,
                    provider: Some(prov.to_string()),
                    pod_id,
                };
                let kind = match prev_f {
                    None => ChangeKind::Added,
                    Some(p) if p.public_port != f.public_port || p.target_ip != f.target_ip || p.target_port != f.target_port => {
                        ChangeKind::Changed { from: p.clone() }
                    }
                    Some(_) => ChangeKind::Unchanged,
                };
                plan.forwards.push(f.clone());
                plan.changes.push(Change { forward: f, kind });
            }
            // R2: listed without a usable endpoint → keep the previous forward. The pod that
            // holds the name now becomes its owner (whose listing decides the entry's fate
            // from here on) only once the *previous* owner is confirmed gone — its provider
            // listed OK, or for a legacy entry every provider did. Otherwise a same-name pod
            // elsewhere (a stale stopped duplicate, a `create` while the real owner's provider
            // was hiding it) would inherit the forward, and the next sync would drop it on
            // that pod's disappearance while the real owner still hadn't answered (R3/R4).
            Err(issue) => match prev_f {
                Some(p) if safe_host(&p.target_ip) && p.target_port != 0 => {
                    let listed = format!("listed by {prov} {}", issue.short());
                    let (f, reason) = match owner_unconfirmed(p.provider.as_deref(), listing) {
                        None => (Forward { public_port, provider: Some(prov.to_string()), pod_id, ..p.clone() }, listed),
                        Some(why) => (Forward { public_port, ..p.clone() }, format!("{listed}; owner kept — {why}")),
                    };
                    let moved_from = (p.public_port != public_port).then_some(p.public_port);
                    plan.forwards.push(f.clone());
                    plan.changes.push(Change { forward: f, kind: ChangeKind::Kept { reason, moved_from } });
                }
                Some(p) => {
                    plan.skipped.push(Skipped { name: name.clone(), reason: issue.skip_reason() });
                    plan.changes.push(Change {
                        forward: p.clone(),
                        kind: ChangeKind::Removed { reason: format!("previous target {:?} is not a plain host:port", p.target()) },
                    });
                }
                None => {
                    plan.skipped.push(Skipped { name: name.clone(), reason: issue.skip_reason() });
                    plan.pending.push(name.clone());
                }
            },
        }
    }

    // Previous entries whose name no successful listing mentioned: R3 (confirmed gone)
    // or R4 (can't tell — keep).
    for p in prev {
        if handled.contains(&p.name) || !prev_by_name.get(p.name.as_str()).is_some_and(|f| std::ptr::eq(*f, p)) {
            continue;
        }
        let removed = |reason: String| Change { forward: p.clone(), kind: ChangeKind::Removed { reason } };
        if !safe_name(&p.name) {
            plan.changes.push(removed("name contains characters never written to nginx".into()));
            continue;
        }
        let public_port = match port_for(&p.name) {
            Ok(port) => port,
            Err(why) => {
                plan.changes.push(removed(why));
                continue;
            }
        };
        if !safe_host(&p.target_ip) || p.target_port == 0 {
            plan.changes.push(removed(format!("previous target {:?} is not a plain host:port", p.target())));
            continue;
        }
        let Some(keep_reason) = owner_unconfirmed(p.provider.as_deref(), listing) else {
            plan.changes.push(removed(match p.provider.as_deref() {
                Some(owner) => format!("no longer listed by {owner} (terminated or renamed)"),
                None => "not listed by any provider (legacy entry, owner unknown)".into(),
            }));
            continue;
        };
        let moved_from = (p.public_port != public_port).then_some(p.public_port);
        let f = Forward { public_port, ..p.clone() };
        plan.forwards.push(f.clone());
        plan.changes.push(Change { forward: f, kind: ChangeKind::Kept { reason: keep_reason, moved_from } });
    }
    for d in prev_dupes {
        plan.changes.push(Change {
            forward: d.clone(),
            kind: ChangeKind::Removed { reason: "duplicate entry for this name in the current config".into() },
        });
    }

    plan.forwards.sort_by_key(|f| f.public_port);
    plan.changes.sort_by(|a, b| {
        (a.forward.public_port, &a.forward.name).cmp(&(b.forward.public_port, &b.forward.name))
    });
    plan
}

/// Comment tag on the machine-readable line above each rendered `server` block.
pub const FORWARD_TAG: &str = "arena-forward";

/// Render the nginx config: one bare `server {}` per forward, each streaming a stable
/// public port to a pod's current SSH endpoint. `proxy_timeout 24h` keeps long-lived VS
/// Code SSH sessions from being dropped. Above each block, a `# arena-forward k=v …` line
/// records name, port, target, provider and pod id, so [`parse_nginx`] can read the
/// previous forwards — and who owns them — back on the next apply. No separate state file:
/// the live config *is* the state.
pub fn render_nginx(forwards: &[Forward]) -> String {
    // Bare `server` blocks (NOT wrapped in `stream { }`): this file is meant to be included
    // from inside nginx's top-level `stream { }` block — the standard drop-in pattern
    // (`stream { include /etc/nginx/streams-enabled/*.conf; }`). A `stream {}` here would
    // nest and fail with "stream directive is not allowed here".
    let mut out = String::from(
        "# Generated by arena-infra-rs (`arena proxy apply`). Do not edit by hand: the\n\
         # `# arena-forward` line above each block is read back on the next apply (it records\n\
         # which provider/pod owns the forward, so a provider outage never drops it).\n\
         # Bare `server` blocks: include from inside nginx's top-level `stream { }` block\n\
         # (e.g. `stream { include /etc/nginx/streams-enabled/*.conf; }`), then reload.\n",
    );
    for f in forwards {
        // The planner already rejects these; re-check here because this is the last step
        // before text reaches nginx, and a Forward can be built by hand.
        if !safe_name(&f.name) || !safe_host(&f.target_ip) {
            continue;
        }
        let mut meta = format!("# {FORWARD_TAG} name={} port={} target={}", f.name, f.public_port, f.target());
        if let Some(p) = f.provider.as_deref().filter(|p| safe_name(p)) {
            meta.push_str(&format!(" provider={p}"));
        }
        if let Some(id) = f.pod_id.as_deref().filter(|id| safe_name(id)) {
            meta.push_str(&format!(" pod_id={id}"));
        }
        out.push_str(&format!(
            "{meta}\nserver {{\n    listen {pub_p};\n    proxy_pass {target};\n    \
             proxy_timeout 24h;\n    proxy_connect_timeout 10s;\n}}\n",
            pub_p = f.public_port,
            target = f.target(),
        ));
    }
    out
}

/// What [`parse_nginx_detailed`] found: the forwards, plus how many `server` blocks it
/// couldn't read (so the CLI can warn that the merge is working from a partial `prev`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedNginx {
    pub forwards: Vec<Forward>,
    pub ignored_blocks: usize,
}

/// Parse the forwards back out of a config rendered by [`render_nginx`] — or by the
/// **legacy** renderer (`# <name>` then `server { listen P; proxy_pass ip:port; … }`, with
/// or without an outer `stream {}`), which is what's live in prod today; legacy entries
/// get `provider`/`pod_id = None`. Never panics: junk lines are ignored and a block
/// without a name, `listen` port, or `host:port` `proxy_pass` is skipped.
pub fn parse_nginx(text: &str) -> Vec<Forward> {
    parse_nginx_detailed(text).forwards
}

/// [`parse_nginx`] plus a count of the `server` blocks it had to ignore. The `listen` and
/// `proxy_pass` directives are the truth for port/target (they're what nginx runs); the
/// `# arena-forward` line supplies name and owner, else the last plain `# <name>` comment
/// before the block names it.
pub fn parse_nginx_detailed(text: &str) -> ParsedNginx {
    #[derive(Default)]
    struct Block {
        name: Option<String>,
        provider: Option<String>,
        pod_id: Option<String>,
        listen: Option<u16>,
        target: Option<(String, u16)>,
    }

    let mut out = ParsedNginx::default();
    let mut meta: Option<HashMap<String, String>> = None;
    let mut label: Option<String> = None;
    let mut block: Option<Block> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if let Some(b) = block.as_mut() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('}') {
                let b = block.take().unwrap_or_default();
                match (b.name, b.listen, b.target) {
                    (Some(name), Some(public_port), Some((target_ip, target_port))) if !name.is_empty() => {
                        out.forwards.push(Forward {
                            name,
                            public_port,
                            target_ip,
                            target_port,
                            provider: b.provider,
                            pod_id: b.pod_id,
                        });
                    }
                    _ => out.ignored_blocks += 1,
                }
                continue;
            }
            if let Some(v) = directive(line, "listen") {
                if b.listen.is_none() {
                    b.listen = v.rsplit(':').next().and_then(|p| p.parse::<u16>().ok());
                }
            } else if let Some(v) = directive(line, "proxy_pass") {
                if b.target.is_none() {
                    b.target = split_host_port(v);
                }
            }
            continue;
        }

        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            let comment = comment.trim();
            match comment.strip_prefix(FORWARD_TAG) {
                Some(kv) if kv.is_empty() || kv.starts_with(char::is_whitespace) => {
                    meta = Some(
                        kv.split_whitespace()
                            .filter_map(|t| t.split_once('='))
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect(),
                    );
                }
                _ => label = comment.split_whitespace().next().map(String::from),
            }
            continue;
        }
        if line.strip_prefix("server").is_some_and(|r| r.trim() == "{") {
            let m = meta.take().unwrap_or_default();
            block = Some(Block {
                name: m.get("name").cloned().or_else(|| label.take()),
                provider: m.get("provider").cloned(),
                pod_id: m.get("pod_id").cloned(),
                ..Default::default()
            });
            label = None;
            continue;
        }
        if line.starts_with("server") && line.contains('{') {
            out.ignored_blocks += 1; // e.g. a hand-written one-line block
        }
        // Anything else (an old `stream {` wrapper, its closing brace, stray directives)
        // isn't a forward; drop held comments so they can't label the wrong block.
        meta = None;
        label = None;
    }
    if block.is_some() {
        out.ignored_blocks += 1; // unterminated block
    }
    out
}

/// The value of `<name> <value>;` on a config line, if this line is that directive.
fn directive<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(name)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    rest.trim().trim_end_matches(';').split_whitespace().next()
}

/// `host:port` / `[v6]:port` → (host, port).
fn split_host_port(v: &str) -> Option<(String, u16)> {
    let (host, port) = v.rsplit_once(':')?;
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    let port = port.parse::<u16>().ok()?;
    (!host.is_empty()).then(|| (host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ProxyConfig {
        ProxyConfig {
            proxy_user: "root".into(),
            proxy_host: "cute.sus.cat".into(),
            nginx_path: "~/proxy.conf".into(),
            starting_port: 7000,
            local: false,
            reload_cmd: DEFAULT_RELOAD_CMD.into(),
        }
    }

    fn pod_on(provider: &str, id: &str, name: &str, ip: Option<&str>, port: Option<u16>) -> Pod {
        Pod {
            id: id.into(),
            name: name.into(),
            provider: provider.into(),
            status: "RUNNING".into(),
            ssh_ip: ip.map(String::from),
            ssh_port: port,
            ..Default::default()
        }
    }

    fn pod(name: &str, ip: Option<&str>, port: Option<u16>) -> Pod {
        pod_on("runpod", &format!("id-{name}"), name, ip, port)
    }

    fn fwd(name: &str, port: u16, ip: &str, tport: u16, provider: Option<&str>) -> Forward {
        Forward {
            name: name.into(),
            public_port: port,
            target_ip: ip.into(),
            target_port: tport,
            provider: provider.map(String::from),
            pod_id: provider.map(|_| format!("id-{name}")),
        }
    }

    fn candidates() -> Vec<String> {
        ["apple", "autumn", "bloom"].iter().map(|s| s.to_string()).collect()
    }

    /// One provider, listed OK.
    fn ok(provider: &str, pods: Vec<Pod>) -> ProviderListing {
        ProviderListing { provider: provider.into(), pods: Ok(pods) }
    }
    fn failed(provider: &str, err: &str) -> ProviderListing {
        ProviderListing { provider: provider.into(), pods: Err(err.into()) }
    }
    fn listing(providers: Vec<ProviderListing>) -> Listing {
        Listing { providers }
    }
    /// Shorthand for the common single-runpod listing with no previous config.
    fn plan_runpod(pods: Vec<Pod>) -> ProxyPlan {
        plan_forwards(&cfg(), "arena8", &candidates(), &[], &listing(vec![ok("runpod", pods)]))
    }
    fn kind_of<'a>(plan: &'a ProxyPlan, name: &str) -> &'a ChangeKind {
        &plan.changes.iter().find(|c| c.forward.name == name).unwrap_or_else(|| panic!("no change for {name}: {plan:?}")).kind
    }

    // ---- carried over from the rebuild-only planner ----

    #[test]
    fn port_is_anchored_to_candidate_index_not_pod_order() {
        // Only the *second* candidate is up; it must still get index-1's port (7001),
        // proving the allocation doesn't depend on which other pods exist.
        let plan = plan_runpod(vec![pod("arena8-autumn", Some("1.2.3.4"), Some(22001))]);
        assert_eq!(plan.forwards.len(), 1);
        let f = &plan.forwards[0];
        assert_eq!(f.public_port, 7001);
        assert_eq!(f.target_ip, "1.2.3.4");
        assert_eq!(f.target_port, 22001);
        assert_eq!(f.provider.as_deref(), Some("runpod"));
        assert_eq!(f.pod_id.as_deref(), Some("id-arena8-autumn"));
        assert!(plan.skipped.is_empty());
        assert!(plan.abort.is_none());
    }

    #[test]
    fn skips_unknown_names_and_pods_without_ssh() {
        let plan = plan_runpod(vec![
            pod("arena8-apple", Some("1.1.1.1"), Some(22000)),
            pod("arena8-ghost", Some("9.9.9.9"), Some(22099)), // not in candidates
            pod("arena8-bloom", None, None),                   // no ssh endpoint yet, no prev
        ]);
        assert_eq!(plan.forwards.len(), 1);
        assert_eq!(plan.forwards[0].name, "arena8-apple");
        assert_eq!(plan.forwards[0].public_port, 7000);
        assert_eq!(plan.skipped.len(), 2);
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-ghost"));
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-bloom" && s.reason.contains("no SSH endpoint")));
        // Only the on-list pod still waiting for an endpoint is "pending" (the off-list
        // ghost never gets a forward, so it isn't waiting for one).
        assert_eq!(plan.pending, vec!["arena8-bloom".to_string()]);
    }

    #[test]
    fn skips_port_overflow_and_empty_host_instead_of_silently_colliding() {
        let mut c = cfg();
        c.starting_port = 65535;
        // index 0 -> 65535 (ok); index 1 -> 65536 (overflow -> skipped, not saturated).
        let pods = vec![
            pod("arena8-apple", Some("1.1.1.1"), Some(22000)),
            pod("arena8-autumn", Some("2.2.2.2"), Some(22001)),
        ];
        let plan = plan_forwards(&c, "arena8", &candidates(), &[], &listing(vec![ok("runpod", pods)]));
        assert_eq!(plan.forwards.len(), 1);
        assert_eq!(plan.forwards[0].name, "arena8-apple");
        assert_eq!(plan.forwards[0].public_port, 65535);
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-autumn" && s.reason.contains("65535")));

        // empty/garbage host -> skipped (with no previous forward to keep)
        let plan = plan_runpod(vec![pod("arena8-bloom", Some("   "), Some(22002))]);
        assert!(plan.forwards.is_empty());
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-bloom" && s.reason.contains("empty")));
    }

    #[test]
    fn absolute_name_forwards_to_bare_pod_at_its_index_port() {
        // An `@`-marked candidate forwards to the bare pod name and still gets its
        // index-anchored port (index 2 -> 7002), exactly like a prefixed machine.
        let candidates = ["apple".to_string(), "autumn".into(), "@james-gpu".into()].to_vec();
        let pods = vec![pod("james-gpu", Some("5.6.7.8"), Some(22055))];
        let plan = plan_forwards(&cfg(), "arena8", &candidates, &[], &listing(vec![ok("runpod", pods)]));
        assert_eq!(plan.forwards.len(), 1, "skipped: {:?}", plan.skipped);
        let f = &plan.forwards[0];
        assert_eq!(f.name, "james-gpu");
        assert_eq!(f.public_port, 7002);
        assert_eq!(f.target_ip, "5.6.7.8");
        assert_eq!(f.target_port, 22055);
    }

    #[test]
    fn renders_nginx_stream_to_pod_ssh_endpoint() {
        let plan = plan_runpod(vec![pod("arena8-apple", Some("1.1.1.1"), Some(22000))]);
        let nginx = render_nginx(&plan.forwards);
        // Bare server blocks — no actual `stream {` *directive* (it's only in a comment
        // example); the file is included inside nginx's own top-level `stream { }`.
        assert!(!nginx.lines().any(|l| l.trim_start().starts_with("stream {")));
        assert!(nginx.contains("server {"));
        assert!(nginx.contains("listen 7000;"));
        assert!(nginx.contains("proxy_pass 1.1.1.1:22000;"));
        assert!(nginx.contains("proxy_timeout 24h;"));
        assert!(nginx.contains(
            "# arena-forward name=arena8-apple port=7000 target=1.1.1.1:22000 provider=runpod pod_id=id-arena8-apple\nserver {"
        ));
    }

    // ---- merge rules ----

    #[test]
    fn r1_listed_with_endpoint_is_added_changed_or_unchanged() {
        // (prev entry for apple, listed endpoint, expected kind)
        let table: Vec<(Option<Forward>, (&str, u16), &str)> = vec![
            (None, ("1.1.1.1", 22000), "added"),
            (Some(fwd("arena8-apple", 7000, "1.1.1.1", 22000, Some("runpod"))), ("1.1.1.1", 22000), "unchanged"),
            (Some(fwd("arena8-apple", 7000, "1.1.1.1", 22000, Some("runpod"))), ("2.2.2.2", 22000), "changed"),
            (Some(fwd("arena8-apple", 7000, "1.1.1.1", 22000, Some("runpod"))), ("1.1.1.1", 23000), "changed"),
            // a legacy entry with the same routing just gains its owner — no routing change
            (Some(fwd("arena8-apple", 7000, "1.1.1.1", 22000, None)), ("1.1.1.1", 22000), "unchanged"),
            // the port slot moved (list reordered earlier) — that's a change too
            (Some(fwd("arena8-apple", 7005, "1.1.1.1", 22000, Some("runpod"))), ("1.1.1.1", 22000), "changed"),
        ];
        for (prev, (ip, port), want) in table {
            let prev_v: Vec<Forward> = prev.clone().into_iter().collect();
            let l = listing(vec![ok("runpod", vec![pod("arena8-apple", Some(ip), Some(port))])]);
            let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev_v, &l);
            let got = match kind_of(&plan, "arena8-apple") {
                ChangeKind::Added => "added",
                ChangeKind::Changed { from } => {
                    assert_eq!(Some(from), prev.as_ref());
                    "changed"
                }
                ChangeKind::Unchanged => "unchanged",
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(got, want, "prev={prev:?} listed={ip}:{port}");
            assert_eq!(plan.forwards, vec![Forward {
                name: "arena8-apple".into(),
                public_port: 7000,
                target_ip: ip.into(),
                target_port: port,
                provider: Some("runpod".into()),
                pod_id: Some("id-arena8-apple".into()),
            }]);
        }
    }

    #[test]
    fn r2_listed_without_endpoint_keeps_previous_forward() {
        let prev = vec![fwd("arena8-apple", 7000, "1.1.1.1", 22000, Some("runpod"))];
        // Each of these is "listed, but no usable endpoint" — never a reason to drop.
        let variants: Vec<(Option<&str>, Option<u16>, &str)> = vec![
            (None, None, "without endpoint"),
            (Some(""), Some(22000), "empty SSH host"),
            (Some("1.1.1.1"), Some(0), "SSH port 0"),
            (Some("1.1.1.1;include x"), Some(22), "rejected SSH host"),
        ];
        for (ip, port, why) in variants {
            let l = listing(vec![ok("runpod", vec![pod("arena8-apple", ip, port)])]);
            let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &l);
            assert_eq!(plan.forwards, prev, "{why}");
            assert!(plan.pending.is_empty(), "{why}: a kept forward isn't pending");
            match kind_of(&plan, "arena8-apple") {
                ChangeKind::Kept { reason, .. } => {
                    assert!(reason.starts_with("listed by runpod"), "{reason}");
                    assert!(reason.contains(why), "{reason} !~ {why}");
                }
                other => panic!("{why}: expected Kept, got {other:?}"),
            }
        }
    }

    #[test]
    fn r3_absent_from_a_provider_that_listed_ok_is_removed() {
        let prev = vec![
            fwd("arena8-apple", 7000, "1.1.1.1", 22000, Some("runpod")),
            fwd("arena8-bloom", 7002, "3.3.3.3", 22, Some("hetzner")),
        ];
        let l = listing(vec![
            ok("runpod", vec![pod("arena8-apple", Some("1.1.1.1"), Some(22000))]),
            ok("hetzner", vec![]), // listed fine, bloom isn't there → terminated
        ]);
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &l);
        assert_eq!(plan.forwards, vec![prev[0].clone()]);
        match kind_of(&plan, "arena8-bloom") {
            ChangeKind::Removed { reason } => assert!(reason.contains("no longer listed by hetzner"), "{reason}"),
            other => panic!("expected Removed, got {other:?}"),
        }
    }

    #[test]
    fn r3_legacy_entry_removed_only_when_every_provider_listed_ok() {
        let prev = vec![fwd("arena8-bloom", 7002, "3.3.3.3", 22, None)]; // owner unknown
        let all_ok = listing(vec![ok("runpod", vec![]), ok("vast", vec![])]);
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &all_ok);
        assert!(plan.forwards.is_empty());
        assert!(matches!(kind_of(&plan, "arena8-bloom"), ChangeKind::Removed { reason } if reason.contains("legacy")));

        let one_failed = listing(vec![ok("runpod", vec![]), failed("vast", "vast list HTTP 429")]);
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &one_failed);
        assert_eq!(plan.forwards, prev);
        match kind_of(&plan, "arena8-bloom") {
            ChangeKind::Kept { reason, .. } => assert!(reason.contains("owner unknown") && reason.contains("vast"), "{reason}"),
            other => panic!("expected Kept, got {other:?}"),
        }
    }

    #[test]
    fn r4_owner_failed_or_not_queried_is_kept() {
        let prev = vec![fwd("arena8-autumn", 7001, "ssh4.vast.ai", 31000, Some("vast"))];
        let cases = vec![
            (listing(vec![ok("runpod", vec![]), failed("vast", "vast list HTTP 429 Too Many Requests")]), "vast listing failed: vast list HTTP 429"),
            (listing(vec![ok("runpod", vec![])]), "vast was not queried"),
        ];
        for (l, want) in cases {
            let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &l);
            assert_eq!(plan.forwards, prev);
            assert!(plan.abort.is_none());
            match kind_of(&plan, "arena8-autumn") {
                ChangeKind::Kept { reason, .. } => assert!(reason.contains(want), "{reason}"),
                other => panic!("expected Kept, got {other:?}"),
            }
        }
    }

    #[test]
    fn r5_every_provider_failed_or_empty_listing_aborts() {
        let prev = vec![
            fwd("arena8-bloom", 7002, "3.3.3.3", 22, Some("hetzner")),
            fwd("arena8-apple", 7000, "1.1.1.1", 22000, None),
        ];
        for l in [
            listing(vec![failed("runpod", "HTTP 500"), failed("vast", "HTTP 429")]),
            listing(vec![]),
        ] {
            let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &l);
            let abort = plan.abort.clone().expect("must abort");
            assert!(abort.contains("every provider failed") || abort.contains("no provider"), "{abort}");
            // Previous set untouched (sorted by port), nothing removed.
            assert_eq!(plan.forwards, vec![prev[1].clone(), prev[0].clone()]);
            assert_eq!(plan.counts().removed, 0);
        }
    }

    #[test]
    fn flapping_endpoint_keeps_then_updates_through_the_rendered_file() {
        // Drive three rounds through the real render → parse loop, the way successive
        // `proxy apply`s see it: endpoint present → absent → present (new).
        let l1 = listing(vec![ok("runpod", vec![pod("arena8-apple", Some("1.1.1.1"), Some(22000))])]);
        let p1 = plan_forwards(&cfg(), "arena8", &candidates(), &[], &l1);
        assert_eq!(kind_of(&p1, "arena8-apple"), &ChangeKind::Added);
        let file1 = render_nginx(&p1.forwards);

        let l2 = listing(vec![ok("runpod", vec![pod("arena8-apple", None, None)])]);
        let p2 = plan_forwards(&cfg(), "arena8", &candidates(), &parse_nginx(&file1), &l2);
        assert!(matches!(kind_of(&p2, "arena8-apple"), ChangeKind::Kept { .. }));
        let file2 = render_nginx(&p2.forwards);
        assert_eq!(file2, file1, "a flap must not change the live config at all");

        let l3 = listing(vec![ok("runpod", vec![pod("arena8-apple", Some("9.9.9.9"), Some(40000))])]);
        let p3 = plan_forwards(&cfg(), "arena8", &candidates(), &parse_nginx(&file2), &l3);
        match kind_of(&p3, "arena8-apple") {
            ChangeKind::Changed { from } => assert_eq!(from.target(), "1.1.1.1:22000"),
            other => panic!("expected Changed, got {other:?}"),
        }
        assert_eq!(p3.forwards[0].target(), "9.9.9.9:40000");
    }

    #[test]
    fn partial_provider_failure_keeps_the_failed_providers_forwards() {
        // vast 429s while runpod answers: runpod's forwards update, vast's stay put.
        let prev = vec![
            fwd("arena8-apple", 7000, "1.1.1.1", 22000, Some("runpod")),
            fwd("arena8-autumn", 7001, "ssh4.vast.ai", 31000, Some("vast")),
        ];
        let l = listing(vec![
            ok("runpod", vec![pod("arena8-apple", Some("2.2.2.2"), Some(22001))]),
            failed("vast", "vast list HTTP 429 Too Many Requests: {}"),
        ]);
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &l);
        assert!(plan.abort.is_none());
        assert_eq!(plan.forwards.len(), 2);
        assert!(matches!(kind_of(&plan, "arena8-apple"), ChangeKind::Changed { .. }));
        assert!(matches!(kind_of(&plan, "arena8-autumn"), ChangeKind::Kept { reason, .. } if reason.contains("vast listing failed")));
        assert_eq!(plan.summary(), "+0 added, ~1 changed, -0 removed, =1 kept (stale), 0 unchanged");
    }

    #[test]
    fn rename_moves_the_forward_only_once_the_provider_confirms() {
        // Pod id-X was arena8-apple and is now arena8-bloom.
        let prev = vec![Forward { pod_id: Some("X".into()), ..fwd("arena8-apple", 7000, "1.1.1.1", 22000, Some("runpod")) }];
        let renamed = pod_on("runpod", "X", "arena8-bloom", Some("1.1.1.1"), Some(22000));

        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &listing(vec![ok("runpod", vec![renamed])]));
        assert_eq!(kind_of(&plan, "arena8-bloom"), &ChangeKind::Added);
        assert_eq!(plan.forwards.iter().map(|f| (f.name.as_str(), f.public_port)).collect::<Vec<_>>(), [("arena8-bloom", 7002)]);
        assert!(matches!(kind_of(&plan, "arena8-apple"), ChangeKind::Removed { reason } if reason.contains("renamed")));

        // Same rename, but runpod's listing failed (vast answered): nothing is known, the
        // old name stays and the new one can't be added yet.
        let l = listing(vec![failed("runpod", "HTTP 502"), ok("vast", vec![])]);
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &l);
        assert_eq!(plan.forwards, prev);
        assert!(matches!(kind_of(&plan, "arena8-apple"), ChangeKind::Kept { .. }));
    }

    #[test]
    fn port_follows_the_current_list_index_and_leaving_the_list_removes() {
        let prev = vec![
            fwd("arena8-apple", 7000, "1.1.1.1", 22000, Some("vast")),
            fwd("arena8-autumn", 7001, "2.2.2.2", 22000, Some("vast")),
        ];
        // apple moved to index 1; autumn left the list. vast is down, so both would
        // otherwise be kept as-is.
        let cands: Vec<String> = vec!["bloom".into(), "apple".into()];
        let l = listing(vec![ok("runpod", vec![]), failed("vast", "HTTP 429")]);
        let plan = plan_forwards(&cfg(), "arena8", &cands, &prev, &l);
        assert_eq!(plan.forwards, vec![Forward { public_port: 7001, ..prev[0].clone() }]);
        assert!(matches!(kind_of(&plan, "arena8-autumn"), ChangeKind::Removed { reason } if reason.contains("MACHINE_NAME_LIST")));
    }

    #[test]
    fn duplicate_name_picks_deterministically_and_reports() {
        let a = pod_on("runpod", "r1", "arena8-apple", Some("1.1.1.1"), Some(22000));
        let b = pod_on("vast", "v1", "arena8-apple", Some("ssh4.vast.ai"), Some(31000));
        let no_ep = pod_on("runpod", "r0", "arena8-apple", None, None);

        // No prev: endpoint first, then provider order → runpod's r1.
        let l = listing(vec![ok("runpod", vec![no_ep.clone(), a.clone()]), ok("vast", vec![b.clone()])]);
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &[], &l);
        assert_eq!(plan.forwards.len(), 1);
        assert_eq!(plan.forwards[0].pod_id.as_deref(), Some("r1"));
        assert_eq!(plan.skipped.iter().filter(|s| s.reason.starts_with("duplicate name")).count(), 2);

        // The previous owner wins over provider order (no flapping between the two).
        let prev = vec![Forward { pod_id: Some("v1".into()), ..fwd("arena8-apple", 7000, "ssh4.vast.ai", 31000, Some("vast")) }];
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &l);
        assert_eq!(plan.forwards[0].pod_id.as_deref(), Some("v1"));
        assert_eq!(kind_of(&plan, "arena8-apple"), &ChangeKind::Unchanged);
    }

    #[test]
    fn rejects_injection_from_provider_supplied_fields() {
        let cands: Vec<String> = vec!["apple".into(), "autumn".into(), "evil;x".into()];
        let pods = vec![
            // A host that would smuggle a directive into proxy_pass.
            pod("arena8-apple", Some("1.2.3.4:22; } server { listen 22; proxy_pass 6.6.6.6"), Some(22)),
            // A name that would end the comment line and start a block.
            pod("arena8-autumn\nserver { listen 1; }", Some("1.1.1.1"), Some(22)),
            // A name matching a (bad) list entry still isn't written.
            pod("arena8-evil;x", Some("1.1.1.1"), Some(22)),
            // A pod id that isn't a plain token is dropped from the metadata, not rendered.
            pod_on("runpod", "id with space\n", "arena8-autumn", Some("5.5.5.5"), Some(22)),
        ];
        let plan = plan_forwards(&cfg(), "arena8", &cands, &[], &listing(vec![ok("runpod", pods)]));
        assert_eq!(plan.forwards.len(), 1, "{plan:?}");
        assert_eq!(plan.forwards[0].name, "arena8-autumn");
        assert_eq!(plan.forwards[0].pod_id, None);
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-apple" && s.reason.contains("rejected SSH host")));
        // Off-list, and shown escaped (no raw newline reaches the terminal either).
        assert!(plan.skipped.iter().any(|s| s.name.starts_with("arena8-autumn\\nserver") && s.reason.contains("MACHINE_NAME_LIST")));
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-evil;x" && s.reason.contains("never written")));
        let text = render_nginx(&plan.forwards);
        assert!(!text.contains("6.6.6.6") && !text.contains("listen 1;") && !text.contains("evil"), "{text}");
        assert_eq!(parse_nginx(&text), plan.forwards);

        // The renderer itself refuses a hand-built unsafe Forward.
        let bad = vec![Forward { target_ip: "1.1.1.1; deny all".into(), ..fwd("arena8-apple", 7000, "x", 22, None) }];
        assert!(!render_nginx(&bad).contains("deny all"));
    }

    #[test]
    fn legacy_file_migrates_to_owned_entries() {
        // Byte-for-byte what the previous renderer wrote (and what's live in prod today).
        let legacy = "# Generated by arena-infra-rs `arena proxy plan`. Do not edit by hand.\n\
                      # Bare `server` blocks: include from inside nginx's top-level `stream { }` block\n\
                      # (e.g. `stream { include /etc/nginx/streams-enabled/*.conf; }`), then reload.\n\
                      # arena8-apple\nserver {\n    listen 7000;\n    proxy_pass 1.1.1.1:22000;\n    \
                      proxy_timeout 24h;\n    proxy_connect_timeout 10s;\n}\n\
                      # arena8-autumn\nserver {\n    listen 7001;\n    proxy_pass ssh4.vast.ai:31000;\n    \
                      proxy_timeout 24h;\n    proxy_connect_timeout 10s;\n}\n";
        let prev = parse_nginx(legacy);
        assert_eq!(prev, vec![
            fwd("arena8-apple", 7000, "1.1.1.1", 22000, None),
            fwd("arena8-autumn", 7001, "ssh4.vast.ai", 31000, None),
        ]);

        // runpod lists apple at the same endpoint; vast (autumn's real owner) is down.
        let l = listing(vec![
            ok("runpod", vec![pod("arena8-apple", Some("1.1.1.1"), Some(22000))]),
            failed("vast", "HTTP 429"),
        ]);
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &l);
        assert_eq!(kind_of(&plan, "arena8-apple"), &ChangeKind::Unchanged);
        assert!(matches!(kind_of(&plan, "arena8-autumn"), ChangeKind::Kept { reason, .. } if reason.contains("legacy")));
        let migrated = render_nginx(&plan.forwards);
        assert!(migrated.contains("name=arena8-apple port=7000 target=1.1.1.1:22000 provider=runpod"));
        // autumn stays legacy (no provider=) until a listing actually claims it.
        assert!(migrated.contains("# arena-forward name=arena8-autumn port=7001 target=ssh4.vast.ai:31000\nserver {"));
        assert_eq!(parse_nginx(&migrated), plan.forwards);
    }

    #[test]
    fn parses_the_oldest_nested_stream_format_too() {
        let nested = "# Generated by arena-infra-rs `arena proxy plan`.\nstream {\n    # arena8-apple\n    server {\n        \
                      listen 7000;\n        proxy_pass 1.1.1.1:22000;\n        proxy_timeout 24h;\n    }\n}\n";
        assert_eq!(parse_nginx(nested), vec![fwd("arena8-apple", 7000, "1.1.1.1", 22000, None)]);
    }

    #[test]
    fn render_parse_round_trip() {
        let forwards = vec![
            fwd("arena8-apple", 7000, "1.1.1.1", 22000, Some("runpod")),
            fwd("arena8-autumn", 7001, "ssh4.vast.ai", 31000, Some("vast")),
            fwd("arena8-bloom", 7002, "2001:db8::1", 22, Some("hetzner")),
            fwd("james-gpu", 7003, "5.6.7.8", 22055, None),
        ];
        let text = render_nginx(&forwards);
        assert!(text.contains("proxy_pass [2001:db8::1]:22;"));
        assert_eq!(parse_nginx(&text), forwards);
        assert_eq!(parse_nginx_detailed(&text).ignored_blocks, 0);
        assert_eq!(parse_nginx(&render_nginx(&[])), vec![]);
    }

    #[test]
    fn parse_ignores_junk_and_never_panics() {
        let junk = "garbage line\n}\n}\nserver {\n    listen 7000;\n}\n\
                    # arena8-x\nserver {\n    proxy_pass 1.1.1.1:22;\n}\n\
                    # arena8-y\nserver {\n    listen notaport;\n    proxy_pass 1.1.1.1:22;\n}\n\
                    # arena8-z\nserver {\n    listen 7003;\n    proxy_pass upstream_name;\n}\n\
                    server { listen 7004; proxy_pass 1.1.1.1:22; }\n\
                    # arena-forward\n# arena-forward name=\nserver {\n    listen 7005;\n    proxy_pass 1.1.1.1:22;\n}\n\
                    # arena8-ok\nserver {\n    listen 0.0.0.0:7006 reuseport;\n    proxy_pass 9.9.9.9:22;\n}\n\
                    # arena8-unterminated\nserver {\n    listen 7007;\n";
        let parsed = parse_nginx_detailed(junk);
        assert_eq!(parsed.forwards, vec![fwd("arena8-ok", 7006, "9.9.9.9", 22, None)]);
        assert_eq!(parsed.ignored_blocks, 7);
        for s in ["", "\0\u{7f}{{{", "server {", "#", "# arena-forward name=a port=x", "}}}}server{"] {
            let _ = parse_nginx(s);
        }
    }

    #[test]
    fn change_lines_mark_each_kind() {
        let prev = vec![
            fwd("arena8-apple", 7000, "1.1.1.1", 22000, Some("runpod")),
            fwd("arena8-autumn", 7001, "ssh4.vast.ai", 31000, Some("vast")),
            fwd("james-gpu", 7003, "7.7.7.7", 22, Some("runpod")),
        ];
        let cands: Vec<String> = vec!["apple".into(), "autumn".into(), "bloom".into(), "@james-gpu".into()];
        let l = listing(vec![
            ok("runpod", vec![pod("arena8-apple", Some("2.2.2.2"), Some(22000)), pod("arena8-bloom", Some("3.3.3.3"), Some(22))]),
            failed("vast", "HTTP 429"),
        ]);
        let plan = plan_forwards(&cfg(), "arena8", &cands, &prev, &l);
        let lines = plan.change_lines("cute.sus.cat", true);
        assert_eq!(lines.len(), 4, "{lines:#?}");
        assert!(lines[0].starts_with("~ arena8-apple") && lines[0].contains("cute.sus.cat:7000") && lines[0].contains("(was 1.1.1.1:22000)"), "{}", lines[0]);
        assert!(lines[1].starts_with("= arena8-autumn") && lines[1].contains("kept: vast listing failed: HTTP 429"), "{}", lines[1]);
        assert!(lines[2].starts_with("+ arena8-bloom") && lines[2].contains("-> 3.3.3.3:22"), "{}", lines[2]);
        assert!(lines[3].starts_with("- james-gpu") && lines[3].contains("removed: no longer listed by runpod"), "{}", lines[3]);
        assert_eq!(plan.summary(), "+1 added, ~1 changed, -1 removed, =1 kept (stale), 0 unchanged");
        assert_eq!(plan.counts().compact(), "+1 ~1 -1 =1");
        // Unchanged lines are opt-in (the `up` poll loop only prints real changes).
        let same = plan_forwards(&cfg(), "arena8", &cands, &plan.forwards, &l);
        assert!(same.change_lines("h", false).iter().all(|l| l.starts_with('=')));
        assert!(same.change_lines("h", true).iter().any(|l| l.starts_with("  arena8-apple")));
    }

    #[test]
    fn r2_never_hands_a_forward_to_a_same_name_pod_while_its_owner_is_unconfirmed() {
        // apple's real pod is on vast (v1). Tick 1: vast 429s while runpod lists a stale
        // same-name pod r0 without an endpoint. Tick 2: r0 is gone, vast still 429s. The
        // forward must survive both — vast never confirmed v1 gone. Driven through the
        // rendered file, the way successive syncs see it. Then the same for a legacy entry
        // (no recorded owner), which only goes once *every* provider lists OK.
        let owned = Forward { pod_id: Some("v1".into()), ..fwd("arena8-apple", 7000, "ssh4.vast.ai", 31000, Some("vast")) };
        let legacy = fwd("arena8-apple", 7000, "ssh4.vast.ai", 31000, None);
        let stale = pod_on("runpod", "r0", "arena8-apple", None, None);
        let tick1 = listing(vec![ok("runpod", vec![stale]), failed("vast", "vast list HTTP 429")]);
        let tick2 = listing(vec![ok("runpod", vec![]), failed("vast", "vast list HTTP 429")]);
        for prev in [owned, legacy] {
            let file0 = render_nginx(std::slice::from_ref(&prev));
            let p1 = plan_forwards(&cfg(), "arena8", &candidates(), &parse_nginx(&file0), &tick1);
            match kind_of(&p1, "arena8-apple") {
                ChangeKind::Kept { reason, moved_from: None } => {
                    assert!(reason.starts_with("listed by runpod without endpoint; owner kept"), "{reason}")
                }
                other => panic!("tick 1: expected Kept, got {other:?}"),
            }
            assert_eq!(p1.forwards, vec![prev.clone()], "the owner must not move to runpod/r0");
            let file1 = render_nginx(&p1.forwards);
            assert_eq!(file1, file0, "nothing to write on tick 1");

            let p2 = plan_forwards(&cfg(), "arena8", &candidates(), &parse_nginx(&file1), &tick2);
            assert!(matches!(kind_of(&p2, "arena8-apple"), ChangeKind::Kept { .. }), "tick 2: {p2:?}");
            assert_eq!(p2.forwards, vec![prev.clone()]);
            assert_eq!(p2.counts().removed, 0);
        }
    }

    #[test]
    fn r2_hands_the_forward_over_once_the_previous_owner_is_confirmed_gone() {
        // (prev owner, listing) → the holder (runpod/r0, no endpoint) takes ownership.
        let stale = pod_on("runpod", "r0", "arena8-apple", None, None);
        let cases = vec![
            // vast listed OK without apple: v1 is confirmed gone.
            (Some("vast"), listing(vec![ok("runpod", vec![stale.clone()]), ok("vast", vec![])])),
            // the holder is on the owner's own provider, which listed OK.
            (Some("runpod"), listing(vec![ok("runpod", vec![stale.clone()]), failed("vast", "HTTP 429")])),
            // legacy entry and every provider answered.
            (None, listing(vec![ok("runpod", vec![stale.clone()]), ok("vast", vec![])])),
        ];
        for (owner, l) in cases {
            let prev = vec![Forward { pod_id: owner.map(|_| "old".into()), ..fwd("arena8-apple", 7000, "1.1.1.1", 22000, owner) }];
            let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &l);
            let f = &plan.forwards[0];
            assert_eq!((f.provider.as_deref(), f.pod_id.as_deref()), (Some("runpod"), Some("r0")), "owner={owner:?}");
            assert_eq!(f.target(), "1.1.1.1:22000", "the target itself is still the kept one");
            assert!(matches!(kind_of(&plan, "arena8-apple"), ChangeKind::Kept { reason, .. } if reason == "listed by runpod without endpoint"));
            assert!(plan.routed().is_empty(), "a kept forward is never 'routed'");
        }
    }

    #[test]
    fn kept_entry_whose_port_moved_is_reported_as_a_change() {
        // apple moved from index 0 to 1 while its owner (vast) is down (R4), and bloom moved
        // while listed without an endpoint (R2): both keep their targets, but the stable
        // port changed — that must read as `~`, not as a quiet `=`.
        let prev = vec![
            fwd("arena8-apple", 7000, "ssh4.vast.ai", 31000, Some("vast")),
            fwd("arena8-bloom", 7002, "3.3.3.3", 22, Some("runpod")),
        ];
        let cands: Vec<String> = vec!["bloom".into(), "apple".into()];
        let l = listing(vec![ok("runpod", vec![pod("arena8-bloom", None, None)]), failed("vast", "HTTP 429")]);
        let plan = plan_forwards(&cfg(), "arena8", &cands, &prev, &l);
        assert_eq!(
            plan.forwards.iter().map(|f| (f.name.as_str(), f.public_port)).collect::<Vec<_>>(),
            [("arena8-bloom", 7000), ("arena8-apple", 7001)]
        );
        assert!(matches!(kind_of(&plan, "arena8-apple"), ChangeKind::Kept { moved_from: Some(7000), .. }));
        assert!(matches!(kind_of(&plan, "arena8-bloom"), ChangeKind::Kept { moved_from: Some(7002), .. }));
        let lines = plan.change_lines("h", false);
        assert!(lines[0].starts_with("~ arena8-bloom") && lines[0].contains("port moved :7002 -> :7000; kept: listed by runpod"), "{}", lines[0]);
        assert!(lines[1].starts_with("~ arena8-apple") && lines[1].contains("port moved :7000 -> :7001; kept: vast listing failed"), "{}", lines[1]);
        assert_eq!(plan.counts().compact(), "+0 ~2 -0 =0");
        assert_eq!(plan.summary(), "+0 added, ~2 changed, -0 removed, =0 kept (stale), 0 unchanged");

        // An unmoved kept entry stays a quiet `=`.
        let same = plan_forwards(&cfg(), "arena8", &cands, &plan.forwards, &l);
        assert_eq!(same.counts().compact(), "+0 ~0 -0 =2");
    }

    #[test]
    fn routed_is_only_what_the_listing_confirmed() {
        let prev = vec![
            fwd("arena8-apple", 7000, "1.1.1.1", 22000, Some("runpod")),
            fwd("arena8-autumn", 7001, "ssh4.vast.ai", 31000, Some("vast")),
        ];
        let l = listing(vec![
            ok("runpod", vec![pod("arena8-apple", Some("1.1.1.1"), Some(22000)), pod("arena8-bloom", Some("3.3.3.3"), Some(22))]),
            failed("vast", "HTTP 429"),
        ]);
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &prev, &l);
        let routed: Vec<String> = plan.routed().into_iter().map(|f| f.name).collect();
        assert_eq!(routed, ["arena8-apple", "arena8-bloom"], "autumn is kept-stale, not routed");
    }

    #[test]
    fn listing_status_and_from_results() {
        let l = Listing::from_results(vec![
            ("runpod".into(), Ok(vec![pod("arena8-apple", None, None)])),
            ("vast".into(), Err(Error::provider("vast list HTTP 429"))),
        ]);
        assert_eq!(l.status("runpod"), ListingStatus::Ok);
        assert!(matches!(l.status("vast"), ListingStatus::Failed(e) if e.contains("429")));
        assert_eq!(l.status("hetzner"), ListingStatus::NotQueried);
        assert!(l.any_ok() && !l.all_ok());
        assert_eq!(l.pods().len(), 1);
        assert_eq!(l.errors().len(), 1);
        assert!(!Listing::default().any_ok() && !Listing::default().all_ok());
    }

    #[test]
    fn reload_cmd_absent_empty_or_custom() {
        let base = "SSH_PROXY_HOST=proxy.example.com\n";
        let px = |extra: &str| ProxyConfig::from_config(&Config::parse(&format!("{base}{extra}"))).unwrap();
        assert_eq!(px("").reload_cmd, DEFAULT_RELOAD_CMD);
        assert!(!px("").write_only());
        assert!(px("SSH_PROXY_RELOAD_CMD=\"\"\n").write_only());
        assert!(px("SSH_PROXY_RELOAD_CMD=\n").write_only());
        let custom = px("SSH_PROXY_RELOAD_CMD=\"sudo systemctl reload nginx\"\n");
        assert_eq!(custom.reload_cmd, "sudo systemctl reload nginx");
        assert!(!custom.write_only());
    }
}
