//! `pods create` and the create half of `up`/`replace`/`migrate`: which names, the spec with its
//! overrides, `--owner`, and the create with its capacity retries.

use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::provider::{ContainerReset, Provider};
use arena_core::{Config, PodSpec};

use crate::keys::{keys_dir, openrouter_client};
use crate::pods::placement::{created_line, single_option_plan};
use crate::{image, reap, spend, Interrupt};

/// The policy `up`/`create` records, in words.
pub(crate) fn create_policy_line(p: &reap::CreatePolicy) -> String {
    let mut v = Vec::new();
    if let Some(e) = p.expires_at {
        v.push(format!("expires {} UTC", arena_core::snapshot::rfc3339(e)));
    }
    if let Some(w) = p.idle_after_secs {
        v.push(format!("idle after {}", arena_core::reap::fmt_dur(w)));
    }
    v.join(", ")
}

/// A fleet provider that records a reap policy for every pod the moment its create returns —
/// `create`, `up` and `up --check`'s replacements go through `create_pod`, so none is missed.
/// Everything else is the inner provider's, method for method.
pub(crate) struct RecordingProvider<'a> {
    pub inner: &'a dyn Provider,
    pub policy: reap::CreatePolicy,
    pub store: arena_core::reap::store::Store,
    pub set_by: &'static str,
}

#[async_trait::async_trait]
impl Provider for RecordingProvider<'_> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn describe(&self, spec: &PodSpec) -> String {
        self.inner.describe(spec)
    }
    fn backend(&self) -> Option<arena_core::capability::Backend> {
        self.inner.backend()
    }
    fn supports(&self, op: arena_core::capability::Op, pod: Option<&arena_core::Pod>) -> arena_core::capability::Support {
        self.inner.supports(op, pod)
    }
    async fn list_pods(&self) -> arena_core::Result<Vec<arena_core::Pod>> {
        self.inner.list_pods().await
    }
    async fn list_by_provider(&self) -> Vec<(String, arena_core::Result<Vec<arena_core::Pod>>)> {
        self.inner.list_by_provider().await
    }
    async fn enrich(&self, pods: &mut [arena_core::Pod]) -> arena_core::Result<()> {
        self.inner.enrich(pods).await
    }
    async fn create_pod(&self, spec: &PodSpec) -> arena_core::Result<arena_core::Pod> {
        let pod = self.inner.create_pod(spec).await?;
        reap::record_created(&self.store, &pod, self.policy, arena_core::snapshot::unix_now(), self.set_by);
        Ok(pod)
    }
    async fn stop_pod(&self, id: &str) -> arena_core::Result<()> {
        self.inner.stop_pod(id).await
    }
    async fn stop_pod_outcome(&self, id: &str) -> arena_core::Result<arena_core::provider::StopOutcome> {
        self.inner.stop_pod_outcome(id).await
    }
    async fn restart_pod(&self, id: &str) -> arena_core::Result<()> {
        self.inner.restart_pod(id).await
    }
    fn restart_wipes_container_disk(&self, pod: &arena_core::Pod) -> bool {
        self.inner.restart_wipes_container_disk(pod)
    }
    fn restart_resets_container(&self, pod: &arena_core::Pod) -> ContainerReset {
        self.inner.restart_resets_container(pod)
    }
    async fn start_pod(&self, id: &str) -> arena_core::Result<()> {
        self.inner.start_pod(id).await
    }
    fn ready_deadline(&self, pod: &arena_core::Pod) -> Option<Duration> {
        self.inner.ready_deadline(pod)
    }
    async fn terminate_pod(&self, id: &str) -> arena_core::Result<()> {
        self.inner.terminate_pod(id).await
    }
    async fn terminate_pod_on(&self, provider: &str, id: &str) -> arena_core::Result<()> {
        self.inner.terminate_pod_on(provider, id).await
    }
    async fn rename_pod(&self, id: &str, new_name: &str) -> arena_core::Result<()> {
        self.inner.rename_pod(id, new_name).await
    }
    async fn reimage_pod(&self, id: &str, image: &str, env: &[(String, String)]) -> arena_core::Result<()> {
        self.inner.reimage_pod(id, image, env).await
    }
    async fn authorize_ssh_keys(&self, pod: &arena_core::Pod, keys: &[String]) -> arena_core::Result<()> {
        self.inner.authorize_ssh_keys(pod, keys).await
    }
    async fn set_locked(&self, id: &str, locked: bool) -> arena_core::Result<()> {
        self.inner.set_locked(id, locked).await
    }
    fn check_create_extra(&self, extra: &arena_core::apiextra::Extra) -> arena_core::Result<()> {
        self.inner.check_create_extra(extra)
    }
    fn preview_create_body(&self, spec: &PodSpec) -> arena_core::Result<serde_json::Value> {
        self.inner.preview_create_body(spec)
    }
    async fn pod_spec(&self, id: &str) -> arena_core::Result<PodSpec> {
        self.inner.pod_spec(id).await
    }
}

