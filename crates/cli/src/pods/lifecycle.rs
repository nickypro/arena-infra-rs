//! `pods reimage`, `start`, `stop`/`terminate` (`--revoke-key`) and `restart`: what a container
//! reset destroys, and coming back set up and proxied.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::capability::Op;
use arena_core::out;
use arena_core::provider::{ContainerReset, Provider};
use arena_core::remote::{Remote, PROBE_TIMEOUT};
use arena_core::selector::Naming;
use arena_core::ssh::SshTarget;
use arena_core::volume::RepoSite;
use arena_core::{Config, PodSpec};

use crate::keys::{machine_key_names, shared_name_holder, KeyBook};
use crate::pods::exec_each_pod;
use crate::pods::lock::print_would_refuse;
use crate::pods::replace::wait_for_stable_endpoint_every;
use crate::pods::selection::{just_these, Selected};
use crate::pods::setup::setup_pods;
use crate::proxy::{sync_proxy, ProxySync};
use crate::{confirm_brief, spend};

/// `pods reimage`: swap image + re-seed keys in place — a fresh container (a Hetzner VM:
/// rebuilt), so its disk is wiped — then bring the pods back usable like a restart does
/// ([`come_back`]): settled endpoints, setup again, the proxy.
///
/// Gated exactly like restart/stop (live finding #2: a reimage wiped participants' files
/// with only `-y`, and `--all -y` would wipe the cohort): each pod's fate is judged by where
/// its repo really is ([`repo_sites`]), and a pod whose work isn't on a confirmed volume is
/// refused without `--wipe-ok`; the dry run says so. Then, unless `force`, a pod someone is on
/// — or that can't be read — is refused, as `replace` refuses one ([`busy_refusals`]): the
/// wipe would take what they're working on. Asked of the whole selection before the prompt,
/// and of each pod again right before its own reimage (one found busy then is skipped, and
/// fails the exit). And it re-runs setup afterwards (live
/// finding #8: a reimaged pod was left with no `~/.name`, the public git remote and no
/// deploy key — backups then silently couldn't push).
///
/// What each pod gets ([`reimage_images`]): a container pod `--image` / RUNPOD_DOCKER_IMAGE; a
/// Hetzner VM `--image` / HETZNER_IMAGE, `arena` resolved now for its server type and said —
/// the snapshot, its name and age (ops feedback #2: the dry run named the RunPod image).
///
/// What it says (docs/OUTPUT.md): by default the image lines, then the prompt or the dry run in
/// one line ([`reimage_what`]: whose disk goes, and whose work), and a pod found busy right
/// before its own reimage as one line with its retry. `-v` says what it said before: the keys,
/// each pod's plan, the whole refusal.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_reimage(
    provider: &dyn Provider,
    // How the reimaged pods are set up again: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    // The pods to reimage, resolved by the caller (which demands names or --all).
    sel: &Selected,
    image: Option<String>,
    // Go on with someone on a pod (or a pod that couldn't be read).
    force: bool,
    opts: RestartOpts,
    settle: SettleWait,
    // Where the per-host API-key CSVs live (`keys_dir`; a temp dir in tests).
    keys_dir: &str,
) -> Result<()> {
    let RestartOpts { wipe_ok, no_setup, skip_proxy, dry_run, yes } = opts;
    let pods = &sel.pods;
    if pods.is_empty() {
        out!(Out, "(no pods to reimage)");
        return Ok(());
    }
    let refs: Vec<&arena_core::Pod> = pods.iter().collect();
    // `--image` as given, for a busy pod's retry.
    let given_image = image.clone();
    // A pod whose provider can't reimage (Vast, Shadeform) refuses the whole selection, dry run
    // or not — before the wipe gate and the confirm, which would only mislead: it used to fail
    // there, after both, reading the pod's spec.
    if let Some(r) = crate::pods::capable::refusal(provider, Op::Reimage, "reimage", &refs) {
        anyhow::bail!("{r}");
    }
    // A reimage resets the container — exactly what a lock is for ("cannot be stopped or
    // reset"); a Hetzner VM's lock is its rebuild protection. Refused here, up front, rather
    // than left to an API refusal nobody has verified for the reimage PATCH.
    let locked = arena_core::lock::refusal("reimage", &refs, "");
    if let (Some(r), false) = (&locked, dry_run) {
        anyhow::bail!("{r}");
    }
    let (container_image, vm_setting) = reimage_images(pods, image, cfg)?;
    let pubkeys = arena_core::ssh::authorized_pubkeys(cfg);
    if pubkeys.is_empty() {
        anyhow::bail!("no readable public keys from SHARED_SSH_KEY_PATH/GIT_SSH_KEY_LOCAL/ADMIN_SSH_KEY_PATH — refusing to lock pods out");
    }
    // Setup is what brings a reimaged pod back: make sure it can run before wiping one (and a
    // malformed SETUP_TIMEOUT_SECS fails now, as in `up`).
    let setup = !no_setup;
    let timeouts = if setup {
        arena_core::setup::SetupConfig::from_config(cfg)
            .context("re-running setup after the reimage needs the setup config — fix it, or pass --no-setup")?;
        arena_core::setup::SetupTimeouts::from_config(cfg, None)?
    } else {
        arena_core::setup::SetupTimeouts::default()
    };
    // Each VM's image, `arena` resolved for its server type — before anything is asked.
    let vm_images = match &vm_setting {
        Some(setting) => vm_images(cfg, pods, setting).await?,
        None => Vec::new(),
    };
    // What the reimage costs each pod, by where its repo really is (asked of pods with a
    // volume only, bounded, all at once).
    let mut fates = Vec::with_capacity(pods.len());
    for p in pods {
        fates.push(reimage_fate(provider, p).await);
    }
    let repo = arena_core::backup::repo_path(cfg);
    let sites = repo_sites(&remote, cfg, &refs, &fates, &repo).await;
    let lines: Vec<String> = pods
        .iter()
        .zip(fates.iter().zip(&sites))
        .map(|(p, (f, site))| format!("  {} (id={}, {}): {}", p.name, p.id, p.provider, f.describe(reimage_op(p), site)))
        .collect();
    let blocked: Vec<&arena_core::Pod> =
        pods.iter().zip(fates.iter().zip(&sites)).filter(|(_, (f, site))| f.needs_wipe_ok(site)).map(|(p, _)| p).collect();
    let blocked_names: Vec<&str> = blocked.iter().map(|p| p.name.as_str()).collect();
    // The refusal's words: a VM's rebuild when only VMs are refused, a container's otherwise.
    let blocked_op = if blocked.iter().all(|p| reimage_op(p) == DiskOp::Rebuild) { DiskOp::Rebuild } else { DiskOp::Reimage };
    let then = afterwards_line(ContainerReset::Yes, setup, skip_proxy);
    let onto = image_lines(container_image.as_deref(), &vm_images);
    // What each kind of pod goes onto, by default (ops feedback #2: the image, `arena` resolved,
    // before anything is asked); the keys that go in are `-v`'s.
    for l in &onto {
        out!(Out, "{l}");
    }
    out!(Detail, "keys:  {}", pubkeys.iter().map(|k| k.split_whitespace().last().unwrap_or("?")).collect::<Vec<_>>().join(", "));
    if !(blocked.is_empty() || wipe_ok || dry_run) {
        anyhow::bail!("{}", wipe_refusal(blocked_op, false, &blocked_names, &repo));
    }
    // Who's on them: asked once the wipe is allowed (or for the dry run) — an SSH probe each.
    let busy = if force { Vec::new() } else { busy_refusals(&remote, cfg, &refs).await };
    let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
    let brief = reimage_what(&names, &blocked_names);
    if dry_run {
        out!(Detail, "[dry-run] would reimage {} pod(s):\n{}\n  {then}", pods.len(), lines.join("\n"));
        out!(Brief, "[dry-run] would reimage {brief}");
        if let Some(r) = &locked {
            print_would_refuse(r);
        }
        if !blocked.is_empty() && !wipe_ok {
            print_would_refuse(&wipe_refusal(blocked_op, false, &blocked_names, &repo));
        }
        for b in &busy {
            print_would_refuse(&b.refusal);
        }
        return Ok(());
    }
    if !busy.is_empty() {
        anyhow::bail!("{}", busy.iter().map(|b| b.refusal.as_str()).collect::<Vec<_>>().join("\n"));
    }
    let what = onto.iter().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n");
    if !confirm_brief(yes, &format!("Will reimage {} pod(s):\n{}\n{what}\n  {then}", pods.len(), lines.join("\n")), &format!("Reimage {brief}?"))? {
        out!(Out, "aborted.");
        return Ok(());
    }
    let policy = arena_core::retry::RetryPolicy::default();
    let mut failed = Vec::new();
    for pod in pods {
        // Asked again right before its wipe (review F2), of the pod as it's listed NOW (R2-1: one
        // stopped at the start may have been started since): the prompt may have waited, and a
        // rebuild before it may have taken 15 min — someone on it since isn't wiped unasked.
        if !force {
            if let Some(b) = busy_now(provider, &remote, cfg, pod, &policy).await {
                out!(ErrDetail, "[not reimaged] {}: {}", pod.name, b.refusal);
                let retry = reimage_retry(&pod.name, given_image.as_deref(), opts);
                out!(ErrBrief, "[{}] {}: {} (retry: {retry}, or add --force)", out::bold(&pod.name), out::red("not reimaged"), b.why);
                failed.push(pod.name.clone());
                continue;
            }
        }
        let image = match reimage_op(pod) {
            DiskOp::Rebuild => vm_images.iter().find(|v| v.ids.contains(&pod.id)).map(|v| v.image.clone()),
            _ => container_image.clone(),
        }
        .expect("every pod has an image: reimage_images and vm_images cover the selection");
        let retry = || reimage_retry(&pod.name, given_image.as_deref(), opts);
        let mut env = match provider.pod_spec(&pod.id).await {
            Ok(spec) => spec.env,
            Err(e) => {
                out!(ErrDetail, "[failed] {}: reading current env: {e}", pod.name);
                out!(ErrBrief, "{}", failed_line(&pod.name, "reimage", &format!("reading its env: {}", out::reason(&e)), &retry()));
                failed.push(pod.name.clone());
                continue;
            }
        };
        env.retain(|(k, _)| k != "PUBLIC_KEY" && k != "MACHINE_NAME");
        env.push(("MACHINE_NAME".into(), pod.name.clone()));
        env.push(("PUBLIC_KEY".into(), pubkeys.join("\n")));
        match arena_core::retry::retrying(&policy, || provider.reimage_pod(&pod.id, &image, &env)).await {
            Ok(()) => out!(Out, "[reimaged] {}", out::bold(&pod.name)),
            Err(e) => {
                out!(ErrDetail, "[failed] {}: {}", pod.name, arena_core::lock::explain(&pod.name, &e));
                out!(ErrBrief, "{}", failed_on(&pod.name, "reimage", &e, &retry()));
                failed.push(pod.name.clone());
            }
        }
    }
    // The reimaged pods come back on a new SSH port (a VM on its own IP, freshly booted):
    // settle, set up, sync (nothing to do if every reimage failed).
    let done: Vec<arena_core::Pod> = pods.iter().filter(|p| !failed.contains(&p.name)).cloned().collect();
    let back = Comeback { setup, sync: !skip_proxy, settle, timeouts, reset: ContainerReset::Yes };
    let outcome = come_back(provider, remote, cfg, &done, back, keys_dir, "reimage").await;
    if !failed.is_empty() {
        if let Err(e) = &outcome {
            out!(Err, "{e:#}");
        }
        anyhow::bail!("{} pod(s) failed to reimage: {}", failed.len(), failed.join(" "));
    }
    outcome
}

