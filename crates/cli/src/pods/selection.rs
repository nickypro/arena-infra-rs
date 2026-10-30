//! Which pods a fleet command acts on: the selector over the listing, the cohort scope, fresh
//! endpoints, and `say` (a line the operator reads, kept under test).

use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::selector::{Naming, SelectArgs, Selector};
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::cli::CohortScope;
use crate::pods::list::{enrich_best_effort, ENRICH_TIMEOUT};

/// Resolve a user-supplied target (machine name, bare short name, `@name` OR raw provider
/// id) by searching EVERY configured provider, returning the owning provider too — so
/// `restart`/`replace`/`migrate <name>` work whatever backend the pod lives on, without
/// passing `--provider`. The mutation must go to the owning provider's API. Matched with
/// the shared selector's single-pod rule (`arena_core::selector::resolve_one`): a typo
/// fails with the closest names, and a name two pods share is refused rather than taking
/// whichever listed first. Best-effort listing; a provider that errors is warned about and
/// skipped.
pub(crate) async fn resolve_target_any(
    cfg: &Config,
    target: &str,
) -> Result<(Box<dyn Provider>, String, String)> {
    let policy = arena_core::retry::RetryPolicy::default();
    let mut backends: Vec<(&str, Box<dyn Provider>)> = Vec::new();
    let mut pods: Vec<arena_core::Pod> = Vec::new();
    let mut owner_of: Vec<usize> = Vec::new(); // pods[i] belongs to backends[owner_of[i]]
    for name in ["runpod", "vast", "hetzner", "shadeform"] {
        let Ok(p) = arena_core::provider::build(name, cfg) else { continue };
        match arena_core::retry::retrying(&policy, || p.list_pods()).await {
            Ok(listed) => {
                owner_of.extend(std::iter::repeat_n(backends.len(), listed.len()));
                pods.extend(listed);
                backends.push((name, p));
            }
            Err(e) => eprintln!("warning: couldn't list {name} pods ({e})"),
        }
    }
    let i = arena_core::selector::resolve_one(&Naming::from_config(cfg), &pods, target)?;
    let (name, _) = &backends[owner_of[i]];
    let label = format!("{} (id={}, {name})", pods[i].name, pods[i].id);
    let id = pods[i].id.clone();
    let (_, owner) = backends.swap_remove(owner_of[i]);
    Ok((owner, id, label))
}

/// The line an SSH command prints for a stopped pod it can't reach.
pub(crate) fn stopped_skip(pod: &arena_core::Pod) -> String {
    format!("skip {} — stopped ({}); `arena pods start {}` brings it back", pod.name, pod.status, pod.name)
}

/// The error an SSH command ends with when every pod it was told to reach is stopped —
/// one wording for all of them (live R: `copy-keys` said "has an SSH endpoint yet" instead).
pub(crate) fn all_stopped_error(names: &[&str]) -> String {
    format!("the selected pod(s) are stopped ({}) — `arena pods start {}` first", names.join(", "), names.join(" "))
}

/// How long `run` / `pull` / `backup` wait before asking once more for a running pod the
/// listing showed without an SSH endpoint ([`refresh_missing_endpoints`]).
pub(crate) const ENDPOINT_RETRY: Duration = Duration::from_secs(20);

