//! The pure lifecycle planners: what `archive up` / `down` / `resize` would do to the project,
//! from one inventory — table-tested, so the executor in the CLI only carries them out.
//!
//! The rule for anything ambiguous is to **refuse and say exactly what's there** — two servers,
//! two volumes of one tier, a volume attached elsewhere or in another location, a volume not
//! formatted ext4 by Hetzner, an unlabelled lookalike, a shrink. The planners never
//! force-detach, never format, never adopt something they didn't label, and never plan a
//! volume deletion unless `--delete-volumes` came with the typed `--confirm <prefix>-archive`.

use std::collections::BTreeSet;

use super::cloud::{CloudServer, CloudVolume, Inventory};
use super::{key_blob, ArchiveNames, ArchiveSettings, Tier, LABEL_KEY, MAX_GB, MIN_GB};

/// One step of `archive up`, in execution order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpStep {
    /// Upload the admin pubkey as the labelled SSH-key object (only to create the server).
    CreateSshKey,
    /// Create the server there (trying `ARCHIVE_SERVER_TYPES` in order), then wait for it.
    CreateServer { location: String },
    /// Turn on delete + rebuild protection (a new server, or an existing unprotected one).
    ProtectServer,
    /// Create the volume attached to the server (Hetzner formats it ext4), wait, protect it.
    CreateVolume { tier: Tier, size_gb: u32 },
    /// Attach an existing detached volume (after a server loss), wait.
    AttachVolume { tier: Tier, id: u64 },
    /// Turn on an existing volume's delete protection.
    ProtectVolume { tier: Tier, id: u64 },
    /// Power on the server, which is off (a shutdown inside it, a console power-off), and wait
    /// for the action — after its volumes are attached, so it boots with them.
    PowerOn { id: u64 },
    /// Wait for SSH and pin the host key; `fresh` = this run creates the server, so the first
    /// connection may trust on first use into an emptied known_hosts.
    PinHostKey { fresh: bool },
    /// Run `setup.sh` (idempotent) as root.
    RunSetup,
    /// Write `archive.json`.
    WriteState,
}

impl UpStep {
    /// The plan line for it.
    pub fn describe(&self, names: &ArchiveNames) -> String {
        match self {
            UpStep::CreateSshKey => format!("create the SSH-key object {} (the archive's own admin key)", names.ssh_key()),
            UpStep::CreateServer { location } => format!("create the server {} in {location}", names.server()),
            UpStep::ProtectServer => format!("turn on delete + rebuild protection for {}", names.server()),
            UpStep::CreateVolume { tier, size_gb } => {
                format!("create the volume {} ({size_gb} GB, ext4 by Hetzner), attached, then protect it", names.volume(*tier))
            }
            UpStep::AttachVolume { tier, id } => format!("attach the existing volume {} (id {id}) — never formatted", names.volume(*tier)),
            UpStep::ProtectVolume { tier, id } => format!("turn on delete protection for {} (id {id})", names.volume(*tier)),
            UpStep::PowerOn { id } => format!("power on {} (id {id}) — it is off", names.server()),
            UpStep::PinHostKey { fresh: true } => "wait for SSH, then pin the new server's host key".into(),
            UpStep::PinHostKey { fresh: false } => "wait for SSH on the pinned host key".into(),
            UpStep::RunSetup => "run the setup script as root (users, mounts, read-only binds, sshd; idempotent)".into(),
            UpStep::WriteState => "write archive.json".into(),
        }
    }

    /// Whether it creates (and so bills) something.
    pub fn creates(&self) -> bool {
        matches!(self, UpStep::CreateSshKey | UpStep::CreateServer { .. } | UpStep::CreateVolume { .. })
    }
}

/// What `up` will do, or why it won't do anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpPlan {
    Steps(Vec<UpStep>),
    Refuse(String),
}

/// `up`'s explicit `--snap-gb` / `--big-gb` (config sizes are creation sizes only, never
/// compared with an existing volume — else every `up` after a `resize` would refuse).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SizeFlags {
    pub snap_gb: Option<u32>,
    pub big_gb: Option<u32>,
}

impl SizeFlags {
    pub fn of(&self, tier: Tier) -> Option<u32> {
        match tier {
            Tier::Snap => self.snap_gb,
            Tier::Big => self.big_gb,
        }
    }
}

