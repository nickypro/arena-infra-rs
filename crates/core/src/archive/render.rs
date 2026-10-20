//! Everything arena sends to the archive VM or hands to participants, rendered purely: the
//! setup/probe commands (scripts base64-encoded inside the ssh command, like the deep check —
//! one exec, nothing to quote wrong), the key files, the sshd drop-in, the fstab block, the
//! README, and the laptop/pod ssh-config blocks and known_hosts.

use super::{key_type_and_blob, Tier, PUSH_ROOT, VIEW_ROOT};
use crate::base64::encode as base64_encode;

/// The setup script (see its header).
pub const SETUP_SCRIPT: &str = include_str!("setup.sh");
/// The read-only probe `status` runs.
pub const PROBE_SCRIPT: &str = include_str!("probe.sh");

/// Where rrsync goes in the push key's forced command until setup has found it on the VM
/// (`setup.sh` substitutes the path it found; `status` compares with the path the probe reports).
pub const RRSYNC_PLACEHOLDER: &str = "@RRSYNC@";

/// `NAME='value'` for a POSIX shell (single quotes; a `'` as `'\''`): the value reaches the
/// script byte for byte, newlines included.
pub fn sh_assign(name: &str, value: &str) -> String {
    format!("{name}='{}'\n", value.replace('\'', "'\\''"))
}

/// What `setup.sh` is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupInputs {
    pub snap_dev: String,
    pub big_dev: String,
    /// The `arena-push` key file, with [`RRSYNC_PLACEHOLDER`] for rrsync's path.
    pub push_keys: String,
    /// The `backups` key file (empty = access off).
    pub backups_keys: String,
    pub readme: String,
    pub setup_version: u32,
}

/// The assignments `setup.sh` reads, in front of it.
pub fn render_setup_inputs(i: &SetupInputs) -> String {
    [
        sh_assign("ARENA_SNAP_DEV", &i.snap_dev),
        sh_assign("ARENA_BIG_DEV", &i.big_dev),
        sh_assign("ARENA_PUSH_KEY_TEMPLATE", &i.push_keys),
        sh_assign("ARENA_BACKUPS_KEYS", &i.backups_keys),
        sh_assign("ARENA_SSHD_DROPIN", &render_sshd_dropin()),
        sh_assign("ARENA_FSTAB_BLOCK", &render_fstab_block(&i.snap_dev, &i.big_dev)),
        sh_assign("ARENA_README", &i.readme),
        sh_assign("ARENA_SETUP_VERSION", &i.setup_version.to_string()),
    ]
    .concat()
}

/// A script with its inputs, as one command: `bash -c "$(printf %s <b64> | base64 -d)" <name>`.
fn embedded(script: &str, name: &str) -> String {
    format!("bash -c \"$(printf %s {} | base64 -d)\" {name} </dev/null", base64_encode(script.as_bytes()))
}

/// The setup command (`up` runs it once as root, ≤ 10 min).
pub fn setup_command(i: &SetupInputs) -> String {
    embedded(&format!("{}{SETUP_SCRIPT}", render_setup_inputs(i)), "arena-archive-setup")
}

/// The probe command (`status`; read-only).
pub fn probe_command(snap_dev: &str, big_dev: &str) -> String {
    let inputs = [sh_assign("ARENA_SNAP_DEV", snap_dev), sh_assign("ARENA_BIG_DEV", big_dev)].concat();
    embedded(&format!("{inputs}{PROBE_SCRIPT}"), "arena-archive-probe")
}

/// The push key's line in `/etc/ssh/arena-keys/arena-push` (S5): `restrict` (no pty, no
/// forwarding, no agent, no X11, no user rc) and a forced command — rrsync, write-only, no
/// deletes, no lock (snap and big push at once), rooted at the push dir, and `-munge`: the
/// receiving rsync stores every symlink defanged (`/rsyncd-munged/<target>`) and never follows
/// one, so a link a participant planted can't steer a write anywhere — whatever arena's argv
/// (ARCH-SEC-1/3). Whatever the client asks to run, this runs. The key is re-rendered from its
/// type and blob only: a comment can't smuggle anything in.
pub fn render_push_key_line(rrsync: &str, push_pub: &str) -> Result<String, String> {
    let (ty, blob) = key_type_and_blob(push_pub).ok_or("the push key's .pub isn't an OpenSSH public key")?;
    Ok(format!("restrict,command=\"{rrsync} -munge -wo -no-del -no-lock {PUSH_ROOT}\" {ty} {blob} arena-archive-push\n"))
}

/// The `backups` key file (S5): the cohort key (laptops) and the per-cohort read key (pods),
/// each `restrict`ed — or nothing at all when access is off. Never the deploy key (every pod of
/// every cohort holds its private half), the admin key or an `EXTRA_SSH_KEYS` key: if the
/// cohort key IS one of those (a config mix-up), this refuses rather than publish the archive
/// to past cohorts.
pub fn render_backups_keys(cohort_pub: Option<&str>, read_pub: &str, access_on: bool, forbidden: &[String]) -> Result<String, String> {
    if !access_on {
        return Ok(String::new());
    }
    let cohort = cohort_pub.ok_or("access needs the cohort key's public half (SHARED_SSH_KEY_PATH) — it couldn't be read")?;
    let mut out = String::new();
    for (what, line) in [("cohort", cohort), ("read", read_pub)] {
        let (ty, blob) = key_type_and_blob(line).ok_or_else(|| format!("the {what} key isn't an OpenSSH public key"))?;
        if forbidden.iter().any(|f| super::key_blob(f) == Some(blob)) {
            return Err(format!(
                "the {what} key is also the deploy key, the archive admin key or an EXTRA_SSH_KEYS key — it would open the \
                 archive to every holder of that key; refusing"
            ));
        }
        out.push_str(&format!("restrict {ty} {blob} arena-archive-{what}\n"));
    }
    Ok(out)
}