/// A RUNNING pod listed without an SSH endpoint is usually a passing glitch: RunPod v1 once
/// listed one that way for ~20 s, and `pull` skipped it and exited 1 — so a backup cron tick
/// silently missed that pod. Ask once more, `wait` later, and fill in the endpoints that
/// came back (by id). Only RUNNING pods: one still provisioning or stopped has no endpoint
/// for a reason, and isn't worth the wait. Best-effort: a failed re-list changes nothing.
pub(crate) async fn refresh_missing_endpoints(provider: &dyn Provider, sel: &mut Selected, wait: Duration) {
    let missing = |p: &arena_core::Pod| p.status.eq_ignore_ascii_case("RUNNING") && (p.ssh_ip.is_none() || p.ssh_port.is_none());
    let names: Vec<String> = sel.pods.iter().filter(|p| missing(p)).map(|p| p.name.clone()).collect();
    if names.is_empty() {
        return;
    }
    eprintln!("{} listed running without an SSH endpoint — asking again in {}s…", names.join(", "), wait.as_secs());
    tokio::time::sleep(wait).await;
    let Ok(Ok(fresh)) = arena_core::ratelimit::bounded(arena_core::provider::LIST_TIMEOUT, provider.list_pods()).await else {
        return;
    };
    for pod in sel.pods.iter_mut().filter(|p| missing(p)) {
        if let Some(now) = fresh.iter().find(|f| f.id == pod.id && f.ssh_ip.is_some() && f.ssh_port.is_some()) {
            pod.ssh_ip = now.ssh_ip.clone();
            pod.ssh_port = now.ssh_port;
            eprintln!("  {} has an endpoint now", pod.name);
        }
    }
}

/// What a fleet command does when the operator names no pods (no targets, no `--all`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Unscoped {
    /// The whole fleet: read-only checks, and the commands that always defaulted to it
    /// (`run`, `test`, `setup`, `copy-keys`, `cp`, `backup`, `pull`, `init-branches` — `test`
    /// and the last three, like `set-branch --all`, then keep only this cohort's pods:
    /// [`cohort_only`]).
    All,
    /// Refuse: a command that changes pods (`stop`, `reimage`, `set-branch`, `keys`) must
    /// be told which — names or `--all` (the verb goes in the message).
    Refuse(&'static str),
}

/// The pods a fleet command acts on, resolved once at the command layer, and whether the
/// operator *named* them. Named matters downstream: a named pod that can't be reached is a
/// failure to report, while one swept in by `--all` (or the default) is skipped with a note.
#[derive(Debug, Clone, Default)]
pub(crate) struct Selected {
    pub pods: Vec<arena_core::Pod>,
    pub named: bool,
}

impl Selected {
    /// The whole given list, as if nothing was named (what an unscoped selection of that
    /// fleet resolves to) — for tests that drive a handler directly.
    #[cfg(test)]
    pub(crate) fn all(pods: Vec<arena_core::Pod>) -> Selected {
        Selected { pods, named: false }
    }

    /// `name, name, …` — for prompts and dry-runs, so the operator sees what will run.
    pub(crate) fn names(&self) -> String {
        self.pods.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
    }

    /// Every selected pod with an SSH endpoint, with its target, in selection order. A pod
    /// without one is reported (`skip <name> — no SSH endpoint yet`), never silently dropped;
    /// when the operator named pods and *none* is reachable, that's an error — not a quiet
    /// "nothing to do" that reads like success. A STOPPED pod is skipped saying so, with the
    /// way back (`pods start`), even if the listing still shows its last endpoint — that
    /// belongs to nothing now (live finding #15: "no SSH endpoint yet" read like a pod
    /// still booting).
    pub(crate) fn ssh_targets(&self, cfg: &Config) -> Result<Vec<(&arena_core::Pod, SshTarget)>> {
        let mut targets = Vec::new();
        let mut stopped = Vec::new();
        for pod in &self.pods {
            if arena_core::status::is_stopped(&pod.status) {
                eprintln!("{}", stopped_skip(pod));
                stopped.push(pod.name.as_str());
                continue;
            }
            match SshTarget::from_pod(pod, cfg) {
                Ok(t) => targets.push((pod, t)),
                // A stopped or proxy-only pod says so, not "yet" (it won't get one by waiting).
                Err(_) => eprintln!(
                    "{} {} — {}",
                    if arena_core::status::endpoint_overdue(pod, arena_core::status::now_secs()) { "⚠ FAILED" } else { "skip" },
                    pod.name,
                    arena_core::fleet::no_endpoint_reason(pod).unwrap_or_else(|| "no SSH endpoint yet".into())
                ),
            }
        }
        if targets.is_empty() && self.named {
            if stopped.len() == self.pods.len() {
                anyhow::bail!("{}", all_stopped_error(&stopped));
            }
            anyhow::bail!("none of the selected pods ({}) has an SSH endpoint yet — check `arena pods list`", self.names());
        }
        Ok(targets)
    }