/// `create`/`up --owner`: print what will be recorded and which names' AI keys are handed over
/// (rotated: the old owner keeps what they spent); returns those handovers. `Err` when the
/// ledger can't be read (whose keys these are is unknown): before anything is created.
pub(crate) fn owner_plan(tracking: &spend::SpendCtx, owner: &spend::OwnerArgs, names: &[String], dry_run: bool) -> Result<Vec<spend::Handover>> {
    let handing = spend::handovers(tracking, names, owner.owner.as_deref(), owner.keep_key)?;
    for line in spend::owner_lines(owner.owner.as_deref(), names, &handing, owner.keep_key, dry_run) {
        println!("{line}");
    }
    Ok(handing)
}

/// Run the handovers [`owner_plan`] found, after the prompt and before any create: whether
/// the keys CSV changed. A handover that failed stops the command before anything is
/// created — the new machine would get the old owner's key, or none that works.
pub(crate) async fn hand_keys_over(tracking: &spend::SpendCtx, cfg: &Config, owner: &spend::OwnerArgs, handing: &[spend::Handover]) -> Result<bool> {
    let Some(o) = owner.owner.as_deref().filter(|_| !handing.is_empty()) else { return Ok(false) };
    let api = openrouter_client(cfg);
    let done = spend::hand_over(tracking, cfg, api.as_ref().map(|a| a as &dyn arena_core::openrouter::KeyApi), std::path::Path::new(&keys_dir(cfg)), handing, o).await;
    for l in &done.lines {
        if l.starts_with('✗') {
            eprintln!("{l}");
        } else {
            println!("{l}");
        }
    }
    if !done.failed.is_empty() {
        anyhow::bail!(
            "the OpenRouter key handover failed for {} — nothing was created. Re-run once `arena keys rotate <name>` \
             works, or pass --keep-key to create with the name's current key (a name whose old key is already gone \
             gets a fresh one from `pods up`, or from `arena keys gen <name>` after a `create`)",
            done.failed.join(", ")
        );
    }
    Ok(done.csv_changed)
}

/// Warn when pods would be created with no persistent volume where a restart or a stop may wipe
/// the container disk — uncommitted work would be lost. Where the provider takes no volume (the
/// capability table), it says so rather than suggest VOLUME_GB; a VM that keeps its disk
/// (Hetzner) needs no warning.
pub(crate) fn warn_no_volume(provider: &dyn Provider, spec: &PodSpec) {
    use arena_core::capability::Op;
    let target = arena_core::Pod { provider: provider.name().to_string(), ..Default::default() };
    let volumes = provider.supports(Op::Volume, None).works();
    if !volumes && !provider.supports(Op::Stop, None).works() {
        // No volumes there, and no stop (Shadeform): say both.
        eprintln!(
            "⚠ {}: no persistent volume and no stop — work survives only through backups \
             (`arena pods backup`), and each VM bills until it's terminated.",
            provider_label(provider)
        );
        return;
    }
    if !provider.restart_wipes_container_disk(&target) {
        return;
    }
    if !volumes {
        eprintln!(
            "⚠ {}: no persistent volume — a restart or a stop may wipe the container disk; work survives \
             only through backups (`arena pods backup`).",
            provider_label(provider)
        );
    } else if spec.volume_gb == 0 {
        eprintln!(
            "⚠ no persistent volume (VOLUME_GB=0): work is lost when a pod restarts. \
             Set VOLUME_GB to keep it."
        );
    }
}

/// The create target's name as the capability table writes it (`Shadeform`, `Vast`).
fn provider_label(provider: &dyn Provider) -> String {
    provider.backend().map_or_else(|| provider.name().to_string(), |b| b.label().to_string())
}

/// Compute the next free machine names for `count` pods, warning if fewer are
/// available than requested. (Read-only: lists current pods to know what's taken.)
///
/// CRITICAL: this propagates a list failure instead of swallowing it. If we can't
/// confirm what already exists, we must NOT proceed — treating a failed list as "zero
/// pods" would create a duplicate of the entire fleet on the live account. Better to
/// abort with an error the operator can see.
/// How many pods to make: a TOTAL to reach (top up to N) or a number to ADD.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Want {
    Total(usize),
    Add(usize),
}

/// Resolve the `-n`(total) / `-a`(add) flags into a `Want`. Exactly one is required.
pub(crate) fn resolve_want(count: Option<usize>, add: Option<usize>) -> Result<Want> {
    match (count, add) {
        (Some(n), None) => Ok(Want::Total(n)),
        (None, Some(a)) => Ok(Want::Add(a)),
        (Some(_), Some(_)) => anyhow::bail!("pass either -n/--count (total) or -a/--add, not both"),
        (None, None) => anyhow::bail!("pass -n/--count <total> or -a/--add <count>"),
    }
}

