//! `pods test` (torch, or `--deep` GPU checks) and the health cache it writes for `snapshot`, `up`
//! and `config check`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use arena_core::health::judge_deep_call;
use arena_core::provider::Provider;
use arena_core::remote::Remote;
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::disk;
use crate::pods::list::{enrich_best_effort, ENRICH_TIMEOUT};
use crate::pods::selection::{stopped_skip, Selected};
use crate::pods::{exec_each_pod, failure_line, PodCall};

/// `pods test --deep`: run the deep check on the pods (see [`deep_check_fleet`]), print
/// the pass/warn/fail table (or JSON), and fail the command if any pod FAILed — so a
/// script or `up --check` can gate on it. Warnings (slow network, maintenance, …) are
/// reported but don't change the exit status.
pub(crate) async fn handle_deep_test(
    provider: &dyn Provider,
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    json: bool,
    verbose: bool,
) -> Result<()> {
    use arena_core::health::Status;
    let results = deep_check_fleet(provider, &remote, cfg, sel).await?;
    let (stdout, stderr) = render_deep_test(&results, json, verbose)?;
    print!("{stdout}");
    for line in stderr {
        eprintln!("{line}");
    }
    // For `arena snapshot` (and the dashboard): remember each verdict with its time. One more
    // read-only list (each backend bounded) lets the cache drop pods since terminated.
    if !results.is_empty() && health_cache_path(cfg).is_some() {
        let listing = arena_core::proxy::Listing::from_results(provider.list_by_provider().await);
        if let Some(warning) = record_health(cfg, &results, Some(&listing)) {
            eprintln!("{warning}");
        }
    }
    let failed = results.iter().filter(|h| h.status == Status::Fail).count();
    // Over the cohort (what the daily health line runs, nothing named): this box too — how full
    // its disk is, what arena left on it, and the copies skipped for space in the last day, each
    // a failure. After the table (on stderr under --json, whose stdout stays the array).
    let box_health = if sel.named { None } else { disk::health(cfg, arena_core::snapshot::unix_now()) };
    if let Some(h) = &box_health {
        for line in &h.lines {
            if json {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    }
    let (skipped, invalid) = box_health.map_or((0, None), |h| (h.skipped, h.invalid));
    let no_room = match ((skipped > 0).then(|| format!("copies of {skipped} pod(s) onto this box were skipped for space in the last day")), invalid) {
        (Some(r), Some(e)) => Some(format!("{r}; {e}")),
        (r, e) => r.or(e),
    };
    match (failed, no_room) {
        (0, None) => Ok(()),
        (0, Some(r)) => anyhow::bail!("{r}"),
        (n, None) => anyhow::bail!("{n} pod(s) failed the deep check"),
        (n, Some(r)) => anyhow::bail!("{n} pod(s) failed the deep check; {r}"),
    }
}

/// Where this fleet's health cache lives ([`arena_core::snapshot::health_cache_path`]):
/// under `ARENA_STATE_DIR` (config.env or the environment), else the XDG state dir — per
/// `MACHINE_NAME_PREFIX`. `None` in this crate's unit tests unless the test's config sets
/// `ARENA_STATE_DIR`: a test must never read or write the developer's real
/// `~/.local/state`.
pub(crate) fn health_cache_path(cfg: &Config) -> Option<std::result::Result<PathBuf, String>> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    Some(arena_core::snapshot::health_cache_path_for(cfg))
}

/// Record deep-check verdicts in the health cache that `arena snapshot` reads, dropping the
/// pods `listing` confirms gone. Best effort: a warning line, never an error — the verdicts
/// were already printed and decided the exit status; the cache only feeds the snapshot.
pub(crate) fn record_health(
    cfg: &Config,
    results: &[arena_core::health::PodHealth],
    listing: Option<&arena_core::proxy::Listing>,
) -> Option<String> {
    let path = match health_cache_path(cfg)? {
        Ok(p) => p,
        Err(e) => return Some(format!("warning: deep-check results not cached for `arena snapshot`: {e}")),
    };
    match arena_core::snapshot::record_health(&path, results, listing, arena_core::snapshot::unix_now()) {
        Ok(warning) => warning,
        Err(e) => Some(format!("warning: couldn't write the health cache {}: {e}", path.display())),
    }
}

/// What `pods test --deep` prints: (stdout, stderr lines). With `json`, stdout is always
/// the JSON array — `[]` when no pod could be checked (an empty or still-booting fleet is
/// exactly when a `| jq` or `up --check` consumer must not get prose) — and the human
/// summary goes to stderr. Pure, so that contract is tested without capturing stdout.
pub(crate) fn render_deep_test(
    results: &[arena_core::health::PodHealth],
    json: bool,
    verbose: bool,
) -> Result<(String, Vec<String>)> {
    use arena_core::health::{render_report, render_summary};
    const NONE: &str = "(no pods with an SSH endpoint)";
    Ok(match (json, results.is_empty()) {
        (true, true) => ("[]\n".into(), vec![NONE.into()]),
        (true, false) => (format!("{}\n", serde_json::to_string_pretty(results)?), render_summary(results)),
        (false, true) => (format!("{NONE}\n"), vec![]),
        (false, false) => (render_report(results, verbose), vec![]),
    })
}

/// Deep-check the selected pods concurrently over `remote`, one exec each bounded by
/// [`arena_core::health::DEEP_CHECK_TIMEOUT`]; results sorted by name. A pod the operator
/// named that has no SSH endpoint is a FAIL (it was asked about and couldn't be checked);
/// one swept in by the default/`--all` is skipped with a note, like `pods test`. An
/// unreachable or timed-out pod is a FAIL with the reason. The provider's maintenance
/// windows are fetched alongside the SSH runs (best-effort, bounded) — it's an API-side
/// fact the pod can't report.
///
/// Results are matched back to pods by position, not name: two pods can share a name (a
/// double create, a leftover), and each must get its own row and verdict — keyed by name,
/// one would take the other's result and the other would vanish from the report (and from
/// the exit status). A pod that was probed but has no result is a FAIL, never dropped.
pub(crate) async fn deep_check_fleet(
    provider: &dyn Provider,
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
) -> Result<Vec<arena_core::health::PodHealth>> {
    use arena_core::health::{deep_check_command, HealthPolicy, PodHealth, DEEP_CHECK_TIMEOUT};
    // Config first: a malformed MIN_DRIVER_VERSION fails before any pod is touched.
    let policy = HealthPolicy::from_config(cfg)?;
    let mut pods = sel.pods.clone();
    pods.sort_by(|a, b| a.name.cmp(&b.name));
    for (name, ids) in duplicate_names(&pods) {
        eprintln!(
            "warning: {} pods are named {name} (ids {}) — each is checked and reported separately",
            ids.len(),
            ids.join(", ")
        );
    }

    /// What happens to each pod (by its index in `pods`).
    enum Slot {
        /// No endpoint and not asked for by name: left out, with a note.
        Skipped,
        /// Asked for by name but has no endpoint: a FAIL.
        NoEndpoint,
        Probed,
    }
    // `CONDA_ENV=""` disables activation (like `pods run`); python is then whatever the
    // rc puts on PATH.
    let cmd = deep_check_command(Some(cfg.get("CONDA_ENV").unwrap_or("arena-env")));
    let mut jobs = Vec::new();
    let mut slots = Vec::with_capacity(pods.len());
    for (i, pod) in pods.iter().enumerate() {
        // A stopped pod's listed endpoint (if any) belongs to nothing: never probed.
        let target = if arena_core::status::is_stopped(&pod.status) { None } else { SshTarget::from_pod(pod, cfg).ok() };
        slots.push(match target {
            Some(t) => {
                jobs.push(((i, pod.name.clone()), t, cmd.clone()));
                Slot::Probed
            }
            None if sel.named => Slot::NoEndpoint,
            None if arena_core::status::is_stopped(&pod.status) => {
                eprintln!("{}", stopped_skip(pod));
                Slot::Skipped
            }
            None => {
                let why = arena_core::fleet::no_endpoint_reason(pod).unwrap_or_else(|| "no SSH endpoint yet".into());
                eprintln!("skip {} — {why}", pod.name);
                Slot::Skipped
            }
        });
    }
    if !jobs.is_empty() {
        eprintln!(
            "Deep-checking {} pod(s) ({}s budget each)…",
            jobs.len(),
            DEEP_CHECK_TIMEOUT.as_secs()
        );
    }
    let mut calls: std::collections::HashMap<usize, PodCall> = std::collections::HashMap::new();
    let probe = exec_each_pod(remote, jobs, DEEP_CHECK_TIMEOUT, |done, total, (i, name), call| {
        match &call {
            Ok(out) if out.success => eprintln!("[{done}/{total}] {name}: checked"),
            failed => eprintln!("{}", failure_line(done, total, name, failed)),
        }
        calls.insert(*i, call);
    });
    // `enrich` fills fields in place (a slice: it can't add, drop or move pods), so the
    // indices the jobs carry still point at the same pods afterwards.
    let (warning, ()) = tokio::join!(enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT), probe);
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }

    let mut results = Vec::new();
    for (i, pod) in pods.iter().enumerate() {
        match slots[i] {
            Slot::Skipped => continue,
            Slot::NoEndpoint if arena_core::status::is_stopped(&pod.status) => {
                let why = format!("stopped ({}) — `arena pods start {}` brings it back", pod.status, pod.name);
                results.push(PodHealth::unreachable(pod, why));
                continue;
            }
            Slot::NoEndpoint => {
                let why = arena_core::fleet::no_endpoint_reason(pod)
                    .unwrap_or_else(|| format!("no SSH endpoint yet (status {})", pod.status));
                results.push(PodHealth::unreachable(pod, why));
                continue;
            }
            Slot::Probed => {}
        }
        let Some(call) = calls.remove(&i) else {
            results.push(PodHealth::unreachable(pod, "no result from the check"));
            continue;
        };
        results.push(judge_deep_call(pod, call, &policy));
    }
    Ok(results)
}

/// Names held by more than one pod, with those pods' ids (in listing order) — for the
/// warning that the report will show the name twice. Sorted by name.
pub(crate) fn duplicate_names(pods: &[arena_core::Pod]) -> Vec<(String, Vec<String>)> {
    let mut by_name: std::collections::BTreeMap<&str, Vec<String>> = std::collections::BTreeMap::new();
    for p in pods {
        by_name.entry(&p.name).or_default().push(p.id.clone());
    }
    by_name.into_iter().filter(|(_, ids)| ids.len() > 1).map(|(n, ids)| (n.to_string(), ids)).collect()
}
