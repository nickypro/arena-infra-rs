//! Extra disk for a Hetzner VM: a Hetzner volume attached to it and mounted (`arena pods volume
//! add|grow|list|detach|delete`), counted wherever money is, and carried along when the VM is
//! replaced.
//!
//! What Hetzner says (checked 2026-10-31): a volume is 10 GB to 10 TB, lives in one location and
//! attaches to one server at a time in that location only; it attaches to a running server
//! without a reboot — though a server that has run a long time may need one full power cycle
//! before it sees its first volume; it grows online (never shrinks), and the filesystem then
//! grows with `resize2fs`/`xfs_growfs`; it is NOT deleted with its server (it goes on billing,
//! detached, until deleted).
//!
//! How arena does it: created by Hetzner already formatted (ext4) and attached, never
//! automounted — mounted over SSH at a path of the operator's (`/mnt/data` by default) through
//! an fstab line by its stable device path with `nofail` (a missing volume never stops a boot).
//! Labelled `arena-volume=<prefix>` and named `<pod>-data` (`-data-2`, …: Hetzner's names are
//! unique per project). The backup archive's volumes are its own (`arena archive`), never these.
//!
//! Pure here (table-tested): names, labels, the checks, the on-VM scripts and their replies,
//! the table, the cost. The API calls are [`VolumeCloud`]'s — the archive's Hetzner client
//! ([`crate::archive::cloud::HetznerArchiveCloud`], under the fleet's token) and its fake.

use async_trait::async_trait;

use crate::archive::cloud::{ArchiveCloud, CloudVolume, Labels};
use crate::error::Result;
use crate::table::{self, Align};

/// The label on a fleet VM's volume (value: the cohort prefix, as a label value).
pub const LABEL: &str = "arena-volume";

/// Hetzner's smallest and largest volume, in GB.
pub const MIN_GB: u32 = 10;
pub const MAX_GB: u32 = 10_240;

/// Where `pods volume add` mounts one by default.
pub const DEFAULT_MOUNT: &str = "/mnt/data";

/// How long an attach, detach, resize or create may take at Hetzner before arena gives up
/// waiting (it reports where it stopped; the action goes on).
pub const ACTION_WAIT: std::time::Duration = std::time::Duration::from_secs(180);

/// Hours in a month, as Hetzner bills a monthly price by the hour.
const HOURS_PER_MONTH: f64 = 730.0;

/// The Hetzner calls for a fleet VM's volumes: the archive's client (every call it has) plus
/// the project's whole volume list.
#[async_trait]
pub trait VolumeCloud: ArchiveCloud {
    /// Every volume in the project (every page; fails closed, as the archive's listing does).
    async fn all_volumes(&self) -> Result<Vec<CloudVolume>>;
}

/// The label value for `prefix`.
pub fn label_value(prefix: &str) -> String {
    crate::image::label_value(prefix)
}

/// The labels a fleet VM's volume is created with.
pub fn labels(prefix: &str) -> Labels {
    let mut l = Labels::new();
    l.insert(LABEL.into(), label_value(prefix));
    l
}

/// Whether `v` is the backup archive's (its own label): never a fleet VM's, never touched here.
pub fn is_archive(v: &CloudVolume) -> bool {
    v.labels.contains_key(crate::archive::LABEL_KEY)
}

/// Whether `v` was made by `pods volume add` for `prefix`'s fleet.
pub fn is_ours(v: &CloudVolume, prefix: &str) -> bool {
    v.labels.get(LABEL).is_some_and(|p| *p == label_value(prefix))
}

/// The volumes a fleet's commands see: every volume in the project but the archive's. A volume
/// made by hand and attached to a fleet VM is the VM's like any (it bills the same). Pure.
pub fn fleet_volumes(all: &[CloudVolume]) -> Vec<CloudVolume> {
    all.iter().filter(|v| !is_archive(v)).cloned().collect()
}

/// The volumes this cohort may act on (grow, detach, delete, count): the ones `pods volume add`
/// made for it (its label), and any attached to one of its VMs (`fleet_ids`, the Hetzner
/// servers its listing shows — a volume attached by hand bills it the same). Never the
/// archive's, never another cohort's or an unlabelled one lying detached in a shared project
/// (review: its teardown would have printed their deletes). Pure.
pub fn cohort_volumes(all: &[CloudVolume], prefix: &str, fleet_ids: &[String]) -> Vec<CloudVolume> {
    all.iter()
        .filter(|v| !is_archive(v))
        .filter(|v| is_ours(v, prefix) || v.server.is_some_and(|s| fleet_ids.contains(&s.to_string())))
        .cloned()
        .collect()
}

/// The volumes attached to server `id`. Pure.
pub fn attached_to<'a>(all: &'a [CloudVolume], server_id: &str) -> Vec<&'a CloudVolume> {
    all.iter().filter(|v| v.server.is_some_and(|s| s.to_string() == server_id)).collect()
}

/// A size the API takes (10 GB to 10 TB). Pure.
pub fn check_size(gb: u32) -> std::result::Result<(), String> {
    if (MIN_GB..=MAX_GB).contains(&gb) {
        Ok(())
    } else {
        Err(format!("a Hetzner volume is {MIN_GB} GB to {MAX_GB} GB (10 TB), not {gb} GB"))
    }
}

