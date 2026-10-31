//! `pods setup`: provision pods over SSH (keys, repo, tokens, VS Code; bare VMs run
//! hetzner_setup.sh), each step within its budget.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::Config;

use crate::keys::handle_copy_keys;
use crate::pods::selection::Selected;
use crate::{archive, reap};

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

/// Provision every job concurrently over `remote`, emitting a `[done/total] ✓/✗ name`
/// line as each pod finishes (scp+ssh is slow serially). Each pod runs under its own
/// per-step budgets (see `arena_core::setup::provision`), so a wedged pod reports
/// `timed out at <step>` and never holds up the others — the whole run takes as long as
/// the slowest pod's budget, not forever. The flow is *data* (`provisioning_steps`) run
/// by a generic runner — nothing here names a provider. Returns the names of the pods
/// that were fully provisioned (in finishing order), the number that failed — the
/// caller only follows up (API keys) on the former, never on a pod that just timed out —
/// and the names of those whose failure was the pod refusing our key (a provider may be
/// able to repair that: [`repair_pod_keys`]).
async fn provision_fleet(
    remote: std::sync::Arc<dyn arena_core::remote::Remote>,
    jobs: Vec<SetupJob>,
    boot: arena_core::setup::BootRetry,
    mut emit: impl FnMut(&str),
) -> (Vec<String>, usize, Vec<String>) {
    use arena_core::setup::{key_rejected, progress_line, provision, provision_after_key_repair};
    let total = jobs.len();
    let mut set = tokio::task::JoinSet::new();
    // Task id -> pod name, so even a task that panicked is reported by name.
    let mut names = std::collections::HashMap::new();
    for job in jobs {
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
    let (mut provisioned, mut failed, mut rejected, mut done) = (Vec::new(), 0, Vec::new(), 0);
    while let Some(joined) = set.join_next().await {
        done += 1;
        match joined {
            Ok((name, outcome)) => {
                emit(&progress_line(done, total, &name, &outcome));
                if outcome.is_done() {
                    provisioned.push(name);
                } else {
                    failed += 1;
                    if key_rejected(&outcome) {
                        rejected.push(name);
                    }
                }
            }
            // A panicked task still counts — as a failure, never silently.
            Err(e) => {
                failed += 1;
                let name = names.get(&e.id()).map(String::as_str).unwrap_or("?");
                emit(&format!("[{done}/{total}] ✗ {name} (its setup task crashed: {e})"));
            }
        }
    }
    (provisioned, failed, rejected)
}

/// The provisioning settings `pods setup` (and `up`'s per-pod setup) run with: config's,
/// plus the broadcast tokens — `--hf-token`/`--cc-token` over config, so a token can be
/// supplied without editing the read-only prod config — `--zsh-install` and `--no-vscode`.
/// Also returns the `Broadcast tokens: …` line saying which will be exported.
pub(crate) fn setup_config(
    cfg: &Config,
    hf_token: Option<String>,
    cc_token: Option<String>,
    zsh_install: bool,
    no_vscode: bool,
) -> Result<(arena_core::setup::SetupConfig, String)> {
    let token_value = |k: &str| -> Option<String> {
        let flag = match k {
            "HF_TOKEN" => hf_token.clone(),
            "CLAUDE_CODE_OAUTH_TOKEN" => cc_token.clone(),
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
    // Where the per-host API-key CSVs live (`KEYS_DIR`; a temp dir in tests).
    keys_dir: &str,
    // The fleet provider, for the one repair setup can make itself: a pod that refuses our
    // key gets it re-attached through its provider's API (Vast), then setup re-runs on it.
    // `None` = no repair (tests that don't exercise it).
    repair: Option<&dyn Provider>,
) -> Result<()> {
    use arena_core::setup::{provisioning_steps, BootRetry};
    use arena_core::ssh::SshTarget;

    let (scfg, tokens_line) = setup_config(cfg, hf_token, cc_token, zsh_install, no_vscode)?;
    println!("{tokens_line}");
    // Carry each pod's provider so we can pick bare-VM vs image-based provisioning. A named
    // pod without an endpoint is reported; if none of the named pods is reachable, it fails.
    let targets: Vec<(String, String, SshTarget)> =
        sel.ssh_targets(cfg)?.into_iter().map(|(p, t)| (p.name.clone(), p.provider.clone(), t)).collect();
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to set up)");
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
        println!("Dry-run — would provision {} pod(s):\n", targets.len());
        for (name, provider_name, target) in &targets {
            println!("# {name}");
            for step in provisioning_steps(provider_name, &display_scfg, name, force, &hetzner_script, &timeouts) {
                for line in step_preview(&step, target) {
                    println!("  {line}");
                }
            }
            println!();
        }
        let keys_present = arena_core::apikeys::PROVIDERS.iter().any(|(base, _, _)| {
            std::fs::read_to_string(format!("{keys_dir}/{base}_api_keys.csv"))
                .map(|t| !arena_core::apikeys::parse_csv(&t).is_empty())
                .unwrap_or(false)
        });
        println!(
            "API keys: {}",
            if keys_present {
                format!("found in {keys_dir} — would distribute to each pod above that provisions successfully")
            } else {
                format!("none in {keys_dir} — would skip")
            }
        );
        println!("Preview only — run without --dry-run to execute over SSH.");
        return Ok(());
    }

    // Provision concurrently across the fleet, one line per pod as it finishes; a stuck
    // pod times out at its step and never blocks the rest.
    let total = targets.len();
    let vscode = match &scfg.vscode {
        Some(_) => format!(", vscode warm-up {}s", timeouts.vscode.as_secs()),
        None => String::new(),
    };
    println!(
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
    let (mut provisioned, mut failed, rejected) =
        provision_fleet(remote.clone(), jobs, BootRetry::default(), |line| println!("{line}")).await;

    // A pod that refused our key: its provider may re-attach the keys (Vast). Then those pods
    // run setup again, waiting for the attach to land (bounded); the rest stand as reported.
    if let Some(provider) = repair.filter(|_| !rejected.is_empty()) {
        let mut retry = Vec::new();
        for (name, provider_name, target) in targets.iter().filter(|(n, _, _)| rejected.contains(n)) {
            // The pod behind this target (by its endpoint: two pods may share a name).
            let Some(pod) = sel.pods.iter().find(|p| {
                &p.name == name && p.ssh_ip.as_deref() == Some(target.host.as_str()) && p.ssh_port == Some(target.port)
            }) else {
                continue;
            };
            match repair_pod_keys(provider, cfg, pod).await {
                Ok(n) => {
                    println!("  {name} refused our SSH key — re-attached {n} key(s) through the {} API", pod.provider);
                    retry.push(job((name.clone(), provider_name.clone(), target.clone()), true));
                }
                Err(Some(why)) => println!("  {name} refused our SSH key; {why}"),
                Err(None) => {}
            }
        }
        if !retry.is_empty() {
            println!(
                "Retrying setup on {} pod(s) whose keys were re-attached (up to {}×, {}s apart)…",
                retry.len(),
                arena_core::setup::KEY_REPAIR_TRIES,
                arena_core::setup::KEY_REPAIR_EVERY.as_secs()
            );
            let (fixed, _, _) =
                provision_fleet(remote.clone(), retry, BootRetry::default(), |line| println!("{line}")).await;
            failed = failed.saturating_sub(fixed.len());
            provisioned.extend(fixed);
        }
    }
    println!("\nDone: {} provisioned, {failed} failed.", provisioned.len());

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
        println!(
            "API keys: none generated in {keys_dir}/ — skipping (run `arena keys gen --all` \
             or drop in <provider>_api_keys.csv, then re-run setup / `pods copy-keys`)."
        );
    } else if provisioned.is_empty() {
        if !csv_sources.is_empty() {
            println!("API keys: found {} — skipped, no pod provisioned successfully.", csv_sources.join(", "));
        }
    } else {
        // Scope key distribution to exactly the pods that just provisioned OK: never the
        // rest of the fleet (`setup <one-pod>` must not touch the others), and never a pod
        // that just failed or timed out — re-contacting a wedged pod would hang here. Each
        // pod's write is bounded by COPY_KEYS_TIMEOUT anyway.
        let skipped = if failed > 0 { format!(" (skipping the {failed} that failed)") } else { String::new() };
        if csv_sources.is_empty() {
            println!(
                "API keys: none generated in {keys_dir}/ — writing the backup archive's access to the {} provisioned pod(s){skipped}…",
                provisioned.len()
            );
        } else {
            println!(
                "API keys: found {} — distributing to the {} provisioned pod(s){skipped}…",
                csv_sources.join(", "),
                provisioned.len()
            );
        }
        let done = Selected {
            pods: sel.pods.iter().filter(|p| provisioned.contains(&p.name)).cloned().collect(),
            named: true,
        };
        if let Err(e) = handle_copy_keys(remote.clone(), cfg, keys_dir, None, None, &done, false, true).await {
            eprintln!("  (API-key distribution failed: {e})");
        }
    }
    // The reap notice on each pod set up that has a policy (RR11): setup has just written its
    // shell rc files (where the banner goes), and a restart that reset the container lost it.
    let set_up: Vec<arena_core::Pod> = sel.pods.iter().filter(|p| provisioned.contains(&p.name)).cloned().collect();
    reap::reassert_notices(remote.as_ref(), cfg, &set_up).await;

    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed to set up");
    }
    Ok(())
}

/// `pods setup` over a scripted `FakeRemote` on a paused clock: one wedged pod must time
/// out at its step while the rest finish, and the whole run must end at that pod's
/// budget — not at the hang.
#[cfg(test)]
mod setup_tests;
