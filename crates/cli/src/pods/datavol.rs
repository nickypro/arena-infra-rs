//! `pods volume add|grow|mount|list|detach|delete`: extra disk for a Hetzner VM
//! ([`arena_core::datavol`]) — and the volumes a terminate, a replace and a migration meet.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::archive::cloud::CloudVolume;
use arena_core::capability::Op;
use arena_core::datavol::{self, Mounted, VolumeCloud};
use arena_core::provider::Provider;
use arena_core::remote::Remote;
use arena_core::selector::Naming;
use arena_core::ssh::SshTarget;
use arena_core::{Config, Pod};
use clap::Subcommand;

use crate::confirm;

#[derive(Subcommand, Debug)]
pub(crate) enum VolumeCmd {
    /// Add a volume to a Hetzner VM, mounted at --mount (default /mnt/data)
    ///
    /// Created in the VM's location, formatted ext4, attached and mounted (an fstab line with
    /// nofail). 10 GB to 10 TB. It bills until deleted: a terminate keeps it unless
    /// --delete-volumes.
    #[command(verbatim_doc_comment)]
    Add {
        /// The VM (name or id).
        name: String,
        /// Size in GB (10 to 10240).
        gb: u32,
        /// Where to mount it (not inside a home: backups and moves would copy it whole)
        #[arg(long, default_value = datavol::DEFAULT_MOUNT)]
        mount: String,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Grow a VM's volume to GB (online; never shrinks), filesystem included
    Grow {
        /// The VM (name or id) whose volume grows, or the volume's name or id.
        name: String,
        /// The new size in GB.
        gb: u32,
        /// Which of the VM's volumes, when it has several (name or id).
        #[arg(long)]
        volume: Option<String>,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Mount a VM's attached volume (after a power cycle, or a fix)
    Mount {
        /// The VM (name or id).
        name: String,
        /// Where (default: its fstab line's place, else /mnt/data).
        #[arg(long)]
        mount: Option<String>,
        /// Which of the VM's volumes, when it has several (name or id).
        #[arg(long)]
        volume: Option<String>,
    },
    /// List the project's volumes (the archive's aside): size, VM, cost
    List {
        /// JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Unmount and detach a volume (it keeps its files, and keeps billing)
    Detach {
        /// The volume (name or id), or a VM with one volume.
        name: String,
        /// Detach even when the VM can't be reached to unmount it first (unsafe while in use)
        #[arg(long)]
        force: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// DELETE a volume and its files — irreversible (detached first)
    Delete {
        /// The volume, by its name or id (never a VM's name: say which volume).
        name: String,
        /// Detach even when the VM can't be reached to unmount it first (unsafe while in use)
        #[arg(long)]
        force: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
}

/// How long one on-VM step (mount, grow, unmount) may take over SSH.
const STEP_TIMEOUT: Duration = Duration::from_secs(150);

/// The fleet's Hetzner project as a volume API (`HETZNER_API_KEY`), when there's a key — never
/// in this crate's unit tests (no key reaches the network).
pub(crate) fn cloud_for(cfg: &Config) -> Option<arena_core::archive::cloud::HetznerArchiveCloud> {
    if cfg!(test) {
        return None;
    }
    let key = cfg.get("HETZNER_API_KEY").map(str::trim).filter(|k| !k.is_empty())?;
    Some(arena_core::archive::cloud::HetznerArchiveCloud::new(key))
}

/// `pods volume …` on the configured fleet.
pub(crate) async fn handle_volume(cmd: VolumeCmd, remote: Arc<dyn Remote>, fleet: &dyn Provider, cfg: &Config, yes: bool) -> Result<()> {
    let cloud = cloud_for(cfg).context("volumes are Hetzner's: set HETZNER_API_KEY (`arena config check`)")?;
    volume_on(cmd, remote, fleet, &cloud, cfg, yes).await
}

/// This cohort's volumes ([`datavol::cohort_volumes`]: its label, or on one of its VMs) and the
/// project's other ones (never acted on here), from one listing of each.
async fn cohort(cloud: &dyn VolumeCloud, prefix: &str, pods: &[Pod]) -> Result<(Vec<CloudVolume>, Vec<CloudVolume>)> {
    let all = datavol::fleet_volumes(&cloud.all_volumes().await.context("listing the volumes")?);
    let ids: Vec<String> = pods.iter().filter(|p| p.provider == "hetzner").map(|p| p.id.clone()).collect();
    let ours = datavol::cohort_volumes(&all, prefix, &ids);
    let others = all.into_iter().filter(|v| !ours.iter().any(|o| o.id == v.id)).collect();
    Ok((ours, others))
}

/// Refuse a volume arena didn't make (no `arena-volume=<prefix>` label: attached by hand): its
/// layout — partitions, the filesystem, its fstab line — is someone else's, so arena never
/// unmounts, detaches, grows, moves or deletes it (review). Listed and counted all the same.
fn ours_only(v: &CloudVolume, prefix: &str, verb: &str) -> Result<()> {
    if datavol::is_ours(v, prefix) {
        return Ok(());
    }
    anyhow::bail!(
        "not {verb} {}: it was attached by hand (no `{}={prefix}` label) — arena acts only on the volumes it made \
         (`arena pods volume add`); do it in the Hetzner console",
        v.name,
        datavol::LABEL
    )
}

/// One VM by the shared single-pod matcher.
fn one_pod(cfg: &Config, pods: &[Pod], target: &str) -> Result<Pod> {
    let naming = Naming::from_config(cfg);
    Ok(pods[arena_core::selector::resolve_one(&naming, pods, target)?].clone())
}

/// Refuse a pod whose provider can't have a volume (the capability table: RunPod and Vast say
/// what to do instead), then one that isn't running.
fn usable(fleet: &dyn Provider, pod: &Pod, verb: &str) -> Result<u64> {
    if let Some(r) = crate::pods::capable::refusal(fleet, Op::DataVolume, verb, &[pod]) {
        anyhow::bail!("{r}");
    }
    if arena_core::status::is_stopped(&pod.status) {
        anyhow::bail!("{} is {} — `arena pods start {}` first", pod.name, pod.status, pod.name);
    }
    pod.id.parse::<u64>().map_err(|_| anyhow::anyhow!("{} (id {}) isn't a Hetzner server", pod.name, pod.id))
}

/// The identity check for `pod`, by ITS provider (the fleet provider a command holds may be
/// named after another: `--provider`'s — live, a RunPod check refused a Hetzner VM).
fn guard(pod: &Pod) -> Option<String> {
    arena_core::replica::pod_identity(&pod.provider, &pod.id)
}

/// `script` run by `/bin/sh`, whatever the login shell is: sshd hands a command to it, and root's
/// on arena's VMs is zsh, which doesn't split an unquoted `$var` into words — the volume scripts'
/// fstab reading needs POSIX (live: zsh kept every fstab line, ours twice).
fn posix(script: &str) -> String {
    format!("sh -c '{}'", script.replace('\'', "'\\''"))
}

/// Run `script` as root on `pod`, behind its own identity check; its stdout.
async fn on_vm(remote: &dyn Remote, cfg: &Config, _fleet: &dyn Provider, pod: &Pod, script: &str) -> Result<String> {
    let target = SshTarget::from_pod(pod, cfg)?;
    let cmd = posix(&arena_core::replica::guarded(guard(pod).as_deref(), script));
    let out = remote.exec(&target, &cmd, Some(STEP_TIMEOUT)).await.with_context(|| format!("reaching {}", pod.name))?;
    if out.code == Some(arena_core::replica::WRONG_POD_EXIT) {
        anyhow::bail!("{}'s address answered as another machine — nothing was done there", pod.name);
    }
    // A step that failed without saying how (ssh itself, a shell that never ran it) is no
    // answer — never read as "nothing there" (a mount point would fall back to the default).
    if !out.success && !out.stdout.contains("ARENA_VOL_") {
        anyhow::bail!("{} didn't answer the volume step: {}", pod.name, crate::pods::home_copy::copy_failure_reason(&out.stderr));
    }
    Ok(out.stdout)
}

/// Wait for each of Hetzner's actions.
async fn wait_all(cloud: &dyn VolumeCloud, actions: &[u64], what: &str) -> Result<()> {
    for a in actions {
        cloud.wait_action(*a, datavol::ACTION_WAIT).await.with_context(|| format!("waiting for Hetzner to {what}"))?;
    }
    Ok(())
}

/// The volume `token` names (its name or id) among `all`, else the only one attached to the VM
/// it names (`allow_pod`).
fn find_volume(cfg: &Config, all: &[CloudVolume], pods: &[Pod], token: &str, allow_pod: bool) -> Result<CloudVolume> {
    let t = token.trim();
    if let Some(v) = all.iter().find(|v| v.name == t || v.id.to_string() == t) {
        return Ok(v.clone());
    }
    if allow_pod {
        if let Ok(pod) = one_pod(cfg, pods, t) {
            if pod.provider != "hetzner" {
                anyhow::bail!("{} is a {} pod — volumes are Hetzner VMs'", pod.name, pod.provider);
            }
            let on = datavol::attached_to(all, &pod.id);
            return match on.as_slice() {
                [one] => Ok((*one).clone()),
                [] => anyhow::bail!("{} has no volume attached", pod.name),
                many => anyhow::bail!(
                    "{} has {} volumes — name one: {}",
                    pod.name,
                    many.len(),
                    many.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")
                ),
            };
        }
    }
    let names: Vec<&str> = all.iter().map(|v| v.name.as_str()).collect();
    anyhow::bail!(
        "no volume named {t} ({}) — `arena pods volume list`",
        if names.is_empty() { "there are none".to_string() } else { names.join(", ") }
    )
}

/// The one volume of `pod` to act on: `which` (name or id), else its only one. Only a Hetzner
/// VM has any (another provider's pod with the same numeric id is never matched — review).
fn pod_volume(all: &[CloudVolume], pod: &Pod, which: Option<&str>) -> Result<CloudVolume> {
    if pod.provider != "hetzner" {
        anyhow::bail!("{} is a {} pod — volumes are Hetzner VMs' (`arena pods volume` there)", pod.name, pod.provider);
    }
    let on = datavol::attached_to(all, &pod.id);
    match (which, on.as_slice()) {
        (Some(w), _) => on
            .iter()
            .find(|v| v.name == w.trim() || v.id.to_string() == w.trim())
            .map(|v| (*v).clone())
            .with_context(|| format!("{} has no volume {w} attached", pod.name)),
        (None, [one]) => Ok((*one).clone()),
        (None, []) => anyhow::bail!("{} has no volume attached (`arena pods volume add {} <GB>`)", pod.name, pod.name),
        (None, many) => anyhow::bail!(
            "{} has {} volumes — pick one with --volume ({})",
            pod.name,
            many.len(),
            many.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// `€0.44/month`.
fn eur_month(x: f64) -> String {
    format!("{}/month", arena_core::fleet::fmt_money("€", x))
}

/// The words for a mount that didn't happen, after the volume was created/attached.
fn not_mounted(pod: &str, vol: &CloudVolume, m: Mounted) -> anyhow::Error {
    match m {
        Mounted::NoDevice => anyhow::anyhow!(
            "{pod} doesn't see volume {} yet (attached at Hetzner, but no {} after {} s). A server that has run a long time \
             may need one full power cycle the first time: `arena pods stop {pod}` then `arena pods start {pod}` (a restart \
             is a reset, which may not do). Its fstab line is in place, so it mounts at that boot; `arena pods volume mount \
             {pod}` checks.",
            vol.name,
            datavol::device(vol),
            datavol::DEVICE_WAIT_SECS
        ),
        Mounted::Failed(why) => anyhow::anyhow!(
            "volume {} is attached to {pod} but not mounted: {why}. Fix that, then `arena pods volume mount {pod}` (or `arena \
             pods volume delete {}` if it's not wanted — it bills until deleted)",
            vol.name,
            vol.name
        ),
        Mounted::Yes { .. } => anyhow::anyhow!("mounted"),
    }
}

/// [`handle_volume`] against these backends: `fleet` lists and owns the VMs, `cloud` makes the
/// calls — the seam tests drive with fakes.
pub(crate) async fn volume_on(cmd: VolumeCmd, remote: Arc<dyn Remote>, fleet: &dyn Provider, cloud: &dyn VolumeCloud, cfg: &Config, yes: bool) -> Result<()> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
    match cmd {
        VolumeCmd::List { json } => {
            let pods = fleet.list_pods().await.context("listing the fleet")?;
            let (all, others) = cohort(cloud, &prefix, &pods).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?);
                return Ok(());
            }
            if all.is_empty() {
                println!("no volumes (`arena pods volume add <name> <GB>` makes one)");
            } else {
                let names = |id: u64| pods.iter().find(|p| p.provider == "hetzner" && p.id == id.to_string()).map(|p| p.name.clone());
                let price = datavol::price_gb_month(cloud).await;
                print!("{}", datavol::render(&all, &names, price));
            }
            if !others.is_empty() {
                println!(
                    "({} other volume(s) in the project aren't this cohort's — no `{}={prefix}` label, not on its VMs: {})",
                    others.len(),
                    datavol::LABEL,
                    others.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
            Ok(())
        }
        VolumeCmd::Add { name, gb, mount, dry_run } => {
            let pods = fleet.list_pods().await.context("listing the fleet")?;
            let pod = one_pod(cfg, &pods, &name)?;
            let server = usable(fleet, &pod, "add a volume to")?;
            datavol::check_size(gb).map_err(anyhow::Error::msg)?;
            datavol::check_mount(&mount).map_err(anyhow::Error::msg)?;
            let mount = mount.trim_end_matches('/').to_string();
            let all = cloud.all_volumes().await.context("listing the volumes")?;
            let taken: Vec<&str> = all.iter().map(|v| v.name.as_str()).collect();
            let vname = datavol::volume_name(&pod.name, &taken);
            let price = datavol::price_gb_month(cloud).await;
            let at = pod.location.as_deref().unwrap_or("its location");
            let already = datavol::attached_to(&all, &pod.id);
            let plan = format!(
                "create volume {vname} ({gb} GB, ext4, in {at}) attached to {}, mounted at {mount} (fstab, nofail) — \
                 ≈ {} until deleted (a terminate keeps it unless --delete-volumes){}",
                pod.name,
                eur_month(f64::from(gb) * price),
                if already.is_empty() { String::new() } else { format!("; it has {} already", already.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")) }
            );
            if dry_run {
                println!("[dry-run] would {plan}\n[dry-run] nothing changed.");
                return Ok(());
            }
            if !confirm(yes, &format!("Will {plan}."))? {
                println!("aborted.");
                return Ok(());
            }
            let (v, actions) = cloud.create_volume(&datavol::create_body(&vname, gb, server, &prefix)).await.context("creating the volume")?;
            println!("[created] {} (id={}, {gb} GB) — attaching to {}…", v.name, v.id, pod.name);
            wait_all(cloud, &actions, "create and attach the volume").await?;
            let said = on_vm(remote.as_ref(), cfg, fleet, &pod, &datavol::mount_script(&datavol::device(&v), &mount)).await?;
            match datavol::parse_mount(&said) {
                Mounted::Yes { bytes } => {
                    println!(
                        "[mounted] {} at {mount} on {}{}",
                        v.name,
                        pod.name,
                        bytes.map(|b| format!(" ({:.1} GB usable)", b as f64 / 1e9)).unwrap_or_default()
                    );
                    Ok(())
                }
                other => Err(not_mounted(&pod.name, &v, other)),
            }
        }
        VolumeCmd::Mount { name, mount, volume } => {
            let pods = fleet.list_pods().await.context("listing the fleet")?;
            let pod = one_pod(cfg, &pods, &name)?;
            usable(fleet, &pod, "mount a volume on")?;
            let all = cloud.all_volumes().await.context("listing the volumes")?;
            let v = pod_volume(&all, &pod, volume.as_deref())?;
            ours_only(&v, &prefix, "mounting")?;
            let mount = match mount {
                Some(m) => {
                    datavol::check_mount(&m).map_err(anyhow::Error::msg)?;
                    m.trim_end_matches('/').to_string()
                }
                None => mount_point(remote.as_ref(), cfg, fleet, &pod, &v).await?,
            };
            match datavol::parse_mount(&on_vm(remote.as_ref(), cfg, fleet, &pod, &datavol::mount_script(&datavol::device(&v), &mount)).await?) {
                Mounted::Yes { .. } => {
                    println!("[mounted] {} at {mount} on {}", v.name, pod.name);
                    Ok(())
                }
                other => Err(not_mounted(&pod.name, &v, other)),
            }
        }
        VolumeCmd::Grow { name, gb, volume, dry_run } => {
            let pods = fleet.list_pods().await.context("listing the fleet")?;
            let (all, _) = cohort(cloud, &prefix, &pods).await?;
            let v = match one_pod(cfg, &pods, &name) {
                Ok(pod) => pod_volume(&all, &pod, volume.as_deref())?,
                Err(_) => find_volume(cfg, &all, &pods, &name, false)?,
            };
            ours_only(&v, &prefix, "growing")?;
            datavol::check_size(gb).map_err(anyhow::Error::msg)?;
            if gb < v.size_gb {
                anyhow::bail!("{} is {} GB — a Hetzner volume only grows (pass more than {} GB)", v.name, v.size_gb, v.size_gb);
            }
            // The size it has: only the filesystem grows (a retry after a failed filesystem step).
            let fs_only = gb == v.size_gb;
            let pod = v.server.and_then(|s| pods.iter().find(|p| p.provider == "hetzner" && p.id == s.to_string())).cloned();
            if let Some(p) = &pod {
                usable(fleet, p, "grow a volume on")?;
            }
            let price = datavol::price_gb_month(cloud).await;
            let plan = format!(
                "{}{}",
                if fs_only {
                    format!("grow {}'s filesystem to its {gb} GB (Hetzner has that size already)", v.name)
                } else {
                    format!(
                        "grow {} from {} GB to {gb} GB (≈ {} → {})",
                        v.name,
                        v.size_gb,
                        eur_month(f64::from(v.size_gb) * price),
                        eur_month(f64::from(gb) * price)
                    )
                },
                match &pod {
                    Some(p) => format!(", then its filesystem on {} (online)", p.name),
                    None => " — not attached to a fleet VM: its filesystem grows the next time it's mounted and grown".to_string(),
                }
            );
            if dry_run {
                println!("[dry-run] would {plan}\n[dry-run] nothing changed.");
                return Ok(());
            }
            if !confirm(yes, &format!("Will {plan}. A volume never shrinks."))? {
                println!("aborted.");
                return Ok(());
            }
            if !fs_only {
                let a = cloud.resize(v.id, gb).await.context("resizing the volume")?;
                wait_all(cloud, &[a], "resize the volume").await?;
                println!("[resized] {} to {gb} GB at Hetzner", v.name);
            }
            let Some(p) = pod else { return Ok(()) };
            match datavol::parse_grow(&on_vm(remote.as_ref(), cfg, fleet, &p, &datavol::grow_script(&datavol::device(&v))).await?) {
                Ok(bytes) => {
                    println!("[grown] the filesystem on {}{}", p.name, bytes.map(|b| format!(": {:.1} GB", b as f64 / 1e9)).unwrap_or_default());
                    Ok(())
                }
                Err(why) => anyhow::bail!(
                    "{} is {gb} GB at Hetzner, but its filesystem on {} didn't grow: {why} (`arena pods volume grow {} {gb}` \
                     retries that step alone)",
                    v.name,
                    p.name,
                    v.name
                ),
            }
        }
        VolumeCmd::Detach { name, force, dry_run } => {
            let pods = fleet.list_pods().await.context("listing the fleet")?;
            let (all, _) = cohort(cloud, &prefix, &pods).await?;
            let v = find_volume(cfg, &all, &pods, &name, true)?;
            ours_only(&v, &prefix, "detaching")?;
            let Some(server) = v.server else {
                println!("{} isn't attached to anything — nothing to do.", v.name);
                return Ok(());
            };
            let price = datavol::price_gb_month(cloud).await;
            let on = pods.iter().find(|p| p.provider == "hetzner" && p.id == server.to_string()).map(|p| p.name.clone()).unwrap_or_else(|| format!("server {server}"));
            let plan = format!(
                "unmount {} on {on} and detach it — its files stay on it, and it bills ≈ {} until `arena pods volume delete {}`",
                v.name,
                eur_month(f64::from(v.size_gb) * price),
                v.name
            );
            if dry_run {
                println!("[dry-run] would {plan}\n[dry-run] nothing changed.");
                return Ok(());
            }
            if !confirm(yes, &format!("Will {plan}."))? {
                println!("aborted.");
                return Ok(());
            }
            unmount_and_detach(remote.as_ref(), cfg, fleet, cloud, &pods, &v, force).await?;
            println!("[detached] {} (still billing; `arena pods volume delete {}` deletes it)", v.name, v.name);
            Ok(())
        }
        VolumeCmd::Delete { name, force, dry_run } => {
            let pods = fleet.list_pods().await.context("listing the fleet")?;
            let (all, _) = cohort(cloud, &prefix, &pods).await?;
            let v = find_volume(cfg, &all, &pods, &name, false)?;
            ours_only(&v, &prefix, "deleting")?;
            if v.delete_protected {
                anyhow::bail!("{} is delete-protected at Hetzner — lift that in its console first (arena never does)", v.name);
            }
            let on = v.server.map(|s| {
                pods.iter().find(|p| p.provider == "hetzner" && p.id == s.to_string()).map(|p| p.name.clone()).unwrap_or_else(|| format!("server {s}"))
            });
            let plan = format!(
                "{}DELETE volume {} ({} GB, id={}) and every file on it — irreversible",
                on.as_ref().map(|o| format!("unmount it on {o} and detach it, then ")).unwrap_or_default(),
                v.name,
                v.size_gb,
                v.id
            );
            if dry_run {
                println!("[dry-run] would {plan}\n[dry-run] nothing changed.");
                return Ok(());
            }
            if !confirm(yes, &format!("Will {plan}. Back up what's wanted first (it's in no backup)."))? {
                println!("aborted.");
                return Ok(());
            }
            if v.server.is_some() {
                unmount_and_detach(remote.as_ref(), cfg, fleet, cloud, &pods, &v, force).await?;
            }
            cloud.delete_volume(v.id).await.with_context(|| format!("deleting {}", v.name))?;
            println!("[deleted] volume {} ({} GB)", v.name, v.size_gb);
            Ok(())
        }
    }
}

/// Unmount `v` on the fleet VM it's attached to (over SSH), then detach it and wait. A VM that's
/// stopped needs no unmount; one that can't be reached is refused unless `force`.
pub(crate) async fn unmount_and_detach(
    remote: &dyn Remote,
    cfg: &Config,
    fleet: &dyn Provider,
    cloud: &dyn VolumeCloud,
    pods: &[Pod],
    v: &CloudVolume,
    force: bool,
) -> Result<()> {
    let Some(server) = v.server else { return Ok(()) };
    let pod = pods.iter().find(|p| p.provider == "hetzner" && p.id == server.to_string());
    match pod {
        Some(p) if !arena_core::status::is_stopped(&p.status) => {
            let said = on_vm(remote, cfg, fleet, p, &datavol::unmount_script(&datavol::device(v))).await;
            match said.map(|s| datavol::parse_unmount(&s)) {
                Ok(Ok(())) => println!("[unmounted] {} on {}", v.name, p.name),
                Ok(Err(why)) if !force => anyhow::bail!("not detaching {}: {why} (nothing was changed)", v.name),
                Err(e) if !force => anyhow::bail!(
                    "not detaching {}: couldn't unmount it on {} ({e:#}) — --force detaches anyway (unsafe while it's in use)",
                    v.name,
                    p.name
                ),
                Ok(Err(why)) => eprintln!("warning: {why} — detaching anyway (--force)"),
                Err(e) => eprintln!("warning: couldn't unmount it on {} ({e:#}) — detaching anyway (--force)", p.name),
            }
        }
        Some(p) => println!("      {} is {} — nothing mounted to unmount", p.name, p.status),
        None if !force => anyhow::bail!(
            "not detaching {}: it's attached to server {server}, which isn't one of the fleet's (arena can't unmount it there) \
             — --force detaches anyway",
            v.name
        ),
        None => {}
    }
    let a = cloud.detach(v.id).await.with_context(|| format!("detaching {}", v.name))?;
    wait_all(cloud, &[a], "detach the volume").await
}

/// The volumes attached to each of `pods` (Hetzner ones), for a terminate: what it keeps
/// billing, or deletes with `--delete-volumes`. `Err`: the volumes couldn't be listed.
pub(crate) async fn attached_volumes(cloud: &dyn VolumeCloud, pods: &[&Pod]) -> Result<Vec<(String, CloudVolume)>> {
    if !pods.iter().any(|p| p.provider == "hetzner") {
        return Ok(Vec::new());
    }
    let all = datavol::fleet_volumes(&cloud.all_volumes().await.context("listing the volumes")?);
    let mut out = Vec::new();
    for p in pods.iter().filter(|p| p.provider == "hetzner") {
        for v in datavol::attached_to(&all, &p.id) {
            out.push((p.id.clone(), v.clone()));
        }
    }
    Ok(out)
}

/// After a terminate: delete each of `volumes` (detached by the server's deletion — waited for,
/// up to 2 min). One that won't go is said, with how to delete it.
pub(crate) async fn delete_after_terminate(cloud: &dyn VolumeCloud, volumes: &[CloudVolume]) -> Vec<String> {
    let mut failed = Vec::new();
    for v in volumes {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let detached = cloud.all_volumes().await.ok().and_then(|all| all.into_iter().find(|x| x.id == v.id)).is_none_or(|x| x.server.is_none());
            if detached || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        match cloud.delete_volume(v.id).await {
            Ok(()) => println!("[deleted volume] {} ({} GB)", v.name, v.size_gb),
            Err(e) => {
                eprintln!("[FAILED] deleting volume {}: {e} — `arena pods volume delete {}`", v.name, v.name);
                failed.push(v.name.clone());
            }
        }
    }
    failed
}

// ---------------------------------------------------------------------------------------
// The volumes a replace or a migration carries
// ---------------------------------------------------------------------------------------

/// The label on a volume made to hold a copy of another (a move to another location): the
/// original's id — how a re-run finds its copy.
pub(crate) const COPY_OF_LABEL: &str = "arena-volume-copy-of";

/// What happens to the source's volumes when a VM is replaced: none to carry, moved (detach
/// from the old VM, attach to the new: same location only), or copied onto new volumes made on
/// the new VM (another location — a volume can't leave its own).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Carry {
    Nothing,
    Move(Vec<CloudVolume>),
    Copy(Vec<CloudVolume>),
}

/// `nbg1` of `nbg1 (Nuremberg, DE)`. Pure.
fn location_key(l: &str) -> &str {
    l.split_whitespace().next().unwrap_or("")
}

/// How `vols` (the source's) go to a VM in `new_location` ([`Carry`]). Pure.
pub(crate) fn carry(vols: Vec<CloudVolume>, new_location: Option<&str>) -> Carry {
    if vols.is_empty() {
        return Carry::Nothing;
    }
    let same = new_location.is_some_and(|l| vols.iter().all(|v| location_key(&v.location) == location_key(l)));
    if same {
        Carry::Move(vols)
    } else {
        Carry::Copy(vols)
    }
}

/// The source's volumes (`src`: a Hetzner VM): the ones attached to it (the archive's never).
/// Empty without a volume API (not Hetzner, no key).
pub(crate) async fn source_volumes(cloud: Option<&dyn VolumeCloud>, src: &Pod, prefix: &str) -> Result<Vec<CloudVolume>> {
    let Some(cloud) = cloud.filter(|_| src.provider == "hetzner") else { return Ok(Vec::new()) };
    let all = datavol::fleet_volumes(&cloud.all_volumes().await.context("listing the volumes")?);
    let on: Vec<CloudVolume> = datavol::attached_to(&all, &src.id).into_iter().cloned().collect();
    // One attached by hand can't be carried safely (its layout is someone else's): refused
    // before anything is made or moved.
    for v in &on {
        ours_only(v, prefix, "carrying").with_context(|| format!("{} has volume {} attached — detach it first, then run this again", src.name, v.name))?;
    }
    Ok(on)
}

/// The plan's line for the source's volumes, before the new VM's location is known. Pure.
pub(crate) fn plan_line(vols: &[CloudVolume]) -> Option<String> {
    if vols.is_empty() {
        return None;
    }
    let names: Vec<String> = vols.iter().map(|v| format!("{} ({} GB, {})", v.name, v.size_gb, v.location)).collect();
    Some(format!(
        "volumes {}: moved to the new VM at the swap when it's in the same location (detach, attach), else copied onto \
         new volumes there (the originals kept, billing, until `arena pods volume delete`)",
        names.join(", ")
    ))
}

pub(crate) use arena_core::datavol::MoveRecord;

/// `<state dir>/volume-moves/<machine>.json` — in this crate's tests without ARENA_STATE_DIR, a
/// temp dir of the process's (never the real state dir).
fn record_path(cfg: &Config, canonical: &str) -> Option<std::path::PathBuf> {
    let dir = if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        std::env::temp_dir().join(format!("arena-volume-moves-{}", std::process::id()))
    } else {
        arena_core::snapshot::state_dir_for(cfg).ok()?.join("volume-moves")
    };
    Some(dir.join(format!("{canonical}.json")))
}

/// The record for `canonical`'s migration of server `src` (an empty one if none, or if it's
/// about another server).
pub(crate) fn load_record(cfg: &Config, canonical: &str, src: &str) -> MoveRecord {
    let read = record_path(cfg, canonical).and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str::<MoveRecord>(&t).ok());
    match read {
        Some(r) if r.src == src => r,
        _ => MoveRecord { src: src.to_string(), ..Default::default() },
    }
}

/// Save it (0600, through a temp file); a failure is said, never fatal.
pub(crate) fn save_record(cfg: &Config, canonical: &str, r: &MoveRecord) -> Result<()> {
    let path = record_path(cfg, canonical).context("no state dir to keep the migration's volume record in (ARENA_STATE_DIR / HOME)")?;
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(path.parent().expect("a parent"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(r).unwrap_or_default())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path)
    };
    write().with_context(|| format!("saving the migration's volume record {}", path.display()))
}

/// [`save_record`] where a failure only weakens what a later step knows: said, not fatal.
pub(crate) fn save_record_or_say(cfg: &Config, canonical: &str, r: &MoveRecord) {
    if let Err(e) = save_record(cfg, canonical, r) {
        eprintln!("warning: {e:#} — a later cutover/revert won't know where the volumes are mounted");
    }
}

/// Forget it (the migration is finished).
pub(crate) fn clear_record(cfg: &Config, canonical: &str) {
    if let Some(p) = record_path(cfg, canonical) {
        let _ = std::fs::remove_file(p);
    }
}

/// Where each of `vols` is mounted on `pod`: asked there when it runs (its fstab line, else the
/// live mount, else the default), else `known` (read while it ran). A stopped VM whose volume
/// isn't in `known` is refused — never guessed (review: several would all land on /mnt/data).
async fn mounts_on(
    remote: &dyn Remote,
    cfg: &Config,
    owner: &dyn Provider,
    pod: &Pod,
    vols: &[CloudVolume],
    known: &std::collections::BTreeMap<u64, String>,
) -> Result<std::collections::BTreeMap<u64, String>> {
    let mut out = std::collections::BTreeMap::new();
    let up = !arena_core::status::is_stopped(&pod.status);
    for v in vols {
        let at = if up {
            mount_point(remote, cfg, owner, pod, v).await?
        } else {
            known.get(&v.id).cloned().with_context(|| {
                format!(
                    "{} is {} and where its volume {} is mounted wasn't recorded (`arena pods migrate copy` records it) — start it \
                     first, or move the volume by hand (`arena pods volume detach` / `mount`)",
                    pod.name, pod.status, v.name
                )
            })?
        };
        out.insert(v.id, at);
    }
    Ok(out)
}

/// Where `v` is mounted on the running `pod`: its fstab line, else the live mount, else the
/// default.
pub(crate) async fn mount_point(remote: &dyn Remote, cfg: &Config, owner: &dyn Provider, pod: &Pod, v: &CloudVolume) -> Result<String> {
    let said = on_vm(remote, cfg, owner, pod, &datavol::where_script(&datavol::device(v))).await?;
    match datavol::parse_where(&said) {
        datavol::Where::At(p) => Ok(p),
        datavol::Where::Nowhere => Ok(datavol::DEFAULT_MOUNT.to_string()),
        // Never carried somewhere else than it is (review): said, nothing moved.
        datavol::Where::Odd(p) => anyhow::bail!(
            "volume {} is mounted at {p} on {}, a place arena won't put a volume (a home, a system dir) — mount it at a \
             path like {} first (`arena pods volume mount {} --mount …` after unmounting it)",
            v.name,
            pod.name,
            datavol::DEFAULT_MOUNT,
            pod.name
        ),
    }
}

/// Mount `v` on `pod` at `mount`, or why not.
async fn mount_on(remote: &dyn Remote, cfg: &Config, owner: &dyn Provider, pod: &Pod, v: &CloudVolume, mount: &str) -> Result<()> {
    match datavol::parse_mount(&on_vm(remote, cfg, owner, pod, &datavol::mount_script(&datavol::device(v), mount)).await?) {
        Mounted::Yes { .. } => Ok(()),
        other => Err(not_mounted(&pod.name, v, other)),
    }
}

/// Make sure `v` is mounted on `pod` at `mount` (the mount step is idempotent: one already there
/// only gets its fstab line checked).
pub(crate) async fn ensure_mounted(remote: &dyn Remote, cfg: &Config, owner: &dyn Provider, pod: &Pod, v: &CloudVolume, mount: &str) -> Result<()> {
    mount_on(remote, cfg, owner, pod, v, mount).await
}

/// Unmount `v` on `pod` (a stopped one has nothing mounted), or why not.
async fn unmount_on(remote: &dyn Remote, cfg: &Config, owner: &dyn Provider, pod: &Pod, v: &CloudVolume) -> Result<()> {
    if arena_core::status::is_stopped(&pod.status) {
        return Ok(());
    }
    let said = on_vm(remote, cfg, owner, pod, &datavol::unmount_script(&datavol::device(v))).await?;
    datavol::parse_unmount(&said).map_err(anyhow::Error::msg).with_context(|| format!("unmounting {} on {}", v.name, pod.name))
}

fn server_id(p: &Pod) -> Result<u64> {
    p.id.parse().map_err(|_| anyhow::anyhow!("{} (id {}) isn't a Hetzner server", p.name, p.id))
}

/// Move `vols` from VM `from` to VM `to` (same location): where each is mounted is read for all
/// of them first (else `known`, for a stopped `from`), then each is unmounted on `from`,
/// detached, attached to `to` and mounted there at the same place. Any failure — of any step,
/// any volume — puts every one moved so far back ([`put_back`]: unmounted on `to` first) and
/// says how that went. The mount points used, on success.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn move_volumes(
    remote: &dyn Remote,
    cfg: &Config,
    owner: &dyn Provider,
    cloud: &dyn VolumeCloud,
    from: &Pod,
    to: &Pod,
    vols: &[CloudVolume],
    known: &std::collections::BTreeMap<u64, String>,
) -> Result<std::collections::BTreeMap<u64, String>> {
    let to_id = server_id(to)?;
    server_id(from)?;
    // Every mount point first: a lookup that fails moves nothing (review).
    let mounts = mounts_on(remote, cfg, owner, from, vols, known).await?;
    let mut touched: Vec<CloudVolume> = Vec::new();
    for v in vols {
        let mount = mounts[&v.id].clone();
        touched.push(v.clone());
        let step = async {
            unmount_on(remote, cfg, owner, from, v).await?;
            let a = cloud.detach(v.id).await.with_context(|| format!("detaching {}", v.name))?;
            wait_all(cloud, &[a], "detach the volume").await?;
            let a = cloud.attach(v.id, to_id).await.with_context(|| format!("attaching {} to {}", v.name, to.name))?;
            wait_all(cloud, &[a], "attach the volume").await?;
            mount_on(remote, cfg, owner, to, v, &mount).await
        };
        if let Err(e) = step.await {
            let failed = put_back(remote, cfg, owner, cloud, from, to, &touched, &mounts).await;
            anyhow::bail!(
                "moving volume {} to {}: {e:#} — {}",
                v.name,
                to.name,
                if failed.is_empty() {
                    format!("the volume(s) are back on {}", from.name)
                } else {
                    format!("putting them back on {} needs a hand: {}", from.name, failed.join("; "))
                }
            );
        }
        println!("      volume {} moved: {} → {} (mounted at {mount})", v.name, from.name, to.name);
    }
    Ok(mounts)
}

/// Put `vols` back on `from`, mounted where they were (`mounts`): one attached to `to` is
/// unmounted there first — never detached while mounted (review); one that won't unmount is
/// left where it is, mounted, and said. What didn't go back.
#[allow(clippy::too_many_arguments)]
async fn put_back(
    remote: &dyn Remote,
    cfg: &Config,
    owner: &dyn Provider,
    cloud: &dyn VolumeCloud,
    from: &Pod,
    to: &Pod,
    vols: &[CloudVolume],
    mounts: &std::collections::BTreeMap<u64, String>,
) -> Vec<String> {
    let mut failed = Vec::new();
    let (Ok(from_id), Ok(to_id)) = (server_id(from), server_id(to)) else { return vec!["the servers' ids aren't Hetzner's".into()] };
    for v in vols {
        let now = cloud.all_volumes().await.ok().and_then(|all| all.into_iter().find(|x| x.id == v.id));
        let mount = mounts.get(&v.id).cloned().unwrap_or_else(|| datavol::DEFAULT_MOUNT.to_string());
        let r = async {
            match now.as_ref().and_then(|x| x.server) {
                Some(s) if s == from_id => {}
                Some(s) if s == to_id => {
                    unmount_on(remote, cfg, owner, to, v).await.with_context(|| format!("it stays on {}", to.name))?;
                    let a = cloud.detach(v.id).await?;
                    wait_all(cloud, &[a], "detach the volume").await?;
                    let a = cloud.attach(v.id, from_id).await?;
                    wait_all(cloud, &[a], "attach the volume").await?;
                }
                Some(s) => anyhow::bail!("it's attached to server {s} now — left there"),
                None => {
                    let a = cloud.attach(v.id, from_id).await?;
                    wait_all(cloud, &[a], "attach the volume").await?;
                }
            }
            if arena_core::status::is_stopped(&from.status) {
                return Ok(());
            }
            mount_on(remote, cfg, owner, from, v, &mount).await
        };
        if let Err(e) = r.await {
            failed.push(format!("{}: {e:#}", v.name));
        }
    }
    failed
}

/// Copy `vols` (on VM `from`, another location than `to`) onto volumes of the same sizes on `to`:
/// each made the first time (named after `base`, labelled as its copy) and found again on a
/// re-run, mounted where the original is, then synced straight from `from` — each side only with
/// its own volume mounted there; what the sync replaces on the copy kept in its
/// `arena-sync-kept/<stamp>/` ([`datavol::copy_command`]). The originals stay where they are.
/// Original id → copy id, for every one synced.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn copy_volumes(
    remote: &dyn Remote,
    cfg: &Config,
    owner: &dyn Provider,
    cloud: &dyn VolumeCloud,
    from: &Pod,
    to: &Pod,
    base: &str,
    vols: &[CloudVolume],
    known: &std::collections::BTreeMap<u64, String>,
) -> Result<std::collections::BTreeMap<u64, u64>> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
    let to_id = server_id(to)?;
    let mounts = mounts_on(remote, cfg, owner, from, vols, known).await?;
    let stamp = arena_core::replica::sync_stamp(arena_core::snapshot::unix_now());
    let mut done = std::collections::BTreeMap::new();
    for v in vols {
        let mount = mounts[&v.id].clone();
        let all = cloud.all_volumes().await.context("listing the volumes")?;
        let copy = match all.iter().find(|x| x.labels.get(COPY_OF_LABEL) == Some(&v.id.to_string()) && x.server == Some(to_id)) {
            Some(c) => c.clone(),
            None => {
                let taken: Vec<&str> = all.iter().map(|x| x.name.as_str()).collect();
                let name = datavol::volume_name(base, &taken);
                let mut body = datavol::create_body(&name, v.size_gb, to_id, &prefix);
                body["labels"][COPY_OF_LABEL] = serde_json::Value::from(v.id.to_string());
                let (c, actions) = cloud.create_volume(&body).await.with_context(|| format!("creating {name}, the copy of {}", v.name))?;
                wait_all(cloud, &actions, "create and attach the copy").await?;
                println!("      volume {} made on {} ({} GB) for the copy of {}", c.name, to.name, c.size_gb, v.name);
                c
            }
        };
        mount_on(remote, cfg, owner, to, &copy, &mount).await?;
        let remote_key = cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519").to_string();
        let user = cfg.get("SSH_USER").unwrap_or("root").to_string();
        let ip = to.ssh_ip.clone().with_context(|| format!("{} has no address", to.name))?;
        let cmd = datavol::copy_command(
            &ip,
            to.ssh_port.unwrap_or(22),
            &user,
            &remote_key,
            &mount,
            &datavol::device(v),
            &datavol::device(&copy),
            guard(to).as_deref(),
            &stamp,
        );
        let target = SshTarget::from_pod(from, cfg)?;
        let guarded = posix(&arena_core::replica::guarded(guard(from).as_deref(), &cmd));
        let out = remote.exec(&target, &guarded, Some(crate::pods::POD_COPY_TIMEOUT)).await.with_context(|| format!("copying {}", v.name))?;
        if !out.success {
            anyhow::bail!(
                "copying volume {} ({mount}) {} → {}: {}",
                v.name,
                from.name,
                to.name,
                crate::pods::home_copy::copy_failure_reason(&out.stderr)
            );
        }
        println!(
            "      volume {} copied {} → {} ({mount}, onto {}; anything it replaced there is kept in {mount}/{}/{stamp}/)",
            v.name,
            from.name,
            to.name,
            copy.name,
            datavol::COPY_KEPT_DIR
        );
        done.insert(v.id, copy.id);
    }
    Ok(done)
}

/// Whether every one of `vols` has a finished copy on `to` (the record's, its volume still there
/// attached to `to`): what a cutover without a final sync needs before it promotes `to`. `Err`:
/// which haven't.
pub(crate) async fn copies_ready(cloud: &dyn VolumeCloud, record: &MoveRecord, to: &Pod, vols: &[CloudVolume]) -> Result<()> {
    let all = cloud.all_volumes().await.context("listing the volumes")?;
    let missing: Vec<&str> = vols
        .iter()
        .filter(|v| {
            !record
                .copied
                .get(&v.id)
                .is_some_and(|c| all.iter().any(|x| x.id == *c && x.server.is_some_and(|s| s.to_string() == to.id)))
        })
        .map(|v| v.name.as_str())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "no finished copy of volume(s) {} on {} — a cutover without a final sync would promote it without their files: run \
         `arena pods migrate copy` while the original is up, or move them by hand",
        missing.join(", "),
        to.name
    )
}

/// Keep in `record.moved` only the volumes not verifiably back on `src` (attached there): a
/// failed move-back leaves the rest — on `-new`, detached, elsewhere, gone — to recover, never
/// forgotten (review r3). A listing that fails keeps them all.
pub(crate) async fn keep_unresolved(cloud: &dyn VolumeCloud, record: &mut MoveRecord, src: &str) {
    let Ok(all) = cloud.all_volumes().await else { return };
    record.moved.retain(|id| !all.iter().any(|v| v.id == *id && v.server.is_some_and(|s| s.to_string() == src)));
    if record.moved.is_empty() {
        record.moved_to = None;
    }
}

/// A revert's half for the volumes: exactly the ones the record says a cutover moved onto `new`
/// go back to `old`, mounted where they were. `Ok` only when every recorded one is attached to
/// `old` again (review r3: one detached, elsewhere or gone is unresolved, never success).
pub(crate) async fn revert_moves(remote: &dyn Remote, cfg: &Config, owner: &dyn Provider, cloud: &dyn VolumeCloud, new: &Pod, old: &Pod, record: &MoveRecord) -> Result<()> {
    if record.moved.is_empty() || record.moved_to.as_deref() != Some(new.id.as_str()) {
        return Ok(());
    }
    let all = cloud.all_volumes().await.context("listing the volumes (`arena pods volume list` shows where they are)")?;
    let on_new: Vec<CloudVolume> = all
        .iter()
        .filter(|v| record.moved.contains(&v.id) && v.server.is_some_and(|s| s.to_string() == new.id))
        .cloned()
        .collect();
    let moved = if on_new.is_empty() { Ok(Default::default()) } else { move_volumes(remote, cfg, owner, cloud, new, old, &on_new, &record.mounts).await };
    let now = cloud.all_volumes().await.context("listing the volumes again")?;
    let unresolved: Vec<String> = record
        .moved
        .iter()
        .filter(|id| !now.iter().any(|v| v.id == **id && v.server.is_some_and(|s| s.to_string() == old.id)))
        .map(|id| now.iter().find(|v| v.id == *id).map_or_else(|| format!("id {id} (gone)"), |v| v.name.clone()))
        .collect();
    match (unresolved.is_empty(), moved) {
        (true, _) => Ok(()),
        (false, Err(e)) => Err(e).with_context(|| format!("not back on {}: {}", old.name, unresolved.join(", "))),
        (false, Ok(_)) => anyhow::bail!("not back on {}: {} (detached or elsewhere — the Hetzner console)", old.name, unresolved.join(", ")),
    }
}

/// After a move's swap: what became of the source's volumes, in words. Pure.
pub(crate) fn after_line(carry: &Carry, old_name: &str, kept_old: bool) -> Option<String> {
    match carry {
        Carry::Nothing | Carry::Move(_) => None,
        Carry::Copy(vols) => {
            let names: Vec<&str> = vols.iter().map(|v| v.name.as_str()).collect();
            Some(if kept_old {
                format!(
                    "the original volume(s) {} stay on {old_name}; delete them once the copies are checked: `arena pods volume delete {}`",
                    names.join(", "),
                    names[0]
                )
            } else {
                format!(
                    "the original volume(s) {} are KEPT, detached, billing — delete them once the copies are checked: `arena pods volume delete {}`",
                    names.join(", "),
                    names[0]
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::archive::cloud::{ArchiveCloud, CloudCall, CloudServer, FakeArchiveCloud, Inventory};
    use arena_core::remote::{FakeRemote, FakeReply};

    fn vm(id: &str, name: &str, status: &str) -> Pod {
        Pod {
            id: id.into(),
            name: name.into(),
            provider: "hetzner".into(),
            status: status.into(),
            gpu_type: Some("cx23".into()),
            ssh_ip: Some(format!("10.0.0.{id}")),
            ssh_port: Some(22),
            location: Some("nbg1 (Nuremberg, DE)".into()),
            ..Default::default()
        }
    }

    fn cfg() -> Config {
        Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo)\n")
    }

    /// The fleet's VMs as servers the fake project knows (a volume created attached takes its
    /// location from them).
    fn project(pods: &[Pod]) -> FakeArchiveCloud {
        FakeArchiveCloud::with(Inventory {
            servers: pods
                .iter()
                .map(|p| CloudServer {
                    id: p.id.parse().unwrap(),
                    name: p.name.clone(),
                    status: "running".into(),
                    location: "nbg1".into(),
                    ipv4: p.ssh_ip.clone(),
                    labels: Default::default(),
                    delete_protected: false,
                    server_type: "cx23".into(),
                })
                .collect(),
            ..Default::default()
        })
    }

    /// The fleet provider, named after `--provider`'s default (RunPod) as the real one is — a
    /// VM's steps must still check the VM's own (Hetzner) id.
    struct Fleet(Vec<Pod>);

    #[async_trait::async_trait]
    impl Provider for Fleet {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _: &arena_core::PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
            Ok(self.0.clone())
        }
        async fn create_pod(&self, _: &arena_core::PodSpec) -> arena_core::Result<Pod> {
            unreachable!()
        }
        async fn stop_pod(&self, _: &str) -> arena_core::Result<()> {
            unreachable!()
        }
        async fn restart_pod(&self, _: &str) -> arena_core::Result<()> {
            unreachable!()
        }
        async fn terminate_pod(&self, _: &str) -> arena_core::Result<()> {
            unreachable!()
        }
    }

    /// add: created attached (ext4, labelled, not automounted), then mounted over SSH behind the
    /// VM's own id; a VM that doesn't see it says power-cycle; a size or a mount path it can't
    /// take is refused before any call.
    #[tokio::test(start_paused = true)]
    async fn add_creates_attaches_and_mounts() {
        let alpha = vm("11", "devtest-alpha", "RUNNING");
        let fleet = Fleet(vec![alpha.clone()]);
        let cloud = project(&fleet.0);
        let remote = Arc::new(FakeRemote::new());
        remote.on("10.0.0.11:22", "ARENA_VOL_MOUNTED", FakeReply::stdout("ARENA_VOL_MOUNTED 10501771264\n"));
        let add = |gb: u32, mount: &str| VolumeCmd::Add { name: "alpha".into(), gb, mount: mount.into(), dry_run: false };
        volume_on(add(10, "/mnt/data"), remote.clone(), &fleet, &cloud, &cfg(), true).await.unwrap();
        let created = cloud.calls().into_iter().find_map(|c| if let CloudCall::CreateVolume(b) = c { Some(b) } else { None }).unwrap();
        assert_eq!(created["name"], "devtest-alpha-data");
        assert_eq!((created["size"].as_u64(), created["server"].as_u64(), created["format"].as_str()), (Some(10), Some(11), Some("ext4")));
        assert_eq!(created["automount"], false);
        assert_eq!(created["labels"]["arena-volume"], "devtest");
        let execs: Vec<String> = remote
            .calls_to("10.0.0.11:22")
            .into_iter()
            .filter_map(|c| if let arena_core::remote::RemoteCall::Exec { cmd, .. } = c { Some(cmd) } else { None })
            .collect();
        assert!(execs.iter().any(|c| c.contains("scsi-0HC_Volume_") && c.contains("'/mnt/data'") && c.contains("instance-id")), "{execs:?}");
        assert!(!execs.iter().any(|c| c.contains("RUNPOD_POD_ID")), "the VM's own id, not the fleet provider's kind: {execs:?}");
        // A second one gets the next name.
        volume_on(add(20, "/mnt/more"), remote.clone(), &fleet, &cloud, &cfg(), true).await.unwrap();
        assert!(cloud.state().volumes.iter().any(|v| v.name == "devtest-alpha-data-2" && v.size_gb == 20));
        // Not seen yet: the power-cycle advice.
        let unseen = Arc::new(FakeRemote::new());
        unseen.on("10.0.0.11:22", "ARENA_VOL_", FakeReply::stdout("ARENA_VOL_NO_DEVICE\n"));
        let e = volume_on(add(10, "/mnt/x"), unseen, &fleet, &cloud, &cfg(), true).await.unwrap_err().to_string();
        assert!(e.contains("power cycle") && e.contains("arena pods stop devtest-alpha"), "{e}");
        // Refused before any call.
        cloud.clear_calls();
        for (cmd, why) in [(add(5, "/mnt/data"), "10 GB to 10240 GB"), (add(10, "/root/data"), "inside a home")] {
            let e = volume_on(cmd, remote.clone(), &fleet, &cloud, &cfg(), true).await.unwrap_err().to_string();
            assert!(e.contains(why), "{e}");
        }
        assert!(cloud.mutations().is_empty(), "{:?}", cloud.mutations());
        let off = Fleet(vec![vm("11", "devtest-alpha", "OFF")]);
        let e = volume_on(add(10, "/mnt/data"), remote, &off, &cloud, &cfg(), true).await.unwrap_err().to_string();
        assert!(e.contains("arena pods start devtest-alpha"), "{e}");
    }

    /// grow: only up; resized at Hetzner, then the filesystem online. detach: unmounted first
    /// (a busy one is refused, nothing changed), then detached. delete: by the volume's name
    /// only, unmounted and detached first, then gone.
    #[tokio::test(start_paused = true)]
    async fn grow_detach_and_delete() {
        let alpha = vm("11", "devtest-alpha", "RUNNING");
        let fleet = Fleet(vec![alpha.clone()]);
        let cloud = project(&fleet.0);
        let remote = Arc::new(FakeRemote::new());
        remote.on("10.0.0.11:22", "resize2fs", FakeReply::stdout("ARENA_VOL_GROWN 11552346112\n"));
        remote.on("10.0.0.11:22", "umount", FakeReply::stdout("ARENA_VOL_UNMOUNTED\n"));
        remote.on("10.0.0.11:22", "ARENA_VOL_MOUNTED", FakeReply::stdout("ARENA_VOL_MOUNTED 10501771264\n"));
        let c = cfg();
        volume_on(VolumeCmd::Add { name: "alpha".into(), gb: 10, mount: "/mnt/data".into(), dry_run: false }, remote.clone(), &fleet, &cloud, &c, true)
            .await
            .unwrap();
        let id = cloud.state().volumes[0].id;
        let grow = |gb: u32| VolumeCmd::Grow { name: "alpha".into(), gb, volume: None, dry_run: false };
        volume_on(grow(11), remote.clone(), &fleet, &cloud, &c, true).await.unwrap();
        assert!(cloud.calls().contains(&CloudCall::Resize(id, 11)));
        let e = volume_on(grow(10), remote.clone(), &fleet, &cloud, &c, true).await.unwrap_err().to_string();
        assert!(e.contains("only grows"), "{e}");
        // Busy: refused, still attached.
        let busy = Arc::new(FakeRemote::new());
        busy.on("10.0.0.11:22", "umount", FakeReply::stdout("ARENA_VOL_BUSY /mnt/data 2 process(es)\n"));
        let e = volume_on(VolumeCmd::Detach { name: "alpha".into(), force: false, dry_run: false }, busy, &fleet, &cloud, &c, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("in use") && e.contains("nothing was changed"), "{e}");
        assert_eq!(cloud.state().volumes[0].server, Some(11));
        // Delete wants the volume's own name, never the VM's.
        let e = volume_on(VolumeCmd::Delete { name: "alpha".into(), force: false, dry_run: false }, remote.clone(), &fleet, &cloud, &c, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("no volume named alpha"), "{e}");
        volume_on(VolumeCmd::Delete { name: "devtest-alpha-data".into(), force: false, dry_run: false }, remote.clone(), &fleet, &cloud, &c, true)
            .await
            .unwrap();
        assert!(cloud.state().volumes.is_empty());
        let m = cloud.mutations();
        let (d, del) = (m.iter().position(|x| *x == CloudCall::Detach(id)).unwrap(), m.iter().position(|x| *x == CloudCall::DeleteVolume(id)).unwrap());
        assert!(d < del, "detached before the delete: {m:?}");
    }

    /// grow to the size it has: the filesystem step alone (a retry — review); another cohort's
    /// volume, or an unlabelled one lying detached, is never deleted from here (review).
    #[tokio::test(start_paused = true)]
    async fn grow_retries_the_filesystem_and_delete_keeps_to_this_cohort() {
        let alpha = vm("11", "devtest-alpha", "RUNNING");
        let fleet = Fleet(vec![alpha.clone()]);
        let cloud = project(&fleet.0);
        let remote = Arc::new(FakeRemote::new());
        remote.on("10.0.0.11:22", "resize2fs", FakeReply::stdout("ARENA_VOL_GROWN 10501771264\n"));
        remote.on("10.0.0.11:22", "ARENA_VOL_MOUNTED", FakeReply::stdout("ARENA_VOL_MOUNTED 10501771264\n"));
        let c = cfg();
        volume_on(VolumeCmd::Add { name: "alpha".into(), gb: 10, mount: "/mnt/data".into(), dry_run: false }, remote.clone(), &fleet, &cloud, &c, true)
            .await
            .unwrap();
        cloud.clear_calls();
        volume_on(VolumeCmd::Grow { name: "alpha".into(), gb: 10, volume: None, dry_run: false }, remote.clone(), &fleet, &cloud, &c, true).await.unwrap();
        assert!(!cloud.calls().iter().any(|c| matches!(c, CloudCall::Resize(..))), "{:?}", cloud.calls());
        // Another cohort's (detached) and an unlabelled detached one: not this cohort's.
        for (name, labels) in [("arena9-x-data", serde_json::json!({"arena-volume": "arena9"})), ("loose", serde_json::json!({}))] {
            let body = serde_json::json!({"name": name, "size": 10, "location": "nbg1", "labels": labels});
            cloud.create_volume(&body).await.unwrap();
            let e = volume_on(VolumeCmd::Delete { name: name.into(), force: false, dry_run: false }, remote.clone(), &fleet, &cloud, &c, true)
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains(&format!("no volume named {name}")), "{e}");
        }
        assert!(!cloud.mutations().iter().any(|c| matches!(c, CloudCall::DeleteVolume(_))));
    }

    /// A stopped VM's mount points come from what was recorded while it ran — never guessed.
    #[tokio::test(start_paused = true)]
    async fn a_stopped_vms_mount_points_are_recorded_not_guessed() {
        let off = vm("11", "devtest-alpha", "OFF");
        let cloud = project(std::slice::from_ref(&off));
        let (v, _) = cloud.create_volume(&datavol::create_body("devtest-alpha-data", 10, 11, "devtest")).await.unwrap();
        let fleet = Fleet(vec![off.clone()]);
        let remote = FakeRemote::new();
        let e = mounts_on(&remote, &cfg(), &fleet, &off, std::slice::from_ref(&v), &Default::default()).await.unwrap_err().to_string();
        assert!(e.contains("wasn't recorded") && e.contains("start it"), "{e}");
        let known = [(v.id, "/srv/models".to_string())].into();
        assert_eq!(mounts_on(&remote, &cfg(), &fleet, &off, std::slice::from_ref(&v), &known).await.unwrap()[&v.id], "/srv/models");
        assert!(remote.calls().is_empty(), "a stopped VM isn't asked");
    }

    /// A volume attached by hand (no label) is listed but never acted on, nor carried by a move;
    /// a pod of another provider with the same numeric id is never matched (review r2).
    #[tokio::test(start_paused = true)]
    async fn arena_acts_only_on_the_volumes_it_made() {
        let alpha = vm("11", "devtest-alpha", "RUNNING");
        let vast = Pod { provider: "vast".into(), name: "devtest-bravo".into(), ..vm("11", "devtest-bravo", "RUNNING") };
        let fleet = Fleet(vec![alpha.clone(), vast]);
        let cloud = project(std::slice::from_ref(&alpha));
        let (v, _) = cloud.create_volume(&serde_json::json!({"name": "by-hand", "size": 10, "server": 11, "labels": {}})).await.unwrap();
        let remote = Arc::new(FakeRemote::new());
        let c = cfg();
        for cmd in [
            VolumeCmd::Delete { name: "by-hand".into(), force: false, dry_run: false },
            VolumeCmd::Detach { name: "by-hand".into(), force: false, dry_run: false },
            VolumeCmd::Grow { name: "alpha".into(), gb: 20, volume: None, dry_run: false },
        ] {
            let e = volume_on(cmd, remote.clone(), &fleet, &cloud, &c, true).await.unwrap_err().to_string();
            assert!(e.contains("attached by hand"), "{e}");
        }
        assert!(cloud.mutations().iter().all(|m| matches!(m, CloudCall::CreateVolume(_))), "{:?}", cloud.mutations());
        let e = source_volumes(Some(&cloud), &alpha, "devtest").await.unwrap_err();
        assert!(format!("{e:#}").contains("detach it first"), "{e:#}");
        // The Vast pod shares the id 11: never its volume.
        let e = volume_on(VolumeCmd::Detach { name: "bravo".into(), force: false, dry_run: false }, remote.clone(), &fleet, &cloud, &c, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("vast pod"), "{e}");
        assert_eq!(cloud.state().volumes.iter().find(|x| x.id == v.id).unwrap().server, Some(11));
        // Mounted somewhere arena won't carry it: said, never guessed.
        let odd = Arc::new(FakeRemote::new());
        odd.on("10.0.0.11:22", "ARENA_VOL_AT", FakeReply::stdout("ARENA_VOL_AT /home/ubuntu/work\n"));
        let e = mount_point(odd.as_ref(), &c, &fleet, &alpha, &v).await.unwrap_err().to_string();
        assert!(e.contains("/home/ubuntu/work") && e.contains("won't put a volume"), "{e}");
    }

    /// A fleet whose terminate deletes the server in the fake project too (which detaches its
    /// volumes, as Hetzner does).
    struct Gone<'a>(std::sync::Mutex<Vec<Pod>>, &'a FakeArchiveCloud);

    #[async_trait::async_trait]
    impl Provider for Gone<'_> {
        fn name(&self) -> &'static str {
            "hetzner"
        }
        fn describe(&self, _: &arena_core::PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
            Ok(self.0.lock().unwrap().clone())
        }
        async fn create_pod(&self, _: &arena_core::PodSpec) -> arena_core::Result<Pod> {
            unreachable!()
        }
        async fn stop_pod(&self, _: &str) -> arena_core::Result<()> {
            unreachable!()
        }
        async fn restart_pod(&self, _: &str) -> arena_core::Result<()> {
            unreachable!()
        }
        async fn terminate_pod(&self, id: &str) -> arena_core::Result<()> {
            self.0.lock().unwrap().retain(|p| p.id != id);
            self.1.delete_server(id.parse().unwrap()).await
        }
    }