/// Count the pods belonging to this provider's cohort: its backend name *and* the
/// configured machine-name prefix. `pods` is the whole-fleet snapshot from `list_pods()`
/// (which spans every backend), so the provider filter is load-bearing — without it a
/// per-provider top-up gets sized against the entire fleet. (`up -a 1 --provider hetzner`
/// once tried to make ~23 pods because this very count wasn't provider-scoped.)
fn provider_pod_count(pods: &[arena_core::Pod], provider_name: &str, prefix: &str) -> usize {
    let pre = format!("{prefix}-");
    pods.iter()
        .filter(|p| p.provider.as_str() == provider_name && p.name.starts_with(&pre))
        .count()
}

/// The provider-scoped *total* a create aims for: `-n` is that total outright; `-a` adds
/// to what this provider already has. One definition, used for both preview and executor,
/// so they can never disagree about how big the create is.
fn target_total(want: Want, pods: &[arena_core::Pod], provider_name: &str, prefix: &str) -> usize {
    match want {
        Want::Total(n) => n,
        Want::Add(a) => provider_pod_count(pods, provider_name, prefix) + a,
    }
}

/// A resolved create plan: the provider-scoped total to reach, plus the concrete new names
/// to make this round (`target - have`, capped by free names). Built in ONE place
/// (`plan_create`) so the dry-run preview, the `[y/N]` confirmation, and the `[created] …`
/// lines are the *same* plan — not three independent recomputations that can drift apart
/// (which is exactly how a confirmed "1 pod" turned into a 23-pod create).
pub(crate) struct CreatePlan {
    /// Provider-scoped total we're topping up to (carried into the retry loop).
    pub target: usize,
    /// New names to create this round.
    pub names: Vec<String>,
}

/// Resolve a `-n`/`-a` request into a concrete [`CreatePlan`] against the current fleet.
/// Lists pods (failing closed — a failed list could otherwise duplicate pods) and picks
/// the next free machine names for the shortfall. The listing must be whole
/// ([`arena_core::provider::list_complete`]): with one provider down, its pods' names would
/// read as free and its pods wouldn't count toward `-n`.
pub(crate) async fn plan_create(provider: &dyn Provider, cfg: &Config, want: Want) -> Result<CreatePlan> {
    let policy = arena_core::retry::RetryPolicy::default();
    let existing = arena_core::retry::retrying(&policy, || arena_core::provider::list_complete(provider))
        .await
        .context(
            "listing existing pods (refusing to allocate names — a failed list could \
             create duplicate pods)",
        )?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let have = provider_pod_count(&existing, provider.name(), prefix);
    let target = target_total(want, &existing, provider.name(), prefix);
    if let Want::Total(n) = want {
        if n <= have {
            eprintln!("already have {have} {} pod(s) (target {n}) — nothing to create", provider.name());
        }
    }
    let to_create = target.saturating_sub(have);
    // `existing` spans every provider, so a name live on another backend won't be reused.
    let names = arena_core::naming::next_free_names(prefix, &cfg.machine_names, &existing, to_create);
    if names.len() < to_create {
        eprintln!(
            "warning: need {to_create} but only {} free machine name(s) available",
            names.len()
        );
    }
    Ok(CreatePlan { target, names })
}

/// Resolve operator-supplied machine names for `create <names…>`: prefix bare names
/// with `MACHINE_NAME_PREFIX`, warn about any not in the configured `MACHINE_NAME_LIST`
/// (allowed, but usually a typo), and drop any that already exist (so re-running is
/// safe). Propagates a list failure — any provider's ([`arena_core::provider::list_complete`])
/// — rather than risking a duplicate create.
pub(crate) async fn resolve_explicit_names(
    provider: &dyn Provider,
    cfg: &Config,
    raw: &[String],
) -> Result<Vec<String>> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let policy = arena_core::retry::RetryPolicy::default();
    let existing = arena_core::retry::retrying(&policy, || arena_core::provider::list_complete(provider))
        .await
        .context("listing existing pods (refusing to allocate names — a failed list could create duplicates)")?;
    // `existing` spans every provider, so the same name can't be live on two backends.
    let taken: std::collections::HashSet<&str> = existing.iter().map(|p| p.name.as_str()).collect();

    let mut out: Vec<String> = Vec::new();
    for n in raw {
        let n = n.trim();
        if n.is_empty() {
            continue;
        }
        let full = arena_core::naming::canonical_name(prefix, &cfg.machine_names, n);
        // Membership check against the list: compare on the qualified name so absolute
        // (`@james-gpu` → `james-gpu`) and prefixed entries both match without false warnings.
        let known = cfg.machine_names.iter().any(|m| arena_core::naming::qualify(prefix, m) == full);
        if !known {
            eprintln!("warning: '{full}' is not in MACHINE_NAME_LIST (creating anyway)");
        }
        if taken.contains(full.as_str()) {
            eprintln!("skip {full} — already exists");
            continue;
        }
        if !out.contains(&full) {
            out.push(full);
        }
    }
    Ok(out)
}