/// Plan `archive up` (spec §6.1 step 4). `inv` is `inventory(Some(prefix))` — this cohort's
/// labelled resources plus same-named unlabelled ones; `admin_pub` the local admin pubkey (a
/// labelled key object holding another key can't create a server we could reach). Returns the
/// plan and notes to print (a resize hint, a server that isn't running).
pub fn plan_up(
    inv: &Inventory,
    names: &ArchiveNames,
    settings: &ArchiveSettings,
    machine_names: &[String],
    flags: SizeFlags,
    admin_pub: Option<&str>,
) -> (UpPlan, Vec<String>) {
    let refuse = |why: String| (UpPlan::Refuse(why), Vec::new());
    let mut notes = Vec::new();

    // A pod could never share the server's name.
    if let Some(e) = machine_names.iter().find(|e| crate::naming::qualify(&names.prefix, e) == names.server() || e.trim() == "archive") {
        return refuse(format!(
            "MACHINE_NAME_LIST has `{e}`, which names a pod {} — the archive's own name; take it out of the list first",
            names.server()
        ));
    }
    for tier in Tier::ALL {
        if let Some(n) = flags.of(tier).filter(|n| !(MIN_GB..=MAX_GB).contains(n)) {
            return refuse(format!("{} {n}: a volume is {MIN_GB}..={MAX_GB} GB", tier.gb_flag()));
        }
    }

    // Lookalikes: named like ours, not labelled ours. Never adopted, never collided with.
    let foreign = |labels: &super::Labels| !names.owns(labels);
    if let Some(s) = inv.servers.iter().find(|s| s.name == names.server() && foreign(&s.labels)) {
        return refuse(format!(
            "a server named {} (id {}) exists without the label {LABEL_KEY}={} — arena won't adopt or replace it; rename or \
             delete it in the Hetzner console first",
            s.name, s.id, names.prefix
        ));
    }
    if let Some(v) = inv.volumes.iter().find(|v| (v.name == names.volume(Tier::Snap) || v.name == names.volume(Tier::Big)) && foreign(&v.labels)) {
        return refuse(format!(
            "a volume named {} (id {}) exists without the label {LABEL_KEY}={} — arena won't adopt it (or format a new one \
             over its name); rename or delete it in the Hetzner console first",
            v.name, v.id, names.prefix
        ));
    }
    if let Some(k) = inv.ssh_keys.iter().find(|k| k.name == names.ssh_key() && foreign(&k.labels)) {
        return refuse(format!(
            "an SSH key named {} (id {}) exists without the label {LABEL_KEY}={} — delete or rename it in the Hetzner console first",
            k.name, k.id, names.prefix
        ));
    }

    let ours = inv.owned_by(names);
    let server: Option<&CloudServer> = match ours.servers.as_slice() {
        [] => None,
        [one] => Some(one),
        many => {
            let ids: Vec<String> = many.iter().map(|s| format!("{} (id {})", s.name, s.id)).collect();
            return refuse(format!("{} servers carry {LABEL_KEY}={}: {} — keep one (delete the others in the console)", many.len(), names.prefix, ids.join(", ")));
        }
    };
    if ours.ssh_keys.len() > 1 {
        return refuse(format!("{} SSH-key objects carry {LABEL_KEY}={} — delete all but one in the console", ours.ssh_keys.len(), names.prefix));
    }
    let mut volumes: [Option<&CloudVolume>; 2] = [None, None];
    for v in &ours.volumes {
        let Some(tier) = v.tier() else {
            return refuse(format!("the volume {} (id {}) carries {LABEL_KEY}={} but no valid tier label — fix its labels in the console", v.name, v.id, names.prefix));
        };
        let slot = &mut volumes[tier as usize];
        if let Some(other) = slot {
            return refuse(format!(
                "two {tier} volumes carry {LABEL_KEY}={}: {} (id {}) and {} (id {}) — keep one",
                names.prefix, other.name, other.id, v.name, v.id
            ));
        }
        *slot = Some(v);
    }
    for v in volumes.iter().flatten() {
        if v.format.as_deref() != Some("ext4") {
            return refuse(format!(
                "the volume {} (id {}) is {} — the archive only mounts volumes Hetzner formatted ext4, and arena never formats one",
                v.name,
                v.id,
                v.format.as_deref().map_or("unformatted".to_string(), |f| format!("formatted {f}"))
            ));
        }
        match (v.server, server) {
            (Some(at), Some(s)) if at != s.id => {
                return refuse(format!("the volume {} is attached to server {at}, not to {} (id {}) — arena never force-detaches", v.name, s.name, s.id))
            }
            (Some(at), None) => {
                return refuse(format!("the volume {} is attached to server {at}, which isn't the archive — arena never force-detaches", v.name))
            }
            _ => {}
        }
        if let Some(s) = server {
            if v.location != s.location {
                return refuse(format!("the volume {} is in {}, the server {} in {} — a volume only attaches in its own location", v.name, v.location, s.name, s.location));
            }
        }
    }
    let vol_locations: BTreeSet<&str> = volumes.iter().flatten().map(|v| v.location.as_str()).collect();
    if server.is_none() && vol_locations.len() > 1 {
        return refuse(format!("the archive's volumes are in different locations ({}) — one server can't attach both", vol_locations.into_iter().collect::<Vec<_>>().join(", ")));
    }

    // Sizes: an explicit flag smaller than the volume is a shrink (refused); larger, a hint.
    for tier in Tier::ALL {
        if let (Some(want), Some(v)) = (flags.of(tier), volumes[tier as usize]) {
            if want < v.size_gb {
                return refuse(format!("{} {want}: {} is already {} GB and a volume can't shrink", tier.gb_flag(), v.name, v.size_gb));
            }
            if want > v.size_gb {
                notes.push(format!(
                    "{} already exists at {} GB ({} applies at creation) — to grow it: arena archive resize {} {want}",
                    v.name,
                    v.size_gb,
                    tier.gb_flag(),
                    tier.gb_flag()
                ));
            }
        }
    }

    let mut steps = Vec::new();
    let fresh = server.is_none();
    let mut power_on = None;
    match server {
        None => {
            let key = ours.ssh_keys.first();
            match (key, admin_pub) {
                (None, _) => steps.push(UpStep::CreateSshKey),
                (Some(k), Some(local)) if key_blob(&k.public_key) != key_blob(local) => {
                    return refuse(format!(
                        "the SSH-key object {} (id {}) holds another public key than this state dir's admin_ed25519.pub — a server \
                         created with it would lock arena out. Delete that key object in the console (or restore the state dir it \
                         came from), then re-run",
                        k.name, k.id
                    ));
                }
                _ => {}
            }
            let location = vol_locations.iter().next().map(|l| l.to_string()).unwrap_or_else(|| settings.location.clone());
            if !vol_locations.is_empty() {
                notes.push(format!("no server, but the volumes survive: creating it in their location ({location}); they are attached, never formatted"));
            }
            steps.push(UpStep::CreateServer { location });
            steps.push(UpStep::ProtectServer);
        }
        Some(s) => {
            if !s.delete_protected {
                steps.push(UpStep::ProtectServer);
            }
            match s.status.as_str() {
                "running" => {}
                // Nothing else starts it (fleet isolation hides it from `pods start`): `up` does.
                "off" => power_on = Some(s.id),
                "stopping" => notes.push(format!(
                    "{} is stopping — once it is off, `arena archive up` powers it on again (SSH waits for it meanwhile)",
                    s.name
                )),
                other => notes.push(format!("{} is {other} — SSH waits for it to come up", s.name)),
            }
        }
    }
    for tier in Tier::ALL {
        match volumes[tier as usize] {
            None => steps.push(UpStep::CreateVolume { tier, size_gb: flags.of(tier).unwrap_or_else(|| settings.size_of(tier)) }),
            Some(v) => {
                if v.server.is_none() {
                    steps.push(UpStep::AttachVolume { tier, id: v.id });
                }
                if !v.delete_protected {
                    steps.push(UpStep::ProtectVolume { tier, id: v.id });
                }
            }
        }
    }
    steps.extend(power_on.map(|id| UpStep::PowerOn { id }));
    steps.extend([UpStep::PinHostKey { fresh }, UpStep::RunSetup, UpStep::WriteState]);
    (UpPlan::Steps(steps), notes)
}