    /// terminate: a VM's volume outlives it — kept (and said: billing) by default, deleted with
    /// --delete-volumes once the server's deletion has detached it.
    #[tokio::test(start_paused = true)]
    async fn terminate_keeps_a_volume_unless_told_to_delete_it() {
        use crate::pods::lifecycle::{handle_terminate, TerminateOpts};
        let dir = std::env::temp_dir().join(format!("arena-datavol-term-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let book = crate::keys::KeyBook { csv: dir.join("keys.csv"), api: None };
        for delete_volumes in [false, true] {
            let pods = vec![vm("11", "devtest-alpha", "RUNNING"), vm("12", "devtest-bravo", "RUNNING")];
            let cloud = project(&pods);
            let (v, _) = cloud.create_volume(&datavol::create_body("devtest-alpha-data", 10, 11, "devtest")).await.unwrap();
            let fleet = Gone(std::sync::Mutex::new(pods), &cloud);
            let opts = TerminateOpts { delete_volumes, volumes: Some(&cloud), skip_proxy: true, yes: true, ..Default::default() };
            handle_terminate(&fleet, &cfg(), &book, Some("alpha"), opts).await.unwrap();
            let left = cloud.state().volumes;
            if delete_volumes {
                assert!(left.is_empty(), "{left:?}");
                assert!(cloud.mutations().contains(&CloudCall::DeleteVolume(v.id)));
            } else {
                assert_eq!(left.len(), 1);
                assert_eq!(left[0].server, None, "kept, detached (billing)");
            }
        }
        // --delete-volumes with no volume API: refused before anything is terminated.
        let pods = vec![vm("11", "devtest-alpha", "RUNNING")];
        let cloud = project(&pods);
        let fleet = Gone(std::sync::Mutex::new(pods), &cloud);
        let opts = TerminateOpts { delete_volumes: true, volumes: None, skip_proxy: true, yes: true, ..Default::default() };
        let e = handle_terminate(&fleet, &cfg(), &book, Some("alpha"), opts).await.unwrap_err().to_string();
        assert!(e.contains("HETZNER_API_KEY") && e.contains("nothing was terminated"), "{e}");
        assert_eq!(fleet.0.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