/// Seconds to wait between capacity retries when `--keep-trying` is set.
pub(crate) const CAPACITY_RETRY_SECS: u64 = 30;

/// Max capacity retries per machine under `--keep-trying` (~1h at 30s) — generous
/// enough to "wait around" for a free GPU, bounded enough to never spin forever.
pub(crate) const MAX_CAPACITY_ATTEMPTS: u32 = 120;

/// Start-command script for `--bootstrap`: makes a non-arena base image (e.g. an NVIDIA
/// NGC image) SSH-reachable. Installs openssh-server, authorizes `$PUBLIC_KEY` (which
/// RunPod injects from the pod's env), generates host keys, and runs sshd in the
/// foreground so it's the long-lived process — if it dies, RunPod restarts the container.
/// `$PUBLIC_KEY` stays a runtime variable so it expands inside the container, not here.
/// Sent as `dockerStartCmd` argv `["bash","-c", THIS]` so no outer shell-quoting is needed.
/// The prebuilt arena image needs none of this, so `--bootstrap` is opt-in.
const BOOTSTRAP_SCRIPT: &str = r#"export DEBIAN_FRONTEND=noninteractive; apt-get update && apt-get install -y --no-install-recommends openssh-server && mkdir -p ~/.ssh && echo "$PUBLIC_KEY" >> ~/.ssh/authorized_keys && chmod 700 ~/.ssh && ssh-keygen -A && mkdir -p /run/sshd && /usr/sbin/sshd -D"#;

/// Command-line overrides for the create spec, so you don't have to edit config.env
/// to spin up a different GPU/count/cloud for one run.
#[derive(Debug, Clone, Default)]
pub(crate) struct SpecOverrides {
    pub gpu: Option<String>,
    pub gpus: Option<u32>,
    pub cloud: Option<String>,
    pub disk: Option<u32>,
    pub volume: Option<u32>,
    pub image: Option<String>,
    /// Set the `--bootstrap` start command (`dockerArgs`) so a non-arena base image
    /// brings up sshd on boot. Off => use the image's own entrypoint.
    pub bootstrap: bool,
    /// `--api-json` over `CREATE_EXTRA_JSON`, already checked ([`create_extra`]).
    pub api_extra: Option<arena_core::apiextra::Extra>,
    /// Machine ids not to place on (`PodSpec::avoid_machines`): `up`'s remembered bad ones.
    pub avoid_machines: Vec<String>,
    /// `--server-type` (Hetzner), already checked against the create target ([`sizing_flags`]).
    pub server_type: Option<String>,
    /// `--image` on Hetzner: the VM's image ([`image_flag`]).
    pub vm_image: Option<String>,
}

/// The base spec from config, with any command-line overrides applied.
pub(crate) fn spec_with_overrides(cfg: &Config, ov: &SpecOverrides) -> PodSpec {
    let mut spec = PodSpec::from_config(cfg);
    apply_spec_overrides(&mut spec, ov);
    spec
}

/// Apply CLI `--gpu/--disk/...` overrides onto an existing spec base. Factored out so
/// `replace` can layer the same flags onto a *snapshot* of the source pod (rather than the
/// config base), giving "same spec as the source unless overridden".
pub(crate) fn apply_spec_overrides(spec: &mut PodSpec, ov: &SpecOverrides) {
    if let Some(g) = &ov.gpu {
        spec.gpu_type = arena_core::gpu::resolve(g);
    }
    if let Some(n) = ov.gpus {
        spec.gpu_count = n;
    }
    if let Some(c) = &ov.cloud {
        spec.cloud_type = c.to_uppercase();
    }
    if let Some(d) = ov.disk {
        spec.disk_gb = d;
    }
    if let Some(v) = ov.volume {
        spec.volume_gb = v;
    }
    if let Some(img) = &ov.image {
        spec.image = img.clone();
    }
    if ov.bootstrap {
        spec.docker_args =
            Some(vec!["bash".to_string(), "-c".to_string(), BOOTSTRAP_SCRIPT.to_string()]);
    }
    if let Some(extra) = &ov.api_extra {
        spec.api_extra = Some(extra.clone());
    }
    if !ov.avoid_machines.is_empty() {
        spec.avoid_machines = ov.avoid_machines.clone();
    }
    if let Some(t) = &ov.server_type {
        spec.server_type = Some(t.clone());
    }
    if let Some(i) = &ov.vm_image {
        spec.vm_image = Some(i.clone());
    }
}

/// `--image` for the create target: a container image (RunPod, Vast — `spec.image`), or on
/// Hetzner the VM's image (`spec.vm_image`): `arena`, a snapshot id or a system image name, over
/// HETZNER_IMAGE. Returns `(container image, VM image)`. Pure.
pub(crate) fn image_flag(provider: &str, image: Option<String>) -> Result<(Option<String>, Option<String>)> {
    if provider != "hetzner" {
        return Ok((image, None));
    }
    let Some(i) = image.map(|i| i.trim().to_string()) else { return Ok((None, None)) };
    if i.is_empty() || i.len() > 64 || !i.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')) {
        anyhow::bail!("--image on Hetzner takes `arena`, a snapshot id or an image name like ubuntu-24.04");
    }
    Ok((None, Some(i)))
}