/// Whether `pod` is reimaged as a VM — Hetzner's rebuild of its whole disk — or as a container.
fn reimage_op(pod: &arena_core::Pod) -> DiskOp {
    if pod.provider == "hetzner" {
        DiskOp::Rebuild
    } else {
        DiskOp::Reimage
    }
}

/// What `pods reimage` puts on `pods`: `(container image, VM image setting)` — the container
/// image for the RunPod (and any container) pods among them, `--image` else RUNPOD_DOCKER_IMAGE;
/// the VM image for the Hetzner VMs, `--image` else HETZNER_IMAGE (`arena`, a snapshot id or a
/// system image name). `None` where no such pod is selected. `--image` is one of the two kinds,
/// so with both kinds selected it's refused: which one it meant can't be known. Pure.
pub(crate) fn reimage_images(pods: &[arena_core::Pod], image: Option<String>, cfg: &Config) -> Result<(Option<String>, Option<String>)> {
    let vms = pods.iter().filter(|p| reimage_op(p) == DiskOp::Rebuild).count();
    let containers = pods.len() - vms;
    if image.is_some() && vms > 0 && containers > 0 {
        anyhow::bail!(
            "--image is a container image for a RunPod pod and the VM's image for a Hetzner VM — reimage the two apart \
             (`--on hetzner`, then the others with `--on` their provider), or leave --image out: each then gets its configured \
             image (RUNPOD_DOCKER_IMAGE, HETZNER_IMAGE)"
        );
    }
    let container = if containers == 0 {
        None
    } else {
        let image = image.clone().unwrap_or_else(|| PodSpec::from_config(cfg).image);
        if image.is_empty() {
            anyhow::bail!("no image: pass --image or set RUNPOD_DOCKER_IMAGE");
        }
        Some(image)
    };
    let vm = if vms == 0 {
        None
    } else {
        let (_, given) = crate::pods::create::image_flag("hetzner", image)?;
        Some(given.unwrap_or_else(|| arena_core::provider::hetzner::HetznerOpts::from_config(cfg).image))
    };
    Ok((container, vm))
}

/// The image a reimage sends the VMs of one server type, and what to say about it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VmImage {
    /// The VMs' server type (`None`: the listing didn't say — `arena` is then resolved at the
    /// rebuild, for the server's own type).
    pub server_type: Option<String>,
    /// Their pod ids.
    pub ids: Vec<String>,
    /// What goes to the rebuild: a snapshot id or an image name.
    pub image: String,
    /// The line that says it (`image arena → snapshot 449743460 (…)`).
    pub said: String,
}

/// Each VM's image from `setting`, per server type (a Hetzner VM lists its type where a GPU pod
/// lists its GPU): `arena` is resolved against the project's snapshots for each type, as `up`
/// resolves it (bounded; a failed lookup stops the reimage before anything is asked — never a
/// quiet fall back to a 10-minute Ubuntu setup); anything else goes as given.
async fn vm_images(cfg: &Config, pods: &[arena_core::Pod], setting: &str) -> Result<Vec<VmImage>> {
    use arena_core::image::{Resolved, Setting};
    let parsed = Setting::parse(setting);
    let now = arena_core::snapshot::unix_now();
    let mut out: Vec<VmImage> = Vec::new();
    for p in pods.iter().filter(|p| reimage_op(p) == DiskOp::Rebuild) {
        let ty = p.gpu_type.clone().filter(|t| !t.trim().is_empty());
        if let Some(v) = out.iter_mut().find(|v| v.server_type == ty) {
            v.ids.push(p.id.clone());
            continue;
        }
        let resolved = match (&parsed, &ty) {
            (Setting::Arena, Some(t)) => {
                let cloud = crate::image::hetzner(cfg)?;
                tokio::time::timeout(crate::image::LOOKUP_TIMEOUT, crate::image::resolve(&cloud, cfg, &parsed, t))
                    .await
                    .map_err(|_| anyhow::anyhow!("looking up the arena image took over {}s", crate::image::LOOKUP_TIMEOUT.as_secs()))??
                    .0
            }
            _ => Resolved::Given(parsed.clone()),
        };
        let said = match (&parsed, &ty) {
            (Setting::Arena, None) => format!("image {setting} (resolved for its server type at the rebuild)"),
            _ => resolved.describe(now),
        };
        let image = if parsed == Setting::Arena && ty.is_none() { setting.to_string() } else { resolved.as_setting() };
        out.push(VmImage { server_type: ty, ids: vec![p.id.clone()], image, said });
    }
    Ok(out)
}

/// The plan's lines about what the pods get: `image: <container image>` and, per server type
/// of the VMs, `VM image (cx33): image arena → snapshot …`. Pure.
pub(crate) fn image_lines(container: Option<&str>, vms: &[VmImage]) -> Vec<String> {
    let mut out: Vec<String> = container.map(|i| format!("image: {i}")).into_iter().collect();
    for v in vms {
        let ty = v.server_type.as_deref().map(|t| format!(" ({t})")).unwrap_or_default();
        out.push(format!("VM image{ty}: {}", v.said));
    }
    out
}

/// The default output's words for what a reimage does to `names` (docs/OUTPUT.md): who, whose
/// disk goes, and whose work with it (`lost`: the pods whose repo isn't on a confirmed volume) —
/// `devtest-apple, devtest-bloom (wipes their disks and the work on devtest-bloom)`. The dry run
/// says it and the prompt asks it; each pod's plan is `-v`'s. Pure (but for the styles).
pub(crate) fn reimage_what(names: &[&str], lost: &[&str]) -> String {
    let (disk, them, volume) = if names.len() == 1 { ("its disk", "it", "its volume") } else { ("their disks", "them", "their volumes") };
    let work = if lost.is_empty() {
        format!("; the repo stays on {volume}")
    } else if lost.len() == names.len() {
        format!(" and the work on {them}")
    } else {
        format!(" and the work on {}", lost.join(", "))
    };
    format!("{} (wipes {disk}{work})", crate::pods::setup::all_names(names))
}

/// The command that reimages `name` again as this run would (`--image` as given, and the
/// switches): a busy pod's retry. Pure.
pub(crate) fn reimage_retry(name: &str, image: Option<&str>, opts: RestartOpts) -> String {
    let mut cmd = format!("arena pods reimage {name}");
    if let Some(i) = image {
        let plain = !i.is_empty() && i.chars().all(|c| c.is_ascii_alphanumeric() || "._:/@+-".contains(c));
        cmd.push_str(&format!(" --image {}", if plain { i.to_string() } else { crate::shell_quote(i) }));
    }
    for (on, flag) in [(opts.wipe_ok, "--wipe-ok"), (opts.no_setup, "--no-setup"), (opts.skip_proxy, "--skip-proxy")] {
        if on {
            cmd.push_str(&format!(" {flag}"));
        }
    }
    cmd
}

/// A pod's failure as the default output says it (docs/OUTPUT.md): `[NAME] FAILED stop: <why>
/// (retry: <cmd>)` — the dashboard's runs panel reads `[NAME] FAILED…` as a failed pod. Pure
/// (but for the styles).
pub(crate) fn failed_line(name: &str, what: &str, why: &str, retry: &str) -> String {
    format!("[{}] {} {what}: {why} (retry: {retry})", out::bold(name), out::red("FAILED"))
}

/// [`failed_line`] for a provider's error `e`: a locked pod's refusal names the unlock it needs
/// first, anything else is its reason ([`out::reason`]; the whole error is `-v`'s). Pure.
pub(crate) fn failed_on(name: &str, what: &str, e: &arena_core::Error, retry: &str) -> String {
    if e.is_locked() {
        format!("[{}] {} {what}: locked — `arena pods unlock {name}` first", out::bold(name), out::red("FAILED"))
    } else {
        failed_line(name, what, &out::reason(e), retry)
    }
}

/// An error as the default output's reason: a provider's ([`out::reason`]) wherever it is in the
/// chain, else what its words say ([`out::reason_text`]). Pure.
pub(crate) fn reason_of(e: &anyhow::Error) -> String {
    match e.chain().find_map(|c| c.downcast_ref::<arena_core::Error>()) {
        Some(core) => out::reason(core),
        None => out::reason_text(&format!("{e:#}")),
    }
}

/// `devtest-a, devtest-b`, bold, for a one-line plan or question. Pure (but for the bold).
fn names_brief<S: AsRef<str>>(names: &[S]) -> String {
    crate::pods::setup::all_names(names)
}

/// What `op` does to a group of pods' disks, in a few words, for the default output's plan or
/// question (each pod's whole line is `-v`'s: [`DiskFate::describe`]). `n`: how many pods
/// (its/their). Pure.
fn fate_words(op: DiskOp, fate: DiskFate, site: &RepoSite, n: usize) -> String {
    let (its, it, s) = if n == 1 { ("its", "it", "") } else { ("their", "them", "s") };
    let verb = match (op, fate) {
        (DiskOp::Stop, _) => "discards",
        (_, DiskFate::MayWipe { .. }) => "may wipe",
        _ => "wipes",
    };
    match fate {
        DiskFate::Kept => match op {
            DiskOp::Restart => "disk kept: a power-cycle".into(),
            _ => "disk kept".into(),
        },
        _ if !fate.needs_wipe_ok(site) => format!("{verb} {its} container disk{s}; the repo stays on {its} volume{s}"),
        _ => format!("{verb} {its} container disk{s} and the work on {it}"),
    }
}

/// The pods of a stop or a restart grouped by what it does to them, each group with its words
/// ([`fate_words`]): `devtest-alpha (disk kept)`, `devtest-a, devtest-b (discards their
/// container disks and the work on them); devtest-delta (disk kept)`. Pure (but for the bold).
pub(crate) fn disk_brief(op: DiskOp, pods: &[&arena_core::Pod], fates: &[DiskFate], sites: &[RepoSite]) -> String {
    let mut groups: Vec<(String, Vec<&str>, DiskFate, &RepoSite)> = Vec::new();
    for ((p, f), site) in pods.iter().zip(fates).zip(sites) {
        let key = fate_words(op, *f, site, 1);
        match groups.iter_mut().find(|(k, ..)| *k == key) {
            Some((_, names, ..)) => names.push(&p.name),
            None => groups.push((key, vec![&p.name], *f, site)),
        }
    }
    groups
        .iter()
        .map(|(_, names, f, site)| format!("{} ({})", names_brief(names), fate_words(op, *f, site, names.len())))
        .collect::<Vec<_>>()
        .join("; ")
}

/// A pod a reimage won't wipe: someone is on it, or whether anyone is couldn't be told.
struct Busy {
    /// Why, in a few words (`someone is on it (1 SSH session)`): the default output's.
    why: String,
    /// The whole refusal, with what to do: the error's, and `-v`'s.
    refusal: String,
}

/// [`busy_refusals`] for `pod` as the provider lists it now — its status and endpoint may have
/// changed since the selection (started, restarted). Not listed now, or no listing: refused, as
/// someone may be on it.
async fn busy_now(
    provider: &dyn Provider,
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    pod: &arena_core::Pod,
    policy: &arena_core::retry::RetryPolicy,
) -> Option<Busy> {
    let now = match arena_core::retry::retrying(policy, || provider.list_pods()).await {
        Ok(listed) => listed.into_iter().find(|p| p.id == pod.id && p.provider == pod.provider),
        Err(_) => None,
    };
    match now {
        Some(p) => busy_refusals(remote, cfg, &[&p]).await.pop(),
        None => Some(Busy {
            why: "couldn't list it again, so whether anyone is on it isn't known".into(),
            refusal: format!(
                "refusing to reimage {}: couldn't list it again right before its reimage, so whether anyone is on it now \
                 isn't known. Or pass --force.",
                pod.name
            ),
        }),
    }
}

