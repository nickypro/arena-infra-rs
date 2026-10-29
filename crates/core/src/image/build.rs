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
use super::{parse_scrub, Baked, Scope, Snapshot, BUILD_ID_LABEL};
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
/// Tries at deleting the VM before it's reported as left behind, and the pause between them
/// (also: how often a bake whose create answer never came looks for what it made).
pub const DELETE_TRIES: u32 = 4;
pub const DELETE_PAUSE: Duration = Duration::from_secs(5);
/// How many times, [`DELETE_PAUSE`] apart, a bake whose create or snapshot request may have been
/// taken (its answer never came) looks for what it made, before saying it can't tell.
pub const LOOK_TRIES: u32 = 12;

/// What to build.
pub struct BuildPlan {
    /// The VM's name, which is also the snapshot's description ([`super::build_name`]).
    pub name: String,
    /// This bake's own id ([`super::new_build_id`]), labelled on its VM and its snapshot.
    pub build_id: String,
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
    /// A create was sent and its answer never came, and no VM with this bake's id turned up in a
    /// minute of looking: one may still appear, billing (`teardown --check` lists it).
    Unsure { build_id: String, why: String },
}

/// How a build ended: the snapshot, or why not — and the VM's fate either way.
#[derive(Debug)]
pub struct Report {
    pub result: std::result::Result<Snapshot, String>,
    pub vm: VmFate,
    /// A snapshot that may be left: half-made and its delete failed, or asked for with no
    /// answer and not found — what to say about it.
    pub orphan_image: Option<String>,
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
    // The create: sent, and how it ended — refused by Hetzner (an HTTP 4xx answer: nothing was
    // made), or unknown (an error after Hetzner may have taken it, a Ctrl+C before the answer):
    // then a server may exist with its id never seen, and it's looked for by this bake's id.
    let create: Mutex<Request> = Mutex::new(Request::NotSent);
    // Likewise the snapshot: `(id, done)` once known.
    let image: Mutex<Option<(u64, bool)>> = Mutex::new(None);
    let snapshot: Mutex<Request> = Mutex::new(Request::NotSent);
    let took: Mutex<(Option<Duration>, Option<Duration>)> = Mutex::new((None, None));

    let work = async {
        emit(&format!("creating {} ({}, {})…", plan.name, plan.server_type, super::FALLBACK));
        let labels = super::build_vm_labels(&plan.scope, &plan.build_id);
        *create.lock().unwrap() = Request::Sent;
        let made = cloud.create_server(&plan.name, &plan.server_type, labels).await;
        *create.lock().unwrap() = Request::ended(&made);
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
        let baked = Baked { build_id: plan.build_id.clone(), built: plan.built, commit, req, arch: plan.arch.clone(), disk_gb: plan.disk_gb };
        let labels = super::labels(&plan.scope, &baked);
        emit("snapshotting the disk (a few minutes)…");
        let started = tokio::time::Instant::now();
        *snapshot.lock().unwrap() = Request::Sent;
        let asked = cloud.create_image(&pod.id, &plan.name, &labels).await;
        *snapshot.lock().unwrap() = Request::ended(&asked);
        let (id, action) = asked.map_err(|e| format!("snapshotting: {e}"))?;
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
    let asked = *snapshot.lock().unwrap();
    let selector = format!("{BUILD_ID_LABEL}={}", super::label_value(&plan.build_id));
    if result.is_err() {
        let ids: Vec<u64> = match (half_made, asked) {
            (Some((id, false)), _) => vec![id],
            (Some((_, true)), _) => Vec::new(),
            // Asked for, answer never seen: this bake's own snapshot, once Hetzner lists it.
            (None, Request::Sent | Request::Unknown) => {
                let found = look_for(|| async { cloud.snapshots(Some(&selector)).await.map(|s| s.into_iter().map(|s| s.id).collect::<Vec<_>>()) }).await;
                match found {
                    Ok(ids) => ids,
                    Err(why) => {
                        orphan_image = Some(format!("a snapshot was asked for and no answer came; {why} — look for label {selector} (`arena image list`)"));
                        Vec::new()
                    }
                }
            }
            _ => Vec::new(),
        };
        for id in ids {
            if let Err(e) = cloud.delete_image(id).await {
                orphan_image = Some(format!("the half-made image {id} couldn't be deleted ({e}) — `arena image prune --keep 0` or the Hetzner console"));
            }
        }
    }
    let known = server.lock().unwrap().clone();
    let sent = *create.lock().unwrap();
    let vm = delete_vm(cloud, plan, known, sent, emit).await;
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

/// A request to Hetzner that makes something (the VM, the snapshot), as far as the bake knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    NotSent,
    /// Sent, no answer yet (a Ctrl+C now leaves it unknown).
    Sent,
    /// Answered with what it made.
    Made,
    /// Hetzner answered no (an HTTP 4xx): nothing was made.
    Refused,
    /// Failed some other way (the connection, a 5xx): it may have been made.
    Unknown,
}

impl Request {
    fn ended<T>(r: &crate::Result<T>) -> Self {
        match r {
            Ok(_) => Request::Made,
            Err(e) if e.to_string().contains(" HTTP 4") => Request::Refused,
            Err(_) => Request::Unknown,
        }
    }
}

/// Look for what a request whose answer never came made, [`LOOK_TRIES`] times [`DELETE_PAUSE`]
/// apart (it may take a moment to be listed): what turned up, or why it can't be told — none
/// found, or the listing failing.
async fn look_for<T, F, Fut>(list: F) -> std::result::Result<Vec<T>, String>
where
    F: Fn() -> Fut,
    Fut: Future<Output = crate::Result<Vec<T>>>,
{
    let mut last = String::from("none found in a minute of looking");
    for i in 0..LOOK_TRIES {
        if i > 0 {
            tokio::time::sleep(DELETE_PAUSE).await;
        }
        match list().await {
            Ok(found) if !found.is_empty() => return Ok(found),
            Ok(_) => last = "none found in a minute of looking".into(),
            Err(e) => last = format!("couldn't list them: {e}"),
        }
    }
    Err(last)
}

/// Delete the build VM: `known` (its id); else, when the create's answer never came (`sent`:
/// [`Request::Sent`] / [`Request::Unknown`] — an error after Hetzner may have taken it, a Ctrl+C
/// before it came), the servers labelled with this bake's id (never by name: a namesake is another
/// bake's), looked for over [`LOOK_TRIES`]; none found is [`VmFate::Unsure`], never "never made".
/// A refused create made nothing. Retried.
async fn delete_vm(cloud: &dyn ImageCloud, plan: &BuildPlan, known: Option<String>, sent: Request, emit: &(dyn Fn(&str) + Sync)) -> VmFate {
    let ids: Vec<String> = match (known, sent) {
        (Some(id), _) => vec![id],
        (None, Request::NotSent | Request::Refused | Request::Made) => return VmFate::NeverMade,
        (None, Request::Sent | Request::Unknown) => {
            let selector = format!("{BUILD_ID_LABEL}={}", super::label_value(&plan.build_id));
            match look_for(|| async { cloud.servers(&selector).await.map(|s| s.into_iter().map(|p| p.id).collect::<Vec<_>>()) }).await {
                Ok(ids) => ids,
                Err(why) => return VmFate::Unsure { build_id: plan.build_id.clone(), why },
            }
        }
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
