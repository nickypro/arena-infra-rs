//! `pods setup`: provision pods over SSH (keys, repo, tokens, VS Code; bare VMs run
//! hetzner_setup.sh), each step within its budget.
//!
//! What it prints (docs/OUTPUT.md): by default a line per pod as it ends — `[NAME] setup ✓
//! (3m02s)` or `[NAME] FAILED setup: <why> (retry: arena pods setup NAME)` — which the
//! dashboard's runs panel reads as the pod's state (a `pods start`'s setup); a log also gets
//! `[NAME] setup…` as each starts, and a terminal an in-place line instead. `-v` prints what it
//! printed before: the plan, the budgets, `[done/total] ✓ NAME (…)`, the `Done:` tally.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::out;
use arena_core::out::To;
use arena_core::provider::Provider;
use arena_core::setup::ProvisionOutcome;
use arena_core::Config;

use crate::keys::handle_copy_keys;
use crate::pods::selection::Selected;
use crate::{archive, reap};

/// What a fleet run is called in its lines (`setup`, `notes`) and the command that runs it
/// again on one pod: the fix each failure line ends with.
#[derive(Debug, Clone, Copy)]
struct Says {
    what: &'static str,
    retry: &'static str,
}

/// `pods setup`'s lines.
const SETUP: Says = Says { what: "setup", retry: "arena pods setup" };
/// `pods setup --notes-only`'s lines.
const NOTES: Says = Says { what: "notes", retry: "arena pods setup --notes-only" };

/// The key of a fleet run's in-place line on a terminal ([`out::progress`]).
const PROGRESS: &str = "setup";

/// Up to three names, bold, then how many more: `devtest-a, devtest-b, devtest-c +2 more` — the
/// pods a one-line prompt or plan acts on. Pure (but for the bold).
pub(crate) fn some_names<S: AsRef<str>>(names: &[S]) -> String {
    const SHOWN: usize = 3;
    let shown = names.iter().take(SHOWN).map(|n| out::bold(n.as_ref())).collect::<Vec<_>>().join(", ");
    if names.len() > SHOWN {
        format!("{} pods: {shown} +{} more", names.len(), names.len() - SHOWN)
    } else {
        shown
    }
}

/// A pod's default line once its run ends: `[NAME] setup ✓ (3m02s)`, or `[NAME] FAILED setup:
/// <why> (retry: arena pods setup NAME)` — the dashboard's runs panel reads `[NAME] FAILED…` as a
/// failed pod and any other `[NAME] …` as its step. Pure (but for the styles).
fn ended_line(says: Says, name: &str, outcome: &ProvisionOutcome, took: Duration) -> String {
    if outcome.is_done() {
        format!("[{}] {} {} ({})", out::bold(name), says.what, out::tick(), arena_core::pipeline::fmt_elapsed(took))
    } else {
        failed_line(says, name, &outcome.describe())
    }
}

/// `[NAME] FAILED setup: <why> (retry: arena pods setup NAME)`. Pure (but for the styles).
fn failed_line(says: Says, name: &str, why: &str) -> String {
    format!("[{}] {} {}: {why} (retry: {} {name})", out::bold(name), out::red("FAILED"), says.what, says.retry)
}

/// `name` gone from the pods still running (one of them: two pods may share a name).
fn take_one(left: &mut Vec<String>, name: &str) {
    if let Some(i) = left.iter().position(|n| n == name) {
        left.remove(i);
    }
}

/// The in-place line while a fleet run goes: `setup 1m10s · devtest-alpha` (one pod), `setup
/// 1m10s · 1 of 3 done · devtest-bravo, devtest-cloud`. Pure.
fn progress_text(says: Says, took: Duration, done: usize, left: &[String]) -> String {
    let took = arena_core::pipeline::fmt_elapsed(took);
    if done == 0 && left.len() == 1 {
        format!("{} {took} · {}", says.what, left[0])
    } else {
        format!("{} {took} · {done} of {} done · {}", says.what, done + left.len(), left.join(", "))
    }
}