/// A mount path arena will put in fstab: absolute, plain characters, not a system directory and
/// not inside a home — `pods pull`, backups and moves copy the home, so a volume there would be
/// copied whole (link to it from the home instead). Pure.
pub fn check_mount(path: &str) -> std::result::Result<(), String> {
    let p = path.trim_end_matches('/');
    if !p.starts_with('/') || p.len() < 2 {
        return Err(format!("--mount takes an absolute path like {DEFAULT_MOUNT}, not `{path}`"));
    }
    if !p.chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)) || p.split('/').any(|c| c == "." || c == "..") || p.contains("//") {
        return Err(format!("--mount takes a plain path (letters, digits, `/._-`), not `{path}`"));
    }
    const SYSTEM: [&str; 14] = ["/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/proc", "/run", "/sbin", "/sys", "/tmp", "/usr", "/var"];
    let top = format!("/{}", p.trim_start_matches('/').split('/').next().unwrap_or_default());
    if SYSTEM.contains(&top.as_str()) {
        return Err(format!("--mount {path}: {top} is the system's — use a path like {DEFAULT_MOUNT}"));
    }
    if top == "/root" || top == "/home" {
        return Err(format!(
            "--mount {path}: inside a home, every `pods pull`, backup and move would copy the whole volume — mount it at \
             {DEFAULT_MOUNT} and link to it from the home (`ln -s {DEFAULT_MOUNT} ~/data`)"
        ));
    }
    Ok(())
}

/// `<pod>-data`, else the first free `<pod>-data-<n>` (n ≥ 2) among `taken` (every volume name
/// in the project). Pure.
pub fn volume_name(pod: &str, taken: &[&str]) -> String {
    let base = format!("{pod}-data");
    if !taken.contains(&base.as_str()) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|n| !taken.contains(&n.as_str())).expect("a free name")
}

/// The `POST /volumes` body: created attached to `server_id` (in its location), formatted ext4
/// by Hetzner, never automounted (arena mounts it where it's asked to), labelled. Pure.
pub fn create_body(name: &str, size_gb: u32, server_id: u64, prefix: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "size": size_gb,
        "server": server_id,
        "format": "ext4",
        "automount": false,
        "labels": labels(prefix),
    })
}

/// A device path safe in fstab and a shell (Hetzner's `/dev/disk/by-id/scsi-0HC_Volume_<id>`).
pub fn plain_device(dev: &str) -> bool {
    crate::archive::render::plain_device(dev)
}

/// The device path of volume `v`: Hetzner's `linux_device`, else the one it always is.
pub fn device(v: &CloudVolume) -> String {
    if plain_device(&v.linux_device) {
        v.linux_device.clone()
    } else {
        format!("/dev/disk/by-id/scsi-0HC_Volume_{}", v.id)
    }
}

/// The fstab line arena keeps for a volume: by its stable device path, its filesystem told by
/// the kernel (`auto`: ext4 as arena makes them, xfs or another one attached by hand — review),
/// `nofail` (a detached volume never drops the VM into emergency mode), a device wait bounded at
/// 30 s. Pure.
pub fn fstab_line(device: &str, mount: &str) -> String {
    format!("{device} {mount} auto discard,nofail,defaults,x-systemd.device-timeout=30s 0 2")
}

/// The lock every fstab change on a VM takes (two volume steps at once never lose each other's
/// line — review): `flock` on a file in `/run`, waited for up to 30 s.
const FSTAB_LOCK: &str = "exec 9>/run/arena-fstab.lock && flock -w 30 9";

fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The shell every volume script starts with: `dev` (the device path), `real` (what it resolves
/// to now; empty when it isn't there) and `same SRC` — whether an fstab source names this volume,
/// by its path or any alias (`/dev/sdb`, `UUID=…`, `LABEL=…`: review — a literal match missed
/// those), and `parts` — the device and its partitions (a partitioned one's filesystems are on
/// those). `set -f`: fstab fields are split, never globbed.
fn prelude(device: &str) -> String {
    format!(
        "set -u; set -f; dev={dev}\n\
         real=$(readlink -f \"$dev\" 2>/dev/null); [ -e \"$real\" ] || real=\n\
         same() {{ case \"$1\" in UUID=*|LABEL=*|PARTUUID=*|PARTLABEL=*) r=$(findfs \"$1\" 2>/dev/null) ;; *) r=$(readlink -f \"$1\" 2>/dev/null) ;; esac; \
         [ \"$1\" = \"$dev\" ] || {{ [ -n \"$real\" ] && [ -n \"$r\" ] && {{ [ \"$r\" = \"$real\" ] || lsblk -nrpo NAME \"$real\" 2>/dev/null | grep -qxF \"$r\"; }}; }}; }}\n\
         parts() {{ [ -n \"$real\" ] && {{ lsblk -nrpo NAME \"$real\" 2>/dev/null || echo \"$real\"; }}; }}",
        dev = q(device)
    )
}

/// A volume arena made is one filesystem on the whole device; one partitioned since is
/// someone's own layout, and arena never rewrites its fstab line nor unmounts it (review r3).
/// Shell, after [`prelude`].
const PARTITIONED: &str = "if [ -n \"$real\" ] && [ \"$(lsblk -nrpo NAME \"$real\" 2>/dev/null | wc -l)\" -gt 1 ]; then \
     echo \"ARENA_VOL_FAIL the volume is partitioned now — its layout is someone's own: arena leaves it be\"; exit 1; fi";

/// Rewrite /etc/fstab without any line for this volume (by any of its names: [`prelude`]'s
/// `same`), then (`Some`) with `line` — atomically, through a temp file beside it. Comments and
/// every other line kept as they are. Shell, after [`prelude`], under [`FSTAB_LOCK`].
fn fstab_rewrite(line: Option<&str>) -> String {
    let add = line.map(|l| format!(" && printf '%s\\n' {} >> \"$t\"", q(l))).unwrap_or_default();
    format!(
        "{{ t=$(mktemp /etc/fstab.arena.XXXXXX) && \
         {{ while IFS= read -r l || [ -n \"$l\" ]; do set -- $l; case \"${{1:-}}\" in ''|\\#*) printf '%s\\n' \"$l\" ;; \
         *) same \"$1\" || printf '%s\\n' \"$l\" ;; esac; done < /etc/fstab > \"$t\"; }}{add} && chmod 0644 \"$t\" && mv -f \"$t\" /etc/fstab; }}"
    )
}

/// What the mount script ([`mount_script`]) said.
#[derive(Debug, Clone, PartialEq)]
pub enum Mounted {
    /// Mounted (or it already was), with the filesystem's size in bytes.
    Yes { bytes: Option<u64> },
    /// The VM doesn't see the volume (no device after the wait): a server that has run long
    /// may need one full power cycle first. The fstab line is in place for its next boot.
    NoDevice,
    /// Why not (a mount point in use by something else, not empty, a failed mount…).
    Failed(String),
}

/// How long the mount script waits for the device to appear (udev, after an attach).
pub const DEVICE_WAIT_SECS: u32 = 60;

/// Run as root on the VM: under the fstab lock, refuse a mount point that fstab gives another
/// device (review: never two at one place); then, already mounted there: make sure its fstab line
/// is arena's (review: a hand mount would vanish at the next boot) and say so. Else refuse a mount
/// point that holds files or another mount (mounting over files would hide them), put the fstab
/// line (replacing any other for this volume), wait up to [`DEVICE_WAIT_SECS`] for the device,
/// and mount it. Says how it went on its last line ([`parse_mount`]). Pure.
pub fn mount_script(device: &str, mount: &str) -> String {
    let line = fstab_line(device, mount);
    format!(
        "{prelude}\nmnt={mnt}\n\
         {lock} || {{ echo \"ARENA_VOL_FAIL another volume step holds /etc/fstab\"; exit 1; }}\n\
         clash=; while IFS= read -r l || [ -n \"$l\" ]; do set -- $l; case \"${{1:-}}\" in ''|\\#*) ;; *) [ \"${{2:-}}\" = \"$mnt\" ] && ! same \"$1\" && clash=\"$1\" ;; esac; done < /etc/fstab\n\
         [ -z \"$clash\" ] || {{ echo \"ARENA_VOL_FAIL /etc/fstab mounts another device at $mnt ($clash)\"; exit 1; }}\n\
         rp=$(readlink -m \"$mnt\" 2>/dev/null || echo \"$mnt\"); [ \"$rp\" = \"$mnt\" ] || {{ echo \"ARENA_VOL_FAIL $mnt goes through a symlink (to $rp): mount at a real path\"; exit 1; }}\n\
         {partitioned}\n\
         if [ -n \"$real\" ] && [ \"$(findmnt -rn -o TARGET -T \"$mnt\" 2>/dev/null)\" = \"$mnt\" ] && same \"$(findmnt -rn -o SOURCE -T \"$mnt\")\"; then\n\
         \x20 {fstab} || {{ echo \"ARENA_VOL_FAIL couldn't rewrite /etc/fstab\"; exit 1; }}\n\
         \x20 echo \"ARENA_VOL_MOUNTED $(df -P -B1 \"$mnt\" | awk 'NR==2{{print $2}}')\"; exit 0\n\
         fi\n\
         if mountpoint -q \"$mnt\" 2>/dev/null; then echo \"ARENA_VOL_FAIL $mnt is a mount point of something else ($(findmnt -rn -o SOURCE -T \"$mnt\"))\"; exit 1; fi\n\
         if [ -d \"$mnt\" ] && [ -n \"$(ls -A \"$mnt\" 2>/dev/null)\" ]; then echo \"ARENA_VOL_FAIL $mnt holds files (mounting over them would hide them)\"; exit 1; fi\n\
         if [ -e \"$mnt\" ] && [ ! -d \"$mnt\" ]; then echo \"ARENA_VOL_FAIL $mnt is a file, not a directory\"; exit 1; fi\n\
         mkdir -p \"$mnt\" || {{ echo \"ARENA_VOL_FAIL couldn't make $mnt\"; exit 1; }}\n\
         {fstab} || {{ echo \"ARENA_VOL_FAIL couldn't rewrite /etc/fstab\"; exit 1; }}\n\
         i=0; while [ ! -e \"$dev\" ] && [ \"$i\" -lt {tries} ]; do sleep 2; i=$((i + 1)); done\n\
         [ -e \"$dev\" ] || {{ echo ARENA_VOL_NO_DEVICE; exit 3; }}\n\
         systemctl daemon-reload >/dev/null 2>&1 || true\n\
         out=$(mount \"$mnt\" 2>&1) || {{ echo \"ARENA_VOL_FAIL mount: $out\"; exit 1; }}\n\
         echo \"ARENA_VOL_MOUNTED $(df -P -B1 \"$mnt\" | awk 'NR==2{{print $2}}')\"",
        prelude = prelude(device),
        mnt = q(mount),
        lock = FSTAB_LOCK,
        partitioned = PARTITIONED,
        fstab = fstab_rewrite(Some(&line)),
        tries = DEVICE_WAIT_SECS / 2,
    )
}

/// Read [`mount_script`]'s reply (its stdout). A reply without its marker is a failure, with
/// the last thing it said. Pure.
pub fn parse_mount(stdout: &str) -> Mounted {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ARENA_VOL_MOUNTED") {
            return Mounted::Yes { bytes: rest.trim().parse().ok() };
        }
        if line == "ARENA_VOL_NO_DEVICE" {
            return Mounted::NoDevice;
        }
        if let Some(why) = line.strip_prefix("ARENA_VOL_FAIL ") {
            return Mounted::Failed(why.to_string());
        }
    }
    Mounted::Failed(format!("no answer from the mount step ({})", stdout.lines().last().unwrap_or("nothing").trim()))
}