/// What a reimage tells the operator who'd wipe someone's session: wait, and keep their work.
const REIMAGE_BUSY: &str = "A reimage wipes what they're working on: wait until they're off, and back it up first \
     (`arena pods backup <name>`).";

/// Who is on each of `pods` right now, asked as `replace` asks its source ([`adopt::probe`]'s
/// probe, all at once, each bounded): the refusal for each one someone is on — SSH, another
/// inbound connection, a VS Code server — or that couldn't be read (no endpoint, the probe
/// failed), which a wipe doesn't take for "no one". A stopped pod has no one on it. Empty: no
/// one is on any.
async fn busy_refusals(remote: &Arc<dyn Remote>, cfg: &Config, pods: &[&arena_core::Pod]) -> Vec<Busy> {
    use arena_core::idle::{self, Probe};
    let running: Vec<&arena_core::Pod> = pods.iter().copied().filter(|p| !arena_core::status::is_stopped(&p.status)).collect();
    let mut probes: Vec<Probe> = running.iter().map(|_| Probe::NoEndpoint).collect();
    let cmd = idle::probe_command(&arena_core::backup::repo_path(cfg));
    let jobs: Vec<(usize, SshTarget, String)> =
        running.iter().enumerate().filter_map(|(i, p)| SshTarget::from_pod(p, cfg).ok().map(|t| (i, t, cmd.clone()))).collect();
    exec_each_pod(remote, jobs, crate::money::IDLE_TIMEOUT, |_, _, &i, call| probes[i] = crate::money::read_probe(call)).await;
    running
        .iter()
        .zip(&probes)
        .filter_map(|(p, probe)| {
            let alternative = REIMAGE_BUSY.replace("<name>", &p.name);
            let presence = idle::presence(probe);
            let why = crate::pods::adopt::presence_words(&presence)?;
            let refusal = crate::pods::replace::presence_refusal("reimage", &p.name, &presence, &alternative)?;
            Some(Busy { why, refusal })
        })
        .collect()
}

/// `pods start`: start the selected STOPPED pods again ([`Provider::start_pod`]), then bring
/// them back usable ([`come_back`]): a RunPod/Vast pod's container disk went at the stop,
/// so it's set up again (unless `--no-setup`) once its endpoint settles, and the proxy is
/// synced. A Hetzner VM kept its disk: only the sync. Pods that aren't stopped are said and
/// left alone. A start the provider refuses (e.g. its host has no free GPU left) is
/// reported per pod and fails the exit; the others carry on.
/// The pods among `pods` whose backend can't do `op` — `Op::Stop` / `Op::Start`: Shadeform has
/// no stop (an instance bills until it's deleted, so there's nothing to stop or start) — each
/// with why ([`Provider::supports`]); then the rest. Pure (no request): `pods stop`/`start` skip
/// the first and act on the second (review SF3).
pub(crate) fn split_no_stop<'a>(provider: &dyn Provider, op: Op, pods: Vec<&'a arena_core::Pod>) -> (Vec<(&'a arena_core::Pod, String)>, Vec<&'a arena_core::Pod>) {
    crate::pods::capable::split_by(provider, op, pods)
}

pub(crate) fn names_of(no_stop: &[(&arena_core::Pod, String)]) -> Vec<String> {
    no_stop.iter().map(|(p, _)| p.name.clone()).collect()
}

/// The refusal when everything `pods stop`/`start` (`what`) would act on has no stop.
pub(crate) fn no_stop_refusal(no_stop: &[(&arena_core::Pod, String)], what: &str) -> String {
    let why = no_stop.first().map(|(_, w)| w.as_str()).unwrap_or_default();
    format!("can't {what} {} — {why}", names_of(no_stop).join(", "))
}

/// The exit of a `pods stop` that skipped pods with no stop (review SF3): **non-zero**, after
/// every other pod was stopped — what was asked ("stop these") isn't done while one of them
/// still bills, and a script or the operator reading the exit must see that, as for a stop that
/// failed. `None` when nothing was skipped.
pub(crate) fn no_stop_left(no_stop: &[(&arena_core::Pod, String)]) -> Option<String> {
    (!no_stop.is_empty()).then(|| {
        format!(
            "{} pod(s) still billing — their provider has no stop: {} (back up, then `arena pods terminate`)",
            no_stop.len(),
            names_of(no_stop).join(", ")
        )
    })
}

pub(crate) async fn handle_start(
    provider: &dyn Provider,
    // How the started pods are set up again: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    opts: RestartOpts,
    settle: SettleWait,
    // Where the per-host API-key CSVs live (`keys_dir`; a temp dir in tests).
    keys_dir: &str,
) -> Result<()> {
    let RestartOpts { no_setup, skip_proxy, dry_run, yes, .. } = opts;
    // A pod whose backend has no stop is never stopped, so there's nothing to start: said and
    // skipped — a selection of only such pods is refused (review SF3: never a veto on the rest).
    let (no_stop, startable) = split_no_stop(provider, Op::Start, sel.pods.iter().collect());
    if startable.is_empty() && !no_stop.is_empty() {
        anyhow::bail!("{}", no_stop_refusal(&no_stop, "start"));
    }
    for (p, why) in &no_stop {
        out!(Err, "skip {} — {why}", p.name);
    }
    for p in startable.iter().filter(|p| !arena_core::status::is_stopped(&p.status)) {
        out!(Err, "skip {} — not stopped ({})", p.name, p.status);
    }
    let pods: Vec<arena_core::Pod> = startable.iter().filter(|p| arena_core::status::is_stopped(&p.status)).map(|p| (*p).clone()).collect();
    if pods.is_empty() {
        out!(Out, "(no stopped pods to start)");
        return Ok(());
    }
    let setup = !no_setup;
    let timeouts = if setup && pods.iter().any(|p| provider.restart_wipes_container_disk(p)) {
        arena_core::setup::SetupConfig::from_config(cfg)
            .context("setting the started pods up again needs the setup config — fix it, or pass --no-setup")?;
        arena_core::setup::SetupTimeouts::from_config(cfg, None)?
    } else {
        arena_core::setup::SetupTimeouts::default()
    };
    let lines: Vec<String> = pods
        .iter()
        .map(|p| {
            let comes = if provider.restart_wipes_container_disk(p) {
                "comes back as a fresh image (its disk went at the stop); only a /workspace volume kept its files"
            } else {
                "disk kept"
            };
            format!("  {} (id={}, {}): {comes}", p.name, p.id, p.provider)
        })
        .collect();
    let wiped = pods.iter().any(|p| provider.restart_wipes_container_disk(p));
    let then = afterwards_line(if wiped { ContainerReset::Yes } else { ContainerReset::No }, setup, skip_proxy);
    let brief = start_brief(provider, &pods, setup);
    if dry_run {
        out!(Detail, "[dry-run] would start {} pod(s):\n{}\n  {then}", pods.len(), lines.join("\n"));
        out!(Brief, "[dry-run] would start {brief}");
        return Ok(());
    }
    let what = format!("Will start {} stopped pod(s):\n{}\n  {then}", pods.len(), lines.join("\n"));
    if !confirm_brief(yes, &what, &format!("Start {brief}, billing again?"))? {
        out!(Out, "aborted.");
        return Ok(());
    }
    let since = tokio::time::Instant::now();
    let (mut started, mut failed) = (Vec::new(), Vec::new());
    for p in &pods {
        match provider.start_pod(&p.id).await {
            Ok(()) => {
                out!(Out, "[started] {}", out::bold(&p.name));
                started.push(p.clone());
            }
            Err(e) => {
                out!(ErrDetail, "[FAILED] {}: {}", p.name, arena_core::lock::explain(&p.name, &e));
                out!(ErrBrief, "{}", failed_on(&p.name, "start", &e, &format!("arena pods start {}", p.name)));
                failed.push(p.name.clone());
            }
        }
    }
    let (fresh, kept): (Vec<arena_core::Pod>, Vec<arena_core::Pod>) =
        started.into_iter().partition(|p| provider.restart_wipes_container_disk(p));
    let back = Comeback { setup, sync: !skip_proxy, settle, timeouts, reset: ContainerReset::Yes };
    let outcome = come_back(provider, remote, cfg, &fresh, back, keys_dir, "start").await;
    if !kept.is_empty() && fresh.is_empty() && !skip_proxy {
        settled_sync(cfg, provider, &kept, settle, "start").await;
    }
    // A VM that kept its disk is back once it's started (and its forward synced): nothing to set
    // up — its READY line, as the dashboard's runs panel reads one.
    for p in &kept {
        out!(Brief, "{}", ready_line(&p.name, since.elapsed(), Some("disk kept")));
    }
    if !failed.is_empty() {
        if let Err(e) = &outcome {
            out!(Err, "{e:#}");
        }
        anyhow::bail!(
            "{} pod(s) failed to start: {} — still stopped (a host with no free GPU left can't start it: \
             retry later, or terminate it and create a new one)",
            failed.len(),
            failed.join(" ")
        );
    }
    outcome
}

/// What a start does to `pods`, grouped, for the default output's dry run and question:
/// `devtest-alpha (a fresh image, set up again)`, `devtest-delta (disk kept)`. Pure (but for
/// the bold).
fn start_brief(provider: &dyn Provider, pods: &[arena_core::Pod], setup: bool) -> String {
    let fresh = if setup { "a fresh image, set up again" } else { "a fresh image, not set up: --no-setup" };
    let (wiped, kept): (Vec<&arena_core::Pod>, Vec<&arena_core::Pod>) = pods.iter().partition(|p| provider.restart_wipes_container_disk(p));
    let names = |ps: &[&arena_core::Pod]| names_brief(&ps.iter().map(|p| p.name.as_str()).collect::<Vec<_>>());
    let mut parts = Vec::new();
    if !wiped.is_empty() {
        parts.push(format!("{} ({fresh})", names(&wiped)));
    }
    if !kept.is_empty() {
        parts.push(format!("{} (disk kept)", names(&kept)));
    }
    parts.join("; ")
}

/// A pod that came back from a start, a restart or a reimage, set up again (or a VM that kept
/// its disk): `[NAME] READY after 2m31s`, as `up` says one — the dashboard's runs panel reads it
/// as the pod's end. Pure (but for the styles).
pub(crate) fn ready_line(name: &str, after: Duration, note: Option<&str>) -> String {
    let note = note.map(|n| format!(" · {n}")).unwrap_or_default();
    format!("[{}] {} after {}{note}", out::bold(name), out::green(out::bold("READY")), arena_core::pipeline::fmt_elapsed(after))
}

/// What a terminate does, in a line, for the default output's dry run and question: every pod
/// named, then what it costs — `devtest-alpha (irreversible; volume devtest-alpha-1 stays,
/// billing)`, `devtest-a, devtest-b (irreversible; unlocks devtest-b first; revokes their
/// OpenRouter keys)`. Pure (but for the bold).
fn terminate_brief(
    pods: &[arena_core::Pod],
    locked: &[&str],
    revoke_key: bool,
    attached: &[(String, arena_core::archive::cloud::CloudVolume)],
    delete_volumes: bool,
) -> String {
    let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
    let mut costs = vec!["irreversible".to_string()];
    if !locked.is_empty() {
        costs.push(format!("unlocks {} first", locked.join(", ")));
    }
    if revoke_key {
        costs.push(if pods.len() == 1 { "revokes its OpenRouter key".into() } else { "revokes their OpenRouter keys".into() });
    }
    let vols: Vec<String> = attached.iter().map(|(_, v)| format!("{} ({} GB)", v.name, v.size_gb)).collect();
    match (vols.as_slice(), delete_volumes) {
        ([], _) => {}
        ([one], true) => costs.push(format!("deletes volume {one} and its files")),
        (many, true) => costs.push(format!("deletes volumes {} and their files", many.join(", "))),
        ([one], false) => costs.push(format!("volume {one} stays, billing")),
        (many, false) => costs.push(format!("volumes {} stay, billing", many.join(", "))),
    }
    format!("{} ({})", names_brief(&names), costs.join("; "))
}

/// `pods terminate`'s switches, passed through to [`handle_terminate`] as one value.
#[derive(Clone, Copy, Default)]
pub(crate) struct TerminateOpts<'a> {
    pub unlock: bool,
    pub revoke_key: bool,
    /// Also delete the Hetzner volumes attached to it (they outlive their server otherwise).
    pub delete_volumes: bool,
    /// The fleet's Hetzner volumes (`None`: no Hetzner key — none to look for).
    pub volumes: Option<&'a dyn arena_core::datavol::VolumeCloud>,
    pub skip_proxy: bool,
    pub dry_run: bool,
    pub yes: bool,
}

