//! A `Provider` that fans every command out across all configured backends, so the CLI
//! and TUI present a single fleet view spanning runpod + vast + hetzner.
//!
//! - `list_pods` concatenates every backend (caching pod-id → backend so mutations route
//!   correctly). A backend that errors is skipped (optionally warned about); only a total
//!   wipeout — every backend failing — is fatal.
//! - `create_pod` goes to the chosen primary (the `--provider` / `ARENA_PROVIDER`).
//! - `stop`/`restart`/`terminate` route to whichever backend actually owns the pod id.
//! - `enrich` hands each pod to the backend that listed it; a failing backend is reported
//!   (as one error naming it) without costing the other backends' pods their details.
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
    async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
        // Route each pod to the backend that listed it: by its `provider` tag first (set by
        // that backend, so unambiguous even if two backends' numeric ids collide), else the
        // id→backend cache. Resolve under the lock, then release it before any await.
        let owners: Vec<Option<usize>> = {
            let map = self.owner.lock().unwrap();
            pods.iter()
                .map(|p| {
                    self.backends
                        .iter()
                        .position(|b| b.name() == p.provider)
                        .or_else(|| map.get(&p.id).copied())
                })
                .collect()
        };
        // Backends are independent: one failing (rate limit, outage) must not cost the
        // others their details, so try them all and report the failures together.
        let mut errs: Vec<String> = Vec::new();
        for (i, b) in self.backends.iter().enumerate() {
            let idx: Vec<usize> = (0..pods.len()).filter(|&j| owners[j] == Some(i)).collect();
            if idx.is_empty() {
                continue; // no API call for a backend with nothing to enrich
            }
            let mut mine: Vec<Pod> = idx.iter().map(|&j| pods[j].clone()).collect();
            let r = b.enrich(&mut mine).await;
            // Write back even on Err: whatever the backend filled before failing is valid.
            for (j, p) in idx.into_iter().zip(mine) {
                pods[j] = p;
            }
            if let Err(e) = r {
                errs.push(format!("{}: {e}", b.name()));
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(Error::provider(format!("pod details unavailable ({})", errs.join("; "))))
        }
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
    async fn rename_pod(&self, id: &str, new_name: &str) -> Result<()> {
        self.backend_for(id).await?.rename_pod(id, new_name).await
    }
    async fn reimage_pod(&self, id: &str, image: &str, env: &[(String, String)]) -> Result<()> {
        self.backend_for(id).await?.reimage_pod(id, image, env).await
    }
    async fn pod_spec(&self, id: &str) -> Result<PodSpec> {
        self.backend_for(id).await?.pod_spec(id).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// A backend that records which pod ids it was asked to enrich and stamps a marker
    /// `gpu_count` on them (then optionally fails, to model a rate-limited provider).
    struct FakeBackend {
        name: &'static str,
        marker: u32,
        fail: bool,
        seen: Arc<Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait]
    impl Provider for FakeBackend {
        fn name(&self) -> &'static str {
            self.name
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            Ok(Vec::new())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
            unimplemented!("not exercised by enrich tests")
        }
        async fn stop_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn restart_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn terminate_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
            self.seen.lock().unwrap().push(pods.iter().map(|p| p.id.clone()).collect());
            for p in pods.iter_mut() {
                p.gpu_count = Some(self.marker);
            }
            if self.fail {
                return Err(Error::provider("HTTP 429"));
            }
            Ok(())
        }
    }

    fn backend(name: &'static str, marker: u32, fail: bool) -> (Box<dyn Provider>, Arc<Mutex<Vec<Vec<String>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (Box::new(FakeBackend { name, marker, fail, seen: seen.clone() }), seen)
    }

    fn multi(backends: Vec<Box<dyn Provider>>, owner: HashMap<String, usize>) -> MultiProvider {
        MultiProvider { backends, primary: 0, owner: Mutex::new(owner), warn_on_partial: false }
    }

    fn pod(id: &str, provider: &str) -> Pod {
        Pod { id: id.into(), name: format!("devtest-{id}"), provider: provider.into(), ..Default::default() }
    }

    #[tokio::test]
    async fn enrich_routes_each_pod_to_its_backend() {
        let (rp, rp_seen) = backend("runpod", 1, false);
        let (va, va_seen) = backend("vast", 2, false);
        let (hz, hz_seen) = backend("hetzner", 3, false);
        let m = multi(vec![rp, va, hz], HashMap::new());
        let mut pods = vec![pod("r1", "runpod"), pod("v1", "vast"), pod("r2", "runpod"), pod("x1", "lambda")];
        m.enrich(&mut pods).await.unwrap();
        // One batched call per backend, with only its own pods, in list order.
        assert_eq!(*rp_seen.lock().unwrap(), vec![vec!["r1".to_string(), "r2".to_string()]]);
        assert_eq!(*va_seen.lock().unwrap(), vec![vec!["v1".to_string()]]);
        // A backend with no pods is not called at all (no wasted API request).
        assert!(hz_seen.lock().unwrap().is_empty());
        let counts: Vec<Option<u32>> = pods.iter().map(|p| p.gpu_count).collect();
        // Results land on the right pods; a pod no backend owns is left untouched.
        assert_eq!(counts, vec![Some(1), Some(2), Some(1), None]);
    }

    #[tokio::test]
    async fn enrich_falls_back_to_owner_cache_when_tag_is_unknown() {
        let (rp, rp_seen) = backend("runpod", 1, false);
        let owner = HashMap::from([("odd".to_string(), 0usize)]);
        let m = multi(vec![rp], owner);
        let mut pods = vec![pod("odd", "")];
        m.enrich(&mut pods).await.unwrap();
        assert_eq!(*rp_seen.lock().unwrap(), vec![vec!["odd".to_string()]]);
        assert_eq!(pods[0].gpu_count, Some(1));
    }

    #[tokio::test]
    async fn enrich_failure_is_reported_but_other_backends_still_fill() {
        let (rp, _) = backend("runpod", 1, true); // e.g. rate-limited
        let (va, _) = backend("vast", 2, false);
        let m = multi(vec![rp, va], HashMap::new());
        let mut pods = vec![pod("r1", "runpod"), pod("v1", "vast")];
        let err = m.enrich(&mut pods).await.unwrap_err().to_string();
        assert!(err.contains("runpod: provider error: HTTP 429"), "{err}");
        assert!(!err.contains("vast"), "{err}");
        // vast's details still applied; runpod's partial fill is kept too.
        assert_eq!(pods[1].gpu_count, Some(2));
        assert_eq!(pods[0].gpu_count, Some(1));
    }
}