    /// Count running pods missing SSH beyond the boot grace as failed work, while still
    /// allowing the rest of a fleet to be backed up, copied or run on.
    pub(crate) fn endpoint_failures(&self) -> Vec<String> {
        let now = arena_core::status::now_secs();
        self.pods.iter().filter(|p| arena_core::status::endpoint_overdue(p, now))
            .map(|p| p.name.clone()).collect()
    }

    /// [`ssh_targets`](Self::ssh_targets) keyed by pod name — what the per-pod runners take.
    pub(crate) fn named_targets(&self, cfg: &Config) -> Result<Vec<(String, SshTarget)>> {
        Ok(self.ssh_targets(cfg)?.into_iter().map(|(p, t)| (p.name.clone(), t)).collect())
    }
}

/// Resolve the operator's selection (PLAN 1.C): validate it (`arena_core::selector`: token
/// syntax, ranges against MACHINE_NAME_LIST, `--on`) *before* listing, refuse an unscoped
/// selection where the command needs one, list the fleet (retrying transient failures),
/// then resolve — a typo or a narrowed-to-nothing selection is an error before anything is
/// touched. With `--gpus`, the provider's details are fetched first (best-effort, bounded):
/// a listing may omit GPU counts, and an unknown count never matches.
pub(crate) async fn select(provider: &dyn Provider, cfg: &Config, sel: &SelectArgs, unscoped: Unscoped) -> Result<Selected> {
    let naming = Naming::from_config(cfg);
    let selector = Selector::parse(sel, &naming)?;
    if let (true, Unscoped::Refuse(verb)) = (selector.is_unscoped(), unscoped) {
        anyhow::bail!("name the pods to {verb} (names, ids or a range like apple..mayor), or pass --all");
    }
    let policy = arena_core::retry::RetryPolicy::default();
    let mut pods = arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?;
    if selector.gpus.is_some() {
        if let Some(warning) = enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT).await {
            eprintln!("{warning}");
        }
    }
    let picked = selector.resolve(&naming, &pods)?;
    Ok(Selected { pods: picked.into_iter().map(|i| pods[i].clone()).collect(), named: sel.names_pods() })
}

/// Narrow a fleet-wide selection of the git backup commands (`pods backup`, `pull`,
/// `init-branches`, `set-branch`) to this cohort's machines, saying in one line who was left
/// out ([`split_cohort`], [`non_cohort_line`]). A selection that named pods comes back as it was,
/// and so does every selection with `--include-non-cohort` (nothing left out, nothing said).
pub(crate) fn cohort_only(cfg: &Config, sel: Selected, scope: CohortScope) -> Selected {
    if scope.include_non_cohort {
        return sel;
    }
    let naming = Naming::from_config(cfg);
    let (kept, left_out) = split_cohort(&naming, sel);
    if let Some(line) = non_cohort_line(naming.prefix, &left_out) {
        say(&line);
    }
    kept
}

/// `set-branch --hard --include-non-cohort` with no pod named: refused, before any pod is listed
/// (review ai-review claude #5). The flag takes every pod on the account — staff boxes, other
/// cohorts' pods, a pod the provider named — which is harmless for a `pull`, but `--hard` resets
/// each one, discarding its uncommitted work. Naming the pods (the flag then changes nothing:
/// named pods are acted on exactly) is how to reset one of them. `None` otherwise. Pure.
pub(crate) fn hard_non_cohort_refusal(hard: bool, scope: CohortScope, names_pods: bool) -> Option<String> {
    (hard && scope.include_non_cohort && !names_pods).then(|| {
        "set-branch --hard with --include-non-cohort refused: that takes every pod on the account, so --hard would reset staff \
         boxes and other cohorts' pods too, discarding their uncommitted work — name the pods to reset \
         (`arena pods set-branch <branch> <pod>... --hard`), or leave --include-non-cohort out to reset this cohort's"
            .to_string()
    })
}