/// `pods terminate <target>…` / `--all`, optionally `--revoke-key`. Revocation runs only for
/// pods that actually terminated, after the terminates and the proxy sync — a failed revoke
/// is reported per machine and fails the exit, but never stands in the way of a terminate.
/// Keys go by machine *name*, so a name still held by a pod that is NOT gone (the twin of a
/// double create removed by id, or one whose terminate failed) keeps its key and row: a pod
/// still running keeps a working key.
///
/// A locked pod ([`arena_core::lock`]) is refused — the whole command, before anything is
/// terminated — unless `--unlock`, which unlocks each locked pod right before its terminate
/// (and says so in the confirm text). One whose unlock fails isn't terminated; one unlocked
/// whose terminate then fails is reported as left UNLOCKED. A pod locked since the listing
/// (the API refuses its terminate as locked) is, under `--unlock`, unlocked and retried once.
///
/// Several targets (`terminate apple bloom ember`) are each matched as one is — exactly, no ranges,
/// a shared name refused — all before anything is asked; then one confirm lists them, and the
/// count must be typed back (`ALL` past five, or for every listed pod —
/// [`arena_core::selector::set_confirm_token`], as the dashboard's marked set asks), as must `ALL`
/// for `--all`. One target keeps its `y`. A failure among several is reported and the rest go on,
/// as with `--all`; the exit fails after.
///
/// With `--all`, `--unlock` lifts only this cohort's locks ([`Naming::is_cohort`]): a staff or
/// personal box on the same account (an `@` list entry, or a pod off the list) is often
/// locked precisely so that a fleet-wide command can't take it — RunPod's own refusal kept it
/// alive through `terminate --all` — and the end-of-program one-liner mustn't undo that. Such
/// a pod is KEPT (not unlocked, not terminated) and the exit fails; naming it as the one
/// target (`pods terminate <it> --unlock`) is the operator choosing to.
pub(crate) async fn handle_terminate(
    provider: &dyn Provider,
    cfg: &Config,
    book: &KeyBook<'_>,
    // The pods named (each by the shared single-pod matcher), or `None` = the whole fleet (`--all`).
    targets: Option<&[&str]>,
    opts: TerminateOpts<'_>,
) -> Result<()> {
    let TerminateOpts { unlock, revoke_key, delete_volumes, volumes, skip_proxy, dry_run, yes } = opts;
    // Before anything is listed or touched: revoking needs the provisioning API.
    if revoke_key && book.api.is_none() {
        anyhow::bail!(
            "--revoke-key needs OPENROUTER_PROVISIONING_KEY (keys are found and deleted through the \
             provisioning API) — set it, or drop --revoke-key and `arena keys revoke` later"
        );
    }
    let naming = Naming::from_config(cfg);
    let policy = arena_core::retry::RetryPolicy::default();
    // The whole listing is kept: it says which names other pods still hold.
    let listed = arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?;
    let pods: Vec<arena_core::Pod> = match targets {
        None => {
            let mut all = listed.clone();
            all.sort_by(|a, b| a.name.cmp(&b.name));
            all
        }
        // Each pod matched like every other command's one target (the shared selector's
        // single-pod rule): a typo names the closest pods, a range/`all` is refused, and a
        // name two pods share is refused — pass the id. Every target before anything is asked:
        // one that doesn't match stops them all. A pod named twice is one.
        Some(ts) => {
            let (mut picked, mut errs): (Vec<usize>, Vec<String>) = (Vec::new(), Vec::new());
            for t in ts {
                match arena_core::selector::resolve_one(&naming, &listed, t) {
                    Ok(i) if picked.contains(&i) => {}
                    Ok(i) => picked.push(i),
                    Err(e) if ts.len() == 1 => return Err(e.into()),
                    Err(e) => errs.push(format!("{t}: {e}")),
                }
            }
            if !errs.is_empty() {
                anyhow::bail!("nothing terminated — {}", errs.join("; "));
            }
            picked.into_iter().map(|i| listed[i].clone()).collect()
        }
    };
    // One pod named: its failure is the command's error, and its confirm a `y`. Several, or
    // `--all`: each failure is said and the rest go on, and the confirm wants a typed token.
    let single = targets.is_some_and(|t| t.len() == 1);
    if pods.is_empty() {
        out!(Out, "(no pods to terminate)");
        return Ok(());
    }
    // Every provider can terminate today; asked all the same, as every command asks.
    if let Some(r) = crate::pods::capable::refusal(provider, Op::Terminate, "terminate", &pods.iter().collect::<Vec<_>>()) {
        anyhow::bail!("{r}");
    }
    // Whose lock `--unlock` may lift: a pod named, or (`--all`) a cohort pod's.
    let may_unlock = |p: &arena_core::Pod| targets.is_some() || naming.is_cohort(&p.name);
    let foreign_locked: Vec<String> =
        pods.iter().filter(|p| arena_core::lock::is_locked(p) && !may_unlock(p)).map(|p| p.name.clone()).collect();
    let kept_why = |names: &[String]| {
        format!(
            "{} KEPT, not terminated: locked, and not this cohort's (`{}-…`) — `--all --unlock` lifts only the \
             cohort's locks (a staff box's lock is what protects it); if it really should go, name it: \
             `arena pods terminate {} --unlock`",
            names.join(", "),
            naming.prefix,
            names.first().map_or("<name>", String::as_str)
        )
    };
    let (kept, pods): (Vec<arena_core::Pod>, Vec<arena_core::Pod>) =
        pods.into_iter().partition(|p| unlock && arena_core::lock::is_locked(p) && !may_unlock(p));
    let kept_names: Vec<String> = kept.iter().map(|p| p.name.clone()).collect();
    if pods.is_empty() {
        // Only kept pods were selected: nothing to do, and the operator asked for more.
        if dry_run {
            print_would_refuse(&kept_why(&kept_names));
            return Ok(());
        }
        anyhow::bail!("{}", kept_why(&kept_names));
    }
    let label = |p: &arena_core::Pod| format!("{} (id={}, {})", p.name, p.id, p.provider);
    let refs: Vec<&arena_core::Pod> = pods.iter().collect();
    let locked: Vec<&str> = arena_core::lock::locked(refs.iter().copied()).iter().map(|p| p.name.as_str()).collect();
    let refused = if unlock {
        None
    } else {
        let mut extra = " — or pass --unlock to lift the lock as part of the terminate".to_string();
        if !foreign_locked.is_empty() {
            extra.push_str(&format!(
                " (with --all it unlocks only the cohort's: {} would be KEPT, locked — not this cohort's)",
                foreign_locked.join(", ")
            ));
        }
        arena_core::lock::refusal("terminate", &refs, &extra)
    };
    let then_keys = if revoke_key {
        format!(", revoke the OpenRouter key(s) named after each and drop their rows from {}", book.csv.display())
    } else {
        String::new()
    };
    let then_sync = if skip_proxy { "" } else { ", then sync the proxy" };
    // A Hetzner volume outlives its server (and bills on): said for each, deleted only with
    // --delete-volumes — whose listing must work before anything is terminated.
    let attached: Vec<(String, arena_core::archive::cloud::CloudVolume)> = match volumes {
        Some(cloud) => match crate::pods::datavol::attached_volumes(cloud, &pods.iter().collect::<Vec<_>>()).await {
            Ok(v) => v,
            Err(e) if delete_volumes => anyhow::bail!("--delete-volumes: {e:#} — nothing was terminated"),
            Err(e) => {
                out!(
                    ErrDetail,
                    "warning: {e:#} — can't say whether a volume is attached (a Hetzner volume outlives its server and bills on: \
                     `arena pods volume list`)"
                );
                out!(ErrBrief, "{}", out::warn(format!("volumes not listed ({}): one attached outlives its server and bills on (`arena pods volume list`)", reason_of(&e))));
                Vec::new()
            }
        },
        None if delete_volumes && pods.iter().any(|p| p.provider == "hetzner") => {
            anyhow::bail!("--delete-volumes: volumes are Hetzner's, and HETZNER_API_KEY isn't set — nothing was terminated")
        }
        None => Vec::new(),
    };
    let volume_lines = |p: &arena_core::Pod| -> Vec<String> {
        attached
            .iter()
            .filter(|(id, _)| *id == p.id)
            .map(|(_, v)| {
                if delete_volumes {
                    format!("then DELETE its volume {} ({} GB, id={}) and its files", v.name, v.size_gb, v.id)
                } else {
                    format!(
                        "its volume {} ({} GB) is KEPT — Hetzner doesn't delete it with the server, and it bills until `arena pods \
                         volume delete {}` (or pass --delete-volumes)",
                        v.name, v.size_gb, v.name
                    )
                }
            })
            .collect()
    };
    let brief = terminate_brief(&pods, &locked, revoke_key, &attached, delete_volumes);
    if dry_run {
        let leaving: Vec<&str> = pods.iter().map(|p| p.id.as_str()).collect();
        let staying = |p: &arena_core::Pod| shared_name_holder(&listed, &leaving, &p.name).map(|o| o.id.clone());
        for p in &pods {
            let how = if unlock && arena_core::lock::is_locked(p) { "unlock, then terminate" } else { "terminate" };
            out!(Detail, "[dry-run] would {how} {}", label(p));
            for l in volume_lines(p) {
                out!(Detail, "[dry-run]   {l}");
            }
            if revoke_key {
                match staying(p) {
                    Some(id) => out!(Out, "[dry-run]   key kept: pod id {id} (not being terminated) is also named {}", p.name),
                    None => out!(
                        Detail,
                        "[dry-run]   then revoke OpenRouter key(s) named {} and drop its row from {}",
                        machine_key_names(&naming, &p.name).join(" / "),
                        book.csv.display()
                    ),
                }
            }
        }
        out!(Brief, "[dry-run] would {}{brief}", if unlock && !locked.is_empty() { "unlock and terminate " } else { "terminate " });
        if let Some(r) = &refused {
            print_would_refuse(r);
        }
        for p in &kept {
            out!(Out, "[dry-run] would KEEP {} — locked, not this cohort's", label(p));
        }
        if !kept.is_empty() {
            print_would_refuse(&format!("{} — the exit fails", kept_why(&kept_names)));
        }
        out!(Detail, "\nDry-run only — would terminate {} pod(s){then_keys}{then_sync} (preview).", pods.len());
        return Ok(());
    }
    if let Some(r) = refused {
        anyhow::bail!("{r}");
    }
    // `--unlock` is named in the prompt, with the pods it unlocks — a lock is lifted only on
    // that explicit yes.
    let unlocking = if unlock && !locked.is_empty() {
        format!("UNLOCK {} (locked), then ", locked.join(", "))
    } else {
        String::new()
    };
    let keeping = if kept.is_empty() {
        String::new()
    } else {
        format!(" KEEPING {} (locked, not this cohort's — not unlocked).", kept_names.join(", "))
    };
    let vols = match (attached.len(), delete_volumes) {
        (0, _) => String::new(),
        (n, true) => format!(" DELETING {n} attached volume(s) too: {}.", attached.iter().map(|(_, v)| v.name.as_str()).collect::<Vec<_>>().join(", ")),
        (n, false) => format!(
            " KEEPING {n} attached volume(s), billing: {} (--delete-volumes deletes them).",
            attached.iter().map(|(_, v)| v.name.as_str()).collect::<Vec<_>>().join(", ")
        ),
    };
    let verb = if unlocking.is_empty() { "Terminate" } else { "Unlock and terminate" };
    let go = if single {
        let what = format!("Will {unlocking}TERMINATE {} — irreversible{then_keys}{then_sync}.{vols}", label(&pods[0]));
        confirm_brief(yes, &what, &format!("{verb} {brief}?"))?
    } else {
        let what = match targets {
            None => format!("Will {unlocking}TERMINATE ALL {} pod(s) — irreversible{then_keys}{then_sync}.{keeping}{vols}", pods.len()),
            Some(_) => format!("Will {unlocking}TERMINATE these {} pods — irreversible{then_keys}{then_sync}.{vols}", pods.len()),
        };
        let list: Vec<String> = pods.iter().map(|p| format!("  {}", label(p))).collect();
        let token = if targets.is_none() { "ALL".to_string() } else { arena_core::selector::set_confirm_token(pods.len(), listed.len()) };
        // The default output's question is one line, every pod named (the token is the same).
        let all = if targets.is_none() { "ALL " } else { "" };
        let short = format!("{} {all}{brief}.{}", verb.to_uppercase(), keeping);
        crate::confirm_typed_brief(yes, &format!("{what}\n{}", list.join("\n")), &short, &token)?
    };
    if !go {
        out!(Out, "aborted.");
        return Ok(());
    }
    // The fleet provider routes each terminate (and unlock) to the backend that owns the pod.
    let mut gone: Vec<&arena_core::Pod> = Vec::new();
    for p in &pods {
        let retry = format!("arena pods terminate {}{}", p.name, if unlock { " --unlock" } else { "" });
        let mut unlocked = false;
        if unlock && arena_core::lock::is_locked(p) {
            if let Err(e) = arena_core::retry::retrying(&policy, || provider.set_locked(&p.id, false)).await {
                let why = format!("{}: unlocking failed, so it was NOT terminated: {e}", p.name);
                if single {
                    anyhow::bail!("{why}");
                }
                out!(ErrDetail, "[FAILED] {why}");
                out!(ErrBrief, "{}", failed_line(&p.name, "unlock", &format!("{}, so it was NOT terminated", out::reason(&e)), &retry));
                continue;
            }
            out!(Out, "[unlocked] {}", p.name);
            unlocked = true;
        }
        // An error: the whole of it (`-v`, and a single pod's error), and its reason.
        let failed = |full: String, e: &arena_core::Error| {
            let why = if e.is_locked() { "locked".to_string() } else { out::reason(e) };
            (full, why)
        };
        let outcome: std::result::Result<(), (String, String)> = match provider.terminate_pod(&p.id).await {
            Ok(()) => Ok(()),
            // Locked since the listing (which said unlocked, or didn't say): under --unlock
            // that is the lock the operator asked to lift — once, then one more terminate.
            // (Not a pod `--all --unlock` may not unlock: that one reads as the hint.)
            Err(e) if e.is_locked() && unlock && !unlocked && may_unlock(p) => {
                match arena_core::retry::retrying(&policy, || provider.set_locked(&p.id, false)).await {
                    Err(ue) => Err((
                        format!("{}: locked since it was listed, and unlocking failed, so it was NOT terminated: {ue}", p.name),
                        format!("locked since it was listed, and unlocking failed ({})", out::reason(&ue)),
                    )),
                    Ok(()) => {
                        out!(Out, "[unlocked] {} (locked since it was listed)", p.name);
                        unlocked = true;
                        provider
                            .terminate_pod(&p.id)
                            .await
                            .map_err(|e| failed(format!("{}: {}", p.name, arena_core::lock::explain(&p.name, &e)), &e))
                    }
                }
            }
            Err(e) => Err(failed(format!("{}: {}", p.name, arena_core::lock::explain(&p.name, &e)), &e)),
        };
        match outcome {
            Ok(()) => {
                out!(Detail, "[terminated] {}", if targets.is_some() { label(p) } else { p.name.clone() });
                out!(Brief, "[terminated] {}", out::bold(&p.name));
                gone.push(p);
            }
            Err((mut why, brief_why)) => {
                // A lock is lifted by --unlock: the retry says so.
                let retry = if brief_why == "locked" && !unlock { format!("{retry} --unlock") } else { retry.clone() };
                let mut brief_line = failed_line(&p.name, "terminate", &brief_why, &retry);
                if unlocked {
                    why.push_str(&format!(
                        " — it is now UNLOCKED: `arena pods lock {}` to protect it again, or retry the terminate",
                        p.name
                    ));
                    brief_line.push_str(&format!(" — it is now UNLOCKED (`arena pods lock {}` protects it again)", p.name));
                }
                // One named pod: its failure is the command's error (nothing else to do).
                if single {
                    anyhow::bail!("{why}");
                }
                out!(ErrDetail, "[FAILED] {why}");
                out!(ErrBrief, "{brief_line}");
            }
        }
    }
    for p in &kept {
        out!(Err, "[KEPT] {} — locked, not this cohort's: not unlocked, not terminated", p.name);
    }
    if !single {
        let (n, of) = (gone.len(), pods.len() + kept.len());
        out!(Detail, "\nterminated {n}/{of}");
        out!(Brief, "{}", if n == of { out::ok(format!("terminated {n} of {of}")) } else { format!("terminated {n} of {of}") });
    }
    // Sync whatever did go (the merge only drops what the providers confirm gone).
    if !gone.is_empty() && !skip_proxy {
        sync_proxy(cfg, provider, "terminate").await;
    }
    // The volumes of what went: deleted (--delete-volumes) or said to be kept, billing.
    let mut volumes_failed = Vec::new();
    let gone_volumes: Vec<arena_core::archive::cloud::CloudVolume> =
        attached.iter().filter(|(id, _)| gone.iter().any(|p| p.id == *id)).map(|(_, v)| v.clone()).collect();
    // Only the volumes arena made are deleted; one attached by hand is someone else's to delete.
    let (ours, by_hand): (Vec<_>, Vec<_>) =
        gone_volumes.into_iter().partition(|v| arena_core::datavol::is_ours(v, naming.prefix) || !delete_volumes);
    if let (Some(cloud), true, false) = (volumes, delete_volumes, ours.is_empty()) {
        volumes_failed = crate::pods::datavol::delete_after_terminate(cloud, &ours).await;
    } else {
        for v in &ours {
            out!(Out, "[kept] volume {} ({} GB) — detached, billing until `arena pods volume delete {}`", v.name, v.size_gb, v.name);
        }
    }
    for v in &by_hand {
        out!(Out, "[kept] volume {} ({} GB) — attached by hand, so not deleted here: delete it in the Hetzner console", v.name, v.size_gb);
    }
    let mut revoked = KeyRevoke::default();
    if revoke_key && !gone.is_empty() {
        // Each machine name once (two pods sharing a name are one key), and none that a pod
        // still standing holds — compared against the pre-terminate listing.
        let leaving: Vec<&str> = gone.iter().map(|p| p.id.as_str()).collect();
        let (mut seen, mut machines): (Vec<&str>, Vec<String>) = (Vec::new(), Vec::new());
        for p in &gone {
            if seen.contains(&p.name.as_str()) {
                continue;
            }
            seen.push(&p.name);
            match shared_name_holder(&listed, &leaving, &p.name) {
                Some(other) => out!(
                    Out,
                    "[keys] = {}: key and CSV row kept — pod id {} still has that name (not terminated)",
                    p.name, other.id
                ),
                None => machines.push(p.name.clone()),
            }
        }
        if !machines.is_empty() {
            revoked = revoke_machine_keys(book, &naming, &machines, &spend::SpendCtx::from_config(cfg)).await;
        }
    }
    let mut problems = Vec::new();
    if !volumes_failed.is_empty() {
        problems.push(format!("deleting volume(s) {} failed — the pod(s) ARE terminated", volumes_failed.join(", ")));
    }
    if gone.len() < pods.len() {
        problems.push(format!("{} pod(s) failed to terminate", pods.len() - gone.len()));
    }
    if !kept.is_empty() {
        problems.push(kept_why(&kept_names));
    }
    if !revoked.failed.is_empty() {
        problems.push(format!(
            "revoking the OpenRouter key failed for {} — the pod(s) ARE terminated; retry with `arena keys revoke {}`",
            revoked.failed.join(", "),
            revoked.failed.join(" ")
        ));
    }
    if let Some((names, why)) = &revoked.stale_rows {
        problems.push(format!(
            "the OpenRouter key(s) of {} ARE revoked (or were already gone), but their rows couldn't be \
             removed from {} ({why}) — delete those rows by hand (they hold dead keys)",
            names.join(", "),
            book.csv.display()
        ));
    }
    if !problems.is_empty() {
        anyhow::bail!("{}", problems.join("; "));
    }
    Ok(())
}