/// One step of `archive down`, in execution order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownStep {
    /// Best effort on the VM: end the readers' and pushers' sessions, `sync`, unmount (60 s).
    Quiesce,
    UnprotectServer,
    DetachVolume(Tier, u64),
    DeleteServer(u64),
    UnprotectVolume(Tier, u64),
    DeleteVolume(Tier, u64),
    DeleteSshKey(u64),
    /// Remove `archive.json` and the sync state (the keys stay).
    RemoveLocalState,
}

impl DownStep {
    pub fn describe(&self, names: &ArchiveNames) -> String {
        match self {
            DownStep::Quiesce => "end sftp/push sessions, sync and unmount the volumes on the server (best effort, 60 s)".into(),
            DownStep::UnprotectServer => format!("lift {}'s protection", names.server()),
            DownStep::DetachVolume(t, id) => format!("detach {} (id {id}) — its data stays", names.volume(*t)),
            DownStep::DeleteServer(id) => format!("delete the server {} (id {id})", names.server()),
            DownStep::UnprotectVolume(t, id) => format!("lift {}'s protection (id {id})", names.volume(*t)),
            DownStep::DeleteVolume(t, id) => format!("DELETE the volume {} (id {id}) and every backup on it", names.volume(*t)),
            DownStep::DeleteSshKey(id) => format!("delete the SSH-key object {} (id {id})", names.ssh_key()),
            DownStep::RemoveLocalState => "remove archive.json and the sync state (the keys stay)".into(),
        }
    }
}