/// Bare-VM provisioning script ([`arena_core::setup::HETZNER_SETUP`], in core so the
/// dashboard's setup pushes the same one) — `arena pods setup` stages and runs it.
pub(crate) const HETZNER_SETUP: &str = arena_core::setup::HETZNER_SETUP;

/// One pod's provisioning job for [`provision_fleet`].
struct SetupJob {
    name: String,
    target: arena_core::ssh::SshTarget,
    steps: Vec<arena_core::setup::ProvisionStep>,
    /// The pod refused our key and its provider has just re-attached it: run the steps
    /// through [`arena_core::setup::provision_after_key_repair`] (waits, bounded retries).
    after_repair: bool,
}

/// How long one provider call re-attaching a pod's keys may take ([`repair_pod_keys`]). The
/// HTTP client has no timeout of its own; the attach is idempotent, so giving up is safe.
const KEY_REPAIR_TIMEOUT: Duration = Duration::from_secs(60);

/// The repair for a pod that refused our SSH key during setup: have its provider
/// re-authorize the cohort keys (`ssh::authorized_pubkeys`, what create authorized) through
/// its API — Vast's per-instance attach. `Ok(n)`: `n` keys re-attached, so re-run setup.
/// `Err(Some(why))`: it couldn't, and the report should say why. `Err(None)`: the provider
/// has no such API (RunPod, Hetzner authorize keys only at create) — the failure stands.
pub(crate) async fn repair_pod_keys(
    provider: &dyn Provider,
    cfg: &Config,
    pod: &arena_core::Pod,
) -> std::result::Result<usize, Option<String>> {
    // A provider that authorizes keys only at create (the capability table: RunPod, Hetzner,
    // Shadeform) has nothing to repair with — the pod's report stands, and nothing is asked.
    if !provider.supports(arena_core::capability::Op::KeyRepair, Some(pod)).works() {
        return Err(None);
    }
    let keys = arena_core::ssh::authorized_pubkeys(cfg);
    // Asked even with no key to send (it then sends nothing): the answer still says whether
    // this provider could repair at all — `NotImplemented` keeps the report as is.
    let attach = provider.authorize_ssh_keys(pod, &keys);
    match arena_core::ratelimit::bounded(KEY_REPAIR_TIMEOUT, attach).await {
        Err(_) => {
            let after = KEY_REPAIR_TIMEOUT.as_secs();
            Err(Some(format!("re-attaching our keys via the {} API timed out after {after}s", pod.provider)))
        }
        Ok(Err(arena_core::Error::NotImplemented(_))) => Err(None),
        Ok(Err(e)) => Err(Some(format!("re-attaching our keys via the {} API failed: {e}", pod.provider))),
        Ok(Ok(())) if keys.is_empty() => {
            Err(Some("no public key to re-attach (SHARED_SSH_KEY_PATH / GIT_SSH_KEY_LOCAL / ADMIN_SSH_KEY_PATH not readable)".into()))
        }
        Ok(Ok(())) => Ok(keys.len()),
    }
}