/// What a fleet-wide backup command acts on — one that names no pod (no targets, `all` or
/// `--all`): only this cohort's machines ([`Naming::is_cohort`]: `{prefix}-…`, never an `@`
/// staff entry), plus the names it left out. The account also holds staff and personal boxes
/// (an `@` entry, another prefix, a pod RunPod named itself) with no checkout or no deploy key:
/// every */15 backup failed or skipped on them and buried the real failures, and
/// `set-branch main --all --hard` would reset one that has a checkout. A `{prefix}-…` pod off
/// `MACHINE_NAME_LIST` stays in: it's an extra pod made for someone, or a parked `-old`/`-new`
/// twin, and dropping a participant's backup is the worse mistake (a staff box that wears the
/// prefix is listed as `@{prefix}-name` to leave it out). Named pods (names, ids, ranges) are
/// acted on exactly, staff boxes included. Pure.
pub(crate) fn split_cohort(naming: &Naming, sel: Selected) -> (Selected, Vec<String>) {
    if sel.named {
        return (sel, Vec::new());
    }
    let (pods, out): (Vec<_>, Vec<_>) = sel.pods.into_iter().partition(|p| naming.is_cohort(&p.name));
    let out = out.into_iter().map(|p| if p.name.is_empty() { p.id } else { p.name }).collect();
    (Selected { pods, ..sel }, out)
}

/// The one line for the pods [`split_cohort`] left out — neither a failure nor counted in any
/// other line, so a real failure stands out. `None` when there were none.
pub(crate) fn non_cohort_line(prefix: &str, names: &[String]) -> Option<String> {
    const SHOWN: usize = 6;
    if names.is_empty() {
        return None;
    }
    let mut list = names.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > SHOWN {
        list.push_str(&format!(" (+{} more)", names.len() - SHOWN));
    }
    let n = names.len();
    Some(format!(
        "⊘ skipped {n} non-cohort pod{} (not {prefix}-…, or an @ staff entry): {list} — name one to include it, or pass --include-non-cohort",
        if n == 1 { "" } else { "s" }
    ))
}

/// Print a line the operator reads (a summary, a reminder, a per-pod result). Under test it's
/// also kept, so a scenario test can see what was said ([`take_said`]).
pub(crate) fn say(line: &str) {
    #[cfg(test)]
    SAID.with(|n| n.borrow_mut().push(line.to_string()));
    println!("{line}");
}

#[cfg(test)]
thread_local! {
    static SAID: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Under test: the [`say`] lines printed on this thread so far (taken). A `#[tokio::test]`
/// runs the command on its own thread, so these are that test's.
#[cfg(test)]
pub(crate) fn take_said() -> Vec<String> {
    SAID.with(|n| std::mem::take(&mut *n.borrow_mut()))
}

/// A fresh listing narrowed to the pods with these `ids` — ones the program itself just
/// made (`up`, `replace`, `migrate copy`) and is about to set up. Not operator input, so no
/// typo check: a pod missing from the listing is warned about and left out (the old
/// `setup --only` behaviour); it's an error only if none is left.
pub(crate) async fn just_these(provider: &dyn Provider, ids: &[String]) -> Result<Selected> {
    let listed = provider.list_pods().await.context("listing pods")?;
    let pods: Vec<arena_core::Pod> = listed.into_iter().filter(|p| ids.contains(&p.id)).collect();
    let missing: Vec<&str> = ids.iter().map(String::as_str).filter(|id| !pods.iter().any(|p| p.id == *id)).collect();
    if !missing.is_empty() {
        eprintln!("warning: not in the listing (gone?): id {}", missing.join(", "));
    }
    if pods.is_empty() {
        anyhow::bail!("none of the new pods (id {}) is in the listing — check `arena pods list`", ids.join(", "));
    }
    Ok(Selected { pods, named: true })
}