/// A Hetzner create whose image is `arena`: resolve it now — once for every pod of the run —
/// say which snapshot (or that none fits, and Ubuntu it is), and pin the spec to it. Anything
/// else is left as it is. A failed lookup stops the create: falling back to a 10-minute setup
/// on every VM because a listing failed would be a surprise.
pub(crate) async fn pin_vm_image(provider: &str, cfg: &Config, spec: &mut PodSpec) -> Result<()> {
    if provider != "hetzner" {
        return Ok(());
    }
    let opts = arena_core::provider::hetzner::HetznerOpts::from_config(cfg);
    let setting = arena_core::image::Setting::parse(opts.image_for(spec));
    if setting != arena_core::image::Setting::Arena {
        return Ok(());
    }
    let cloud = image::hetzner(cfg)?;
    let ty = opts.server_type_for(spec).to_string();
    let (resolved, _) = tokio::time::timeout(image::LOOKUP_TIMEOUT, image::resolve(&cloud, cfg, &setting, &ty))
        .await
        .map_err(|_| anyhow::anyhow!("looking up the arena image took over {}s", image::LOOKUP_TIMEOUT.as_secs()))??;
    println!("{}", resolved.describe(arena_core::snapshot::unix_now()));
    spec.vm_image = Some(resolved.as_setting());
    Ok(())
}

/// The sizing flags `create`/`up` were given, checked against the backend the pods would be
/// created on (`provider`: the fleet's primary — `--provider`). Pure. A flag the target can't
/// honour is refused, never dropped: Hetzner sizes a CPU VM by server type and has no GPU, so
/// `--gpu`/`--gpus` there would be ignored (live, an evals node asked for one and got the
/// configured type) — the refusal points at `--server-type`; anywhere else `--server-type` would
/// be. Returns the server type to create with (`None`: the configured one).
pub(crate) fn sizing_flags(provider: &str, gpu: Option<&str>, gpus: Option<u32>, server_type: Option<String>) -> Result<Option<String>> {
    use arena_core::capability::{self, Backend, Op, Support};
    let given: Vec<(&str, Op)> = [
        gpu.map(|_| ("--gpu", Op::Gpu)),
        gpus.map(|_| ("--gpus", Op::Gpu)),
        server_type.as_ref().map(|_| ("--server-type", Op::ServerType)),
    ]
    .into_iter()
    .flatten()
    .collect();
    // The table's answer for the provider by name (GPU sizing is the same on both RunPod APIs).
    let support = |op| Backend::named(provider).map_or(Support::Yes, |b| capability::support(b, op));
    crate::pods::capable::ignored_flags(provider, &given, support)?;
    let Some(t) = server_type else { return Ok(None) };
    let t = t.trim();
    if t.is_empty() || t.len() > 32 || !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        anyhow::bail!("--server-type takes a Hetzner server type name like cx23 or cpx42");
    }
    Ok(Some(t.to_ascii_lowercase()))
}

/// `--api-json` over config `CREATE_EXTRA_JSON`, parsed and checked against the create
/// target's own fields ([`Provider::check_create_extra`]) — before anything is listed,
/// priced or created, so a typo'd JSON or a refused field costs no API call.
pub(crate) fn create_extra(provider: &dyn Provider, cfg: &Config, flag: Option<&str>) -> Result<Option<arena_core::apiextra::Extra>> {
    let extra = arena_core::apiextra::combine(cfg.get("CREATE_EXTRA_JSON"), flag)?;
    if let Some(x) = &extra {
        provider.check_create_extra(x)?;
    }
    Ok(extra)
}

/// The dry run's look at the create request when extra fields are merged in: the body the
/// backend would send for `spec` (one name, on one option), secrets — env values, keys —
/// redacted. `None` without extra fields: the body is then just the flags shown above.
fn create_body_text(provider: &dyn Provider, spec: &PodSpec) -> Option<String> {
    spec.api_extra.as_ref()?;
    Some(match provider.preview_create_body(spec) {
        Ok(body) => format!(
            "[dry-run] create request for {} with {} merged (secrets redacted):\n{}",
            spec.name,
            arena_core::apiextra::SOURCES,
            serde_json::to_string_pretty(&arena_core::apiget::redact(&body)).unwrap_or_default()
        ),
        Err(e) => format!("[dry-run] (no create-request preview: {e})"),
    })
}

pub(crate) fn print_create_body(provider: &dyn Provider, spec: &PodSpec) {
    if let Some(text) = create_body_text(provider, spec) {
        println!("{text}");
    }
}