/// Provision every job concurrently over `remote`, saying each pod's end as it comes (scp+ssh
/// is slow serially): `[done/total] ✓/✗ name` with `-v`, [`ended_line`] by default. A log gets
/// `[name] setup…` as each starts, a terminal an in-place line instead ([`progress_text`]).
/// Each pod runs under its own per-step budgets (see `arena_core::setup::provision`), so a
/// wedged pod reports `timed out at <step>` and never holds up the others — the whole run
/// takes as long as the slowest pod's budget, not forever. The flow is *data*
/// (`provisioning_steps`) run by a generic runner — nothing here names a provider. Returns
/// the names of the pods that were fully provisioned (in finishing order), the number that
/// failed — the caller only follows up (API keys) on the former, never on a pod that just
/// timed out — and those whose failure was the pod refusing our key, with what happened (a
/// provider may be able to repair that: [`repair_pod_keys`]). With `hold_rejected` their
/// default line is the caller's to say once it has tried: a `FAILED` line now would stand on
/// the dashboard against the `✓` of a setup that works after the repair.
async fn provision_fleet(
    remote: std::sync::Arc<dyn arena_core::remote::Remote>,
    jobs: Vec<SetupJob>,
    boot: arena_core::setup::BootRetry,
    says: Says,
    hold_rejected: bool,
    mut emit: impl FnMut(To, &str),
) -> (Vec<String>, usize, Vec<(String, String)>) {
    use arena_core::setup::{key_rejected, progress_line, provision, provision_after_key_repair};
    let total = jobs.len();
    let started = tokio::time::Instant::now();
    let live = out::live();
    let mut left: Vec<String> = jobs.iter().map(|j| j.name.clone()).collect();
    let mut set = tokio::task::JoinSet::new();
    // Task id -> pod name, so even a task that panicked is reported by name.
    let mut names = std::collections::HashMap::new();
    for job in jobs {
        // A retry after a key repair has had its line (`… re-attached it, trying again`).
        if !live && !job.after_repair {
            emit(To::Brief, &format!("[{}] {}…", out::bold(&job.name), says.what));
        }
        let remote = remote.clone();
        let name = job.name.clone();
        let handle = set.spawn(async move {
            let outcome = if job.after_repair {
                provision_after_key_repair(remote.as_ref(), &job.target, &job.steps, boot).await
            } else {
                provision(remote.as_ref(), &job.target, &job.steps, boot).await
            };
            (job.name, outcome)
        });
        names.insert(handle.id(), name);
    }
    let _line = out::progress_guard(PROGRESS);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let (mut provisioned, mut failed, mut rejected, mut done) = (Vec::new(), 0, Vec::new(), 0);
    loop {
        // On a terminal the in-place line is redrawn every second (its clock); elsewhere the
        // run only waits for the next pod to end.
        let joined = if live {
            tokio::select! {
                j = set.join_next() => j,
                _ = tick.tick() => {
                    out::progress(PROGRESS, Some(&progress_text(says, started.elapsed(), done, &left)));
                    continue;
                }
            }
        } else {
            set.join_next().await
        };
        let Some(joined) = joined else { break };
        done += 1;
        // Which pod ended (a crashed task by its id), out of the in-place line before its own
        // line is said: none left, none drawn.
        let ended = match &joined {
            Ok((name, _)) => name.clone(),
            Err(e) => names.get(&e.id()).cloned().unwrap_or_else(|| "?".into()),
        };
        take_one(&mut left, &ended);
        if live {
            let line = (!left.is_empty()).then(|| progress_text(says, started.elapsed(), done, &left));
            out::progress(PROGRESS, line.as_deref());
        }
        match joined {
            Ok((name, outcome)) => {
                emit(To::Detail, &progress_line(done, total, &name, &outcome));
                let held = hold_rejected && key_rejected(&outcome);
                if !held {
                    emit(To::Brief, &ended_line(says, &name, &outcome, started.elapsed()));
                }
                // A best-effort step that didn't work out: said, never a failure (`-v`'s line
                // has it in brackets) — as `up` says it.
                for w in outcome.warnings() {
                    emit(To::ErrBrief, &format!("[{}] {} warning: {w}", out::bold(&name), says.what));
                }
                if outcome.is_done() {
                    provisioned.push(name);
                } else {
                    failed += 1;
                    if key_rejected(&outcome) {
                        rejected.push((name, outcome.describe()));
                    }
                }
            }
            // A panicked task still counts — as a failure, never silently.
            Err(e) => {
                failed += 1;
                emit(To::Detail, &format!("[{done}/{total}] ✗ {ended} (its setup task crashed: {e})"));
                emit(To::Brief, &failed_line(says, &ended, &format!("its task crashed: {e}")));
            }
        }
    }
    (provisioned, failed, rejected)
}