/// Where volume `device` is mounted on the VM, from its fstab line (by any of its names) or the
/// live mounts (of it or a partition of it) — what a move mounts it at on the new VM. Prints
/// `ARENA_VOL_AT <path>` (or nothing). Pure.
pub fn where_script(device: &str) -> String {
    format!(
        "{prelude}\nat=; while IFS= read -r l || [ -n \"$l\" ]; do set -- $l; case \"${{1:-}}\" in ''|\\#*) ;; *) [ -z \"$at\" ] && same \"$1\" && at=\"${{2:-}}\" ;; esac; done < /etc/fstab\n\
         if [ -z \"$at\" ]; then for d in $(parts); do at=$(findmnt -rn -o TARGET -S \"$d\" 2>/dev/null | head -n1); [ -n \"$at\" ] && break; done; fi\n\
         [ -n \"$at\" ] && echo \"ARENA_VOL_AT $at\"; true",
        prelude = prelude(device)
    )
}

/// What [`where_script`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum Where {
    /// Neither in fstab nor mounted.
    Nowhere,
    /// At this path (one a `--mount` could be: [`check_mount`]).
    At(String),
    /// At a path arena won't carry it to (inside a home, a system directory…).
    Odd(String),
}

/// [`where_script`]'s answer. Pure.
pub fn parse_where(stdout: &str) -> Where {
    match stdout.lines().find_map(|l| l.trim().strip_prefix("ARENA_VOL_AT ").map(str::trim).map(String::from)) {
        None => Where::Nowhere,
        Some(p) if check_mount(&p).is_ok() => Where::At(p.trim_end_matches('/').to_string()),
        Some(p) => Where::Odd(p),
    }
}

/// Run as root on the VM after a resize at Hetzner: make the kernel see the new size, grow the
/// filesystem online (`resize2fs` for ext*, `xfs_growfs` for xfs), print `ARENA_VOL_GROWN
/// <bytes>`. A volume that isn't mounted is said (`ARENA_VOL_FAIL not mounted …`): it grows
/// when it's mounted and grown again. Pure.
pub fn grow_script(device: &str) -> String {
    format!(
        "set -u; dev={dev}; real=$(readlink -f \"$dev\" 2>/dev/null) || {{ echo \"ARENA_VOL_FAIL no device $dev on this VM\"; exit 1; }}\n\
         n=$(basename \"$real\"); if [ -w \"/sys/class/block/$n/device/rescan\" ]; then echo 1 > \"/sys/class/block/$n/device/rescan\"; fi\n\
         mnt=$(findmnt -rn -o TARGET -S \"$real\" 2>/dev/null | head -n1)\n\
         [ -n \"$mnt\" ] || {{ echo \"ARENA_VOL_FAIL not mounted — mount it, then grow it again\"; exit 1; }}\n\
         fs=$(findmnt -rn -o FSTYPE -T \"$mnt\")\n\
         case \"$fs\" in\n\
         \x20 ext2|ext3|ext4) out=$(resize2fs \"$real\" 2>&1) || {{ echo \"ARENA_VOL_FAIL resize2fs: $(printf '%s' \"$out\" | tail -n1)\"; exit 1; }} ;;\n\
         \x20 xfs) out=$(xfs_growfs \"$mnt\" 2>&1) || {{ echo \"ARENA_VOL_FAIL xfs_growfs: $(printf '%s' \"$out\" | tail -n1)\"; exit 1; }} ;;\n\
         \x20 *) echo \"ARENA_VOL_FAIL a $fs filesystem — grow it by hand\"; exit 1 ;;\n\
         esac\n\
         echo \"ARENA_VOL_GROWN $(df -P -B1 \"$mnt\" | awk 'NR==2{{print $2}}')\"",
        dev = q(device)
    )
}