/// Plan `archive down` (§6.2). Without `delete_volumes` only the server goes (its volumes are
/// detached and stay protected). `DeleteVolume` is planned **only** with `delete_volumes` and
/// `confirm == "<prefix>-archive"` exactly (S7) — `-y` is never enough, because there is no
/// copy #3.
pub fn plan_down(inv: &Inventory, names: &ArchiveNames, delete_volumes: bool, confirm: Option<&str>) -> Result<Vec<DownStep>, String> {
    if confirm.is_some() && !delete_volumes {
        return Err("--confirm only goes with --delete-volumes".into());
    }
    if delete_volumes && confirm != Some(names.server().as_str()) {
        return Err(format!(
            "--delete-volumes destroys every backup on the archive's volumes and there is no other copy on Hetzner — pass \
             --confirm {} exactly (-y is not enough)",
            names.server()
        ));
    }
    let ours = inv.owned_by(names);
    if ours.servers.len() > 1 {
        return Err(format!("{} servers carry {LABEL_KEY}={} — sort that out in the console first", ours.servers.len(), names.prefix));
    }
    let server = ours.servers.first();
    let mut steps = Vec::new();
    if let Some(s) = server {
        steps.push(DownStep::Quiesce);
        if s.delete_protected {
            steps.push(DownStep::UnprotectServer);
        }
        for v in ours.volumes.iter().filter(|v| v.server == Some(s.id)) {
            let tier = v.tier().ok_or_else(|| format!("the volume {} has no valid tier label", v.name))?;
            steps.push(DownStep::DetachVolume(tier, v.id));
        }
        steps.push(DownStep::DeleteServer(s.id));
    }
    if delete_volumes {
        for v in &ours.volumes {
            let tier = v.tier().ok_or_else(|| format!("the volume {} has no valid tier label", v.name))?;
            if let Some(at) = v.server.filter(|at| Some(*at) != server.map(|s| s.id)) {
                return Err(format!("the volume {} is attached to server {at}, which isn't the archive — arena won't detach it", v.name));
            }
            if v.delete_protected {
                steps.push(DownStep::UnprotectVolume(tier, v.id));
            }
            steps.push(DownStep::DeleteVolume(tier, v.id));
        }
        for k in &ours.ssh_keys {
            steps.push(DownStep::DeleteSshKey(k.id));
        }
        steps.push(DownStep::RemoveLocalState);
    }
    Ok(steps)
}

/// What `archive resize` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizePlan {
    pub tier: Tier,
    pub volume_id: u64,
    pub from_gb: u32,
    pub to_gb: u32,
    /// The device `resize2fs` grows online.
    pub linux_device: String,
}

impl ResizePlan {
    /// No API resize: the volume already has the size, only the filesystem is grown to it.
    pub fn filesystem_only(&self) -> bool {
        self.from_gb == self.to_gb
    }
}

