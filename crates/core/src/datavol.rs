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

/// The fstab line arena keeps for a volume: by its stable device path, `nofail` (a detached
/// volume never drops the VM into emergency mode), a device wait bounded at 30 s. Pure.
pub fn fstab_line(device: &str, mount: &str) -> String {
    format!("{device} {mount} ext4 discard,nofail,defaults,x-systemd.device-timeout=30s 0 2")
}

/// Rewrite /etc/fstab without any line for `device`, then (`Some`) with `line` — atomically,
/// through a temp file beside it. Shell, for the scripts below.
fn fstab_rewrite(device: &str, line: Option<&str>) -> String {
    let add = line.map(|l| format!(" && printf '%s\\n' {}  >> \"$t\"", q(l))).unwrap_or_default();
    format!(
        "t=$(mktemp /etc/fstab.arena.XXXXXX) && awk -v d={} '$1 != d' /etc/fstab > \"$t\"{add} && chmod 0644 \"$t\" && mv -f \"$t\" /etc/fstab",
        q(device)
    )
}

fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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

/// Run as root on the VM: put the fstab line for `device` at `mount` (replacing any earlier one
/// for that device), wait up to [`DEVICE_WAIT_SECS`] for the device, then mount it there —
/// refusing a mount point that holds files or another mount (mounting over files would hide
/// them). Says how it went on its last line ([`parse_mount`]). Idempotent: an already mounted
/// volume is left as it is. Pure.
pub fn mount_script(device: &str, mount: &str) -> String {
    let line = fstab_line(device, mount);
    format!(
        "set -u; dev={dev}; mnt={mnt}\n\
         real=$(readlink -f \"$dev\" 2>/dev/null || true)\n\
         if [ -n \"$real\" ] && [ -e \"$real\" ] && findmnt -rn -S \"$real\" -T \"$mnt\" >/dev/null 2>&1 && [ \"$(findmnt -rn -o TARGET -T \"$mnt\")\" = \"$mnt\" ]; then\n\
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
        dev = q(device),
        mnt = q(mount),
        fstab = fstab_rewrite(device, Some(&line)),
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

/// Where volume `device` is mounted on the VM, from its fstab line or the live mounts — what a
/// move mounts it at on the new VM. Prints `ARENA_VOL_AT <path>` (or nothing). Pure.
pub fn where_script(device: &str) -> String {
    format!(
        "dev={dev}; at=$(awk -v d=\"$dev\" '$1 == d {{ print $2; exit }}' /etc/fstab 2>/dev/null); \
         if [ -z \"$at\" ]; then real=$(readlink -f \"$dev\" 2>/dev/null) && at=$(findmnt -rn -o TARGET -S \"$real\" 2>/dev/null | head -n1); fi; \
         [ -n \"$at\" ] && echo \"ARENA_VOL_AT $at\"; true",
        dev = q(device)
    )
}

/// [`where_script`]'s answer, checked like a `--mount` ([`check_mount`]). Pure.
pub fn parse_where(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|l| l.trim().strip_prefix("ARENA_VOL_AT ").map(str::trim).map(String::from)).filter(|p| check_mount(p).is_ok())
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
        "set -u; dev={dev}; sync\n\
         real=$(readlink -f \"$dev\" 2>/dev/null || true)\n\
         if [ -n \"$real\" ] && [ -e \"$real\" ]; then\n\
         \x20 for m in $(findmnt -rn -o TARGET -S \"$real\" 2>/dev/null); do\n\
         \x20   umount \"$m\" 2>/dev/null || {{ echo \"ARENA_VOL_BUSY $m $(fuser -m \"$m\" 2>/dev/null | wc -w) process(es)\"; exit 1; }}\n\
         \x20 done\n\
         fi\n\
         {fstab} || {{ echo \"ARENA_VOL_FAIL couldn't rewrite /etc/fstab\"; exit 1; }}\n\
         echo ARENA_VOL_UNMOUNTED",
        dev = q(device),
        fstab = fstab_rewrite(device, None),
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

/// The direct copy of a volume's files from one VM to another — run on the source, pushing its
/// `mount` into the destination's `mount` (each VM's own volume mounted there): everything,
/// mirrored (`--delete`; the destination's volume holds nothing of its own), owners kept. The
/// destination's rsync runs behind `dest_guard` (its own id). Pure.
pub fn copy_command(dest_ip: &str, dest_port: u16, dest_user: &str, remote_key: &str, mount: &str, dest_guard: Option<&str>) -> String {
    let ssh = format!(
        "ssh -p {dest_port} -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
         -o ConnectTimeout={} -o ServerAliveInterval=30 -i {remote_key}",
        crate::pull::POD_TO_POD_CONNECT_TIMEOUT_SECS
    );
    let rsync_path = dest_guard.map(|g| format!(" --rsync-path={}", q(&crate::replica::guarded_rsync_path(g)))).unwrap_or_default();
    let m = mount.trim_end_matches('/');
    format!(
        "mountpoint -q {mq} || {{ echo '{m} is not mounted here' >&2; exit 1; }}; rsync -aHAX --delete --numeric-ids{rsync_path} -e {} {mq}/ {}",
        q(&ssh),
        q(&format!("{dest_user}@{dest_ip}:{m}/")),
        mq = q(m),
    )
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
        assert_eq!(parse_where("ARENA_VOL_AT /mnt/data\n"), Some("/mnt/data".into()));
        assert_eq!(parse_where("ARENA_VOL_AT /root/x\n"), None, "never a path a --mount couldn't be");
        assert_eq!(parse_where(""), None);

        // The fstab rewrite, for real, on a copy.
        let dir = std::env::temp_dir().join(format!("arena-datavol-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fstab = dir.join("fstab");
        let dev = "/dev/disk/by-id/scsi-0HC_Volume_42";
        std::fs::write(&fstab, format!("UUID=abc / ext4 defaults 0 1\n{dev} /mnt/old ext4 nofail 0 2\n")).unwrap();
        let rewrite = |s: String| s.replace("/etc/fstab", &fstab.display().to_string());
        let run = |script: &str| {
            let out = std::process::Command::new("sh").arg("-c").arg(script).output().unwrap();
            assert!(out.status.success(), "{script}\n{}", String::from_utf8_lossy(&out.stderr));
        };
        run(&rewrite(fstab_rewrite(dev, Some(&fstab_line(dev, "/mnt/data")))));
        assert_eq!(
            std::fs::read_to_string(&fstab).unwrap(),
            format!("UUID=abc / ext4 defaults 0 1\n{}\n", fstab_line(dev, "/mnt/data")),
            "the other lines kept, ours once"
        );
        run(&rewrite(fstab_rewrite(dev, None)));
        assert_eq!(std::fs::read_to_string(&fstab).unwrap(), "UUID=abc / ext4 defaults 0 1\n");
        // The mount step refuses a mount point that holds files, before it touches fstab.
        let mnt = dir.join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();
        std::fs::write(mnt.join("theirs.txt"), "x").unwrap();
        let out = std::process::Command::new("sh").arg("-c").arg(rewrite(mount_script(dev, &mnt.display().to_string()))).output().unwrap();
        let said = parse_mount(&String::from_utf8_lossy(&out.stdout));
        assert!(matches!(&said, Mounted::Failed(w) if w.contains("holds files")), "{said:?}");
        assert_eq!(std::fs::read_to_string(&fstab).unwrap(), "UUID=abc / ext4 defaults 0 1\n", "fstab untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_copy_runs_on_the_source_into_the_destinations_own_mount() {
        let c = copy_command("10.0.0.2", 22, "root", "/root/.ssh/id_ed25519", "/mnt/data/", Some("[ x = y ]"));
        assert!(c.starts_with("mountpoint -q '/mnt/data' ||"), "{c}");
        assert!(c.contains("rsync -aHAX --delete --numeric-ids --rsync-path="), "{c}");
        assert!(c.ends_with("'/mnt/data'/ 'root@10.0.0.2:/mnt/data/'"), "{c}");
        assert!(c.contains("-i /root/.ssh/id_ed25519"), "{c}");
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