/// [`grow_script`]'s reply: the new size in bytes, or why not. Pure.
pub fn parse_grow(stdout: &str) -> std::result::Result<Option<u64>, String> {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ARENA_VOL_GROWN") {
            return Ok(rest.trim().parse().ok());
        }
        if let Some(why) = line.strip_prefix("ARENA_VOL_FAIL ") {
            return Err(why.to_string());
        }
    }
    Err(format!("no answer from the grow step ({})", stdout.lines().last().unwrap_or("nothing").trim()))
}

/// Run as root on the VM before a detach: sync, unmount the volume wherever it's mounted (one in
/// use is refused, naming who holds it), and drop its fstab line. `ARENA_VOL_UNMOUNTED` when
/// done (also when it wasn't mounted, or isn't there at all). Pure.
pub fn unmount_script(device: &str) -> String {
    format!(
        "{prelude}\nsync\n\
         {lock} || {{ echo \"ARENA_VOL_FAIL another volume step holds /etc/fstab\"; exit 1; }}\n\
         {partitioned}\n\
         for d in $(parts); do\n\
         \x20 for m in $(findmnt -rn -o TARGET -S \"$d\" 2>/dev/null); do\n\
         \x20   umount \"$m\" 2>/dev/null || {{ echo \"ARENA_VOL_BUSY $m $(fuser -m \"$m\" 2>/dev/null | wc -w) process(es)\"; exit 1; }}\n\
         \x20 done\n\
         done\n\
         {fstab} || {{ echo \"ARENA_VOL_FAIL couldn't rewrite /etc/fstab\"; exit 1; }}\n\
         echo ARENA_VOL_UNMOUNTED",
        prelude = prelude(device),
        lock = FSTAB_LOCK,
        partitioned = PARTITIONED,
        fstab = fstab_rewrite(None),
    )
}

/// [`unmount_script`]'s reply: `Ok` once unmounted, else why not (busy, with where). Pure.
pub fn parse_unmount(stdout: &str) -> std::result::Result<(), String> {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if line == "ARENA_VOL_UNMOUNTED" {
            return Ok(());
        }
        if let Some(rest) = line.strip_prefix("ARENA_VOL_BUSY ") {
            let (m, n) = rest.split_once(' ').unwrap_or((rest, ""));
            return Err(format!("{m} is in use ({n} hold it) — close what uses it (`fuser -vm {m}` on the VM shows who), then try again"));
        }
        if let Some(why) = line.strip_prefix("ARENA_VOL_FAIL ") {
            return Err(why.to_string());
        }
    }
    Err(format!("no answer from the unmount step ({})", stdout.lines().last().unwrap_or("nothing").trim()))
}

/// A shell condition that holds when volume `device` is what's mounted at `mount` (and `mount`
/// is a mount point of its own). Pure.
pub fn mounted_condition(device: &str, mount: &str) -> String {
    format!(
        "[ \"$(findmnt -rn -o TARGET -T {m} 2>/dev/null)\" = {m} ] && [ \"$(findmnt -rn -o SOURCE -T {m} 2>/dev/null)\" = \"$(readlink -f {d} 2>/dev/null)\" ]",
        m = q(mount.trim_end_matches('/')),
        d = q(device)
    )
}

/// Where a volume copy keeps what it replaces on the receiving volume: a visible folder at its
/// root, one per sync (`stamp`), never mirrored away.
pub const COPY_KEPT_DIR: &str = "arena-sync-kept";

/// The direct copy of a volume's files from one VM to another — run on the source, pushing its
/// `mount` into the destination's `mount`. Each side runs only with ITS volume mounted there
/// (`src_device`, `dest_device`: checked on the source before, and by the destination's
/// `--rsync-path` with its own id `dest_identity`), so it never reads or writes the disk under
/// an unmounted path. Mirrored (`--delete`), owners kept; whatever it would overwrite or delete
/// on the destination's volume is moved into `<mount>/arena-sync-kept/<stamp>/` there, never
/// destroyed (work made on the new VM's volume between syncs — review), and that folder is never
/// mirrored. Pure.
#[allow(clippy::too_many_arguments)]
pub fn copy_command(
    dest_ip: &str,
    dest_port: u16,
    dest_user: &str,
    remote_key: &str,
    mount: &str,
    src_device: &str,
    dest_device: &str,
    dest_identity: Option<&str>,
    stamp: &str,
) -> String {
    let ssh = format!(
        "ssh -p {dest_port} -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
         -o ConnectTimeout={} -o ServerAliveInterval=30 -i {remote_key}",
        crate::pull::POD_TO_POD_CONNECT_TIMEOUT_SECS
    );
    let m = mount.trim_end_matches('/');
    let dest_cond = match dest_identity {
        Some(id) => format!("{id} && {}", mounted_condition(dest_device, m)),
        None => mounted_condition(dest_device, m),
    };
    format!(
        "{src} || {{ echo '{m} here is not the volume to copy (not mounted?)' >&2; exit 1; }}; rsync -aHAX --delete --numeric-ids \
         --backup --backup-dir={kept} --exclude=/{COPY_KEPT_DIR}/ --rsync-path={rp} -e {e} {mq}/ {to}",
        src = mounted_condition(src_device, m),
        kept = q(&format!("{COPY_KEPT_DIR}/{stamp}")),
        rp = q(&crate::replica::guarded_rsync_path(&dest_cond)),
        e = q(&ssh),
        mq = q(m),
        to = q(&format!("{dest_user}@{dest_ip}:{m}/")),
    )
}