/// Plan `archive resize` — grow only (a volume can't shrink), and only a volume attached to the
/// archive's server (the filesystem is grown there, online). The volume's current size is allowed
/// too: it then only grows the filesystem — how a resize whose `resize2fs` step failed after the
/// API resize went through is finished (`ResizePlan::filesystem_only`).
pub fn plan_resize(inv: &Inventory, names: &ArchiveNames, tier: Tier, to_gb: u32) -> Result<ResizePlan, String> {
    if !(MIN_GB..=MAX_GB).contains(&to_gb) {
        return Err(format!("{} {to_gb}: a volume is {MIN_GB}..={MAX_GB} GB", tier.gb_flag()));
    }
    let server = inv.server(names).ok_or_else(|| format!("no archive server ({}) — run `arena archive up` first", names.server()))?;
    let v = inv.volume(names, tier).ok_or_else(|| format!("no {} volume ({}) — run `arena archive up` first", tier, names.volume(tier)))?;
    if v.server != Some(server.id) {
        return Err(format!("{} isn't attached to {} — run `arena archive up` first (it re-attaches it)", v.name, server.name));
    }
    if to_gb < v.size_gb {
        return Err(format!("{} is {} GB: a volume can only grow (asked for {to_gb} GB)", v.name, v.size_gb));
    }
    Ok(ResizePlan { tier, volume_id: v.id, from_gb: v.size_gb, to_gb, linux_device: v.linux_device.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::cloud::{CloudSshKey, Labels};
    use crate::config::Config;

    fn names() -> ArchiveNames {
        ArchiveNames::new("arena9").unwrap()
    }
    fn settings() -> ArchiveSettings {
        ArchiveSettings::from_config(&Config::parse("")).unwrap()
    }
    fn list() -> Vec<String> {
        ["apple", "bloom", "@james-gpu"].iter().map(|s| s.to_string()).collect()
    }
    const ADMIN: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAdminAdminAdminAdminAdminAdminAdminAdmin1 admin";
    fn server(id: u64, protected: bool) -> CloudServer {
        CloudServer {
            id,
            name: "arena9-archive".into(),
            status: "running".into(),
            location: "nbg1".into(),
            ipv4: Some("192.0.2.1".into()),
            labels: names().labels(),
            delete_protected: protected,
            server_type: "cx23".into(),
        }
    }
    fn vol(id: u64, tier: Tier, at: Option<u64>, protected: bool) -> CloudVolume {
        CloudVolume {
            id,
            name: names().volume(tier),
            size_gb: if tier == Tier::Snap { 50 } else { 200 },
            location: "nbg1".into(),
            server: at,
            format: Some("ext4".into()),
            linux_device: format!("/dev/disk/by-id/scsi-0HC_Volume_{id}"),
            labels: names().volume_labels(tier),
            delete_protected: protected,
        }
    }
    fn key(id: u64, public: &str) -> CloudSshKey {
        CloudSshKey { id, name: "arena9-archive-admin".into(), public_key: public.into(), labels: names().labels() }
    }
    fn all_present() -> Inventory {
        Inventory { servers: vec![server(1, true)], volumes: vec![vol(2, Tier::Snap, Some(1), true), vol(3, Tier::Big, Some(1), true)], ssh_keys: vec![key(4, ADMIN)] }
    }
    fn up(inv: &Inventory) -> (UpPlan, Vec<String>) {
        plan_up(inv, &names(), &settings(), &list(), SizeFlags::default(), Some(ADMIN))
    }
    fn steps(p: (UpPlan, Vec<String>)) -> Vec<UpStep> {
        match p.0 {
            UpPlan::Steps(s) => s,
            UpPlan::Refuse(why) => panic!("refused: {why}"),
        }
    }
    fn refusal(p: (UpPlan, Vec<String>)) -> String {
        match p.0 {
            UpPlan::Refuse(why) => why,
            UpPlan::Steps(s) => panic!("planned {s:?}"),
        }
    }
    const TAIL: [UpStep; 3] = [UpStep::PinHostKey { fresh: false }, UpStep::RunSetup, UpStep::WriteState];

    #[test]
    fn up_from_scratch_creates_key_then_server_then_volumes() {
        let s = steps(up(&Inventory::default()));
        assert_eq!(
            s,
            [
                UpStep::CreateSshKey,
                UpStep::CreateServer { location: "nbg1".into() },
                UpStep::ProtectServer,
                UpStep::CreateVolume { tier: Tier::Snap, size_gb: 50 },
                UpStep::CreateVolume { tier: Tier::Big, size_gb: 200 },
                UpStep::PinHostKey { fresh: true },
                UpStep::RunSetup,
                UpStep::WriteState,
            ]
        );
        // Explicit sizes apply at creation.
        let flags = SizeFlags { snap_gb: Some(10), big_gb: Some(20) };
        let s = steps(plan_up(&Inventory::default(), &names(), &settings(), &list(), flags, Some(ADMIN)));
        assert!(s.contains(&UpStep::CreateVolume { tier: Tier::Snap, size_gb: 10 }) && s.contains(&UpStep::CreateVolume { tier: Tier::Big, size_gb: 20 }));
    }

    #[test]
    fn up_converges_from_every_partial_state() {
        // All present and protected: only pin, setup, state.
        assert_eq!(steps(up(&all_present())), TAIL);
        // Server only (unprotected): protect it, create both volumes.
        let inv = Inventory { servers: vec![server(1, false)], ..Default::default() };
        let s = steps(up(&inv));
        assert_eq!(s[..3], [UpStep::ProtectServer, UpStep::CreateVolume { tier: Tier::Snap, size_gb: 50 }, UpStep::CreateVolume { tier: Tier::Big, size_gb: 200 }]);
        assert_eq!(s[3], UpStep::PinHostKey { fresh: false });
        // One tier missing.
        let mut inv = all_present();
        inv.volumes.retain(|v| v.tier() == Some(Tier::Snap));
        assert_eq!(steps(up(&inv)), [vec![UpStep::CreateVolume { tier: Tier::Big, size_gb: 200 }], TAIL.to_vec()].concat());
        // A detached volume is attached (never formatted); an unprotected one protected.
        let mut inv = all_present();
        inv.volumes[0] = vol(2, Tier::Snap, None, false);
        assert_eq!(
            steps(up(&inv)),
            [vec![UpStep::AttachVolume { tier: Tier::Snap, id: 2 }, UpStep::ProtectVolume { tier: Tier::Snap, id: 2 }], TAIL.to_vec()].concat()
        );
    }

    /// W7 live (A-ckl7): a server that is off is powered on — after its volumes are attached,
    /// before SSH is waited for — never left for SSH to time out on. Other states only wait.
    #[test]
    fn up_powers_on_a_server_that_is_off() {
        let mut inv = all_present();
        inv.servers[0].status = "off".into();
        let (plan, notes) = up(&inv);
        assert_eq!(steps((plan, notes.clone())), [vec![UpStep::PowerOn { id: 1 }], TAIL.to_vec()].concat());
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(UpStep::PowerOn { id: 1 }.describe(&names()), "power on arena9-archive (id 1) — it is off");
        // Detached as well: attached first, then powered on.
        inv.volumes[0] = vol(2, Tier::Snap, None, true);
        let s = steps(up(&inv));
        assert_eq!(s[..2], [UpStep::AttachVolume { tier: Tier::Snap, id: 2 }, UpStep::PowerOn { id: 1 }]);
        for (status, says) in [("starting", "is starting — SSH waits"), ("stopping", "once it is off, `arena archive up` powers it on")] {
            let mut inv = all_present();
            inv.servers[0].status = status.into();
            let (plan, notes) = up(&inv);
            assert_eq!(steps((plan, Vec::new())), TAIL, "{status}");
            assert!(notes.iter().any(|n| n.contains(says)), "{status}: {notes:?}");
        }
    }

    /// Server lost, volumes kept: a new server in the volumes' location, both attached, a fresh pin.
    #[test]
    fn up_after_a_server_loss_recreates_it_where_the_volumes_are() {
        let mut inv = all_present();
        inv.servers.clear();
        for v in &mut inv.volumes {
            v.server = None;
            v.location = "hel1".into();
        }
        let (plan, notes) = up(&inv);
        let UpPlan::Steps(s) = plan else { panic!() };
        assert_eq!(
            s,
            [
                UpStep::CreateServer { location: "hel1".into() },
                UpStep::ProtectServer,
                UpStep::AttachVolume { tier: Tier::Snap, id: 2 },
                UpStep::AttachVolume { tier: Tier::Big, id: 3 },
                UpStep::PinHostKey { fresh: true },
                UpStep::RunSetup,
                UpStep::WriteState,
            ]
        );
        assert!(notes.iter().any(|n| n.contains("never formatted")), "{notes:?}");
        assert!(!s.iter().any(|x| matches!(x, UpStep::CreateVolume { .. })), "never a second volume");
    }

    #[test]
    fn up_refuses_every_ambiguous_inventory() {
        let base = all_present();
        type Case = (&'static str, Box<dyn Fn(&mut Inventory)>, &'static str);
        let cases: Vec<Case> = vec![
            ("two servers", Box::new(|i| i.servers.push(CloudServer { id: 9, ..server(9, true) })), "2 servers"),
            ("duplicate tier", Box::new(|i| i.volumes.push(vol(9, Tier::Snap, None, true))), "two snap volumes"),
            ("attached elsewhere", Box::new(|i| i.volumes[1].server = Some(77)), "attached to server 77"),
            ("location mismatch", Box::new(|i| i.volumes[0].location = "fsn1".into()), "in fsn1"),
            ("xfs", Box::new(|i| i.volumes[1].format = Some("xfs".into())), "formatted xfs"),
            ("unformatted", Box::new(|i| i.volumes[1].format = None), "unformatted"),
            ("no tier label", Box::new(|i| { i.volumes[1].labels.remove(crate::archive::TIER_LABEL_KEY); }), "no valid tier"),
            (
                "unlabelled server lookalike",
                Box::new(|i| {
                    i.servers.clear();
                    i.servers.push(CloudServer { labels: Labels::new(), ..server(5, false) });
                }),
                "without the label",
            ),
            (
                "another cohort's label on our name",
                Box::new(|i| {
                    i.servers[0].labels.insert(LABEL_KEY.into(), "arena8".into());
                }),
                "without the label arena-archive=arena9",
            ),
            ("unlabelled volume lookalike", Box::new(|i| i.volumes.push(CloudVolume { labels: Labels::new(), ..vol(9, Tier::Big, None, false) })), "a volume named"),
            ("unlabelled key lookalike", Box::new(|i| i.ssh_keys.push(CloudSshKey { labels: Labels::new(), ..key(9, ADMIN) })), "an SSH key named"),
            ("two key objects", Box::new(|i| i.ssh_keys.push(key(9, ADMIN))), "SSH-key objects"),
        ];
        for (case, edit, needle) in cases {
            let mut inv = base.clone();
            edit(&mut inv);
            let why = refusal(up(&inv));
            assert!(why.contains(needle), "{case}: {why}");
        }
        // Volumes without a server, split across locations.
        let mut inv = base.clone();
        inv.servers.clear();
        inv.volumes[0].server = None;
        inv.volumes[1].server = None;
        inv.volumes[1].location = "fsn1".into();
        assert!(refusal(up(&inv)).contains("different locations"));
        // A volume attached to some server while ours is gone.
        let mut inv = base.clone();
        inv.servers.clear();
        inv.volumes[1].server = None;
        assert!(refusal(up(&inv)).contains("isn't the archive"));
        // The key object can't create a server we could reach.
        let mut inv = Inventory::default();
        inv.ssh_keys.push(key(4, "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtherOtherOtherOtherOtherOtherOtherOther9 x"));
        assert!(refusal(up(&inv)).contains("another public key"));
    }

    #[test]
    fn up_refuses_a_machine_list_that_would_name_a_pod_like_the_archive() {
        for entry in ["archive", "@arena9-archive"] {
            let mut l = list();
            l.push(entry.into());
            let why = refusal(plan_up(&Inventory::default(), &names(), &settings(), &l, SizeFlags::default(), Some(ADMIN)));
            assert!(why.contains("MACHINE_NAME_LIST"), "{entry}: {why}");
        }
    }

    #[test]
    fn up_sizes_grow_is_a_hint_shrink_is_refused() {
        let flags = SizeFlags { snap_gb: None, big_gb: Some(300) };
        let (plan, notes) = plan_up(&all_present(), &names(), &settings(), &list(), flags, Some(ADMIN));
        assert_eq!(plan, UpPlan::Steps(TAIL.to_vec()));
        assert!(notes.iter().any(|n| n.contains("arena archive resize --big-gb 300")), "{notes:?}");
        let flags = SizeFlags { snap_gb: Some(20), big_gb: None };
        let why = refusal(plan_up(&all_present(), &names(), &settings(), &list(), flags, Some(ADMIN)));
        assert!(why.contains("can't shrink"), "{why}");
        // Config sizes never compare with an existing volume (a resized one stays fine).
        let small = ArchiveSettings::from_config(&Config::parse("ARCHIVE_SNAP_GB=10\nARCHIVE_BIG_GB=10\n")).unwrap();
        assert_eq!(plan_up(&all_present(), &names(), &small, &list(), SizeFlags::default(), Some(ADMIN)).0, UpPlan::Steps(TAIL.to_vec()));
        let flags = SizeFlags { snap_gb: Some(5), big_gb: None };
        assert!(refusal(plan_up(&Inventory::default(), &names(), &settings(), &list(), flags, Some(ADMIN))).contains("10..=10240"));
    }

    /// S7: a volume deletion is planned only with --delete-volumes AND the exact typed name.
    #[test]
    fn down_deletes_volumes_only_with_the_typed_confirmation() {
        let inv = all_present();
        let server_only = plan_down(&inv, &names(), false, None).unwrap();
        assert_eq!(
            server_only,
            [DownStep::Quiesce, DownStep::UnprotectServer, DownStep::DetachVolume(Tier::Snap, 2), DownStep::DetachVolume(Tier::Big, 3), DownStep::DeleteServer(1)]
        );
        for confirm in [None, Some("arena9"), Some("arena9-archive "), Some("ARENA9-ARCHIVE"), Some("arena8-archive"), Some("y")] {
            let e = plan_down(&inv, &names(), true, confirm).unwrap_err();
            assert!(e.contains("--confirm arena9-archive"), "{confirm:?}: {e}");
        }
        assert!(plan_down(&inv, &names(), false, Some("arena9-archive")).is_err(), "--confirm without --delete-volumes");
        let all = plan_down(&inv, &names(), true, Some("arena9-archive")).unwrap();
        assert_eq!(
            all,
            [
                DownStep::Quiesce,
                DownStep::UnprotectServer,
                DownStep::DetachVolume(Tier::Snap, 2),
                DownStep::DetachVolume(Tier::Big, 3),
                DownStep::DeleteServer(1),
                DownStep::UnprotectVolume(Tier::Snap, 2),
                DownStep::DeleteVolume(Tier::Snap, 2),
                DownStep::UnprotectVolume(Tier::Big, 3),
                DownStep::DeleteVolume(Tier::Big, 3),
                DownStep::DeleteSshKey(4),
                DownStep::RemoveLocalState,
            ]
        );
        // Orphan volumes (server already gone): just the volume steps.
        let mut orphans = inv.clone();
        orphans.servers.clear();
        orphans.volumes.iter_mut().for_each(|v| v.server = None);
        assert_eq!(plan_down(&orphans, &names(), false, None).unwrap(), []);
        let steps = plan_down(&orphans, &names(), true, Some("arena9-archive")).unwrap();
        assert_eq!(steps[0], DownStep::UnprotectVolume(Tier::Snap, 2));
        // Another cohort's archive is never touched.
        let other = Inventory { servers: vec![CloudServer { labels: ArchiveNames::new("arena8").unwrap().labels(), ..server(1, true) }], ..Default::default() };
        assert_eq!(plan_down(&other, &names(), true, Some("arena9-archive")).unwrap(), [DownStep::RemoveLocalState]);
    }

    #[test]
    fn resize_only_grows_an_attached_volume() {
        let inv = all_present();
        assert_eq!(
            plan_resize(&inv, &names(), Tier::Big, 300).unwrap(),
            ResizePlan { tier: Tier::Big, volume_id: 3, from_gb: 200, to_gb: 300, linux_device: "/dev/disk/by-id/scsi-0HC_Volume_3".into() }
        );
        // The current size: the filesystem only (AD6 — finishing a resize whose resize2fs failed).
        let fs = plan_resize(&inv, &names(), Tier::Big, 200).unwrap();
        assert!(fs.filesystem_only() && !plan_resize(&inv, &names(), Tier::Big, 300).unwrap().filesystem_only());
        assert!(plan_resize(&inv, &names(), Tier::Snap, 40).unwrap_err().contains("only grow"));
        assert!(plan_resize(&inv, &names(), Tier::Snap, 99999).unwrap_err().contains("10..=10240"));
        let mut detached = inv.clone();
        detached.volumes[1].server = None;
        assert!(plan_resize(&detached, &names(), Tier::Big, 300).unwrap_err().contains("isn't attached"));
        assert!(plan_resize(&Inventory::default(), &names(), Tier::Big, 300).unwrap_err().contains("archive up"));
    }
}