/// What [`revoke_machine_keys`] couldn't do.
#[derive(Debug, Default)]
struct KeyRevoke {
    /// Machines whose key(s) couldn't be revoked (or listed); their CSV rows are kept.
    failed: Vec<String>,
    /// Machines whose keys ARE revoked but whose rows couldn't be removed from the CSV, and
    /// why — a local-file problem, not something `keys revoke` can retry.
    stale_rows: Option<(Vec<String>, String)>,
}

/// Revoke every OpenRouter key that is each of `machines`' (pods just terminated; see
/// [`machine_key_names`]) and drop their rows from the keys CSV. One full key listing for
/// the batch; each key deleted at most once. A machine with no key is said, not failed, and
/// its row (a dead key, if any) is dropped. A row whose key couldn't be deleted stays, as
/// the record that it still works.
async fn revoke_machine_keys(book: &KeyBook<'_>, naming: &Naming<'_>, machines: &[String], tracking: &spend::SpendCtx) -> KeyRevoke {
    let mut out = KeyRevoke::default();
    let Some(api) = book.api else {
        out.failed = machines.to_vec(); // the caller checked up front
        return out;
    };
    let listed_at = arena_core::snapshot::unix_now();
    let keys = match api.list_keys().await {
        Ok(k) => k,
        Err(e) => {
            for m in machines {
                out!(Err, "[keys] ✗ {m}: listing OpenRouter keys failed: {e}");
            }
            out.failed = machines.to_vec();
            return out;
        }
    };
    // Each key's usage before any is deleted: a revoked key's spend stays its owner's.
    tracking.keys(&keys, listed_at);
    let mut csv = book.read_csv();
    // Each row as read before any key goes: at the write, only a row still holding this secret is
    // dropped (one another arena saved meanwhile — a parallel `up`'s fresh key — is kept).
    let as_read: std::collections::HashMap<String, String> = csv.as_deref().map(arena_core::apikeys::parse_csv).unwrap_or_default().into_iter().collect();
    let mut dropped = Vec::new();
    let mut deleted: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in machines {
        let names = machine_key_names(naming, m);
        // Its keys by name, and its CSV row's key renamed on OpenRouter (`keys::keys_of`).
        let mine: Vec<&arena_core::openrouter::KeyInfo> = crate::keys::keys_of(naming, m, as_read.get(m).map(String::as_str), &keys)
            .into_iter()
            .filter(|k| !deleted.contains(k.hash.as_str()))
            .collect();
        let mut errs = Vec::new();
        for k in &mine {
            match api.delete_key(&k.hash).await {
                Ok(()) => {
                    deleted.insert(k.hash.as_str());
                }
                Err(e) => errs.push(e.to_string()),
            }
        }
        tracking.keys_deleted(&mine, &mine.iter().filter(|k| deleted.contains(k.hash.as_str())).map(|k| k.hash.clone()).collect::<Vec<_>>());
        if !errs.is_empty() {
            out!(Err, "[keys] ✗ {m}: revoke failed ({}) — its CSV row is kept", errs.join("; "));
            out.failed.push(m.clone());
            continue;
        }
        let row = match csv.as_deref().and_then(|t| arena_core::apikeys::remove_csv_host(t, m)) {
            Some(t) => {
                csv = Some(t);
                dropped.push(m.clone());
                "; dropped its CSV row"
            }
            None => "",
        };
        match mine.len() {
            0 if row.is_empty() => out!(Detail, "[keys] = {m}: no OpenRouter key ({}), no CSV row", names.join(" / ")),
            0 => out!(Detail, "[keys] ✓ {m}: no OpenRouter key named {} (already revoked?){row}", names.join(" / ")),
            n => out!(Out, "[keys] ✓ {m}: revoked {n} OpenRouter key(s){row}"),
        }
    }
    if csv.is_some() && !dropped.is_empty() {
        // The rows go from the CSV as it is now, under the keys folder's lock — each only while it
        // still holds the secret read before the revoke: a key another arena saved meanwhile (a
        // parallel `up`'s mint for this name, or another's) keeps its row.
        let mut changed: Vec<String> = Vec::new();
        let written = book.lock().and_then(|_held| {
            let Some(now) = book.read_csv() else { return Ok(()) };
            let rows: std::collections::HashMap<String, String> = arena_core::apikeys::parse_csv(&now).into_iter().collect();
            let mut rest = now.clone();
            for m in &dropped {
                if rows.get(m) != as_read.get(m) {
                    changed.push(m.clone());
                } else if let Some(t) = arena_core::apikeys::remove_csv_host(&rest, m) {
                    rest = t;
                }
            }
            if rest == now {
                return Ok(());
            }
            book.write_csv(&rest)
        });
        for m in &changed {
            out!(Out, "[keys] = {m}: its CSV row changed while its key was revoked (a key saved meanwhile) — kept");
        }
        if let Err(e) = written {
            out!(Err, "[keys] ✗ couldn't update {}: {e} — rows kept for {}", book.csv.display(), dropped.join(", "));
            out.stale_rows = Some((dropped, e.to_string()));
        }
    }
    out
}