/// What a migration knows about its volumes between its commands (`migrate copy`, `cutover`,
/// `revert`, `finish`), kept in the state dir per machine: where they're mounted (read while the
/// original ran — a cutover from a stopped one can't ask), the copies that finished, and what a
/// cutover moved (what a revert moves back — and nothing else).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MoveRecord {
    /// The original's server id the record is about.
    pub src: String,
    /// Volume id → where it's mounted on the original.
    #[serde(default)]
    pub mounts: std::collections::BTreeMap<u64, String>,
    /// Original volume id → its copy's id, for every copy whose last sync finished.
    #[serde(default)]
    pub copied: std::collections::BTreeMap<u64, u64>,
    /// The volumes a cutover moved, and the server they moved to.
    #[serde(default)]
    pub moved: Vec<u64>,
    #[serde(default)]
    pub moved_to: Option<String>,
}

/// The volume price per GB-month at Hetzner (gross, the account's currency), from `/pricing`,
/// else the archive's list figure. Prices every volume the same: it's one price project-wide.
pub async fn price_gb_month(cloud: &dyn VolumeCloud) -> f64 {
    match cloud.prices("cx23", "nbg1").await {
        Ok(p) if p.volume_gb_month.is_finite() && p.volume_gb_month > 0.0 => p.volume_gb_month,
        _ => crate::archive::cloud::Prices::LIST.volume_gb_month,
    }
}

/// What `volumes` cost a month at `gb_month` per GB. Pure.
pub fn monthly(volumes: &[CloudVolume], gb_month: f64) -> f64 {
    volumes.iter().map(|v| f64::from(v.size_gb) * gb_month).sum()
}

/// The same per hour (Hetzner bills a monthly price by the hour). Pure.
pub fn per_hr(volumes: &[CloudVolume], gb_month: f64) -> f64 {
    monthly(volumes, gb_month) / HOURS_PER_MONTH
}