/// `/etc/ssh/sshd_config.d/10-arena-archive.conf`: sorts before cloud-init's `50-` drop-in
/// (sshd keeps the FIRST value it reads), key-only logins for three users, and each of the two
/// service users fenced in its `Match` block (keys in root-owned files outside their homes).
pub fn render_sshd_dropin() -> String {
    format!(
        "# Managed by `arena archive up` — edits are overwritten.\n\
         PasswordAuthentication no\n\
         KbdInteractiveAuthentication no\n\
         PermitRootLogin prohibit-password\n\
         AllowUsers root arena-push backups\n\
         ClientAliveInterval 60\n\
         ClientAliveCountMax 3\n\
         Match User backups\n\
         \x20   AuthorizedKeysFile /etc/ssh/arena-keys/%u\n\
         \x20   ChrootDirectory {VIEW_ROOT}\n\
         \x20   ForceCommand internal-sftp -R -d /\n\
         \x20   PermitTTY no\n\
         \x20   DisableForwarding yes\n\
         \x20   PermitTunnel no\n\
         Match User arena-push\n\
         \x20   AuthorizedKeysFile /etc/ssh/arena-keys/%u\n\
         \x20   PermitTTY no\n\
         \x20   DisableForwarding yes\n\
         \x20   PermitTunnel no\n"
    )
}

/// The fstab block: each volume by its stable device path at a fixed mountpoint, then its
/// read-only bind into the chroot. `nofail` everywhere (a detached volume never drops the VM
/// into emergency mode); a bind requires its volume's mount, so it never binds the bare dir.
pub fn render_fstab_block(snap_dev: &str, big_dev: &str) -> String {
    let mut s = String::from("# BEGIN arena-archive\n");
    for (tier, dev) in [(Tier::Snap, snap_dev), (Tier::Big, big_dev)] {
        s.push_str(&format!("{dev} {} ext4 defaults,nofail,noatime,x-systemd.device-timeout=30s 0 2\n", tier.mountpoint()));
    }
    for tier in Tier::ALL {
        let mp = tier.mountpoint();
        s.push_str(&format!("{mp} {} none bind,ro,nofail,x-systemd.requires-mounts-for={mp} 0 0\n", tier.view_mountpoint()));
    }
    s.push_str("# END arena-archive");
    s
}

/// A device path safe for fstab and a shell assignment (Hetzner's `/dev/disk/by-id/…`).
pub fn plain_device(dev: &str) -> bool {
    dev.starts_with("/dev/") && dev.len() > 5 && dev.chars().all(|c| c.is_ascii_alphanumeric() || "/._:-".contains(c))
}

/// What the README needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadmeInputs {
    pub prefix: String,
    pub alias: String,
    pub keep_until: String,
    pub snap_max_size: String,
    pub extra_excludes: Vec<String>,
}

/// `/srv/arena/view/README.txt`, what a participant sees first: the layout, how labels count
/// days, how to copy files back, what is NOT here, and — plainly — who else can read it.
pub fn render_readme(r: &ReadmeInputs) -> String {
    let extra = if r.extra_excludes.is_empty() { String::new() } else { format!("\n  Also left out for this cohort: {}.", r.extra_excludes.join(", ")) };
    let p = &r.prefix;
    let a = &r.alias;
    format!(
        "ARENA backup archive — cohort {p}\n\
         =====================================\n\
         \n\
         A READ-ONLY copy of your cohort's pod backups, kept so they outlive the pods.\n\
         EVERYONE IN YOUR COHORT CAN READ EVERYTHING HERE (as anyone in the cohort can on every pod).\n\
         Kept until: {keep}\n\
         \n\
         Layout\n\
         \x20 snap/<wNdM>/<pod>/  files under {max}, one complete copy per day. w = week, d = day, counted in\n\
         \x20                     UTC days from the program's start (w0d1 = its first day). Unchanged files are\n\
         \x20                     shared between days, so each day costs only what changed.\n\
         \x20 big/<pod>/          every file, large ones too (checkpoints…), the latest copy; nothing is ever\n\
         \x20                     deleted from it, even after it's gone from the pod.\n\
         \x20 snap/baseline/      the course image's own files, which every snapshot shares.\n\
         \n\
         Getting files back (laptop: the cohort key; on a pod it's already set up) — host `{a}`\n\
         \x20 sftp {a}            then: ls snap ; cd snap/w2d5/{p}-apple ; get -r ARENA_materials/chapter1\n\
         \x20                     (`reget` resumes a big download)\n\
         \x20 scp -r {a}:snap/w2d5/{p}-apple/ARENA_materials/chapter1 ./restore\n\
         \x20 scp -r {a}:big/{p}-apple/checkpoints ./restore\n\
         \x20 (scp needs OpenSSH 9 or newer; with an older one use `scp -s`.)\n\
         \x20 Symbolic links are kept defanged: `ls -l` shows them pointing at /rsyncd-munged/<target>,\n\
         \x20 and they don't resolve here, so a download skips them or fails on them. Copy the files,\n\
         \x20 then re-create a link you need with `ln -s <target> <name>` (target = the part after\n\
         \x20 /rsyncd-munged/). A link whose target is over 4080 characters isn't kept at all.\n\
         \x20 A day can be partial for a pod whose copy kept failing that day; ask the organisers.\n\
         \n\
         Not copied here — secrets stay on the organisers' machine:\n\
         \x20 shell rc files (.bashrc, .zshrc, .profile …), shell and REPL histories, .env files, .netrc,\n\
         \x20 .gitconfig, .git-credentials, .pypirc, .npmrc, .pgpass, .s3cfg, .boto, .rclone.conf, .ssh/,\n\
         \x20 .gnupg/, .aws/, .docker/, .kaggle/ and kaggle.json, .config/, .cache/, .huggingface/ and HF\n\
         \x20 token files, .claude*, private keys (id_rsa/id_ecdsa/id_ed25519/id_dsa, *.pem, *.key), and each\n\
         \x20 repo's .git/config files, a bare clone's too (git works without them; add the remote back with\n\
         \x20 `git remote add origin <url>`).{extra}\n\
         \x20 Anything else in your home IS copied: don't keep other secrets there.\n\
         \n\
         Your repo's history is on GitHub too: the autocommit branches, pushed every 15 minutes.\n",
        keep = r.keep_until,
        max = r.snap_max_size,
    )
}