/// One line per pod for `stop`'s dry run and prompt (pure, so tested): which pod, and what
/// the stop does to its disk (judged by where that pod's repo is).
pub(crate) fn stop_lines(pods: &[&arena_core::Pod], fates: &[DiskFate], sites: &[RepoSite]) -> Vec<String> {
    pods.iter()
        .zip(fates)
        .zip(sites)
        .map(|((p, f), site)| format!("  {} (id={}, {}): {}", p.name, p.id, p.provider, f.describe(DiskOp::Stop, site)))
        .collect()
}

/// What a restart (or a stop, then a start) does to one pod's files: what its confirm text
/// says, and whether it needs `--wipe-ok`. Both also take where the ARENA repo is
/// ([`RepoSite`]: the configured `arena_core::backup::repo_path`, refined by the pod's own
/// answer — [`repo_sites`]): the gate is about whether the participants' work survives. With
/// the image's layout (`/root/<repo>`) it is NOT on the volume unless setup moved it there
/// (it does when the pod has one: `arena_core::volume`) — a pod having a `/workspace` volume
/// doesn't by itself make a restart safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiskFate {
    /// The backend keeps the disk (a Hetzner VM).
    Kept,
    /// The container is reset to its image. `volume_gb`: the persistent volume (at
    /// /workspace) that survives — `Some(0)` = none, `None` = couldn't be read.
    Wiped { volume_gb: Option<u32> },
    /// A restart that isn't known to reset the container ([`ContainerReset::Unknown`]: RunPod
    /// v1's was seen keeping it, live 2026-10-09): gated and set up again exactly like
    /// [`DiskFate::Wiped`], but only ever said to be a possible wipe.
    MayWipe { volume_gb: Option<u32> },
}

/// The operations that reset a container disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiskOp {
    Restart,
    Stop,
    /// A fresh container from an image (`pods reimage`) — a wipe on every backend.
    Reimage,
    /// A VM's whole disk rebuilt from an image (`pods reimage` on Hetzner): the server, its IP
    /// and any volume of its own kept, everything on its disk gone.
    Rebuild,
}

impl DiskFate {
    /// A wipe that the participants' work (the repo, where `site` says) doesn't survive: no
    /// volume, none confirmed (an unreadable spec counts as none: guessing "it has one" is the
    /// data-loss direction), or a volume the repo isn't on.
    pub(crate) fn needs_wipe_ok(self, site: &RepoSite) -> bool {
        match self {
            DiskFate::Kept => false,
            DiskFate::Wiped { volume_gb: Some(v) } | DiskFate::MayWipe { volume_gb: Some(v) } if v > 0 => !site.on_volume(),
            DiskFate::Wiped { .. } | DiskFate::MayWipe { .. } => true,
        }
    }

    /// What the operation is known to have done to the container disk, for what the command
    /// says afterwards ([`come_back`]).
    pub(crate) fn reset(self) -> ContainerReset {
        match self {
            DiskFate::Kept => ContainerReset::No,
            DiskFate::Wiped { .. } => ContainerReset::Yes,
            DiskFate::MayWipe { .. } => ContainerReset::Unknown,
        }
    }

    /// One line for the prompt / dry run: what's lost and what survives, naming where the
    /// repo is. Pure, so tested.
    pub(crate) fn describe(self, op: DiskOp, site: &RepoSite) -> String {
        use arena_core::provider::runpod_v2::VOLUME_MOUNT_PATH as VOL;
        let wipe = match op {
            DiskOp::Restart if matches!(self, DiskFate::MayWipe { .. }) => {
                "⚠ may WIPE the container disk (this backend's restart isn't known to reset it to the image \
                 every time — counted as if it does)"
            }
            DiskOp::Restart => "⚠ WIPES the container disk (reset to the image)",
            DiskOp::Stop => "⚠ discards the container disk (it starts again as a fresh image)",
            DiskOp::Reimage => "⚠ WIPES the container disk (a fresh container from the image)",
            DiskOp::Rebuild => "⚠ WIPES the VM's whole disk (rebuilt from the image; it keeps its IP)",
        };
        let rest = "~/.name, setup's git remote, the distributed keys";
        let repo = site.describe();
        let work = format!("the ARENA repo at {repo} (participants' work)");
        // A stopped pod comes back only when someone starts it — no automatic re-setup then.
        let relink = match op {
            DiskOp::Restart | DiskOp::Reimage | DiskOp::Rebuild => "",
            DiskOp::Stop => " (`arena pods start` brings it back and re-links the repo)",
        };
        match self {
            DiskFate::Kept => match op {
                DiskOp::Restart => "disk kept — a hard reset (power-cycle): running processes die, files stay".into(),
                DiskOp::Stop => "disk kept — shut down (powered off only if it hangs), files stay".into(),
                DiskOp::Reimage | DiskOp::Rebuild => "disk kept".into(),
            },
            // A VM has no /workspace volume to keep the repo: all of its disk goes. A Hetzner
            // volume of its own is another disk, kept — but its mount (an fstab line) isn't.
            _ if op == DiskOp::Rebuild => format!(
                "{wipe}: nothing on its disk survives — {work}, {rest} (a volume of its own, `arena pods volume`, keeps \
                 its files but comes back unmounted: `arena pods volume mount` mounts it again)"
            ),
            DiskFate::Wiped { volume_gb: Some(v) } | DiskFate::MayWipe { volume_gb: Some(v) } if v > 0 && site.on_volume() => format!(
                "{wipe}: everything outside {VOL} is lost ({rest}, files outside the repo); the repo at \
                 {repo} is on the {v} GB volume at {VOL} and survives{relink}"
            ),
            DiskFate::Wiped { volume_gb: Some(v) } | DiskFate::MayWipe { volume_gb: Some(v) } if v > 0 => format!(
                "{wipe}: everything outside {VOL} is lost — {work} is NOT on the volume, and {rest}; \
                 only the {v} GB volume at {VOL} survives"
            ),
            DiskFate::Wiped { volume_gb: Some(_) } | DiskFate::MayWipe { volume_gb: Some(_) } => {
                format!("{wipe} and it has NO persistent volume: nothing survives — {work}, {rest}")
            }
            DiskFate::Wiped { volume_gb: None } | DiskFate::MayWipe { volume_gb: None } => {
                format!("{wipe}; couldn't read whether it has a volume, so assume nothing survives — {work}, {rest}")
            }
        }
    }
}

/// Where the gate judges each pod's repo to be. Only a wipe *with* a volume makes the answer
/// matter (no volume: nothing survives anyway; a kept disk: nothing is lost), so only those
/// pods are asked — read-only, all at once, each within [`PROBE_TIMEOUT`] — where their repo
/// really lives: setup symlinks it onto the volume, which the configured path alone can't
/// show. No answer (unreachable, no endpoint, an unexpected reply) → the configured path, as
/// before this check.
pub(crate) async fn repo_sites(
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    pods: &[&arena_core::Pod],
    fates: &[DiskFate],
    repo: &str,
) -> Vec<RepoSite> {
    let mut sites: Vec<RepoSite> = pods.iter().map(|_| RepoSite::configured(repo)).collect();
    let probe = arena_core::volume::probe_command(repo);
    let jobs: Vec<(usize, SshTarget, String)> = pods
        .iter()
        .zip(fates)
        .enumerate()
        .filter(|(_, (_, fate))| matches!(fate, DiskFate::Wiped { volume_gb: Some(v) } | DiskFate::MayWipe { volume_gb: Some(v) } if *v > 0))
        .filter_map(|(i, (pod, _))| SshTarget::from_pod(pod, cfg).ok().map(|t| (i, t, probe.clone())))
        .collect();
    exec_each_pod(remote, jobs, PROBE_TIMEOUT, |_, _, &i, call| {
        match call {
            Ok(out) if out.success => {
                sites[i] = RepoSite::resolve(repo, arena_core::volume::parse_probe(&out.stdout).as_ref());
            }
            _ => {}
        }
    })
    .await;
    sites
}

/// A pod's [`DiskFate`]: the owning backend says whether a restart wipes; if it does, the
/// pod's spec says whether a volume survives (read-only, bounded like a listing — an error
/// or a stall reads as "unknown", which the gate treats like no volume).
pub(crate) async fn disk_fate(provider: &dyn Provider, pod: &arena_core::Pod) -> DiskFate {
    if !provider.restart_wipes_container_disk(pod) {
        return DiskFate::Kept;
    }
    let spec = arena_core::ratelimit::bounded(arena_core::provider::LIST_TIMEOUT, provider.pod_spec(&pod.id)).await;
    DiskFate::Wiped { volume_gb: match spec {
        Ok(Ok(s)) => Some(s.volume_gb),
        _ => None,
    } }
}