/// `pods volume list`'s table: name, id, size, location, the VM it's attached to (by the
/// fleet's name for that server id, `names`), its €/month; then the total. Pure.
pub fn render(volumes: &[CloudVolume], names: &dyn Fn(u64) -> Option<String>, gb_month: f64) -> String {
    let mut rows = Vec::new();
    for v in volumes {
        let on = match v.server {
            None => "- (detached)".to_string(),
            Some(id) => names(id).unwrap_or_else(|| format!("server {id} (not in the fleet)")),
        };
        rows.push(vec![
            v.name.clone(),
            v.id.to_string(),
            format!("{} GB", v.size_gb),
            v.location.clone(),
            on,
            crate::fleet::fmt_money("€", f64::from(v.size_gb) * gb_month),
        ]);
    }
    let header = ["NAME", "ID", "SIZE", "WHERE", "ATTACHED TO", "€/MONTH"];
    let align = [Align::Left, Align::Left, Align::Right, Align::Left, Align::Left, Align::Right];
    let mut out = table::render(&header, &align, &rows);
    let gb: u32 = volumes.iter().map(|v| v.size_gb).sum();
    out.push_str(&format!(
        "{} volume{}, {gb} GB, {}/month — billing until deleted, attached or not\n",
        volumes.len(),
        if volumes.len() == 1 { "" } else { "s" },
        crate::fleet::fmt_money("€", monthly(volumes, gb_month))
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(id: u64, name: &str, gb: u32, server: Option<u64>, labels: &[(&str, &str)]) -> CloudVolume {
        CloudVolume {
            id,
            name: name.into(),
            size_gb: gb,
            location: "nbg1".into(),
            server,
            format: Some("ext4".into()),
            linux_device: format!("/dev/disk/by-id/scsi-0HC_Volume_{id}"),
            labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            delete_protected: false,
        }
    }

    #[test]
    fn sizes_and_mount_paths_are_checked_before_anything() {
        assert!(check_size(10).is_ok() && check_size(10_240).is_ok());
        assert!(check_size(9).unwrap_err().contains("10 GB to 10240 GB"));
        assert!(check_size(10_241).is_err());
        for ok in ["/mnt/data", "/mnt/data/", "/data", "/srv/big_disk-1", "/workspace"] {
            assert!(check_mount(ok).is_ok(), "{ok}");
        }
        for (bad, why) in [
            ("mnt/data", "absolute"),
            ("/", "absolute"),
            ("/mnt/da ta", "plain path"),
            ("/mnt/../etc", "plain path"),
            ("/mnt//x", "plain path"),
            ("/etc/x", "the system's"),
            ("/usr", "the system's"),
            ("/tmp/v", "the system's"),
            ("/root/data", "inside a home"),
            ("/home/ubuntu/x", "inside a home"),
        ] {
            assert!(check_mount(bad).unwrap_err().contains(why), "{bad}");
        }
    }

    #[test]
    fn names_labels_and_whose_a_volume_is() {
        assert_eq!(volume_name("devtest-alpha", &[]), "devtest-alpha-data");
        assert_eq!(volume_name("devtest-alpha", &["devtest-alpha-data", "devtest-alpha-data-2"]), "devtest-alpha-data-3");
        let ours = vol(1, "devtest-alpha-data", 10, Some(7), &[(LABEL, "devtest")]);
        let archive = vol(2, "devtest-archive-snap", 100, Some(9), &[(crate::archive::LABEL_KEY, "devtest")]);
        let by_hand = vol(3, "scratch", 20, None, &[]);
        assert!(is_ours(&ours, "devtest") && !is_ours(&ours, "arena9") && !is_ours(&by_hand, "devtest"));
        let fleet = fleet_volumes(&[ours.clone(), archive, by_hand.clone()]);
        assert_eq!(fleet.iter().map(|v| v.id).collect::<Vec<_>>(), [1, 3], "never the archive's");
        assert_eq!(attached_to(&fleet, "7").len(), 1);
        assert!(attached_to(&fleet, "8").is_empty());
        let body = create_body("devtest-alpha-data", 10, 7, "devtest");
        assert_eq!(
            body,
            serde_json::json!({"name": "devtest-alpha-data", "size": 10, "server": 7, "format": "ext4", "automount": false,
                "labels": {"arena-volume": "devtest"}})
        );
        assert_eq!(device(&ours), "/dev/disk/by-id/scsi-0HC_Volume_1");
        let odd = CloudVolume { linux_device: "/dev/sdb; rm".into(), ..ours };
        assert_eq!(device(&odd), "/dev/disk/by-id/scsi-0HC_Volume_1", "never a device path that isn't plain");
    }

    /// The scripts run for real on a temp "VM": the fstab rewrite keeps every other line and
    /// puts ours once; the mount step refuses a mount point with files in it, says a missing
    /// device as one (after its wait — here none); unmount drops the line.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_on_vm_steps_say_how_they_went() {
        // Pure replies first.
        assert_eq!(parse_mount("…\nARENA_VOL_MOUNTED 10501771264\n"), Mounted::Yes { bytes: Some(10_501_771_264) });
        assert_eq!(parse_mount("ARENA_VOL_NO_DEVICE\n"), Mounted::NoDevice);
        assert_eq!(parse_mount("ARENA_VOL_FAIL /mnt/data holds files (x)\n"), Mounted::Failed("/mnt/data holds files (x)".into()));
        assert!(matches!(parse_mount("ssh: connection reset"), Mounted::Failed(w) if w.contains("connection reset")));
        assert_eq!(parse_grow("ARENA_VOL_GROWN 11552346112"), Ok(Some(11_552_346_112)));
        assert!(parse_grow("ARENA_VOL_FAIL not mounted — mount it").unwrap_err().contains("not mounted"));
        assert_eq!(parse_unmount("ARENA_VOL_UNMOUNTED"), Ok(()));
        assert!(parse_unmount("ARENA_VOL_BUSY /mnt/data 2 process(es)").unwrap_err().contains("fuser -vm /mnt/data"));
        assert_eq!(parse_where("ARENA_VOL_AT /mnt/data\n"), Where::At("/mnt/data".into()));
        assert_eq!(parse_where("ARENA_VOL_AT /root/x\n"), Where::Odd("/root/x".into()), "never carried to a path a --mount couldn't be");
        assert_eq!(parse_where(""), Where::Nowhere);

        // The fstab rewrite, for real, on a copy.
        let dir = std::env::temp_dir().join(format!("arena-datavol-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fstab = dir.join("fstab");
        let dev = "/dev/disk/by-id/scsi-0HC_Volume_42";
        std::fs::write(&fstab, format!("# the root\nUUID=abc / ext4 defaults 0 1\n\n{dev} /mnt/old ext4 nofail 0 2\n")).unwrap();
        let lock = dir.join("fstab.lock");
        let rewrite = |s: String| s.replace("/etc/fstab", &fstab.display().to_string()).replace("/run/arena-fstab.lock", &lock.display().to_string());
        let run = |script: &str| {
            let out = std::process::Command::new("sh").arg("-c").arg(script).output().unwrap();
            assert!(out.status.success(), "{script}\n{}", String::from_utf8_lossy(&out.stderr));
        };
        run(&rewrite(format!("{}\n{}", prelude(dev), fstab_rewrite(Some(&fstab_line(dev, "/mnt/data"))))));
        assert_eq!(
            std::fs::read_to_string(&fstab).unwrap(),
            format!("# the root\nUUID=abc / ext4 defaults 0 1\n\n{}\n", fstab_line(dev, "/mnt/data")),
            "the other lines kept (comments, blanks), ours once"
        );
        // Where it's mounted, read back from its fstab line.
        let out = std::process::Command::new("sh").arg("-c").arg(rewrite(where_script(dev))).output().unwrap();
        assert_eq!(parse_where(&String::from_utf8_lossy(&out.stdout)), Where::At("/mnt/data".into()));
        run(&rewrite(format!("{}\n{}", prelude(dev), fstab_rewrite(None))));
        assert_eq!(std::fs::read_to_string(&fstab).unwrap(), "# the root\nUUID=abc / ext4 defaults 0 1\n\n");
        std::fs::write(&fstab, "UUID=abc / ext4 defaults 0 1\n").unwrap();
        // The mount step refuses a mount point that holds files, before it touches fstab.
        let mnt = dir.join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();
        std::fs::write(mnt.join("theirs.txt"), "x").unwrap();
        let out = std::process::Command::new("sh").arg("-c").arg(rewrite(mount_script(dev, &mnt.display().to_string()))).output().unwrap();
        let said = parse_mount(&String::from_utf8_lossy(&out.stdout));
        assert!(matches!(&said, Mounted::Failed(w) if w.contains("holds files")), "{said:?}");
        assert_eq!(std::fs::read_to_string(&fstab).unwrap(), "UUID=abc / ext4 defaults 0 1\n", "fstab untouched");
        // Another device in fstab at that mount point: refused (review: never two at one place).
        std::fs::remove_file(mnt.join("theirs.txt")).unwrap();
        std::fs::write(&fstab, format!("UUID=abc / ext4 defaults 0 1\n/dev/sdz {} ext4 nofail 0 2\n", mnt.display())).unwrap();
        let out = std::process::Command::new("sh").arg("-c").arg(rewrite(mount_script(dev, &mnt.display().to_string()))).output().unwrap();
        let said = parse_mount(&String::from_utf8_lossy(&out.stdout));
        assert!(matches!(&said, Mounted::Failed(w) if w.contains("mounts another device")), "{said:?}");
        // A mount point that's a symlink (into a home, say): refused (review r3).
        std::fs::write(&fstab, "UUID=abc / ext4 defaults 0 1\n").unwrap();
        let target = dir.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        let link = dir.join("linked");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let out = std::process::Command::new("sh").arg("-c").arg(rewrite(mount_script(dev, &link.display().to_string()))).output().unwrap();
        let said = parse_mount(&String::from_utf8_lossy(&out.stdout));
        assert!(matches!(&said, Mounted::Failed(w) if w.contains("goes through a symlink")), "{said:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_copy_runs_on_the_source_into_the_destinations_own_mount() {
        let (src, dst) = ("/dev/disk/by-id/scsi-0HC_Volume_1", "/dev/disk/by-id/scsi-0HC_Volume_2");
        let c = copy_command("10.0.0.2", 22, "root", "/root/.ssh/id_ed25519", "/mnt/data/", src, dst, Some("[ x = y ]"), "S1");
        assert!(c.starts_with(&mounted_condition(src, "/mnt/data")), "the source's own volume, checked first: {c}");
        assert!(c.contains("rsync -aHAX --delete --numeric-ids --backup --backup-dir='arena-sync-kept/S1' --exclude=/arena-sync-kept/"), "{c}");
        assert!(c.contains("[ x = y ] &&") && c.contains("scsi-0HC_Volume_2"), "the destination's id and volume, in its rsync-path: {c}");
        assert!(c.ends_with("'/mnt/data'/ 'root@10.0.0.2:/mnt/data/'"), "{c}");
        assert!(c.contains("-i /root/.ssh/id_ed25519"), "{c}");
    }

    /// The mounted check, for real: a path that isn't a mount point of its own fails it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_path_that_isnt_the_volume_fails_the_mounted_check() {
        let dir = std::env::temp_dir().join(format!("arena-datavol-mnt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(mounted_condition("/dev/disk/by-id/scsi-0HC_Volume_1", &dir.display().to_string()))
            .status()
            .unwrap()
            .success();
        assert!(!ok, "a plain directory is no volume");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cohort_acts_on_its_own_volumes_only() {
        let ours = vol(1, "devtest-alpha-data", 10, None, &[(LABEL, "devtest")]);
        let theirs = vol(2, "arena9-apple-data", 10, None, &[(LABEL, "arena9")]);
        let by_hand_on_ours = vol(3, "scratch", 20, Some(7), &[]);
        let by_hand_loose = vol(4, "loose", 20, None, &[]);
        let archive = vol(5, "devtest-archive-snap", 100, Some(7), &[(crate::archive::LABEL_KEY, "devtest")]);
        let all = [ours, theirs, by_hand_on_ours, by_hand_loose, archive];
        let got: Vec<u64> = cohort_volumes(&all, "devtest", &["7".into()]).iter().map(|v| v.id).collect();
        assert_eq!(got, [1, 3]);
    }

    #[test]
    fn the_table_and_the_cost() {
        let v = vec![vol(1, "devtest-alpha-data", 10, Some(7), &[]), vol(2, "scratch", 30, None, &[])];
        let names = |id: u64| (id == 7).then(|| "devtest-alpha".to_string());
        let t = render(&v, &names, 0.044);
        assert!(t.contains("devtest-alpha-data") && t.contains("devtest-alpha") && t.contains("- (detached)"), "{t}");
        assert!(t.trim_end().ends_with("2 volumes, 40 GB, €1.76/month — billing until deleted, attached or not"), "{t}");
        assert!((per_hr(&v, 0.044) - 1.76 / 730.0).abs() < 1e-12);
    }
}
