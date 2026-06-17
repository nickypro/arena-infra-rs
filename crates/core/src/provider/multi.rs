//! A `Provider` that fans every command out across all configured backends, so the CLI
//! and TUI present a single fleet view spanning runpod + vast + hetzner.
//!
//! - `list_pods` concatenates every backend (caching pod-id → backend so mutations route
//!   correctly). A backend that errors is skipped (optionally warned about); only a total
//!   wipeout — every backend failing — is fatal.
//! - `create_pod` goes to the chosen primary (the `--provider` / `ARENA_PROVIDER`).
//! - `stop`/`restart`/`terminate` route to whichever backend actually owns the pod id.
//!
//! With a single backend configured it behaves exactly like that one provider, so
//! single-provider setups are unaffected.

use async_trait::async_trait;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::pod::{Pod, PodSpec};
use crate::provider::{build, Provider};

/// The known backends, in the order they're listed/built.
const KNOWN: [&str; 3] = ["runpod", "vast", "hetzner"];

pub struct MultiProvider {
    backends: Vec<Box<dyn Provider>>,
    primary: usize,
    owner: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    /// When false, a partial `list_pods` failure (some backend down) is swallowed silently
    /// instead of warning on stderr — the TUI sets this so provider hiccups (e.g. Vast's
    /// frequent 429s) don't corrupt its alternate-screen rendering.
    warn_on_partial: bool,
}

impl MultiProvider {
    /// The backend that owns `id`, using the cache; on a miss, refresh via a full list.
    async fn backend_for(&self, id: &str) -> Result<&dyn Provider> {
        if let Some(i) = self.owner.lock().unwrap().get(id).copied() {
            return Ok(self.backends[i].as_ref());
        }
        self.list_pods().await?; // cold cache — populate, then look up
        let idx = self.owner.lock().unwrap().get(id).copied();
        match idx {
            Some(i) => Ok(self.backends[i].as_ref()),
            None => Err(Error::provider(format!("no configured provider owns pod id '{id}'"))),
        }
    }
}

#[async_trait]
impl Provider for MultiProvider {
    fn name(&self) -> &'static str {
        self.backends[self.primary].name()
    }
    fn describe(&self, spec: &PodSpec) -> String {
        self.backends[self.primary].describe(spec)
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        // Gather first (awaiting), then populate the cache without holding the lock across
        // an await. A provider that errors is skipped; only a total wipeout is fatal.
        let mut gathered: Vec<(usize, Vec<Pod>)> = Vec::new();
        let mut errs: Vec<String> = Vec::new();
        for (i, b) in self.backends.iter().enumerate() {
            match b.list_pods().await {
                Ok(pods) => gathered.push((i, pods)),
                Err(e) => errs.push(format!("{}: {e}", b.name())),
            }
        }
        if gathered.is_empty() && !errs.is_empty() {
            return Err(Error::provider(format!(
                "all providers failed to list: {}",
                errs.join("; ")
            )));
        }
        if !errs.is_empty() && self.warn_on_partial {
            eprintln!("warning: some providers failed to list ({})", errs.join("; "));
        }
        let mut map = self.owner.lock().unwrap();
        map.clear();
        let mut out = Vec::new();
        for (i, pods) in gathered {
            for p in &pods {
                map.insert(p.id.clone(), i);
            }
            out.extend(pods);
        }
        Ok(out)
    }
    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        // Creation needs a concrete target: always the chosen primary.
        let pod = self.backends[self.primary].create_pod(spec).await?;
        self.owner.lock().unwrap().insert(pod.id.clone(), self.primary);
        Ok(pod)
    }
    async fn stop_pod(&self, id: &str) -> Result<()> {
        self.backend_for(id).await?.stop_pod(id).await
    }
    async fn restart_pod(&self, id: &str) -> Result<()> {
        self.backend_for(id).await?.restart_pod(id).await
    }
    async fn terminate_pod(&self, id: &str) -> Result<()> {
        self.backend_for(id).await?.terminate_pod(id).await
    }
}

/// Build the fleet-wide provider: the chosen `primary` (required, for create) plus every
/// *other* backend that has credentials in config (best-effort). One configured backend →
/// just that provider. `warn_on_partial` controls whether a partial list failure prints a
/// stderr warning (CLI: true; TUI: false, to keep its alternate screen clean).
pub fn build_fleet(primary: &str, cfg: &Config, warn_on_partial: bool) -> Result<Box<dyn Provider>> {
    if !KNOWN.contains(&primary) {
        return Err(Error::Config(format!(
            "unknown provider `{primary}` (known: {})",
            KNOWN.join(", ")
        )));
    }
    let mut backends: Vec<Box<dyn Provider>> = Vec::new();
    let mut primary_idx = None;
    for name in KNOWN {
        let built = if name == primary {
            Some(build(name, cfg)?) // primary's creds are required
        } else {
            build(name, cfg).ok() // others: include only if configured
        };
        if let Some(p) = built {
            if name == primary {
                primary_idx = Some(backends.len());
            }
            backends.push(p);
        }
    }
    Ok(Box::new(MultiProvider {
        backends,
        primary: primary_idx.expect("primary is built or we bailed above"),
        owner: std::sync::Mutex::new(std::collections::HashMap::new()),
        warn_on_partial,
    }))
}