/// A restart's [`DiskFate`]: [`disk_fate`], except that a backend whose restart isn't known to
/// reset the container ([`ContainerReset::Unknown`]: RunPod v1, Vast) gives
/// [`DiskFate::MayWipe`] — the same gate and re-setup, without the claim. The two provider
/// answers agree on what is gated (`ContainerReset::may_wipe`, by the trait's default).
pub(crate) async fn restart_fate(provider: &dyn Provider, pod: &arena_core::Pod) -> DiskFate {
    match (disk_fate(provider, pod).await, provider.restart_resets_container(pod)) {
        (DiskFate::Wiped { volume_gb }, ContainerReset::Unknown) => DiskFate::MayWipe { volume_gb },
        (fate, _) => fate,
    }
}

/// What a reimage does to a pod's disk: on every backend a fresh container (a Hetzner VM is
/// rebuilt), so only a persistent volume survives — whatever `restart_wipes_container_disk`
/// says about a restart. The volume is read from the pod's spec (bounded; unknown = none; a VM
/// has none: its spec says 0).
async fn reimage_fate(provider: &dyn Provider, pod: &arena_core::Pod) -> DiskFate {
    let spec = arena_core::ratelimit::bounded(arena_core::provider::LIST_TIMEOUT, provider.pod_spec(&pod.id)).await;
    DiskFate::Wiped { volume_gb: match spec {
        Ok(Ok(s)) => Some(s.volume_gb),
        _ => None,
    } }
}

/// Why a restart/stop is refused without `--wipe-ok` (pure, so tested): it would destroy
/// the participants' work on these pods, and `--yes` is not an answer to that — the
/// operator has to say so explicitly, after a backup. `maybe`: a restart that isn't known to
/// reset ([`DiskFate::MayWipe`]) — refused all the same, but said as a possibility.
pub(crate) fn wipe_refusal(op: DiskOp, maybe: bool, names: &[&str], repo: &str) -> String {
    let (verb, done) = match op {
        DiskOp::Restart if maybe => (
            "restart",
            "may reset the container disk to the image (this backend's restart isn't known to reset it every time, so it counts as one that does)",
        ),
        DiskOp::Restart => ("restart", "resets the container disk to the image"),
        DiskOp::Stop => ("stop", "discards the container disk (a stopped RunPod pod keeps no data)"),
        DiskOp::Reimage => ("reimage", "replaces the container — and its disk — with a fresh one from the image"),
        DiskOp::Rebuild => ("reimage", "rebuilds the VM's whole disk from the image"),
    };
    let alt = match op {
        DiskOp::Restart | DiskOp::Reimage | DiskOp::Rebuild => "",
        DiskOp::Stop => " If the group is done, terminate instead.",
    };
    // A VM has no volume the repo could be on: all of its disk goes.
    let lost = match op {
        DiskOp::Rebuild => "is on that disk",
        _ => "isn't on a confirmed persistent volume (there's no volume, none could be confirmed, or the repo lives \
              outside /workspace)",
    };
    format!(
        "refusing to {verb} {} without --wipe-ok: this {done}, and the participants' work — the repo \
         at {repo} — {lost}, so it would be lost. Back up first (`arena pods backup {}`), then re-run with \
         --wipe-ok (--yes alone isn't enough).{alt}",
        names.join(", "),
        names.join(" ")
    )
}

/// `pods restart`'s switches, passed through to [`handle_restart`] as one value.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RestartOpts {
    pub wipe_ok: bool,
    pub no_setup: bool,
    pub skip_proxy: bool,
    pub dry_run: bool,
    pub yes: bool,
}

/// How long to wait for a restarted pod's SSH endpoint to settle before re-provisioning
/// it. A parameter so tests don't sleep.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SettleWait {
    pub stable_secs: u64,
    pub timeout_secs: u64,
    pub poll: Duration,
}

/// A restarted container can keep answering on its old endpoint for a moment and then
/// move: wait until the endpoint has stayed put for 30 s (as `reimage` does) so setup lands
/// on the *new* container, not the one being torn down — up to 10 min.
pub(crate) const RESTART_SETTLE: SettleWait = SettleWait { stable_secs: 30, timeout_secs: 600, poll: Duration::from_secs(15) };

/// `pods restart <target>`. On a backend whose restart wipes the container disk (RunPod v2)
/// or may (RunPod v1, Vast: [`restart_fate`]) the prompt says plainly what's lost and what
/// survives, a pod whose repo isn't on a confirmed volume is refused without `--wipe-ok` —
/// even with `--yes` — and afterwards the pod is brought back usable: wait for its endpoint
/// to settle, re-run setup on it (`~/.name`, deploy key + git remote, API keys; `--no-setup`
/// skips), sync the proxy. Only a reset known to happen is said to have. A Hetzner reset
/// keeps the disk: no gate, no re-setup, just the sync.
pub(crate) async fn handle_restart(
    provider: &dyn Provider,
    // How we reach the pod for the re-setup: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    target: &str,
    opts: RestartOpts,
    settle: SettleWait,
    // Where the per-host API-key CSVs live (`keys_dir`; a temp dir in tests).
    keys_dir: &str,
) -> Result<()> {
    // A malformed SETUP_TIMEOUT_SECS fails before anything is touched (as in `up`).
    let timeouts = if opts.no_setup {
        arena_core::setup::SetupTimeouts::default()
    } else {
        arena_core::setup::SetupTimeouts::from_config(cfg, None)?
    };
    let policy = arena_core::retry::RetryPolicy::default();
    let pods = arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?;
    let pod = pods[arena_core::selector::resolve_one(&Naming::from_config(cfg), &pods, target)?].clone();
    let label = format!("{} (id={}, {})", pod.name, pod.id, pod.provider);
    // Every provider can restart today; asked all the same, as every command asks.
    if let Some(r) = crate::pods::capable::refusal(provider, Op::Restart, "restart", &[&pod]) {
        anyhow::bail!("{r}");
    }
    // A restart needs a running pod: RunPod v2 answers 409 for a stopped one, v1 answered
    // 2xx and left it stopped (then a 10-min endpoint wait) — live finding #15.
    if arena_core::status::is_stopped(&pod.status) {
        anyhow::bail!("{}", stopped_restart_refusal(&pod, provider.restart_wipes_container_disk(&pod)));
    }
    // A locked pod refuses a restart (RunPod itself does): say so before anything is read.
    let locked = arena_core::lock::refusal("restart", &[&pod], "");
    if let (Some(r), false) = (&locked, opts.dry_run) {
        anyhow::bail!("{r}");
    }
    let fate = restart_fate(provider, &pod).await;
    let repo = arena_core::backup::repo_path(cfg);
    let site = repo_sites(&remote, cfg, &[&pod], &[fate], &repo).await.remove(0);
    let wiped = fate.reset().may_wipe();
    let setup = wiped && !opts.no_setup;
    if setup {
        // Setup is what brings a wiped pod back: make sure it can run before wiping one.
        arena_core::setup::SetupConfig::from_config(cfg)
            .context("re-running setup after the restart needs the setup config — fix it, or pass --no-setup")?;
    }
    let then = afterwards_line(fate.reset(), setup, opts.skip_proxy);
    let what = format!("restart {label}:\n  {}\n  {then}", fate.describe(DiskOp::Restart, &site));
    // By default the pod and what the restart does to it, in a line (docs/OUTPUT.md).
    let again = if setup { "; set up again" } else { "" };
    let brief = format!("{} ({}{again})", out::bold(&pod.name), fate_words(DiskOp::Restart, fate, &site, 1));
    if opts.dry_run {
        out!(Detail, "[dry-run] would {what}");
        out!(Brief, "[dry-run] would restart {brief}");
        if let Some(r) = &locked {
            print_would_refuse(r);
        }
        if fate.needs_wipe_ok(&site) && !opts.wipe_ok {
            out!(Out, "(would be refused without --wipe-ok)");
        }
        return Ok(());
    }
    if fate.needs_wipe_ok(&site) && !opts.wipe_ok {
        let maybe = matches!(fate, DiskFate::MayWipe { .. });
        anyhow::bail!("{}", wipe_refusal(DiskOp::Restart, maybe, &[pod.name.as_str()], &repo));
    }
    if !confirm_brief(opts.yes, &format!("Will {what}"), &format!("Restart {brief}?"))? {
        out!(Out, "aborted.");
        return Ok(());
    }
    // The fleet provider routes it to the backend that owns the pod id.
    provider.restart_pod(&pod.id).await.map_err(|e| anyhow::anyhow!("{}", arena_core::lock::explain(&pod.name, &e)))?;
    out!(Detail, "[restarted] {label}");
    out!(Brief, "[restarted] {}", out::bold(&pod.name));

    let back = Comeback { setup, sync: !opts.skip_proxy, settle, timeouts, reset: fate.reset() };
    if wiped {
        come_back(provider, remote, cfg, std::slice::from_ref(&pod), back, keys_dir, "restart").await
    } else {
        // A Hetzner reset keeps the VM (and its address): no setup, just the sync.
        if back.sync {
            sync_proxy(cfg, provider, "restart").await;
        }
        Ok(())
    }
}

/// Why `restart` refuses a stopped pod, and the way back (pure, tested): `pods start` — which
/// brings back a pod whose container disk went at the stop (`start_wipes`: RunPod, Vast) as a
/// fresh image, set up again, but a Hetzner VM with its disk as it was (live R: the Hetzner
/// refusal promised a fresh image).
pub(crate) fn stopped_restart_refusal(pod: &arena_core::Pod, start_wipes: bool) -> String {
    let comes = if start_wipes {
        "it comes back as a fresh image and is set up again"
    } else {
        "its disk was kept: it comes back as it was, with nothing set up again"
    };
    format!(
        "{} is stopped ({}) — a restart needs a running pod. Start it with `arena pods start {}` ({comes}).",
        pod.name, pod.status, pod.name
    )
}

/// What a lifecycle command that resets containers says it does afterwards (pure, tested):
/// `reset` is what the operation does to the container disk — a possible reset
/// ([`ContainerReset::Unknown`]) is set up again like a certain one, but never said to have
/// left a bare image.
pub(crate) fn afterwards_line(reset: ContainerReset, setup: bool, skip_proxy: bool) -> &'static str {
    use ContainerReset::{No, Unknown, Yes};
    match (reset, setup, skip_proxy) {
        (Yes | Unknown, true, false) => "then: wait for its SSH endpoint to settle, re-run setup on it (~/.name, deploy key + git remote, API keys; a repo with changes or commits of its own is left as it is), sync the proxy",
        (Yes | Unknown, true, true) => "then: wait for its SSH endpoint to settle, re-run setup on it (~/.name, deploy key + git remote, API keys; a repo with changes or commits of its own is left as it is)",
        (Yes, false, false) => "then: wait for its SSH endpoint to settle and sync the proxy (no setup: it comes back as the bare image)",
        (Yes, false, true) => "then: nothing (no setup, no proxy sync: it comes back as the bare image)",
        (Unknown, false, false) => "then: wait for its SSH endpoint to settle and sync the proxy (no setup: if it was reset, it comes back as the bare image)",
        (Unknown, false, true) => "then: nothing (no setup, no proxy sync: if it was reset, it comes back as the bare image)",
        (No, _, false) => "then: sync the proxy",
        (No, _, true) => "then: nothing",
    }
}