/// The provisioning settings `pods setup` (and `up`'s per-pod setup) run with: config's,
/// plus the broadcast tokens — `--hf-token` over config, so a token can be supplied without
/// editing the read-only prod config — `--zsh-install` and `--no-vscode`. Also returns the
/// `Broadcast tokens: …` line saying which will be exported. (Claude Code's token isn't
/// broadcast: each pod gets its own account's after setup, `crate::claude::stage`.)
pub(crate) fn setup_config(
    cfg: &Config,
    hf_token: Option<String>,
    zsh_install: bool,
    no_vscode: bool,
) -> Result<(arena_core::setup::SetupConfig, String)> {
    let token_value = |k: &str| -> Option<String> {
        let flag = match k {
            "HF_TOKEN" => hf_token.clone(),
            _ => None,
        };
        flag.or_else(|| cfg.get(k).filter(|s| !s.is_empty()).map(String::from))
    };
    let mut scfg = arena_core::setup::SetupConfig::from_config(cfg)?;
    scfg.broadcast_exports = arena_core::apikeys::broadcast_env_vars(token_value);
    scfg.zsh_install = zsh_install;
    if no_vscode {
        scfg.vscode = None;
    }
    // Which broadcast tokens (HF, Claude Code) will be exported (an optional step).
    let token_summary: Vec<String> = arena_core::apikeys::BROADCAST_TOKENS
        .iter()
        .map(|(key, display, _)| format!("{display} {}", if token_value(key).is_some() { "✓" } else { "✗" }))
        .collect();
    let line = format!("Broadcast tokens: {} (✓ exported on each pod; ✗ skipped)", token_summary.join(", "));
    Ok((scfg, line))
}

/// Bare-VM pods (hetzner, shadeform: `setup::is_bare_vm`) scp [`HETZNER_SETUP`]; write it once
/// to a temp file and return its path — only when `needed` (such a pod is in the target set): a runpod-only setup
/// (e.g. `replace`) needs no hetzner script, and writing one is pure overhead (`""` then).
/// The filename carries the pid so concurrent runs (or another user's run) never collide on
/// a fixed path and hit EACCES overwriting a file they don't own.
pub(crate) fn stage_hetzner_script(needed: bool) -> Result<String> {
    if !needed {
        return Ok(String::new());
    }
    let p = hetzner_script_path();
    std::fs::write(&p, HETZNER_SETUP).context("writing hetzner setup script to a temp file")?;
    Ok(p.to_string_lossy().into_owned())
}

/// Where [`stage_hetzner_script`] puts this run's copy of the script.
fn hetzner_script_path() -> PathBuf {
    hetzner_script_in(&std::env::temp_dir())
}

/// This run's copy of the script under `dir`.
fn hetzner_script_in(dir: &std::path::Path) -> PathBuf {
    dir.join(format!("arena-hetzner-setup-{}.sh", std::process::id()))
}

/// Remove this run's copy of the script under `dir`, if it staged one — `main` does, for the temp
/// dir, once the command is done: nothing did before, and prod's /tmp held 63 of them (2026-10-30).
pub(crate) fn unstage_hetzner_script(dir: &std::path::Path) {
    let _ = std::fs::remove_file(hetzner_script_in(dir));
}

