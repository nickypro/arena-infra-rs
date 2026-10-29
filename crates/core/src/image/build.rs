//! `arena image build`: make a temporary VM, set it up as `pods setup` does (no tokens), scrub
//! it, scan it for secrets, power it off, snapshot it — and delete the VM whatever happened,
//! including on Ctrl+C. A VM left behind bills by the hour, so a delete that fails is retried
//! and then reported as loudly as the caller can ([`VmFate::Left`]).
//!
//! Generic over [`ImageCloud`] and [`Remote`], so every path — a failed setup, a scan hit, a
//! failed snapshot, an interrupt at each stage, a delete that fails — is a fake scenario test.

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use super::cloud::ImageCloud;
use super::{parse_scrub, Baked, Scope, Snapshot, BUILD_LABEL};
use crate::pod::Pod;
use crate::remote::Remote;
use crate::setup::{provision, BootRetry, ProvisionStep};
use crate::ssh::SshTarget;

/// How long the scrub and scan may take (the scan reads every text file in the homes and /etc).
pub const SCRUB_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long Hetzner may take to snapshot the disk (a 40 GB disk with ~6 GB on it: minutes).
pub const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// How long a new server may go without an IPv4 in the API.
pub const IP_WAIT: Duration = Duration::from_secs(180);
/// Tries at deleting the VM before it's reported as left behind, and the pause between them.
pub const DELETE_TRIES: u32 = 4;
pub const DELETE_PAUSE: Duration = Duration::from_secs(5);

/// What to build.
pub struct BuildPlan {
    /// The VM's name, which is also the snapshot's description ([`super::build_name`]).
    pub name: String,
    pub server_type: String,
    pub scope: Scope,
    /// The setup's steps (`setup::provisioning_steps` for a Hetzner pod, with no tokens).
    pub setup: Vec<ProvisionStep>,
    /// The scrub-and-scan command ([`super::scrub_command`]).
    pub scrub: String,
    /// The server type's architecture and disk, for the labels.
    pub arch: String,
    pub disk_gb: u32,
    /// When (unix): the `built` label.
    pub built: u64,
}

/// What became of the temporary VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmFate {
    /// The create never made one.
    NeverMade,
    Deleted { id: String },
    /// Still there, billing: every delete failed.
    Left { id: String, why: String },
}

/// How a build ended: the snapshot, or why not — and the VM's fate either way.
#[derive(Debug)]
pub struct Report {
    pub result: std::result::Result<Snapshot, String>,
    pub vm: VmFate,
    /// A snapshot left half-made (its delete failed): its id and why.
    pub orphan_image: Option<(u64, String)>,
    /// How long the setup and the snapshot took.
    pub setup_took: Option<Duration>,
    pub snapshot_took: Option<Duration>,
}