/// How [`come_back`] brings reset pods back.
#[derive(Debug, Clone, Copy)]
struct Comeback {
    /// Re-run setup on them (they're back as the bare image otherwise).
    setup: bool,
    /// Sync the proxy afterwards.
    sync: bool,
    settle: SettleWait,
    timeouts: arena_core::setup::SetupTimeouts,
    /// What the operation did to their container disks: `Yes` (a reimage, a start, a RunPod v2
    /// restart) or `Unknown` (a restart that may have kept it) — what the output may claim.
    reset: ContainerReset,
}

/// After a container reset — `restart`, `reimage`, `start` — bring the pods back usable:
/// wait for each one's SSH endpoint to settle (so setup lands on the NEW container, not the
/// one going away), re-run setup on them (`~/.name`, deploy key + git remote, API keys, the
/// repo re-linked onto its volume), then sync the proxy once their endpoints have settled
/// again ([`settled_sync`]). The proxy is synced whatever setup did: the reset happened
/// either way. `Err` = some pod isn't set up again, with the command that fixes it — and a
/// warning not to reset it again, which would only wipe it once more.
async fn come_back(
    provider: &dyn Provider,
    remote: Arc<dyn Remote>,
    cfg: &Config,
    pods: &[arena_core::Pod],
    how: Comeback,
    keys_dir: &str,
    why: &'static str,
) -> Result<()> {
    if pods.is_empty() || !(how.setup || how.sync) {
        if !how.setup && !pods.is_empty() {
            no_setup_note(pods, how.reset);
        }
        return Ok(());
    }
    let since = tokio::time::Instant::now();
    let s = how.settle;
    let mut settled = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    {
        // A terminal shows the wait in place; a log gets `[NAME] waiting for SSH…` as it starts
        // (the dashboard's runs panel shows it as the pod's step).
        let _line = out::progress_guard(SETTLE);
        for pod in pods {
            out!(Detail, "waiting for {}'s SSH endpoint to settle…", pod.name);
            if out::live() {
                out::progress(SETTLE, Some(&format!("{} waiting for its SSH endpoint to settle…", pod.name)));
            } else {
                out!(Brief, "[{}] waiting for SSH…", out::bold(&pod.name));
            }
            match wait_for_stable_endpoint_every(provider, &pod.id, s.stable_secs, s.timeout_secs, s.poll).await {
                Ok(_) => settled.push(pod.id.clone()),
                Err(e) => {
                    out!(ErrBrief, "{}", failed_line(&pod.name, "waiting for SSH", &format!("{e:#}"), &format!("arena pods setup {} once it answers", pod.name)));
                    problems.push(format!("{}: no SSH endpoint ({e})", pod.name));
                }
            }
        }
    }
    // The pods back by the end (by name): settled, and set up when setup runs.
    let mut back: Vec<String> = pods.iter().filter(|p| settled.contains(&p.id)).map(|p| p.name.clone()).collect();
    if how.setup && !settled.is_empty() {
        out!(Detail, "\n{}", reprovision_line(settled.len(), why, how.reset));
        let setup = match just_these(provider, &settled).await {
            Ok(sel) => {
                setup_pods(remote, cfg, true, false, None, None, false, false, how.timeouts, &sel, keys_dir, Some(provider)).await
            }
            Err(e) => Err(e),
        };
        match setup {
            Ok(done) => {
                back.retain(|n| done.provisioned.contains(n));
                if done.failed > 0 {
                    problems.push(format!("{} pod(s) failed to set up", done.failed));
                }
            }
            Err(e) => {
                back.clear();
                problems.push(format!("{e:#}"));
            }
        }
    }
    if how.sync {
        // Only the pods that settled are waited for again: one that never got an endpoint has
        // had its wait (and is named below) — another one, or two, per pod would only make a
        // dead pod cost the whole command half an hour.
        let back: Vec<arena_core::Pod> = pods.iter().filter(|p| settled.contains(&p.id)).cloned().collect();
        settled_sync(cfg, provider, &back, s, why).await;
    }
    if !how.setup {
        no_setup_note(pods, how.reset);
    } else {
        // Each pod back and set up, as `up` ends one: the dashboard's runs panel reads it as the
        // pod's end (a `start` from there). Not with --no-setup: a bare image isn't ready.
        for name in &back {
            out!(Brief, "{}", ready_line(name, since.elapsed(), None));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
    let (past, again) = match why {
        "restart" => ("restarted", "not another restart"),
        "reimage" => ("reimaged", "not another reimage"),
        _ => ("started", "not a restart"),
    };
    let it = if pods.len() == 1 { "it is" } else { "they are" };
    let wipes = if how.reset == ContainerReset::Unknown { "that may wipe it" } else { "that would wipe it again" };
    anyhow::bail!(
        "{} {past}, but {it} NOT set up again: {}\n  → run `arena pods setup {}` once it answers \
         ({again} — {wipes})",
        names.join(", "),
        problems.join("; "),
        names.join(" ")
    )
}

/// The key of [`come_back`]'s in-place line on a terminal ([`out::progress`]).
const SETTLE: &str = "settle";

/// The line before [`come_back`] re-runs setup (pure, tested): a reset is said to have
/// happened only when it is known to (a RunPod v1 restart may have kept the disk).
pub(crate) fn reprovision_line(n: usize, why: &str, reset: ContainerReset) -> String {
    match reset {
        ContainerReset::Unknown => format!("Re-provisioning {n} pod(s) (in case the {why} reset them to the image — it may not have)…"),
        _ => format!("Re-provisioning {n} pod(s) (the {why} reset them to the image)…"),
    }
}

/// `--no-setup`'s reminder after a reset (or a possible one: `reset`).
fn no_setup_note(pods: &[arena_core::Pod], reset: ContainerReset) {
    out!(Out, "{}", no_setup_line(pods, reset));
}

/// [`no_setup_note`]'s words (pure, tested).
pub(crate) fn no_setup_line(pods: &[arena_core::Pod], reset: ContainerReset) -> String {
    let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
    let back = if reset == ContainerReset::Unknown { "may be back" } else { "back" };
    format!(
        "(--no-setup: {} {back} as the bare image — `arena pods setup {}` restores ~/.name, the git remote and keys)",
        names.join(", "),
        names.join(" ")
    )
}

/// Whether `name` has a stable port on a configured proxy (a MACHINE_NAME_LIST name) — the
/// pods whose forward a sync is expected to route.
pub(crate) fn proxied_name(cfg: &Config, name: &str) -> bool {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    arena_core::proxy::ProxyConfig::from_config(cfg).is_ok()
        && cfg.machine_names.iter().any(|m| arena_core::naming::qualify(prefix, m) == name)
}

/// The pods of `pods` (with a stable port) whose forward `sync` didn't route to them — kept
/// on an old endpoint because the listing showed them without one. Empty when the sync
/// wasn't made (skipped / failed: its own line said so).
fn unrouted<'a>(cfg: &Config, sync: &ProxySync, pods: &'a [arena_core::Pod]) -> Vec<&'a arena_core::Pod> {
    let ProxySync::Synced { routed, .. } = sync else { return Vec::new() };
    pods.iter()
        .filter(|p| proxied_name(cfg, &p.name))
        .filter(|p| !routed.iter().any(|f| f.name == p.name && f.pod_id.as_deref() == Some(p.id.as_str())))
        .collect()
}

/// The proxy sync after a container reset, made once the pods' endpoints have SETTLED: a
/// reset container's endpoint can flap after it first settles (live: the post-restart sync
/// hit a moment the listing showed the pod without one, so its forward stayed on the dead
/// old endpoint). Waits for each pod to hold one endpoint for `settle.stable_secs`
/// (bounded), syncs, and if a pod's forward still isn't routed to it, waits for THAT pod
/// again and syncs once more — not for one whose first wait already ran out (it has no
/// endpoint to wait for). One still not routed is named with the command to run later.
async fn settled_sync(cfg: &Config, provider: &dyn Provider, pods: &[arena_core::Pod], settle: SettleWait, why: &str) -> ProxySync {
    /// Waits for each of `pods`; the ids whose wait ran out.
    async fn wait(provider: &dyn Provider, pods: &[&arena_core::Pod], settle: SettleWait) -> Vec<String> {
        let mut timed_out = Vec::new();
        for pod in pods {
            if let Err(e) =
                wait_for_stable_endpoint_every(provider, &pod.id, settle.stable_secs, settle.timeout_secs, settle.poll).await
            {
                out!(Err, "warning: {} has no settled SSH endpoint ({e})", pod.name);
                timed_out.push(pod.id.clone());
            }
        }
        timed_out
    }
    let dead = wait(provider, &pods.iter().collect::<Vec<_>>(), settle).await;
    let mut sync = sync_proxy(cfg, provider, why).await;
    let again: Vec<&arena_core::Pod> = unrouted(cfg, &sync, pods).into_iter().filter(|p| !dead.contains(&p.id)).collect();
    if !again.is_empty() {
        wait(provider, &again, settle).await;
        sync = sync_proxy(cfg, provider, why).await;
    }
    let missing: Vec<&str> = unrouted(cfg, &sync, pods).iter().map(|p| p.name.as_str()).collect();
    if !missing.is_empty() {
        out!(
            Err,
            "warning: the proxy doesn't route {} to the new endpoint yet (listed without one) — run `arena proxy apply` once it answers",
            missing.join(", ")
        );
    }
    sync
}

#[cfg(test)]
mod tests {
    /// `pods stop`/`start` skip a Shadeform pod (it has no stop — it bills until deleted) and act on
    /// the rest; only a selection of nothing else is refused outright, and the stop's exit stays
    /// non-zero while a skipped one still bills (review SF3).
    #[test]
    fn stop_and_start_skip_a_shadeform_pod_and_refuse_only_it_alone() {
        use arena_core::Config;
        let fleet = arena_core::provider::build_fleet("runpod", &Config::parse("RUNPOD_API_KEY=r\nSHADEFORM_API_KEY=k"), false).unwrap();
        let sf = arena_core::Pod { name: "devtest-alpha".into(), provider: "shadeform".into(), ..Default::default() };
        let rp = arena_core::Pod { name: "devtest-bloom".into(), provider: "runpod".into(), ..Default::default() };
        for (op, what) in [(super::Op::Stop, "stop"), (super::Op::Start, "start")] {
            let (no_stop, rest) = super::split_no_stop(fleet.as_ref(), op, vec![&rp, &sf]);
            assert_eq!((super::names_of(&no_stop), rest.iter().map(|p| p.name.as_str()).collect::<Vec<_>>()), (vec!["devtest-alpha".to_string()], vec!["devtest-bloom"]));
            assert!(no_stop[0].1.starts_with("Shadeform has no stop"), "{}", no_stop[0].1);
            let r = super::no_stop_refusal(&no_stop, what);
            assert!(r.starts_with(&format!("can't {what} devtest-alpha — Shadeform has no stop")), "{r}");
            let (none, all) = super::split_no_stop(fleet.as_ref(), op, vec![&rp]);
            assert!(none.is_empty() && all.len() == 1);
        }
        let (no_stop, _) = super::split_no_stop(fleet.as_ref(), super::Op::Stop, vec![&rp, &sf]);
        assert!(no_stop[0].1.contains("arena pods terminate"), "the way out of a pod that bills: {}", no_stop[0].1);
        let left = super::no_stop_left(&no_stop).unwrap();
        assert!(left.starts_with("1 pod(s) still billing") && left.contains("devtest-alpha"), "{left}");
        assert_eq!(super::no_stop_left(&[]), None);
    }
}