/// The half of the `--api-json` / `CREATE_EXTRA_JSON` check that needs the spec: the
/// backend builds the create body (as the dry run shows it) and a refusal in building it —
/// e.g. RunPod v2's network volume next to `--volume` — fails the command now, before
/// anything is created or confirmed, instead of at each create. Other errors (a backend with
/// no preview) are left to the create itself. A no-op without extra fields.
pub(crate) fn check_create_body(provider: &dyn Provider, spec: &PodSpec) -> Result<()> {
    if spec.api_extra.is_none() {
        return Ok(());
    }
    match provider.preview_create_body(spec) {
        Err(e @ arena_core::Error::Config(_)) => Err(e.into()),
        _ => Ok(()),
    }
}

/// A pod this run created: when its create returned (tokio's clock — `up` times each pod's
/// create→ready from it, not from when a batch of creates finished), and which placement
/// option it landed on (`None` = the single-spec path), so `up --check` knows what a
/// replacement should try first.
#[derive(Debug, Clone)]
pub(crate) struct Made {
    pub pod: arena_core::Pod,
    pub at: tokio::time::Instant,
    pub option: Option<usize>,
}

/// Where the create phase reports each pod the moment its create returns — `up` starts that
/// pod's pipeline then, while placement may still be retrying for the others (finding #14:
/// placed pods sat idle, billing, until a 6-minute `--retry-mins` window ended). `create`
/// passes [`NO_STREAM`].
pub(crate) type OnMade<'a> = &'a (dyn Fn(&Made) + Sync);

/// An [`OnMade`] that does nothing (`pods create`, which has no pipelines).
pub(crate) const NO_STREAM: OnMade<'static> = &|_| {};

/// A create that failed part-way. `created` are the pods made before the failure: they
/// exist (and bill) whatever happened next, so the caller still finishes its job for them
/// — the proxy sync — before reporting `error`.
#[derive(Debug)]
pub(crate) struct CreateFailed {
    pub created: Vec<Made>,
    pub error: anyhow::Error,
}

impl CreateFailed {
    pub(crate) fn new(created: Vec<Made>, error: anyhow::Error) -> Self {
        Self { created, error }
    }
}

/// The capacity wait an `up --check` replacement's placement gets — the one the first create
/// had: `--retry-mins` (rounds every `--retry-secs`) when given; else `--keep-trying`'s own
/// (a retry every [`CAPACITY_RETRY_SECS`], [`MAX_CAPACITY_ATTEMPTS`] of them after the first
/// try, as `create_pods` waits); else one round. Without this a `--keep-trying` run's bad
/// host was terminated and its replacement given a single try, so a momentary capacity gap
/// left the name with no pod although the operator had asked to wait for one.
pub(crate) fn replacement_rounds(keep_trying: bool, retry_mins: u64, retry_secs: u64) -> arena_core::placement::Rounds {
    use arena_core::placement::Rounds;
    if retry_mins > 0 {
        Rounds { window: Duration::from_secs(retry_mins * 60), every: Duration::from_secs(retry_secs.max(1)) }
    } else if keep_trying {
        Rounds {
            window: Duration::from_secs(CAPACITY_RETRY_SECS * u64::from(MAX_CAPACITY_ATTEMPTS)),
            every: Duration::from_secs(CAPACITY_RETRY_SECS),
        }
    } else {
        Rounds { window: Duration::ZERO, every: Duration::from_secs(retry_secs.max(1)) }
    }
}