/// Where a laptop keeps the archive's host key: a file of its own, the key under the alias
/// (`HostKeyAlias`) — never `~/.ssh/known_hosts`. W7 live: a rebuilt archive kept its IPv4 with a
/// new host key, and a laptop that had learnt the old key under that IP in its main file got
/// "REMOTE HOST IDENTIFICATION HAS CHANGED". Under the alias the pin survives a new IP, never
/// collides with whatever else used the address, and re-pinning touches nothing else:
/// `arena ssh-config --out` rewrites the file; by hand, `ssh-keygen -R <alias> -f` it.
pub const LAPTOP_KNOWN_HOSTS: &str = "~/.ssh/arena_archive_known_hosts";

/// The participants' laptop block (§8.2): the cohort key, trust on first use into the block's
/// own known-hosts file ([`LAPTOP_KNOWN_HOSTS`]) under the alias — with the pinned fingerprint to
/// compare by eye, and what to do when the archive is rebuilt. Its alias escapes the fleet's
/// `Host <prefix>-*` wildcard.
pub fn render_laptop_block(alias: &str, host: &str, identity: &str, fingerprint: Option<&str>) -> String {
    let fp = match fingerprint {
        Some(f) => format!("# {f} — compare on first connect\n"),
        None => String::new(),
    };
    let rebuilt = format!(
        "# A rebuilt archive has a new host key: take the new block and its {} file, or run\n\
         #   ssh-keygen -R {alias} -f {LAPTOP_KNOWN_HOSTS}\n# and compare the new fingerprint.\n",
        LAPTOP_KNOWN_HOSTS.trim_start_matches("~/.ssh/")
    );
    let block = crate::sshconfig::render_archive_block(alias, host, super::READ_USER, identity, Some(LAPTOP_KNOWN_HOSTS), "accept-new", Some(alias));
    format!("{fp}{rebuilt}{block}")
}

/// The laptop's own known-hosts file for the archive ([`LAPTOP_KNOWN_HOSTS`]): the pinned key(s)
/// under the alias — what `arena ssh-config --out` writes beside the config.
pub fn render_laptop_known_hosts(alias: &str, keys: &[(String, String)]) -> String {
    render_known_hosts(alias, None, keys)
}

/// What `archive up` says when the archive's pinned host key changed — a new server after a loss
/// (Hetzner may give it the same IPv4), or a re-pin: who holds the old pin and how each gets the
/// new one. `None` when nothing was pinned before (a first `up`) or the key is the same.
pub fn host_key_change_notice(alias: &str, ip: &str, before: &str, after: &str, fingerprint: Option<&str>) -> Option<String> {
    let (old, new) = (parse_known_hosts(before), parse_known_hosts(after));
    if old.is_empty() || new.is_empty() || old == new {
        return None;
    }
    let same_ip = before
        .lines()
        .filter(|l| !l.trim_start().starts_with(['#', '@']))
        .filter_map(|l| l.split_whitespace().next())
        .any(|names| names.split(',').any(|n| n == ip));
    let where_ = if same_ip { format!("the new server kept the IPv4 {ip}") } else { format!("the server is now at {ip}") };
    let fp = fingerprint.map(|f| format!(" (now {f})")).unwrap_or_default();
    Some(format!(
        "⚠ The archive's host key changed{fp} — {where_}. Whoever pinned the old one is refused until re-pinned:\n\
         \x20 pods:    `arena pods copy-keys --all` gives them the new pin (their `sftp {alias}` refuses it until then)\n\
         \x20 laptops: hand out the new `arena ssh-config` (with `--out`: the config and its arena_archive_known_hosts file),\n\
         \x20          or have them run `ssh-keygen -R {alias} -f {LAPTOP_KNOWN_HOSTS}`; a block from before kept the key in\n\
         \x20          ~/.ssh/known_hosts: `ssh-keygen -R {ip}` there"
    ))
}