/// Build the image. `target` turns the VM into its SSH target (the CLI's config: user, keys);
/// `stop` resolves on Ctrl+C. Never returns before the VM is deleted (or reported left).
pub async fn build(
    cloud: &dyn ImageCloud,
    remote: &dyn Remote,
    plan: &BuildPlan,
    target: &(dyn Fn(&Pod) -> crate::Result<SshTarget> + Sync),
    stop: impl Future<Output = ()>,
    emit: &(dyn Fn(&str) + Sync),
) -> Report {
    let server: Mutex<Option<String>> = Mutex::new(None);
    // A create in flight when Ctrl+C came: the server may exist with its id never seen.
    let creating = Mutex::new(false);
    let image: Mutex<Option<(u64, bool)>> = Mutex::new(None);
    let took: Mutex<(Option<Duration>, Option<Duration>)> = Mutex::new((None, None));

    let work = async {
        emit(&format!("creating {} ({}, {})…", plan.name, plan.server_type, super::FALLBACK));
        let labels = super::build_vm_labels(&plan.scope);
        *creating.lock().unwrap() = true;
        let made = cloud.create_server(&plan.name, &plan.server_type, labels).await;
        *creating.lock().unwrap() = false;
        let pod = made.map_err(|e| format!("creating the VM: {e}"))?;
        *server.lock().unwrap() = Some(pod.id.clone());
        let pod = with_ip(cloud, pod).await?;
        let t = target(&pod).map_err(|e| e.to_string())?;
        emit(&format!("setting up {} ({}) — `pods setup`'s steps, no tokens (~10 min)…", plan.name, t.host));
        let started = tokio::time::Instant::now();
        let outcome = provision(remote, &t, &plan.setup, BootRetry::default()).await;
        took.lock().unwrap().0 = Some(started.elapsed());
        if !outcome.is_done() {
            return Err(format!("setup {}", outcome.describe()));
        }
        for w in outcome.warnings() {
            emit(&format!("  warning: {w}"));
        }
        emit("scrubbing it (keys, tokens, history, name, swap, identity, logs) and scanning for secrets…");
        let out = remote.exec(&t, &plan.scrub, Some(SCRUB_TIMEOUT)).await.map_err(|e| format!("scrubbing: {}", crate::remote::describe_error(&e)))?;
        let rep = parse_scrub(&out.stdout);
        if !rep.hits.is_empty() {
            return Err(format!(
                "refusing to snapshot: the scan found a secret in {} file(s) after the scrub: {} — nothing was snapshotted",
                rep.hits.len(),
                rep.hits.join(", ")
            ));
        }
        if !out.success || !rep.done {
            let why = out.stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no message").trim().to_string();
            return Err(format!("the scrub failed ({}): {why} — nothing was snapshotted", out.exit_label()));
        }
        let (commit, req) = match (rep.commit, rep.req) {
            (Some(c), Some(r)) => (c, r),
            _ => return Err("the scrub didn't say the checkout's commit and requirements hash — nothing was snapshotted".into()),
        };
        emit(&format!("✓ scrubbed; the scan found nothing (commit {commit}, requirements {req}); powering off…"));
        cloud.stop(&pod.id).await.map_err(|e| format!("powering off: {e}"))?;
        let baked = Baked { built: plan.built, commit, req, arch: plan.arch.clone(), disk_gb: plan.disk_gb };
        let labels = super::labels(&plan.scope, &baked);
        emit("snapshotting the disk (a few minutes)…");
        let started = tokio::time::Instant::now();
        let (id, action) = cloud.create_image(&pod.id, &plan.name, &labels).await.map_err(|e| format!("snapshotting: {e}"))?;
        *image.lock().unwrap() = Some((id, false));
        if let Some(a) = action {
            cloud.wait_action(a, SNAPSHOT_TIMEOUT).await.map_err(|e| format!("snapshotting: {e}"))?;
        }
        let snap = cloud.image(id).await.map_err(|e| format!("reading the new image {id}: {e}"))?.ok_or_else(|| format!("the new image {id} is gone"))?;
        took.lock().unwrap().1 = Some(started.elapsed());
        *image.lock().unwrap() = Some((id, true));
        Ok(snap)
    };
    let result = tokio::select! {
        r = work => r,
        () = stop => Err("interrupted (Ctrl+C)".to_string()),
    };

    // Whatever happened: a half-made snapshot goes, and the VM always does.
    let mut orphan_image = None;
    let half_made = *image.lock().unwrap();
    if let (Err(_), Some((id, false))) = (&result, half_made) {
        if let Err(e) = cloud.delete_image(id).await {
            orphan_image = Some((id, e.to_string()));
        }
    }
    let known = server.lock().unwrap().clone();
    let in_flight = *creating.lock().unwrap();
    let vm = delete_vm(cloud, plan, known, in_flight, emit).await;
    let (setup_took, snapshot_took) = *took.lock().unwrap();
    Report { result, vm, orphan_image, setup_took, snapshot_took }
}

/// `pod` with its IPv4 (Hetzner usually has it in the create's answer; else asked again).
async fn with_ip(cloud: &dyn ImageCloud, pod: Pod) -> std::result::Result<Pod, String> {
    if pod.ssh_ip.is_some() {
        return Ok(pod);
    }
    let deadline = tokio::time::Instant::now() + IP_WAIT;
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if let Ok(Some(p)) = cloud.server(&pod.id).await {
            if p.ssh_ip.is_some() {
                return Ok(Pod { ssh_port: p.ssh_port.or(Some(22)), ..p });
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("the VM had no IPv4 after {}s", IP_WAIT.as_secs()));
        }
    }
}

/// Delete the build VM: `known` (its id); else, when a create was cut off mid-flight
/// (`in_flight`: the server may exist with its id never seen), the server of this build's name
/// with the build label. Never by name after a create that *failed*: that made no server, and
/// a namesake is someone else's bake. Retried.
async fn delete_vm(cloud: &dyn ImageCloud, plan: &BuildPlan, known: Option<String>, in_flight: bool, emit: &(dyn Fn(&str) + Sync)) -> VmFate {
    let ids: Vec<String> = match known {
        Some(id) => vec![id],
        None if !in_flight => return VmFate::NeverMade,
        // Give the cut-off POST time to land before looking for what it made.
        None => match async {
            tokio::time::sleep(DELETE_PAUSE).await;
            cloud.servers(&plan.scope.selector(BUILD_LABEL)).await
        }
        .await
        {
            Ok(servers) => servers.into_iter().filter(|p| p.name == plan.name).map(|p| p.id).collect(),
            // Couldn't look: say so — a VM may be there.
            Err(e) => {
                return VmFate::Left { id: "?".into(), why: format!("couldn't list the servers to find {}: {e}", plan.name) };
            }
        },
    };
    let Some(id) = ids.first().cloned() else { return VmFate::NeverMade };
    let mut fate = VmFate::Deleted { id: id.clone() };
    for id in ids {
        let mut last = String::new();
        let mut gone = false;
        for i in 0..DELETE_TRIES {
            if i > 0 {
                tokio::time::sleep(DELETE_PAUSE).await;
            }
            match cloud.delete_server(&id).await {
                Ok(()) => {
                    gone = true;
                    break;
                }
                // Already gone counts.
                Err(e) if matches!(cloud.server(&id).await, Ok(None)) => {
                    let _ = e;
                    gone = true;
                    break;
                }
                Err(e) => last = e.to_string(),
            }
        }
        if gone {
            emit(&format!("✓ deleted the build VM {} (id {id})", plan.name));
        } else {
            fate = VmFate::Left { id: id.clone(), why: last };
        }
    }
    fate
}