/// Create the confirmed names (`initial`), retrying the ones not made yet for up to
/// `retry_mins` (rounds every `retry_secs`) while capacity is short — each round after a
/// whole-fleet listing ([`arena_core::placement::relist`]), never past `target` on a top-up —
/// Ctrl+C (`interrupt`, which stays pressed for whatever runs next) stops the loop early and
/// keeps what was made. With `retry_mins == 0` it's a single attempt (honoring `keep_trying`).
/// Returns every pod created across all rounds — on failure, inside [`CreateFailed`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_with_retry(
    provider: &dyn Provider,
    cfg: &Config,
    initial: Vec<String>,
    target: usize,
    ov: &SpecOverrides,
    keep_trying: bool,
    retry_mins: u64,
    retry_secs: u64,
    interrupt: &Interrupt,
    on_made: OnMade<'_>,
) -> std::result::Result<Vec<Made>, CreateFailed> {
    use arena_core::placement::{next_round_in_window, relist, still_needed, Relisted, TopUp};
    // Round 1 creates exactly the names that were previewed + confirmed, so the
    // `[created] …` output matches the `[y/N]` prompt — and a later round only ever retries
    // the confirmed names it hasn't created yet, never picks new ones. `up` brings each pod
    // up while this loop still runs (finding #14), so a name it made may be momentarily
    // absent from a listing while `up --check` replaces its pod: a re-plan from free names
    // (as this loop once did) read that name as free and created a second pod under it next
    // to the replacement. `target` (a provider-scoped total, computed once in plan_create)
    // still bounds the whole operation: a top-up never balloons past what was agreed to.
    let retry_secs = retry_secs.max(1);
    let every = std::time::Duration::from_secs(retry_secs);
    // tokio's clock (not std's), so the paused-clock tests drive the window.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(retry_mins * 60);
    let topup = (target > 0).then(|| TopUp {
        target,
        provider: provider.name().to_string(),
        prefix: cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string(),
    });
    let mut all: Vec<Made> = Vec::new();
    let mut names = initial;
    let shown = if target == 0 { names.len() } else { target };
    let mut round = 0u32;
    'rounds: loop {
        round += 1;
        if names.is_empty() {
            break; // target reached (or no free names left)
        }
        // In retry mode the *loop* is the keep-trying, so each round is one-shot.
        let kt = retry_mins == 0 && keep_trying;
        let made = match create_pods(provider, cfg, &names, kt, ov, on_made).await {
            Ok(made) => made,
            Err(CreateFailed { created, error }) => {
                all.extend(created);
                return Err(CreateFailed::new(all, error));
            }
        };
        let got = made.len();
        all.extend(made);
        if got == names.len() || retry_mins == 0 {
            break; // filled what this round needed, or no retry requested
        }
        names.retain(|n| !all.iter().any(|m| &m.pod.name == n));
        let mut listing_missing = false;
        loop {
            // A round starts only inside the window: if the next one (`retry_secs` from now)
            // would start past it, stop here — not sleep, then create (bill) after the window
            // the operator agreed to. Decided before the sleep, as the placement executor does.
            if !next_round_in_window(tokio::time::Instant::now(), every, deadline) {
                eprintln!("retry window ({retry_mins}m) has no room for another round — have {} of {shown}", all.len());
                break 'rounds;
            }
            let why = if listing_missing { "fleet listing incomplete" } else { "capacity" };
            eprintln!(
                "round {round}: {} short of target {shown} ({why}); retrying in {retry_secs}s (Ctrl+C to stop)…",
                names.len()
            );
            let interrupted = tokio::select! {
                _ = tokio::time::sleep(every) => false,
                _ = interrupt.wait() => true,
            };
            if interrupted {
                eprintln!("interrupted — stopping retries with {} of {shown}", all.len());
                break 'rounds;
            }
            // The whole fleet first (as placement's rounds do): a confirmed name that exists
            // now — made meanwhile, on any backend — is dropped, and a top-up is cut to what
            // the target still lacks. A listing missing a backend that may answer again
            // skips this round; one that won't ends the run. Raced against Ctrl+C, Ctrl+C
            // first: a press during a slow listing used to let one more round of creates
            // through (a listing reads only, so dropping it half-way is safe).
            let relisted = tokio::select! {
                biased;
                _ = interrupt.wait() => None,
                r = relist(provider) => Some(r),
            };
            let Some(relisted) = relisted else {
                eprintln!("interrupted — stopping retries with {} of {shown}", all.len());
                break 'rounds;
            };
            match relisted {
                Relisted::Whole(fleet) => {
                    if tokio::time::Instant::now() > deadline {
                        eprintln!("retry window ({retry_mins}m) elapsed while listing — have {} of {shown}", all.len());
                        break 'rounds;
                    }
                    let (keep, dropped) = still_needed(&names, &fleet, topup.as_ref());
                    for (name, why) in dropped {
                        eprintln!("skip {name} — {why}");
                    }
                    names = keep;
                    break;
                }
                Relisted::Partial(e) => {
                    round += 1;
                    listing_missing = true;
                    eprintln!("round {round}: nothing created — {e}");
                }
                Relisted::Failed(e) => {
                    let error = anyhow::anyhow!(
                        "listing pods before retry round {} (refusing to create — a failed list could duplicate pods): {e}",
                        round + 1
                    );
                    return Err(CreateFailed::new(all, error));
                }
            }
        }
    }
    Ok(all)
}