/// The dry-run's two lines for one provisioning step: `## <label> (timeout Ns…)`, then
/// the exact scp/ssh command — or, for the best-effort warm-up, what it does (its command
/// is a base64 blob nobody can review).
fn step_preview(step: &arena_core::setup::ProvisionStep, target: &arena_core::ssh::SshTarget) -> [String; 2] {
    use arena_core::setup::ProvisionStep;
    let head = |label: &str, extra: &str| format!("## {label} (timeout {}s{extra})", step.timeout().as_secs());
    match step {
        ProvisionStep::Scp { label, local, remote, .. } => [head(label, ""), target.display_scp(local, remote)],
        ProvisionStep::Run { label, cmd, .. } => [head(label, ""), target.display_command(cmd)],
        ProvisionStep::Optional { label, summary, .. } => {
            [head(label, ", best-effort: a failure is a warning, never a failed setup"), summary.clone()]
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_setup(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: std::sync::Arc<dyn arena_core::remote::Remote>,
    cfg: &Config,
    apply: bool,
    force: bool,
    hf_token: Option<String>,
    cc_token: Option<String>,
    zsh_install: bool,
    // `--no-vscode`: skip the VS Code warm-up step (config may already have it off).
    no_vscode: bool,
    // Per-step budgets, resolved by the caller (`SetupTimeouts::from_config`) *before* it
    // does anything costly: `up` / `replace` / `migrate copy` create (billing) pods before
    // they get here, so a malformed SETUP_TIMEOUT_SECS must fail before the create, not
    // after it — taking the resolved value (not the raw flag) makes that the only option.
    timeouts: arena_core::setup::SetupTimeouts,
    // The pods to provision, resolved by the caller: the operator's selection, or the pods
    // `up` / `replace` / `migrate copy` just created.
    sel: &Selected,
    // Where the per-host API-key CSVs live (`keys_dir`; a temp dir in tests).
    keys_dir: &str,
    // The fleet provider, for the one repair setup can make itself: a pod that refuses our
    // key gets it re-attached through its provider's API (Vast), then setup re-runs on it.
    // `None` = no repair (tests that don't exercise it).
    repair: Option<&dyn Provider>,
) -> Result<()> {
    use arena_core::setup::{provisioning_steps, BootRetry};
    use arena_core::ssh::SshTarget;

    let (scfg, tokens_line) = setup_config(cfg, hf_token, zsh_install, no_vscode)?;
    out!(Detail, "{tokens_line}");
    // Claude Code: each pod its own account's token, once it's set up.
    match crate::claude::plan_line(cfg, cc_token.as_deref()) {
        Ok(Some(line)) => out!(Detail, "{line}"),
        Ok(None) => out!(Detail, "Claude Code: no accounts ({} / {} unset) — skipped", arena_core::claude::TOKENS_FILE_KEY, arena_core::claude::TOKEN_KEY),
        Err(e) => out!(Out, "Claude Code: {} {e} — no token goes out", out::cross()),
    }
    // Carry each pod's provider so we can pick bare-VM vs image-based provisioning. A named
    // pod without an endpoint is reported; if none of the named pods is reachable, it fails.
    let targets: Vec<(String, String, SshTarget)> =
        sel.ssh_targets(cfg)?.into_iter().map(|(p, t)| (p.name.clone(), p.provider.clone(), t)).collect();
    if targets.is_empty() {
        out!(Out, "(no pods with an SSH endpoint to set up)");
        return Ok(());
    }

    let hetzner_script = stage_hetzner_script(targets.iter().any(|(_, prov, _)| arena_core::setup::is_bare_vm(prov)))?;

    if !apply {
        // Redact broadcast token values in the *previewed* command — the dry-run prints
        // the exact shell, and real token values must never land in a terminal/log. (The
        // actual run below uses `scfg` with real values and never prints the command.)
        let mut display_scfg = scfg.clone();
        for (name, value) in display_scfg.broadcast_exports.iter_mut() {
            *value = format!("<{name}>");
        }
        out!(Detail, "Dry-run — would provision {} pod(s):\n", targets.len());
        // By default a line per set of steps (bare VMs and images differ), naming its pods.
        let mut plans: Vec<(String, Vec<&str>)> = Vec::new();
        for (name, provider_name, target) in &targets {
            out!(Detail, "# {name}");
            let steps = provisioning_steps(provider_name, &display_scfg, name, force, &hetzner_script, &timeouts);
            for step in &steps {
                for line in step_preview(step, target) {
                    out!(Detail, "  {line}");
                }
            }
            out!(Detail, "");
            let labels = steps.iter().map(|s| s.label()).collect::<Vec<_>>().join(" → ");
            match plans.iter_mut().find(|(l, _)| *l == labels) {
                Some((_, names)) => names.push(name),
                None => plans.push((labels, vec![name])),
            }
        }
        let keys_present = arena_core::apikeys::PROVIDERS.iter().any(|(base, _, _)| {
            std::fs::read_to_string(format!("{keys_dir}/{base}_api_keys.csv"))
                .map(|t| !arena_core::apikeys::parse_csv(&t).is_empty())
                .unwrap_or(false)
        });
        out!(
            Detail,
            "API keys: {}",
            if keys_present {
                format!("found in {keys_dir} — would distribute to each pod above that provisions successfully")
            } else {
                format!("none in {keys_dir} — would skip")
            }
        );
        let keys = if keys_present { " → API keys" } else { "" };
        for (labels, names) in &plans {
            out!(Brief, "Would set up {}: {labels}{keys}", some_names(names));
        }
        out!(Detail, "Preview only — run without --dry-run to execute over SSH.");
        return Ok(());
    }

    // Provision concurrently across the fleet, one line per pod as it finishes; a stuck
    // pod times out at its step and never blocks the rest.
    let total = targets.len();
    let vscode = match &scfg.vscode {
        Some(_) => format!(", vscode warm-up {}s", timeouts.vscode.as_secs()),
        None => String::new(),
    };
    out!(
        Detail,
        "Provisioning {total} pod(s) over SSH (budgets: copy {}s, config {}s, hetzner script {}s{vscode})…",
        timeouts.copy.as_secs(),
        timeouts.config.as_secs(),
        timeouts.bare_vm.as_secs()
    );
    let job = |(name, provider_name, target): (String, String, SshTarget), after_repair: bool| SetupJob {
        steps: provisioning_steps(&provider_name, &scfg, &name, force, &hetzner_script, &timeouts),
        name,
        target,
        after_repair,
    };
    let jobs = targets.iter().cloned().map(|t| job(t, false)).collect();
    // A pod refusing our key may yet be set up after a repair: its default line waits for it.
    let (mut provisioned, mut failed, rejected) =
        provision_fleet(remote.clone(), jobs, BootRetry::default(), SETUP, repair.is_some(), out::emit).await;

    // A pod that refused our key: its provider may re-attach the keys (Vast). Then those pods
    // run setup again, waiting for the attach to land (bounded); the rest stand as reported.
    if let Some(provider) = repair.filter(|_| !rejected.is_empty()) {
        let mut retry = Vec::new();
        for (name, provider_name, target) in targets.iter().filter(|(n, _, _)| rejected.iter().any(|(r, _)| r == n)) {
            // What happened to it, for its default line (held back by `provision_fleet`).
            let why = rejected.iter().find(|(r, _)| r == name).map_or("", |(_, w)| w.as_str());
            let failed_now = || out!(Brief, "{}", failed_line(SETUP, name, why));
            // The pod behind this target (by its endpoint: two pods may share a name).
            let Some(pod) = sel.pods.iter().find(|p| {
                &p.name == name && p.ssh_ip.as_deref() == Some(target.host.as_str()) && p.ssh_port == Some(target.port)
            }) else {
                failed_now();
                continue;
            };
            match repair_pod_keys(provider, cfg, pod).await {
                Ok(n) => {
                    out!(Detail, "  {name} refused our SSH key — re-attached {n} key(s) through the {} API", pod.provider);
                    out!(Brief, "[{}] setup: it refused our SSH key — re-attached it, trying again", out::bold(name));
                    retry.push(job((name.clone(), provider_name.clone(), target.clone()), true));
                }
                Err(Some(repair_why)) => {
                    out!(Detail, "  {name} refused our SSH key; {repair_why}");
                    out!(Brief, "{}", failed_line(SETUP, name, &format!("{why}; {repair_why}")));
                }
                Err(None) => failed_now(),
            }
        }
        if !retry.is_empty() {
            out!(
                Detail,
                "Retrying setup on {} pod(s) whose keys were re-attached (up to {}×, {}s apart)…",
                retry.len(),
                arena_core::setup::KEY_REPAIR_TRIES,
                arena_core::setup::KEY_REPAIR_EVERY.as_secs()
            );
            let (fixed, _, _) = provision_fleet(remote.clone(), retry, BootRetry::default(), SETUP, false, out::emit).await;
            failed = failed.saturating_sub(fixed.len());
            provisioned.extend(fixed);
        }
    }
    out!(Detail, "\nDone: {} provisioned, {failed} failed.", provisioned.len());
    // One pod's line says it all; several get the count (a failure ends in the error instead).
    if total > 1 && failed == 0 {
        out!(Brief, "{}", out::ok(format!("{} of {total} set up", provisioned.len())));
    }

    // Auto-handle API keys: if per-host CSVs have been generated, distribute them and say
    // so; otherwise report they're not set up (rather than silently doing nothing). HF is
    // already handled inline above. Best-effort — never fails the setup. With a backup
    // archive, copy-keys runs anyway: it writes the archive's access (A-ckl9).
    let archive_ready = archive::pod_archive(cfg).ready().is_some();
    let csv_sources: Vec<&str> = arena_core::apikeys::PROVIDERS
        .iter()
        .filter(|(base, _, _)| {
            std::fs::read_to_string(format!("{keys_dir}/{base}_api_keys.csv"))
                .map(|t| !arena_core::apikeys::parse_csv(&t).is_empty())
                .unwrap_or(false)
        })
        .map(|(_, display, _)| *display)
        .collect();
    if csv_sources.is_empty() && !archive_ready {
        out!(
            Detail,
            "API keys: none generated in {keys_dir}/ — skipping (run `arena keys gen --all` \
             or drop in <provider>_api_keys.csv, then re-run setup / `pods copy-keys`)."
        );
    } else if provisioned.is_empty() {
        if !csv_sources.is_empty() {
            out!(Detail, "API keys: found {} — skipped, no pod provisioned successfully.", csv_sources.join(", "));
        }
    } else {
        // Scope key distribution to exactly the pods that just provisioned OK: never the
        // rest of the fleet (`setup <one-pod>` must not touch the others), and never a pod
        // that just failed or timed out — re-contacting a wedged pod would hang here. Each
        // pod's write is bounded by COPY_KEYS_TIMEOUT anyway.
        let skipped = if failed > 0 { format!(" (skipping the {failed} that failed)") } else { String::new() };
        if csv_sources.is_empty() {
            out!(
                Detail,
                "API keys: none generated in {keys_dir}/ — writing the backup archive's access to the {} provisioned pod(s){skipped}…",
                provisioned.len()
            );
        } else {
            out!(
                Detail,
                "API keys: found {} — distributing to the {} provisioned pod(s){skipped}…",
                csv_sources.join(", "),
                provisioned.len()
            );
        }
        let done = Selected {
            pods: sel.pods.iter().filter(|p| provisioned.contains(&p.name)).cloned().collect(),
            named: true,
        };
        // copy-keys runs the Claude Code stage too (with `--cc-token`, if given).
        if let Err(e) = handle_copy_keys(remote.clone(), cfg, keys_dir, None, cc_token.clone(), &done, false, true).await {
            out!(ErrDetail, "  (API-key distribution failed: {e})");
            out!(ErrBrief, "{}", out::warn(format!("API keys: {e} (retry: arena pods copy-keys {})", provisioned.join(" "))));
        }
    }
    // No copy-keys to do it: the Claude Code stage on its own, for the pods that set up OK.
    let copied_keys = (!csv_sources.is_empty() || archive_ready) && !provisioned.is_empty();
    // Even with no pool: a pod that's off still gets a pending removal (review r3).
    if !copied_keys && !provisioned.is_empty() {
        let pods: Vec<(arena_core::Pod, SshTarget)> = sel
            .pods
            .iter()
            .filter(|p| provisioned.contains(&p.name))
            .filter_map(|p| SshTarget::from_pod(p, cfg).ok().map(|t| (p.clone(), t)))
            .collect();
        crate::claude::stage(remote.as_ref(), cfg, &pods, cc_token.as_deref()).await;
    }
    // The reap notice on each pod set up that has a policy (RR11): setup has just written its
    // shell rc files (where the banner goes), and a restart that reset the container lost it.
    let set_up: Vec<arena_core::Pod> = sel.pods.iter().filter(|p| provisioned.contains(&p.name)).cloned().collect();
    reap::reassert_notices(remote.as_ref(), cfg, &set_up).await;

    if failed > 0 {
        let not: Vec<&str> = targets.iter().map(|(n, _, _)| n.as_str()).filter(|n| !provisioned.iter().any(|p| p == n)).collect();
        anyhow::bail!("{failed} pod(s) failed to set up: {}", not.join(", "));
    }
    Ok(())
}

/// `pods setup --notes-only`: just the agents' notes step ([`arena_core::agent_notes`]) on each
/// selected machine — uv and uvx linked into /usr/local/bin where missing, and which Python to use
/// in `~/.claude/CLAUDE.md` and `~/.codex/AGENTS.md` — and nothing else: no deploy key, repo
/// update, tokens, API keys, Claude Code account or VS Code. For machines that were running when
/// the notes came out: a whole setup in the middle of a programme does far more than that. The
/// step is required here (it's all the run does), so a machine it fails on is a ✗. A machine that
/// doesn't answer fails at once: these are running machines, there's no boot to wait out.
pub(crate) async fn handle_notes(
    remote: std::sync::Arc<dyn arena_core::remote::Remote>,
    cfg: &Config,
    notes: &arena_core::agent_notes::AgentNotes,
    apply: bool,
    sel: &Selected,
) -> Result<()> {
    use arena_core::setup::BootRetry;
    let targets: Vec<(String, String, arena_core::ssh::SshTarget)> =
        sel.ssh_targets(cfg)?.into_iter().map(|(p, t)| (p.name.clone(), p.provider.clone(), t)).collect();
    if targets.is_empty() {
        out!(Out, "(no pods with an SSH endpoint)");
        return Ok(());
    }
    if !apply {
        out!(Detail, "Dry-run — would write the agents' notes on {} pod(s):\n", targets.len());
        for (name, provider, _) in &targets {
            out!(Detail, "# {name}\n  {}\n", notes.summary(provider));
        }
        let names: Vec<&str> = targets.iter().map(|(n, _, _)| n.as_str()).collect();
        out!(Brief, "Would write the agents' notes ({}) and link uv on {}", arena_core::agent_notes::FILES.join(", "), some_names(&names));
        out!(Detail, "Preview only — run without --dry-run to execute over SSH.");
        return Ok(());
    }
    out!(
        Detail,
        "Writing the agents' notes on {} pod(s) over SSH (budget {}s each)…",
        targets.len(),
        arena_core::agent_notes::TIMEOUT.as_secs()
    );
    let total = targets.len();
    let jobs = targets
        .into_iter()
        .map(|(name, provider, target)| SetupJob { steps: vec![notes.step(&provider, true)], name, target, after_repair: false })
        .collect();
    let no_boot = BootRetry { window: Duration::ZERO, every: Duration::ZERO };
    let (written, failed, _) = provision_fleet(remote, jobs, no_boot, NOTES, false, out::emit).await;
    out!(Detail, "\nDone: {} up to date, {failed} failed.", written.len());
    if total > 1 && failed == 0 {
        out!(Brief, "{}", out::ok(format!("{} of {total} up to date", written.len())));
    }
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed to get the agents' notes");
    }
    Ok(())
}

/// `pods setup` over a scripted `FakeRemote` on a paused clock: one wedged pod must time
/// out at its step while the rest finish, and the whole run must end at that pod's
/// budget — not at the hang.
#[cfg(test)]
mod setup_tests;

/// Golden tests of `pods setup`'s output (docs/OUTPUT.md), at default and at `-v`.
#[cfg(test)]
mod output_tests;