/// Where a pod keeps the archive's read key and pinned host key (`copy-keys` writes both).
pub const POD_KEY: &str = ".ssh/arena_archive_key";
pub const POD_KNOWN_HOSTS: &str = ".ssh/arena_archive_known_hosts";

/// A pod's block, inside its managed `~/.ssh/config` block: the read key, the pinned host key.
pub fn render_pod_block(alias: &str, host: &str) -> String {
    crate::sshconfig::render_archive_block(alias, host, super::READ_USER, &format!("~/{POD_KEY}"), Some(&format!("~/{POD_KNOWN_HOSTS}")), "yes", None)
}

/// known_hosts lines for the pinned key(s): under the IP, and under `ARCHIVE_HOST` if set.
pub fn render_known_hosts(ip: &str, host: Option<&str>, keys: &[(String, String)]) -> String {
    let mut s = String::new();
    for name in std::iter::once(ip).chain(host.filter(|h| *h != ip)) {
        for (ty, blob) in keys {
            s.push_str(&format!("{name} {ty} {blob}\n"));
        }
    }
    s
}

/// The `(type, blob)` of every host key in a known_hosts file (hashed host names or not;
/// `@revoked`/`@cert-authority` lines skipped).
pub fn parse_known_hosts(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('@')) {
        if let Some((ty, blob)) = key_type_and_blob(line) {
            let k = (ty.to_string(), blob.to_string());
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

/// `archive access on|off` on the VM: replace the `backups` key file atomically (sshd reads it at
/// each login — no reload); off also ends every open read session.
pub fn access_command(keys: &str, access_on: bool) -> String {
    let f = "/etc/ssh/arena-keys/backups";
    let mut c = format!(
        "set -e; t=$(mktemp {f}.XXXXXX); printf '%s' '{}' > \"$t\"; chown root:root \"$t\"; chmod 0644 \"$t\"; mv -f \"$t\" {f}",
        keys.replace('\'', "'\\''")
    );
    if !access_on {
        c.push_str("; pkill -u backups || true");
    }
    c
}

/// `archive down`'s best-effort quiesce (≤ 60 s): end read and push sessions, flush, unmount.
pub fn quiesce_command() -> String {
    let mounts: Vec<String> = Tier::ALL.iter().map(|t| t.view_mountpoint()).chain(Tier::ALL.iter().map(|t| t.mountpoint())).collect();
    format!("timeout 50 sh -c 'pkill -u backups; pkill -u arena-push; sync; umount {}'; true", mounts.join(" "))
}

/// `archive resize`'s filesystem step: make the kernel see the new size, grow ext4 online,
/// print the new size in bytes.
pub fn resize_command(device: &str, tier: Tier) -> String {
    format!(
        "set -e; d=$(readlink -f '{device}'); n=$(basename \"$d\"); \
         if [ -w /sys/class/block/$n/device/rescan ]; then echo 1 > /sys/class/block/$n/device/rescan; fi; \
         resize2fs '{device}' >/dev/null 2>&1 || resize2fs '{device}'; df -P -B1 '{}' | awk 'NR==2{{print $2}}'",
        tier.mountpoint()
    )
}

/// `archive forget --delete-archived` on the VM (as root, any access state): every directory of
/// `pods` on both volumes — `snap/<label>/<pod>/` of every label and `big/<pod>/` — listed with
/// its size (`item=<bytes>\t<dir>`) and measured (`dirs=`, `bytes=` — `du`, so a file counts once
/// even when other pods' copies share it, and such shared files stay for them) and, with
/// `delete`, removed (`deleted=`). Never deletes
/// while a volume isn't mounted (`unmounted_<tier>=1`): its mountpoint would show nothing while
/// the pod's data sits on the volume. Only plain pod names (one path component) are accepted.
pub fn forget_command(pods: &[String], delete: bool) -> Result<String, String> {
    let plain = |p: &String| !p.is_empty() && !p.starts_with('.') && p.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if pods.is_empty() || !pods.iter().all(plain) {
        return Err(format!("not plain pod names: {pods:?}"));
    }
    let quoted: Vec<String> = pods.iter().map(|p| format!("'{p}'")).collect();
    let mut c = format!(
        ": arena-archive-forget; set -u; um=''; for t in {tiers}; do mountpoint -q '{root}'/$t || {{ echo unmounted_$t=1; um=1; }}; done; \
         cd '{root}' || exit 3; set --; \
         for p in {pods}; do for d in snap/*/\"$p\" big/\"$p\"; do if [ -d \"$d\" ] && [ ! -L \"$d\" ]; then set -- \"$@\" \"$d\"; fi; done; done; \
         echo dirs=$#; if [ $# -gt 0 ]; then du -sb -- \"$@\" | sed 's/^/item=/'; echo bytes=$(du -scb -- \"$@\" | tail -n1 | cut -f1); \
         else echo bytes=0; fi; ",
        tiers = Tier::ALL.map(|t| t.as_str()).join(" "),
        root = super::PUSH_ROOT,
        pods = quoted.join(" "),
    );
    if delete {
        c.push_str(
            "if [ -n \"$um\" ]; then echo refused=unmounted; exit 4; fi; \
             if [ $# -gt 0 ]; then rm -rf --one-file-system -- \"$@\" || { echo deleted=failed; exit 5; }; fi; echo deleted=$#; ",
        );
    }
    c.push_str("echo forget_done=1");
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUB:&str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPushPushPushPushPushPushPushPushPushPus1 me@box";
    const COHORT: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICohortCohortCohortCohortCohortCohortCo2 arena9";
    const READ: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIReadReadReadReadReadReadReadReadReadRea3 read";
    const DEPLOY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDeployDeployDeployDeployDeployDeployDe4 arena_infra";

    /// S5: the forced command is exactly rrsync write-only/no-delete/no-lock under the push root.
    #[test]
    fn the_push_key_is_forced_into_write_only_rrsync() {
        let line = render_push_key_line("/usr/bin/rrsync", PUB).unwrap();
        assert_eq!(
            line,
            "restrict,command=\"/usr/bin/rrsync -munge -wo -no-del -no-lock /srv/arena/push\" ssh-ed25519 \
             AAAAC3NzaC1lZDI1NTE5AAAAIPushPushPushPushPushPushPushPushPushPus1 arena-archive-push\n"
        );
        assert!(render_push_key_line("x", "command=\"evil\" not-a-key").is_err());
        // A hostile comment can't add options.
        let sneaky = format!("{} \"\nssh-rsa AAAAB3Nza", PUB);
        let l = render_push_key_line("/usr/bin/rrsync", &sneaky).unwrap();
        assert_eq!(l.lines().count(), 1, "{l}");
    }

    /// S5: {cohort, read} or nothing; never the deploy/admin/extra keys.
    #[test]
    fn the_read_users_keys_are_the_cohort_and_read_keys_or_nothing() {
        let on = render_backups_keys(Some(COHORT), READ, true, &[DEPLOY.to_string()]).unwrap();
        let blobs: Vec<&str> = on.lines().filter_map(crate::archive::key_blob).collect();
        assert_eq!(blobs, [crate::archive::key_blob(COHORT).unwrap(), crate::archive::key_blob(READ).unwrap()]);
        assert!(on.lines().all(|l| l.starts_with("restrict ssh-ed25519 ")), "{on}");
        assert_eq!(render_backups_keys(Some(COHORT), READ, false, &[]).unwrap(), "");
        assert!(render_backups_keys(None, READ, true, &[]).unwrap_err().contains("SHARED_SSH_KEY_PATH"));
        // The cohort key being the deploy key (or the read key colliding) is refused.
        let e = render_backups_keys(Some(DEPLOY), READ, true, &[DEPLOY.to_string()]).unwrap_err();
        assert!(e.contains("refusing"), "{e}");
        assert!(render_backups_keys(Some(COHORT), READ, true, &[READ.into()]).is_err());
    }

    #[test]
    fn the_sshd_drop_in_is_exact() {
        let d = render_sshd_dropin();
        assert!(d.starts_with("# Managed by `arena archive up`"));
        let body: Vec<&str> = d.lines().skip(1).collect();
        assert_eq!(
            body,
            [
                "PasswordAuthentication no",
                "KbdInteractiveAuthentication no",
                "PermitRootLogin prohibit-password",
                "AllowUsers root arena-push backups",
                "ClientAliveInterval 60",
                "ClientAliveCountMax 3",
                "Match User backups",
                "    AuthorizedKeysFile /etc/ssh/arena-keys/%u",
                "    ChrootDirectory /srv/arena/view",
                "    ForceCommand internal-sftp -R -d /",
                "    PermitTTY no",
                "    DisableForwarding yes",
                "    PermitTunnel no",
                "Match User arena-push",
                "    AuthorizedKeysFile /etc/ssh/arena-keys/%u",
                "    PermitTTY no",
                "    DisableForwarding yes",
                "    PermitTunnel no",
            ]
        );
    }

    #[test]
    fn the_fstab_block_mounts_by_device_with_nofail_and_binds_read_only() {
        let f = render_fstab_block("/dev/disk/by-id/scsi-0HC_Volume_1", "/dev/disk/by-id/scsi-0HC_Volume_2");
        let lines: Vec<&str> = f.lines().collect();
        assert_eq!(lines.first(), Some(&"# BEGIN arena-archive"));
        assert_eq!(lines.last(), Some(&"# END arena-archive"));
        assert_eq!(lines[1], "/dev/disk/by-id/scsi-0HC_Volume_1 /srv/arena/push/snap ext4 defaults,nofail,noatime,x-systemd.device-timeout=30s 0 2");
        assert_eq!(lines[2], "/dev/disk/by-id/scsi-0HC_Volume_2 /srv/arena/push/big ext4 defaults,nofail,noatime,x-systemd.device-timeout=30s 0 2");
        assert_eq!(lines[3], "/srv/arena/push/snap /srv/arena/view/snap none bind,ro,nofail,x-systemd.requires-mounts-for=/srv/arena/push/snap 0 0");
        assert_eq!(lines[4], "/srv/arena/push/big /srv/arena/view/big none bind,ro,nofail,x-systemd.requires-mounts-for=/srv/arena/push/big 0 0");
        assert!(lines[1..5].iter().all(|l| l.contains("nofail")));
        assert!(plain_device("/dev/disk/by-id/scsi-0HC_Volume_1"));
        for bad in ["/dev/", "/etc/x", "/dev/x y", "/dev/x;rm"] {
            assert!(!plain_device(bad), "{bad}");
        }
    }

    #[test]
    fn the_readme_says_who_can_read_how_long_and_what_is_missing() {
        let r = render_readme(&ReadmeInputs {
            prefix: "arena9".into(),
            alias: "archive-arena9".into(),
            keep_until: "2027-01-31".into(),
            snap_max_size: "50M".into(),
            extra_excludes: vec!["*.ckpt".into()],
        });
        for needle in ["EVERYONE IN YOUR COHORT CAN READ EVERYTHING HERE", "Kept until: 2027-01-31", "sftp archive-arena9", ".bashrc", ".git/config", "*.ckpt", "w0d1", "autocommit", "/rsyncd-munged/", ".gitconfig"] {
            assert!(r.contains(needle), "{needle}");
        }
        let none = render_readme(&ReadmeInputs {
            prefix: "p".into(),
            alias: "archive-p".into(),
            keep_until: "no retention date set".into(),
            snap_max_size: "50M".into(),
            extra_excludes: vec![],
        });
        assert!(none.contains("Kept until: no retention date set"));
    }

    #[test]
    fn laptop_and_pod_blocks_and_known_hosts() {
        let l = render_laptop_block("archive-arena9", "192.0.2.1", "~/.ssh/arena9_key", Some("ED25519 SHA256:abc"));
        assert_eq!(
            l,
            "# ED25519 SHA256:abc — compare on first connect\n\
             # A rebuilt archive has a new host key: take the new block and its arena_archive_known_hosts file, or run\n\
             #   ssh-keygen -R archive-arena9 -f ~/.ssh/arena_archive_known_hosts\n# and compare the new fingerprint.\n\
             Host archive-arena9\n    HostName 192.0.2.1\n    User backups\n    \
             IdentityFile ~/.ssh/arena9_key\n    IdentitiesOnly yes\n    StrictHostKeyChecking accept-new\n    \
             UserKnownHostsFile ~/.ssh/arena_archive_known_hosts\n    HostKeyAlias archive-arena9\n"
        );
        let p = render_pod_block("archive-arena9", "archive.example.org");
        assert_eq!(
            p,
            "Host archive-arena9\n    HostName archive.example.org\n    User backups\n    IdentityFile ~/.ssh/arena_archive_key\n    \
             IdentitiesOnly yes\n    StrictHostKeyChecking yes\n    UserKnownHostsFile ~/.ssh/arena_archive_known_hosts\n"
        );
        let keys = vec![("ssh-ed25519".to_string(), "AAAAC3NzaC1lZDI1NTE5AAAAIHostHostHostHostHostHostHostHostHostHos5".to_string())];
        assert_eq!(
            render_known_hosts("192.0.2.1", Some("archive.example.org"), &keys),
            "192.0.2.1 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostHostHostHostHostHostHostHostHostHos5\n\
             archive.example.org ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostHostHostHostHostHostHostHostHostHos5\n"
        );
        assert_eq!(render_known_hosts("192.0.2.1", None, &keys).lines().count(), 1);
        let hashed = "|1|c2FsdA==|aGFzaA== ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostHostHostHostHostHostHostHostHostHos5\n# c\n@revoked * ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQ\n";
        assert_eq!(parse_known_hosts(hashed), keys);
        assert_eq!(render_laptop_known_hosts("archive-arena9", &keys), format!("archive-arena9 ssh-ed25519 {}\n", keys[0].1));
    }

    /// W7 live (A9): a rebuilt server kept its IPv4 with a new host key. `up` says so — the pods'
    /// fix and the laptops' (the alias-keyed file, and the old main-file pin under the IP) — and
    /// says nothing on a first pin or when the key is the same.
    #[test]
    fn a_changed_host_key_is_announced_with_each_fix() {
        let old = "2.28.196.210 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOldOldOldOldOldOldOldOldOldOldOldOldOld1\n";
        let new = "2.28.196.210 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINewNewNewNewNewNewNewNewNewNewNewNewNew1\n";
        let n = host_key_change_notice("archive-arena9", "2.28.196.210", old, new, Some("ED25519 SHA256:pXdR")).expect("a notice");
        for needle in [
            "host key changed (now ED25519 SHA256:pXdR)",
            "kept the IPv4 2.28.196.210",
            "`arena pods copy-keys --all`",
            "ssh-keygen -R archive-arena9 -f ~/.ssh/arena_archive_known_hosts",
            "`ssh-keygen -R 2.28.196.210`",
        ] {
            assert!(n.contains(needle), "{needle}: {n}");
        }
        let moved = host_key_change_notice("archive-arena9", "162.55.187.124", old, &new.replace("2.28.196.210", "162.55.187.124"), None).unwrap();
        assert!(moved.contains("now at 162.55.187.124"), "{moved}");
        assert_eq!(host_key_change_notice("archive-arena9", "2.28.196.210", "", new, None), None, "a first pin");
        assert_eq!(host_key_change_notice("archive-arena9", "2.28.196.210", old, old, None), None, "the same key");
    }

    /// S6: no command or script arena runs on the archive can create a filesystem, wipe a
    /// signature, write a raw device or edit a partition table.
    #[test]
    fn nothing_sent_to_the_archive_can_format_or_partition() {
        let inputs = SetupInputs {
            snap_dev: "/dev/disk/by-id/scsi-0HC_Volume_1".into(),
            big_dev: "/dev/disk/by-id/scsi-0HC_Volume_2".into(),
            push_keys: render_push_key_line(RRSYNC_PLACEHOLDER, PUB).unwrap(),
            backups_keys: String::new(),
            readme: render_readme(&ReadmeInputs { prefix: "p".into(), alias: "archive-p".into(), keep_until: "x".into(), snap_max_size: "50M".into(), extra_excludes: vec![] }),
            setup_version: 1,
        };
        let texts = [
            SETUP_SCRIPT.to_string(),
            PROBE_SCRIPT.to_string(),
            render_setup_inputs(&inputs),
            access_command("", false),
            access_command(COHORT, true),
            quiesce_command(),
            resize_command("/dev/disk/by-id/scsi-0HC_Volume_1", Tier::Snap),
        ];
        for text in &texts {
            let words: Vec<&str> = text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).collect();
            for bad in ["mkfs", "wipefs", "dd", "parted", "sfdisk", "fdisk", "mke2fs", "mkswap", "blkdiscard"] {
                assert!(!words.contains(&bad), "{bad} in {}", &text[..text.len().min(80)]);
            }
            assert!(!text.contains("mkfs"), "mkfs in {}", &text[..text.len().min(80)]);
        }
        // The only JSON `format` arena ever sends is the volume create's (cloud.rs, before its
        // tests); the rest of the module never names it.
        let cloud = include_str!("cloud.rs");
        let code = &cloud[..cloud.find("#[cfg(test)]\nmod tests").unwrap()];
        assert_eq!(code.matches("\"format\":").count(), 1, "only volume_body sends a format");
        for other in [include_str!("plan.rs"), include_str!("sync.rs"), include_str!("report.rs"), include_str!("state.rs")] {
            assert!(!other.contains("\"format\":"));
        }
    }

    /// S14's gate, on the real sshd (`sshd -T -f`, unprivileged, a throwaway host key): with the
    /// drop-in included first — before a cloud-init drop-in that turns passwords back on, and the
    /// distribution's own lines after it — each user gets exactly its settings (the `Match`
    /// blocks stay inside their file: root is unchanged), the same checks `setup.sh` makes.
    #[test]
    fn sshd_t_gives_each_user_its_settings_and_root_none_of_them() {
        let sshd = ["sshd", "/usr/sbin/sshd"].into_iter().find(|p| std::process::Command::new(p).arg("-V").output().is_ok());
        let Some(sshd) = sshd else {
            eprintln!("no sshd — skipping");
            return;
        };
        let d = std::env::temp_dir().join(format!("arena-archive-sshd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("conf.d")).unwrap();
        let key = d.join("hostkey");
        let ok = std::process::Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).status();
        if !ok.is_ok_and(|s| s.success()) {
            return;
        }
        std::fs::write(d.join("conf.d/10-arena-archive.conf"), render_sshd_dropin()).unwrap();
        std::fs::write(d.join("conf.d/50-cloud-init.conf"), "PasswordAuthentication yes\n").unwrap();
        std::fs::write(
            d.join("sshd_config"),
            format!(
                "Include {}/conf.d/*.conf\nKbdInteractiveAuthentication no\nUsePAM yes\nX11Forwarding yes\nPrintMotd no\n\
                 AcceptEnv LANG LC_*\nSubsystem sftp /usr/lib/openssh/sftp-server\nHostKey {}\n",
                d.display(),
                key.display()
            ),
        )
        .unwrap();
        let t = |user: &str| {
            let o = std::process::Command::new(sshd).arg("-T").arg("-f").arg(d.join("sshd_config")).arg("-C").arg(format!("user={user},host=x,addr=192.0.2.1")).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).to_lowercase()
        };
        let has = |out: &str, line: &str| out.lines().any(|l| l.trim() == line);
        let backups = t("backups");
        for l in ["chrootdirectory /srv/arena/view", "forcecommand internal-sftp -r -d /", "permittty no", "authorizedkeysfile /etc/ssh/arena-keys/%u", "disableforwarding yes", "passwordauthentication no"] {
            assert!(has(&backups, l), "backups: {l}");
        }
        let push = t("arena-push");
        for l in ["authorizedkeysfile /etc/ssh/arena-keys/%u", "permittty no", "disableforwarding yes", "chrootdirectory none"] {
            assert!(has(&push, l), "arena-push: {l}");
        }
        let root = t("root");
        for l in ["chrootdirectory none", "passwordauthentication no", "forcecommand none", "permittty yes"] {
            assert!(has(&root, l), "root: {l}");
        }
        assert!(!has(&root, "authorizedkeysfile /etc/ssh/arena-keys/%u"));
        // The same lines setup.sh checks.
        for l in ["chrootdirectory /srv/arena/view", "forcecommand internal-sftp -R -d /", "authorizedkeysfile /etc/ssh/arena-keys/%u", "chrootdirectory none", "forcecommand none"] {
            assert!(SETUP_SCRIPT.contains(&format!("\"{l}\"")), "setup.sh checks {l}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The scripts are valid bash, and the commands decode back to inputs + script.
    #[test]
    fn the_embedded_scripts_parse_and_round_trip() {
        let has_bash = std::process::Command::new("bash").arg("-c").arg("true").status().is_ok_and(|s| s.success());
        if !has_bash {
            return;
        }
        for script in [SETUP_SCRIPT, PROBE_SCRIPT] {
            let o = std::process::Command::new("bash").args(["-n", "-c", script]).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
        let cmd = probe_command("/dev/a", "/dev/b");
        let b64 = cmd.split_whitespace().nth(4).unwrap();
        let decoded = String::from_utf8(crate::base64::decode(b64).unwrap()).unwrap();
        assert!(decoded.starts_with("ARENA_SNAP_DEV='/dev/a'\nARENA_BIG_DEV='/dev/b'\n#!/bin/bash"), "{}", &decoded[..80]);
        assert_eq!(sh_assign("X", "it's\nok"), "X='it'\\''s\nok'\n");
    }

    /// A2-3: `forget --delete-archived`'s VM command, run for real by `sh` on a fake push root:
    /// it measures and removes exactly the pod's (and its twins') dirs on both tiers — another
    /// pod's copy of a shared (hard-linked) file stays — and never deletes with a volume unmounted.
    #[cfg(unix)]
    #[test]
    fn forget_removes_only_that_pods_dirs_and_never_with_a_volume_unmounted() {
        let t = std::env::temp_dir().join(format!("arena-archive-forget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let root = t.join("push");
        let put = |rel: &str, body: &str| {
            let f = root.join(rel);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, body).unwrap();
        };
        put("snap/w0d1/devtest-bravo/b.py", "bravo's work\n");
        put("snap/w0d2/devtest-bravo-old/o.py", "old twin\n");
        put("snap/mylabel/devtest-bravo/c.py", "custom label\n");
        put("big/devtest-bravo/ckpt.pt", "weights\n");
        put("snap/w0d1/devtest-alpha/a.py", "alpha's\n");
        put("snap/w0d1/devtest-bravons/x", "a longer name, not bravo's\n");
        std::fs::hard_link(root.join("snap/w0d1/devtest-alpha/a.py"), root.join("snap/w0d1/devtest-bravo/shared.py")).unwrap();
        let bin = t.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let mounted = |yes: bool| {
            crate::remote::write_test_script(bin.join("mountpoint"), format!("#!/bin/sh\nexit {}\n", if yes { 0 } else { 1 })).unwrap();
        };
        let pods: Vec<String> = ["devtest-bravo", "devtest-bravo-old", "devtest-bravo-new"].map(String::from).to_vec();
        let run = |delete: bool| {
            let cmd = forget_command(&pods, delete).unwrap().replace(super::super::PUSH_ROOT, root.to_str().unwrap());
            let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
            let o = std::process::Command::new("sh").arg("-c").arg(&cmd).env("PATH", path).output().unwrap();
            crate::archive::report::parse_kv(&String::from_utf8_lossy(&o.stdout))
        };
        mounted(true);
        let m = run(false);
        assert_eq!(m.get("dirs").map(String::as_str), Some("4"), "{m:?}");
        // Each directory is listed with its size (A3-6), for the operator to see before typing.
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let cmd = forget_command(&pods, false).unwrap().replace(super::super::PUSH_ROOT, root.to_str().unwrap());
        let out = String::from_utf8(std::process::Command::new("sh").arg("-c").arg(&cmd).env("PATH", &path).output().unwrap().stdout).unwrap();
        let mut items: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("item=")).map(|l| l.split('\t').nth(1).unwrap()).collect();
        items.sort();
        assert_eq!(items, ["big/devtest-bravo", "snap/mylabel/devtest-bravo", "snap/w0d1/devtest-bravo", "snap/w0d2/devtest-bravo-old"], "{out}");
        assert!(m["bytes"].parse::<u64>().unwrap() > 0 && m.contains_key("forget_done"), "{m:?}");
        assert!(root.join("snap/w0d1/devtest-bravo").exists(), "measuring deletes nothing");
        // A volume not mounted: nothing deleted.
        mounted(false);
        let m = run(true);
        assert_eq!(m.get("refused").map(String::as_str), Some("unmounted"), "{m:?}");
        assert!(root.join("big/devtest-bravo/ckpt.pt").exists());
        mounted(true);
        let m = run(true);
        assert_eq!(m.get("deleted").map(String::as_str), Some("4"), "{m:?}");
        for gone in ["snap/w0d1/devtest-bravo", "snap/w0d2/devtest-bravo-old", "snap/mylabel/devtest-bravo", "big/devtest-bravo"] {
            assert!(!root.join(gone).exists(), "{gone}");
        }
        assert_eq!(std::fs::read_to_string(root.join("snap/w0d1/devtest-alpha/a.py")).unwrap(), "alpha's\n", "the shared file stays for alpha");
        assert!(root.join("snap/w0d1/devtest-bravons/x").exists(), "only exact names");
        assert_eq!(run(false).get("dirs").map(String::as_str), Some("0"));
        for bad in [vec!["../x".to_string()], vec![".".to_string()], vec!["a/b".to_string()], vec![String::new()], vec![]] {
            assert!(forget_command(&bad, true).is_err(), "{bad:?}");
        }
        let _ = std::fs::remove_dir_all(&t);
    }
}