/// Create one pod per name, returning those that came up. Error handling is typed:
/// - `Capacity` → the pool is exhausted; stop gracefully (or, with `keep_trying`,
///   wait `CAPACITY_RETRY_SECS` and retry that name) — the pods already created are
///   kept, never rolled back.
/// - `Auth` → credentials are wrong; abort immediately (retrying is pointless).
/// - anything else → report what we made so far, then surface the error.
async fn create_pods(
    provider: &dyn Provider,
    cfg: &Config,
    names: &[String],
    keep_trying: bool,
    ov: &SpecOverrides,
    on_made: OnMade<'_>,
) -> std::result::Result<Vec<Made>, CreateFailed> {
    use arena_core::ProviderErrorKind as K;

    let base = spec_with_overrides(cfg, ov);
    // What the `[created]` lines say it was created on (as the placement path's do).
    let on = single_option_plan(provider.name(), &base).options.first().map(|o| o.describe());
    let policy = arena_core::retry::RetryPolicy::default();
    let mut created = Vec::new();
    'names: for name in names {
        let mut spec = base.clone();
        spec.name = name.clone();
        spec.env.push(("MACHINE_NAME".into(), name.clone()));
        // Bound the capacity wait so a *misclassified* permanent error (e.g. a config
        // problem whose message merely looks capacity-ish) can't spin forever.
        let mut cap_attempts = 0u32;
        loop {
            // Retry transient/throttle failures with backoff; capacity & auth fall
            // through immediately to the classification below.
            match arena_core::retry::retrying(&policy, || provider.create_pod(&spec)).await {
                Ok(pod) => {
                    println!("{}", created_line(&pod, on.as_deref()));
                    let made = Made { pod, at: tokio::time::Instant::now(), option: None };
                    on_made(&made);
                    created.push(made);
                    continue 'names;
                }
                Err(e) => match e.kind() {
                    Some(K::Capacity) if keep_trying && cap_attempts < MAX_CAPACITY_ATTEMPTS => {
                        cap_attempts += 1;
                        eprintln!(
                            "[waiting] no capacity for {name}; retry {cap_attempts}/{MAX_CAPACITY_ATTEMPTS} in {CAPACITY_RETRY_SECS}s (have {}/{})",
                            created.len(),
                            names.len()
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(CAPACITY_RETRY_SECS)).await;
                        // loop: retry the same name
                    }
                    Some(K::Capacity) if keep_trying => {
                        eprintln!(
                            "[stop] still no capacity for {name} after {MAX_CAPACITY_ATTEMPTS} attempts (created {}/{}).",
                            created.len(),
                            names.len()
                        );
                        break 'names;
                    }
                    Some(K::Capacity) => {
                        eprintln!(
                            "[stop] no capacity for this GPU right now: {e} (created {}/{}). \
                             Re-run with --retry-mins N (or --keep-trying) to wait for it to free up.",
                            created.len(),
                            names.len()
                        );
                        break 'names;
                    }
                    Some(K::Auth) => {
                        let error = anyhow::anyhow!("authentication failed creating {name}: {e}");
                        return Err(CreateFailed::new(created, error));
                    }
                    _ => {
                        eprintln!("created {}/{} before failure", created.len(), names.len());
                        return Err(CreateFailed::new(created, anyhow::anyhow!("creating {name}: {e}")));
                    }
                },
            }
        }
    }
    println!("\nCreated {} of {} requested.", created.len(), names.len());
    Ok(created)
}

#[cfg(test)]
mod tests {
    fn mk_pod(name: &str, provider: &str) -> arena_core::Pod {
        arena_core::Pod {
            id: name.into(),
            name: name.into(),
            provider: provider.into(),
            status: "RUNNING".into(),
            gpu_type: None,
            cost_per_hr: None,
            ssh_ip: None,
            ssh_port: None,
            ..Default::default()
        }
    }

    #[test]
    fn create_sizing_is_provider_scoped() {
        use super::{provider_pod_count, target_total, Want};
        // A realistic mixed fleet: many runpod GPU pods + a single hetzner pod. `list_pods()`
        // returns this whole thing, which is what makes provider-scoping load-bearing.
        let mut fleet: Vec<arena_core::Pod> =
            (0..22).map(|i| mk_pod(&format!("arena8-gpu{i}"), "runpod")).collect();
        fleet.push(mk_pod("arena8-flutter", "hetzner"));

        // Counting is per-provider, not whole-fleet.
        assert_eq!(provider_pod_count(&fleet, "hetzner", "arena8"), 1);
        assert_eq!(provider_pod_count(&fleet, "runpod", "arena8"), 22);

        // THE REGRESSION: `-a 1 --provider hetzner` targets hetzner's 1 + 1 = 2 (creates
        // exactly 1) — NOT the whole-fleet 23 + 1 that once made it try to create ~23 pods.
        assert_eq!(target_total(Want::Add(1), &fleet, "hetzner", "arena8"), 2);
        assert_eq!(target_total(Want::Add(2), &fleet, "runpod", "arena8"), 24);
        // `-n N` is an absolute total, independent of any other provider's pods.
        assert_eq!(target_total(Want::Total(30), &fleet, "hetzner", "arena8"), 30);
    }

    #[test]
    fn pod_count_excludes_other_prefixes_on_same_provider() {
        use super::provider_pod_count;
        let fleet = vec![
            mk_pod("arena8-flutter", "hetzner"),
            mk_pod("unrelated-box", "hetzner"), // same provider, different cohort prefix
            mk_pod("arena8-bloom", "hetzner"),
        ];
        assert_eq!(provider_pod_count(&fleet, "hetzner", "arena8"), 2);
    }
}

/// `--api-json` / `CREATE_EXTRA_JSON` on `create`/`up`, and `arena api get`: everything is
/// checked before any API call, against the real RunPod v2 rules (pure), and the dry run
/// shows the merged body with its secrets redacted.
#[cfg(test)]
mod passthrough_tests;
