//! The participants' work on the persistent volume.
//!
//! Why this exists: a RunPod restart/stop resets the container disk to the image (live-
//! verified); only the persistent volume mounted at `/workspace` survives. The prebuilt image
//! keeps the ARENA repo at `/root/<repo>` — on the container disk — so a pod created *with*
//! `VOLUME_GB` still lost the participants' work on a restart, and the restart/stop gate (which
//! asks "is the repo on the volume?") never passed. This module, all pure:
//!
//! - [`relocation_command`]: the setup block that puts the repo ON the volume. When
//!   `/workspace` is a real mount, the repo lives at `/workspace/<repo dir name>` and the
//!   configured path (`BACKUP_REPO_PATH`) becomes a symlink to it. After a reset the volume copy
//!   is authoritative: the image's fresh checkout is moved aside (never deleted) and the link
//!   recreated. Without a volume it does nothing. Every git operation of setup (and of `pods
//!   backup`, `set-branch`, …) then acts on the volume copy through the link.
//! - [`probe_command`] / [`parse_probe`]: the read-only question "where does the repo really
//!   live on this pod?" (symlinks resolved, and is the volume mounted) — what the gate
//!   ([`RepoSite`]), `pods pull` ([`repo_pull`]) and `pods restore` act on.
//! - [`repo_pull`]: rsync `-a` copies a symlink *as a symlink*, so a home pull of a pod whose
//!   repo is on the volume would back up a dangling link and none of the work. The pull then
//!   excludes the link and pulls the repo tree from its real path into the same place in the
//!   backup, so the backup looks exactly as it did before the move.

use crate::provider::runpod_v2::VOLUME_MOUNT_PATH;

/// Lines a provisioning command prints on stdout starting with this become warnings on a pod
/// that is set up (`✓ name (warning: …)`) — the way the relocation says "the repo was NOT
/// moved, and why" without failing the pod's whole setup over it.
pub const WARNING_PREFIX: &str = "arena-warning: ";

/// Where the relocation takes its lock: container-local (a restart wipes it with the
/// container), outside the participants' volume.
const LOCK_FILE: &str = "/tmp/.arena-volume.lock";

/// Is `path` at or under the volume mount — lexically: absolute, and no `..` component (a path
/// that can't be judged counts as off the volume: the safe direction for the gate).
pub fn on_volume_path(path: &str) -> bool {
    let under = path == VOLUME_MOUNT_PATH
        || path.strip_prefix(VOLUME_MOUNT_PATH).is_some_and(|rest| rest.starts_with('/'));
    under && !path.split('/').any(|c| c == "..")
}

/// The volume copy's path for the repo at `repo`: `/workspace/<last path component>`. `None`
/// when the configured path can't be relocated: not absolute, has an empty/`.`/`..` component
/// past the root, is the root, or is already on the volume (configured there directly — then
/// there is nothing to move).
pub fn volume_repo_path(repo: &str) -> Option<String> {
    let trimmed = repo.trim_end_matches('/');
    if !trimmed.starts_with('/') || on_volume_path(trimmed) {
        return None;
    }
    let parts: Vec<&str> = trimmed[1..].split('/').collect();
    if parts.iter().any(|c| c.is_empty() || *c == "." || *c == "..") {
        return None;
    }
    let name = parts.last()?;
    Some(format!("{VOLUME_MOUNT_PATH}/{name}"))
}

/// The sh function `ismount <path>`: a real mount point? util-linux `mountpoint` when the
/// image has it, else the kernel's own mount table. Shared by the relocation and the probe so
/// both ask the same question.
const ISMOUNT_SH: &str = r#"ismount() { if command -v mountpoint >/dev/null 2>&1; then mountpoint -q "$1"; else awk -v m="$1" '$5 == m { f = 1 } END { exit !f }' /proc/self/mountinfo; fi; }"#;

/// The relocation script itself, for `repo` with the volume at `volume` and its lock at
/// `lock` — parameters so tests can run it against a temp dir with a fake mount. Callers on a
/// pod use [`relocation_command`]. `None` as for [`volume_repo_path`] (computed under
/// `volume` here).
///
/// Cases (`R` = the configured repo path, `D` = `<volume>/<name>`), all under one lock so two
/// setups can't interleave (a timed-out setup's copy keeps running on the pod after the ssh
/// client is gone — the next one waits for it instead of racing it):
/// - volume not mounted → nothing (unchanged behaviour without a volume);
/// - `R` already links to `D` (a git checkout) → nothing (idempotent);
/// - `R` a directory, `D` absent (a fresh volume) → copy `R` to `D.arena-partial`, rename it to
///   `D` only once complete (an interrupted copy is never mistaken for the volume copy), move
///   `R` aside to `R.arena-aside-<UTC time>` (the container disk, which the next reset wipes;
///   a rename, except on an overlay root that can't rename a directory from the image — there
///   `mv` copies it: the time and space of one more checkout), link `R` → `D`. Refused (a warning)
///   while a process works inside `R` — a shell or kernel there would keep writing into the
///   moved-aside copy;
/// - `R` a directory, `D` a git checkout (the container was reset, the volume kept the work)
///   → the volume copy is authoritative: `R` (the image's fresh checkout) is moved aside, never
///   deleted, `D` never overwritten, and the link recreated;
/// - `R` missing, `D` a checkout (an interrupted earlier run) → link it;
/// - anything else (`D` not a checkout, `R` a link elsewhere or dangling, `R` a file, a copy
///   that fails — e.g. the volume is full) → a warning ([`WARNING_PREFIX`]) and nothing
///   touched: the repo stays where it is and the restart gate stays closed.
///
/// Never fails setup on its own: every branch exits 0, problems are warnings. Plain POSIX sh
/// (it runs under `sh -c`, whatever the login shell is).
pub fn relocation_script(repo: &str, volume: &str, lock: &str) -> Option<String> {
    let repo = repo.trim_end_matches('/');
    let volume = volume.trim_end_matches('/');
    let name = volume_repo_path(repo)?.rsplit('/').next()?.to_string();
    if !volume.starts_with('/') || repo == volume || repo.starts_with(&format!("{volume}/")) {
        return None;
    }
    let script = r#"R=@R@ V=@V@ D=@D@ L=@L@
P="$D.arena-partial"
warn() { printf '%s%s\n' "@WARN@" "$*"; }
@ISMOUNT@
ismount "$V" 2>/dev/null || exit 0
if ( : >>"$L" ) 2>/dev/null; then exec 9>>"$L"; command -v flock >/dev/null 2>&1 && flock 9; fi
A="$R.arena-aside-$(date -u +%Y%m%dT%H%M%SZ)"
if [ -L "$R" ]; then
  T=$(readlink "$R")
  if [ "$T" = "$D" ] && [ -d "$D/.git" ]; then exit 0; fi
  warn "$R links to $T, not to a repo at $D - left as it is"
  exit 0
fi
if [ -d "$R" ]; then
  if [ -e "$D" ] || [ -L "$D" ]; then
    if [ -L "$D" ] || [ ! -d "$D/.git" ]; then
      warn "$D exists but isn't a git checkout - left alone; the repo stays on the container disk at $R"
      exit 0
    fi
    mv "$R" "$A" || { warn "couldn't move $R aside - the repo stays on the container disk"; exit 0; }
  else
    RP=$(cd "$R" 2>/dev/null && pwd -P) || RP="$R"
    busy=$(for p in /proc/[0-9]*; do c=$(readlink "$p/cwd" 2>/dev/null) || continue; case "$c" in "$R"|"$R"/*|"$RP"|"$RP"/*) printf ' %s' "${p#/proc/}";; esac; done)
    if [ -n "$busy" ]; then
      warn "repo not moved onto the $V volume: in use (working directory of pid$busy) - re-run setup when it is idle"
      exit 0
    fi
    rm -rf "$P"
    if ! cp -a "$R" "$P"; then
      rm -rf "$P"
      warn "couldn't copy $R onto the $V volume (full?) - the repo stays on the container disk"
      exit 0
    fi
    mv "$P" "$D" || { rm -rf "$P"; warn "couldn't put the copy at $D - the repo stays on the container disk"; exit 0; }
    mv "$R" "$A" || { warn "copied to $D but couldn't move $R aside - re-run setup"; exit 0; }
  fi
  ln -s "$D" "$R" || { mv "$A" "$R"; warn "couldn't link $R to $D - the repo stays on the container disk"; exit 0; }
  printf 'arena-volume: %s -> %s (what was at %s is at %s)\n' "$R" "$D" "$R" "$A"
  exit 0
fi
if [ -e "$R" ]; then
  warn "$R isn't a directory - left as it is"
  exit 0
fi
if [ -d "$D/.git" ] && [ ! -L "$D" ]; then
  ln -s "$D" "$R" && printf 'arena-volume: %s -> %s\n' "$R" "$D"
fi
exit 0
"#;
    Some(
        script
            .replace("@R@", &shell_quote(repo))
            .replace("@V@", &shell_quote(volume))
            .replace("@D@", &shell_quote(&format!("{volume}/{name}")))
            .replace("@L@", &shell_quote(lock))
            .replace("@WARN@", WARNING_PREFIX)
            .replace("@ISMOUNT@", ISMOUNT_SH),
    )
}

/// The setup step's block (run on the pod, before the repo update): [`relocation_script`] for
/// the real `/workspace`, under `sh -c` so the login shell's dialect can't change it. `None`
/// when the configured repo path can't be relocated (see [`volume_repo_path`]).
pub fn relocation_command(repo: &str) -> Option<String> {
    relocation_script(repo, VOLUME_MOUNT_PATH, LOCK_FILE).map(|s| format!("sh -c {}", shell_quote(&s)))
}

/// The read-only probe script for `repo`, against the volume at `volume` (a parameter for
/// tests). Prints `arena-repo-home=$HOME`, `arena-repo-real=<repo with every symlink
/// resolved>` (only when it is a directory), and `arena-repo-mount=yes|no`.
pub fn probe_script(repo: &str, volume: &str) -> String {
    format!(
        "{ISMOUNT_SH}\nR={r}; V={v}\nprintf 'arena-repo-home=%s\\n' \"$HOME\"\n\
         if [ -d \"$R\" ]; then printf 'arena-repo-real=%s\\n' \"$(readlink -f \"$R\")\"; fi\n\
         if ismount \"$V\" 2>/dev/null; then echo arena-repo-mount=yes; else echo arena-repo-mount=no; fi\n",
        r = shell_quote(repo.trim_end_matches('/')),
        v = shell_quote(volume),
    )
}

/// [`probe_script`] for the real `/workspace`, as one `sh -c` command.
pub fn probe_command(repo: &str) -> String {
    format!("sh -c {}", shell_quote(&probe_script(repo, VOLUME_MOUNT_PATH)))
}

/// What a pod said about its repo ([`probe_command`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoProbe {
    /// The login home (`$HOME`), when absolute.
    pub home: Option<String>,
    /// The repo's real path (symlinks resolved); `None` = no repo directory there.
    pub real: Option<String>,
    /// Whether `/workspace` is a real mount on the pod.
    pub volume_mounted: bool,
}

/// Parse [`probe_command`]'s output. `None` (= "the pod didn't say") unless the mount line is
/// there with `yes`/`no` — fail closed on anything unexpected. A `real` that isn't a clean
/// absolute path is dropped (unknown), never guessed at.
pub fn parse_probe(stdout: &str) -> Option<RepoProbe> {
    let (mut home, mut real, mut mount) = (None, None, None);
    for line in stdout.lines().map(|l| l.trim_end_matches('\r')) {
        if let Some(v) = line.strip_prefix("arena-repo-home=") {
            home = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("arena-repo-real=") {
            real = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("arena-repo-mount=") {
            mount = Some(match v {
                "yes" => true,
                "no" => false,
                _ => return None,
            });
        }
    }
    let clean = |p: String| (p.starts_with('/') && !p.split('/').any(|c| c == "..")).then_some(p);
    Some(RepoProbe { home: home.and_then(clean), real: real.and_then(clean), volume_mounted: mount? })
}

/// Where the restart/stop gate judges the participants' work (the repo) to be: the configured
/// path, refined by what the pod said when it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSite {
    /// `BACKUP_REPO_PATH` (default `/root/<ARENA_REPO_NAME>`).
    pub configured: String,
    /// The pod's answer: the repo's real path and whether the volume is mounted there.
    /// `None` = no answer (unreachable, or no repo directory) — judged by `configured`.
    pub on_pod: Option<(String, bool)>,
}

impl RepoSite {
    /// Judged by the configured path alone (the pod wasn't asked, or didn't answer).
    pub fn configured(repo: &str) -> Self {
        Self { configured: repo.to_string(), on_pod: None }
    }

    /// The configured path refined by the pod's `probe` (if any).
    pub fn resolve(repo: &str, probe: Option<&RepoProbe>) -> Self {
        let on_pod = probe.and_then(|p| p.real.clone().map(|real| (real, p.volume_mounted)));
        Self { configured: repo.to_string(), on_pod }
    }

    /// Does the repo sit on the volume — on a pod whose provider reports one at `/workspace`?
    /// With the pod's answer: its real path is under `/workspace` AND `/workspace` is mounted
    /// there. Without: the configured path is under `/workspace` (as before this check).
    pub fn on_volume(&self) -> bool {
        match &self.on_pod {
            Some((real, mounted)) => *mounted && on_volume_path(real),
            None => on_volume_path(&self.configured),
        }
    }

    /// The repo as a prompt names it: the configured path, plus where it really is when the pod
    /// said something different (`/root/ARENA_materials → /workspace/ARENA_materials on the
    /// pod`) or that the volume isn't mounted.
    pub fn describe(&self) -> String {
        match &self.on_pod {
            Some((real, mounted)) => {
                let same = real.trim_end_matches('/') == self.configured.trim_end_matches('/');
                let base = if same { self.configured.clone() } else { format!("{} → {real} on the pod", self.configured) };
                if !mounted && on_volume_path(real) {
                    format!("{base}, but {VOLUME_MOUNT_PATH} isn't mounted there")
                } else {
                    base
                }
            }
            None => self.configured.clone(),
        }
    }
}

/// The extra rsync job a home pull needs when the repo isn't a plain directory in the home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPull {
    /// The repo's path relative to the login home (`ARENA_materials`): excluded from the home
    /// job (anchored, `/ARENA_materials`) and the repo job's destination inside the backup.
    pub rel: String,
    /// The repo job's rsync source path on the pod (a directory, trailing `/` = its contents).
    pub source: String,
}

impl RepoPull {
    /// The home job's exclude for the link (anchored at the transfer root).
    pub fn home_exclude(&self) -> String {
        format!("/{}", self.rel)
    }
}

/// Plan the repo job for a home pull of a pod whose login user is `user`, with the configured
/// `repo`, given the pod's `probe` (`None` = the probe failed). Pure.
///
/// - the repo isn't under the home (can't be split out of a home pull) → `None`;
/// - the pod answered: no repo there, or its real path IS the configured one (a plain
///   directory — the home job copies it as before) → `None`; a different real path (a symlink,
///   e.g. onto the volume) → pull that path;
/// - no answer → pull `<rel>/` relative to the home: the trailing slash makes the pod's rsync
///   follow a symlink there, and a plain directory comes over the same — so the repo's tree is
///   captured whichever it is (a missing repo then fails that job, loudly).
pub fn repo_pull(repo: &str, user: &str, probe: Option<&RepoProbe>) -> Option<RepoPull> {
    let repo = repo.trim_end_matches('/');
    let default_home = if user == "root" { "/root".to_string() } else { format!("/home/{user}") };
    let home = probe.and_then(|p| p.home.clone()).unwrap_or(default_home);
    let rel = repo.strip_prefix(&format!("{}/", home.trim_end_matches('/')))?;
    if rel.is_empty() || rel.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return None;
    }
    let source = match probe {
        None => format!("{rel}/"),
        Some(RepoProbe { real: None, .. }) => return None,
        Some(RepoProbe { real: Some(real), .. }) if real.trim_end_matches('/') == repo => return None,
        Some(RepoProbe { real: Some(real), .. }) => format!("{}/", real.trim_end_matches('/')),
    };
    Some(RepoPull { rel: rel.to_string(), source })
}

/// One rsync job of a pod's pull, for one tier.
#[derive(Debug, Clone)]
pub struct PullJob {
    /// The repo job (its tree from the real path), as opposed to the home job.
    pub repo: bool,
    /// Local destination dir (trailing `/`).
    pub dest: String,
    pub config: crate::pull::PullConfig,
}

/// A tier's jobs for one pod: the home job into `dest` (`pc` as given) — and, when the repo is
/// split out ([`repo_pull`]), the home job excludes the link and a repo job pulls the real tree
/// into `<dest><rel>/` with the same filters, so the backup's layout is the same as for a repo
/// that is a plain directory (and `pods restore` puts it back through the link).
pub fn pull_jobs(pc: &crate::pull::PullConfig, dest: &str, split: Option<&RepoPull>) -> Vec<PullJob> {
    let Some(split) = split else {
        return vec![PullJob { repo: false, dest: dest.to_string(), config: pc.clone() }];
    };
    let mut home = pc.clone();
    home.excludes.push(split.home_exclude());
    let repo = crate::pull::PullConfig { remote_path: split.source.clone(), ..pc.clone() };
    vec![
        PullJob { repo: false, dest: dest.to_string(), config: home },
        PullJob { repo: true, dest: format!("{}/{}/", dest.trim_end_matches('/'), split.rel), config: repo },
    ]
}

/// The direct pod-to-pod copy behind `pods replace` / `migrate copy` (run ON the source pod,
/// pushing to the destination's endpoint) with the repo at `rel` (home-relative) carried as a
/// tree: the home without it, then — when the source has it — `$HOME/<rel>/` into `<rel>/`.
/// The trailing slashes make each side resolve a link there (the source's repo on its volume,
/// the destination's on its own) instead of copying or replacing the link itself: a plain home
/// copy would carry only the link, and the work would stay behind on the old pod's volume.
/// `rel = None` (the repo isn't under the home) = the plain home copy.
pub fn pod_to_pod_copy(
    dest_ip: &str,
    dest_port: u16,
    dest_user: &str,
    remote_key: &str,
    pc: &crate::pull::PullConfig,
    rel: Option<&str>,
) -> String {
    use crate::pull::{pod_to_pod_command, pod_to_pod_command_into, PullConfig};
    let Some(rel) = rel else {
        return pod_to_pod_command(dest_ip, dest_port, dest_user, remote_key, pc);
    };
    let mut home = pc.clone();
    home.excludes.push(format!("/{rel}"));
    let tree = PullConfig { remote_path: format!("{rel}/"), ..pc.clone() };
    format!(
        "{} && if [ -d \"$HOME\"/{} ]; then {}; fi",
        pod_to_pod_command(dest_ip, dest_port, dest_user, remote_key, &home),
        shell_quote(rel),
        pod_to_pod_command_into(dest_ip, dest_port, dest_user, remote_key, &tree, &format!("{rel}/")),
    )
}

/// The pushes of a via-local copy's staging dir (`stage`, trailing `/`) onto the destination:
/// `(local source, config)` per rsync. When the staged home holds the repo at `rel`
/// (`stage_has_repo`), it goes up as its own push into `<rel>/` (resolved through a link on
/// the destination) and the home push leaves it out — a plain push would replace the
/// destination's link with a directory on its container disk. Pure.
pub fn push_jobs(
    pc: &crate::pull::PullConfig,
    stage: &str,
    rel: Option<&str>,
    stage_has_repo: bool,
) -> Vec<(String, crate::pull::PullConfig)> {
    let stage = format!("{}/", stage.trim_end_matches('/'));
    match rel.filter(|_| stage_has_repo) {
        None => vec![(stage, pc.clone())],
        Some(rel) => {
            let mut home = pc.clone();
            home.excludes.push(format!("/{rel}"));
            let tree = crate::pull::PullConfig { remote_path: format!("{rel}/"), ..pc.clone() };
            vec![(stage.clone(), home), (format!("{stage}{rel}/"), tree)]
        }
    }
}

/// Single-quote for a POSIX `sh -c` string (`'\''` escaping).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A fake `ssh` for driving a *real* local rsync in tests: it drops ssh's options and the
/// host, then runs the remote command (rsync's `--server` side) inside `$FAKE_HOME` — so a
/// pull/push "to a pod" lands in a temp dir, through exactly the argv the CLI builds.
#[cfg(all(test, unix))]
pub(crate) mod fake_ssh {
    use std::path::{Path, PathBuf};

    /// Write the fake transport into `dir`; returns its path. `FAKE_HOME` (set on the rsync
    /// command) is the "pod"'s login home.
    pub fn install(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-ssh");
        std::fs::write(
            &path,
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in -p|-o|-i|-l) shift 2;; -*) shift;; *) break;; esac; done\n\
             shift\ncd \"$FAKE_HOME\" && exec sh -c \"$*\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// `args` (an rsync argv the CLI would run) with the `-e` transport swapped for the fake.
    pub fn swap_transport(args: &[String], fake: &Path) -> Vec<String> {
        let mut out = args.to_vec();
        let e = out.iter().position(|a| a == "-e").expect("an -e transport");
        out[e + 1] = fake.display().to_string();
        out
    }

    /// The fake installed as `ssh` in its own dir (for a command that runs `ssh` from PATH —
    /// the pod-to-pod copy); returns that dir, to put first on PATH.
    pub fn install_as_ssh(dir: &Path) -> PathBuf {
        let bin = dir.join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let _ = std::fs::remove_file(bin.join("ssh"));
        std::os::unix::fs::symlink(install(dir), bin.join("ssh")).unwrap();
        bin
    }

    /// Whether rsync is installed (the real-rsync tests skip without it).
    pub fn have_rsync() -> bool {
        std::process::Command::new("rsync").arg("--version").output().is_ok()
    }

    /// Run the real rsync with `args`, the "pod" being `home`; panics with stderr on failure.
    pub fn rsync(args: &[String], home: &Path) {
        let out = std::process::Command::new("rsync").args(args).env("FAKE_HOME", home).output().unwrap();
        assert!(out.status.success(), "rsync {args:?}:\n{}", String::from_utf8_lossy(&out.stderr));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_paths() {
        for (path, on) in [
            ("/workspace", true),
            ("/workspace/ARENA_materials", true),
            ("/workspace/a/../b", false),
            ("/workspacex/ARENA", false),
            ("/root/ARENA_materials", false),
            ("workspace/ARENA", false),
            ("", false),
        ] {
            assert_eq!(on_volume_path(path), on, "{path}");
        }
        // (configured repo path, its volume copy)
        let cases: &[(&str, Option<&str>)] = &[
            ("/root/ARENA_materials", Some("/workspace/ARENA_materials")),
            ("/root/ARENA_materials/", Some("/workspace/ARENA_materials")),
            ("/home/me/src/ARENA_3.0", Some("/workspace/ARENA_3.0")),
            ("/workspace/ARENA_materials", None), // configured on the volume: nothing to move
            ("/workspace", None),
            ("/", None),
            ("", None),
            ("ARENA_materials", None),
            ("~/ARENA_materials", None),
            ("/root/../etc", None),
            ("/root/./x", None),
            ("/root//x", None),
        ];
        for (repo, want) in cases {
            assert_eq!(volume_repo_path(repo).as_deref(), *want, "{repo}");
            assert_eq!(relocation_command(repo).is_some(), want.is_some(), "{repo}");
        }
    }

    #[test]
    fn relocation_command_is_quoted_sh_with_the_lock_and_never_deletes_the_checkouts() {
        let c = relocation_command("/root/it's").unwrap();
        assert!(c.starts_with("sh -c '"), "{c}");
        let s = relocation_script("/root/it's", "/workspace", LOCK_FILE).unwrap();
        assert!(s.starts_with(r"R='/root/it'\''s' V='/workspace' D='/workspace/it'\''s' L='/tmp/.arena-volume.lock'"), "{s}");
        assert!(s.contains("flock 9") && s.contains("mountpoint -q"), "{s}");
        // The only removals are of its own partial copy — never R, D or an aside dir.
        for line in s.lines().filter(|l| l.contains("rm ")) {
            assert!(line.contains(r#"rm -rf "$P""#), "unexpected removal: {line}");
        }
        assert!(!s.contains("--delete") && !s.contains("rm -rf \"$R") && !s.contains("rm -rf \"$D\""), "{s}");
    }

    #[test]
    fn probe_parses_or_fails_closed() {
        let p = |s: &str| parse_probe(s);
        assert_eq!(
            p("arena-repo-home=/root\narena-repo-real=/workspace/ARENA_materials\narena-repo-mount=yes\n"),
            Some(RepoProbe { home: Some("/root".into()), real: Some("/workspace/ARENA_materials".into()), volume_mounted: true })
        );
        // No repo directory: no real line.
        assert_eq!(
            p("arena-repo-home=/root\r\narena-repo-mount=no\r\n"),
            Some(RepoProbe { home: Some("/root".into()), real: None, volume_mounted: false })
        );
        // No mount line, or a garbled one: the pod didn't say.
        for bad in ["", "arena-repo-real=/workspace/x\n", "arena-repo-mount=maybe\n", "Welcome!\n"] {
            assert_eq!(p(bad), None, "{bad:?}");
        }
        // A real path that isn't clean is unknown, not trusted.
        for real in ["", "relative/x", "/workspace/../root/x"] {
            let got = p(&format!("arena-repo-real={real}\narena-repo-mount=yes\n")).unwrap();
            assert_eq!(got.real, None, "{real:?}");
        }
    }

    #[test]
    fn repo_site_judges_the_real_path_when_the_pod_answers() {
        const CFG: &str = "/root/ARENA_materials";
        let probe = |real: Option<&str>, mounted: bool| RepoProbe {
            home: Some("/root".into()),
            real: real.map(String::from),
            volume_mounted: mounted,
        };
        // (configured, the pod's answer, on the volume?, describe contains)
        let cases: &[(&str, Option<RepoProbe>, bool, &str)] = &[
            // the pod didn't answer: judged by config, as before
            (CFG, None, false, "/root/ARENA_materials"),
            ("/workspace/r", None, true, "/workspace/r"),
            // symlinked onto the mounted volume: survives
            (CFG, Some(probe(Some("/workspace/ARENA_materials"), true)), true, "/root/ARENA_materials → /workspace/ARENA_materials on the pod"),
            // …but not if the volume isn't mounted there
            (CFG, Some(probe(Some("/workspace/ARENA_materials"), false)), false, "isn't mounted there"),
            // a plain directory on the container disk
            (CFG, Some(probe(Some(CFG), true)), false, "/root/ARENA_materials"),
            // configured on the volume but the pod says it isn't mounted: stricter than config
            ("/workspace/r", Some(probe(Some("/workspace/r"), false)), false, "isn't mounted"),
            // no repo directory on the pod: judged by config
            (CFG, Some(probe(None, true)), false, "/root/ARENA_materials"),
        ];
        for (cfg, answer, on, says) in cases {
            let site = RepoSite::resolve(cfg, answer.as_ref());
            assert_eq!(site.on_volume(), *on, "{cfg} {answer:?}");
            assert!(site.describe().contains(says), "{cfg} {answer:?}: {}", site.describe());
        }
        assert_eq!(RepoSite::configured(CFG), RepoSite::resolve(CFG, None));
    }

    #[test]
    fn repo_pull_splits_out_a_symlinked_repo_only() {
        const CFG: &str = "/root/ARENA_materials";
        let probe = |real: Option<&str>| RepoProbe { home: Some("/root".into()), real: real.map(String::from), volume_mounted: true };
        let split = |rel: &str, source: &str| Some(RepoPull { rel: rel.into(), source: source.into() });
        // (repo, user, probe, plan)
        let cases: Vec<(&str, &str, Option<RepoProbe>, Option<RepoPull>)> = vec![
            // a plain directory in the home: the home job copies it, as before
            (CFG, "root", Some(probe(Some(CFG))), None),
            // symlinked onto the volume: exclude the link, pull the real tree
            (CFG, "root", Some(probe(Some("/workspace/ARENA_materials"))), split("ARENA_materials", "/workspace/ARENA_materials/")),
            // no repo on the pod
            (CFG, "root", Some(probe(None)), None),
            // the probe failed: pull it through the home path (follows a link if there is one)
            (CFG, "root", None, split("ARENA_materials", "ARENA_materials/")),
            ("/root/src/ARENA/", "root", None, split("src/ARENA", "src/ARENA/")),
            ("/home/ubuntu/ARENA", "ubuntu", None, split("ARENA", "ARENA/")),
            // a repo outside the home isn't part of a home pull
            ("/workspace/ARENA", "root", Some(probe(Some("/workspace/ARENA"))), None),
            ("/opt/ARENA", "root", None, None),
            ("/root", "root", None, None),
            ("/root/../etc", "root", None, None),
        ];
        for (repo, user, p, want) in cases {
            assert_eq!(repo_pull(repo, user, p.as_ref()), want, "{repo} {user} {p:?}");
        }
        // The home the pod reports wins over the guess from the user name.
        let other = RepoProbe { home: Some("/data/home".into()), real: Some("/workspace/r".into()), volume_mounted: true };
        assert_eq!(repo_pull("/data/home/r", "root", Some(&other)), split("r", "/workspace/r/"));
        assert_eq!(split("ARENA_materials", "x").unwrap().home_exclude(), "/ARENA_materials");
    }

    #[test]
    fn a_split_pull_excludes_the_link_and_pulls_the_tree_into_its_place() {
        use crate::pull::PullConfig;
        let pc = PullConfig::small_tier("50M");
        let one = pull_jobs(&pc, "/b/w1d1/devtest-a/", None);
        assert_eq!(one.len(), 1);
        assert_eq!((one[0].repo, one[0].dest.as_str(), one[0].config.excludes.clone()), (false, "/b/w1d1/devtest-a/", pc.excludes.clone()));
        let split = RepoPull { rel: "ARENA_materials".into(), source: "/workspace/ARENA_materials/".into() };
        let two = pull_jobs(&pc, "/b/w1d1/devtest-a/", Some(&split));
        assert_eq!(two.len(), 2);
        assert!(!two[0].repo && two[0].config.excludes.last().map(String::as_str) == Some("/ARENA_materials"));
        assert_eq!(two[0].config.remote_path, "", "the home");
        assert!(two[1].repo);
        assert_eq!(two[1].dest, "/b/w1d1/devtest-a/ARENA_materials/");
        assert_eq!(two[1].config.remote_path, "/workspace/ARENA_materials/");
        // Same tier filters on both (size cap, .git kept, caches dropped).
        assert_eq!((two[1].config.max_size.clone(), two[1].config.includes.clone()), (pc.max_size.clone(), pc.includes.clone()));
        assert_eq!(two[1].config.excludes, pc.excludes);
    }

    /// The pull's real rsync argv (the transport swapped for a local fake) against a pod whose
    /// repo is a symlink onto its volume: the old single home job backs up only the link; the
    /// split jobs back up the tree, in the same place, with `.git` and the tier's filters.
    #[cfg(unix)]
    #[test]
    fn the_backup_holds_the_repo_tree_not_the_link() {
        use crate::pull::{rsync_args, PullConfig};
        if !fake_ssh::have_rsync() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("arena-volume-pull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let put = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        put("pod/workspace/ARENA_materials/chapter1/work.py", "mine\n");
        put("pod/workspace/ARENA_materials/.git/HEAD", "ref: refs/heads/x\n");
        put("pod/workspace/ARENA_materials/.github/ci.yml", "dropped like any dotdir\n");
        put("pod/root/notes.txt", "notes\n");
        put("pod/root/ARENA_materials.arena-aside-20261008T000000Z/chapter1/work.py", "image\n");
        let (home, vol) = (root.join("pod/root"), root.join("pod/workspace/ARENA_materials"));
        std::os::unix::fs::symlink(&vol, home.join("ARENA_materials")).unwrap();
        let fake = fake_ssh::install(&root);
        let target = crate::ssh::SshTarget {
            user: "root".into(),
            host: "10.0.0.1".into(),
            port: 22,
            key_paths: vec![],
            connect_timeout_secs: 10,
        };
        let run = |jobs: &[PullJob]| {
            for job in jobs {
                std::fs::create_dir_all(&job.dest).unwrap();
                fake_ssh::rsync(&fake_ssh::swap_transport(&rsync_args(&target, &job.config, &job.dest), &fake), &home);
            }
        };
        let pc = PullConfig::small_tier("50M");
        // Before: one home job — the backup holds a dangling link, none of the work.
        let old = format!("{}/", root.join("old/w1d1/devtest-a").display());
        run(&pull_jobs(&pc, &old, None));
        assert!(std::fs::symlink_metadata(root.join("old/w1d1/devtest-a/ARENA_materials")).unwrap().file_type().is_symlink());
        // After: the pod says where the repo really is → the tree, where the repo was.
        let probe = RepoProbe { home: Some(home.display().to_string()), real: Some(vol.display().to_string()), volume_mounted: true };
        let split = repo_pull(&home.join("ARENA_materials").display().to_string(), "root", Some(&probe)).unwrap();
        let new = format!("{}/", root.join("new/w1d1/devtest-a").display());
        run(&pull_jobs(&pc, &new, Some(&split)));
        let b = root.join("new/w1d1/devtest-a");
        assert!(std::fs::symlink_metadata(b.join("ARENA_materials")).unwrap().is_dir(), "a real directory");
        assert_eq!(std::fs::read_to_string(b.join("ARENA_materials/chapter1/work.py")).unwrap(), "mine\n");
        assert!(b.join("ARENA_materials/.git/HEAD").is_file(), ".git kept");
        assert!(!b.join("ARENA_materials/.github").exists(), "dotdirs dropped as in a home pull");
        assert_eq!(std::fs::read_to_string(b.join("notes.txt")).unwrap(), "notes\n");
        assert!(!b.join("ARENA_materials.arena-aside-20261008T000000Z").exists(), "the image's moved-aside checkout isn't backed up");
        // The probe failed: the plan's home-relative source (`ARENA_materials/`) — the trailing
        // slash makes the pod's rsync follow the link — captures the same tree.
        let blind = RepoPull { rel: "ARENA_materials".into(), source: "ARENA_materials/".into() };
        let unprobed = format!("{}/", root.join("blind/w1d1/devtest-a").display());
        run(&pull_jobs(&pc, &unprobed, Some(&blind)));
        let b = root.join("blind/w1d1/devtest-a");
        assert!(std::fs::symlink_metadata(b.join("ARENA_materials")).unwrap().is_dir());
        assert_eq!(std::fs::read_to_string(b.join("ARENA_materials/chapter1/work.py")).unwrap(), "mine\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pod_copy_carries_the_repo_as_a_tree() {
        use crate::pull::PullConfig;
        let pc = PullConfig::replication();
        let plain = pod_to_pod_copy("5.6.7.8", 22042, "root", "/k", &pc, None);
        assert_eq!(plain, crate::pull::pod_to_pod_command("5.6.7.8", 22042, "root", "/k", &pc));
        let c = pod_to_pod_copy("5.6.7.8", 22042, "root", "/k", &pc, Some("ARENA_materials"));
        let (home, tree) = c.split_once(" && ").unwrap();
        assert!(home.contains("'--exclude' '/ARENA_materials'") && home.ends_with(" $HOME/ root@5.6.7.8:"), "{home}");
        assert!(tree.starts_with(r#"if [ -d "$HOME"/'ARENA_materials' ]; then rsync "#), "{tree}");
        assert!(tree.ends_with(" $HOME/ARENA_materials/ 'root@5.6.7.8:ARENA_materials/'; fi"), "{tree}");
        assert!(!c.contains("--delete"));
        // Via local staging: the staged repo goes up on its own, into its place.
        let one = push_jobs(&pc, "/stage", Some("ARENA_materials"), false);
        assert_eq!(one.len(), 1);
        assert_eq!((one[0].0.as_str(), one[0].1.remote_path.as_str()), ("/stage/", ""));
        let two = push_jobs(&pc, "/stage/", Some("ARENA_materials"), true);
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].0, "/stage/");
        assert_eq!(two[0].1.excludes.last().map(String::as_str), Some("/ARENA_materials"));
        assert_eq!((two[1].0.as_str(), two[1].1.remote_path.as_str()), ("/stage/ARENA_materials/", "ARENA_materials/"));
        assert_eq!(push_jobs(&pc, "/stage", None, true).len(), 1);
    }

    /// Both copies of `pods replace`, through the real rsync between two temp-dir "pods" whose
    /// repos are links onto their own volumes: the work lands in the destination's volume copy,
    /// its link stays a link, and nothing on it is deleted. (A plain push of the staged home
    /// — the old way — replaces the destination's link with a directory: shown too.)
    #[cfg(unix)]
    #[test]
    fn a_replacement_gets_the_work_into_its_own_volume_copy() {
        use crate::pull::{push_rsync_args, PullConfig};
        if !fake_ssh::have_rsync() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("arena-volume-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let put = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        let read = |rel: &str| std::fs::read_to_string(root.join(rel)).unwrap();
        let is_link = |rel: &str| std::fs::symlink_metadata(root.join(rel)).unwrap().file_type().is_symlink();
        // Two pods: `old` (the source) and `new` (the destination, already set up).
        let pod = |name: &str, work: &str| {
            put(&format!("{name}/ws/ARENA_materials/chapter1/work.py"), work);
            put(&format!("{name}/ws/ARENA_materials/.git/HEAD"), "ref\n");
            std::fs::create_dir_all(root.join(format!("{name}/home"))).unwrap();
            std::os::unix::fs::symlink(root.join(format!("{name}/ws/ARENA_materials")), root.join(format!("{name}/home/ARENA_materials"))).unwrap();
        };
        pod("old", "mine, after weeks of work\n");
        put("old/home/notes.txt", "notes\n");
        pod("new", "image\n");
        put("new/ws/ARENA_materials/only_new.py", "keep\n");
        let pc = PullConfig::replication();

        // Direct: the command, run on the "source" (HOME = its home), `ssh` = the fake.
        let bin = fake_ssh::install_as_ssh(&root);
        let cmd = pod_to_pod_copy("10.0.0.2", 22, "root", "/k", &pc, Some("ARENA_materials"));
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .env("HOME", root.join("old/home"))
            .env("PATH", path)
            .env("FAKE_HOME", root.join("new/home"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(is_link("new/home/ARENA_materials"), "the destination's link stays a link");
        assert_eq!(read("new/ws/ARENA_materials/chapter1/work.py"), "mine, after weeks of work\n");
        assert_eq!(read("new/ws/ARENA_materials/only_new.py"), "keep\n");
        assert_eq!(read("new/home/notes.txt"), "notes\n");

        // Via local staging: pull the source (split as `pods pull` does), push the stage.
        let fake = fake_ssh::install(&root);
        let target = crate::ssh::SshTarget { user: "root".into(), host: "10.0.0.2".into(), port: 22, key_paths: vec![], connect_timeout_secs: 10 };
        let probe = RepoProbe {
            home: Some(root.join("old/home").display().to_string()),
            real: Some(root.join("old/ws/ARENA_materials").display().to_string()),
            volume_mounted: true,
        };
        let split = repo_pull(&root.join("old/home/ARENA_materials").display().to_string(), "root", Some(&probe)).unwrap();
        let stage = format!("{}/", root.join("stage").display());
        for job in pull_jobs(&pc, &stage, Some(&split)) {
            std::fs::create_dir_all(&job.dest).unwrap();
            fake_ssh::rsync(&fake_ssh::swap_transport(&crate::pull::rsync_args(&target, &job.config, &job.dest), &fake), &root.join("old/home"));
        }
        assert!(!is_link("stage/ARENA_materials"), "staged as a tree");
        pod("via", "image\n");
        for (src, cfg) in push_jobs(&pc, &stage, Some("ARENA_materials"), true) {
            fake_ssh::rsync(&fake_ssh::swap_transport(&push_rsync_args(&target, &cfg, &src), &fake), &root.join("via/home"));
        }
        assert!(is_link("via/home/ARENA_materials"));
        assert_eq!(read("via/ws/ARENA_materials/chapter1/work.py"), "mine, after weeks of work\n");
        // The old single push of the stage: the link becomes a directory on the container disk.
        pod("plain", "image\n");
        fake_ssh::rsync(&fake_ssh::swap_transport(&push_rsync_args(&target, &pc, &stage), &fake), &root.join("plain/home"));
        assert!(!is_link("plain/home/ARENA_materials"), "replaced by a directory");
        assert_eq!(read("plain/ws/ARENA_materials/chapter1/work.py"), "image\n", "the volume copy never got the work");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The rendered relocation, run by a real `sh` against a temp-dir "pod": a fake home, a
    /// fake `/workspace`, and a stub `mountpoint` on PATH that says whether that dir is
    /// mounted. Each case is a state a real pod can be in.
    #[cfg(unix)]
    mod sim {
        use super::super::{probe_script, relocation_script, WARNING_PREFIX};
        use std::path::{Path, PathBuf};
        use std::process::Command;

        struct Pod {
            root: PathBuf,
        }

        impl Drop for Pod {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        impl Pod {
            /// A pod with the image's checkout at `home/ARENA_materials` (a `.git` and a file
            /// saying `image`), an empty `workspace/`, and the volume mounted or not.
            fn new(tag: &str, mounted: bool) -> Self {
                use std::os::unix::fs::PermissionsExt;
                let root = std::env::temp_dir().join(format!("arena-volume-sim-{}-{tag}", std::process::id()));
                let _ = std::fs::remove_dir_all(&root);
                for d in ["bin", "home/ARENA_materials/.git", "home/ARENA_materials/chapter1", "workspace"] {
                    std::fs::create_dir_all(root.join(d)).unwrap();
                }
                std::fs::write(root.join("home/ARENA_materials/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
                std::fs::write(root.join("home/ARENA_materials/chapter1/work.py"), "image\n").unwrap();
                let stub = root.join("bin/mountpoint");
                let ws = root.join("workspace");
                std::fs::write(&stub, format!("#!/bin/sh\n[ -e '{}' ] && [ \"$2\" = '{}' ]\n", root.join("mounted").display(), ws.display())).unwrap();
                std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
                let pod = Pod { root };
                if mounted {
                    std::fs::write(pod.root.join("mounted"), "").unwrap();
                }
                pod
            }

            fn p(&self, rel: &str) -> PathBuf {
                self.root.join(rel)
            }
            fn repo(&self) -> PathBuf {
                self.p("home/ARENA_materials")
            }
            fn vol_repo(&self) -> PathBuf {
                self.p("workspace/ARENA_materials")
            }

            fn sh(&self, script: &str) -> (String, String) {
                let path = format!("{}:{}", self.p("bin").display(), std::env::var("PATH").unwrap_or_default());
                let out = Command::new("sh").arg("-c").arg(script).env("PATH", path).env("HOME", self.p("home")).output().unwrap();
                let (stdout, stderr) =
                    (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
                assert!(out.status.success(), "exit {:?}\n{stdout}\n{stderr}", out.status.code());
                (stdout, stderr)
            }

            /// Run setup's relocation; returns stdout.
            fn relocate(&self) -> String {
                let script = relocation_script(
                    &self.repo().display().to_string(),
                    &self.p("workspace").display().to_string(),
                    &self.p("lock").display().to_string(),
                )
                .unwrap();
                self.sh(&script).0
            }

            fn read(&self, path: &Path) -> String {
                std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            }

            fn link_target(&self) -> Option<PathBuf> {
                std::fs::read_link(self.repo()).ok()
            }

            /// The `ARENA_materials.arena-aside-*` dirs next to the repo.
            fn asides(&self) -> Vec<PathBuf> {
                let mut v: Vec<PathBuf> = std::fs::read_dir(self.p("home"))
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("ARENA_materials.arena-aside-"))
                    .collect();
                v.sort();
                v
            }

            fn warnings(out: &str) -> Vec<&str> {
                out.lines().filter_map(|l| l.strip_prefix(WARNING_PREFIX)).collect()
            }
        }

        #[test]
        fn no_volume_changes_nothing() {
            let pod = Pod::new("novol", false);
            assert_eq!(pod.relocate(), "");
            assert!(pod.repo().is_dir() && pod.link_target().is_none(), "still a plain directory");
            assert!(!pod.vol_repo().exists() && pod.asides().is_empty());
        }

        #[test]
        fn a_fresh_volume_gets_the_repo_and_the_path_becomes_a_link() {
            let pod = Pod::new("fresh", true);
            let out = pod.relocate();
            assert!(Pod::warnings(&out).is_empty(), "{out}");
            assert_eq!(pod.link_target(), Some(pod.vol_repo()));
            // The whole tree, .git included, is on the volume — and reachable through the link.
            assert_eq!(pod.read(&pod.vol_repo().join("chapter1/work.py")), "image\n");
            assert!(pod.vol_repo().join(".git/HEAD").is_file());
            assert_eq!(pod.read(&pod.repo().join("chapter1/work.py")), "image\n");
            // The container-disk checkout was moved aside, not deleted; no partial copy left.
            let asides = pod.asides();
            assert_eq!(asides.len(), 1, "{asides:?}");
            assert_eq!(pod.read(&asides[0].join("chapter1/work.py")), "image\n");
            assert!(!pod.p("workspace/ARENA_materials.arena-partial").exists());
            // Idempotent: a second setup changes nothing.
            assert_eq!(pod.relocate(), "");
            assert_eq!((pod.link_target(), pod.asides().len()), (Some(pod.vol_repo()), 1));
        }

        #[test]
        fn after_a_reset_the_volume_copy_wins_and_the_fresh_checkout_is_kept_aside() {
            let pod = Pod::new("restarted", true);
            pod.relocate();
            // The participant works (through the link, i.e. on the volume)…
            std::fs::write(pod.repo().join("chapter1/work.py"), "mine\n").unwrap();
            // …then the container is reset: the home is the image again, the volume kept.
            std::fs::remove_dir_all(pod.p("home")).unwrap();
            std::fs::create_dir_all(pod.repo().join(".git")).unwrap();
            std::fs::create_dir_all(pod.repo().join("chapter1")).unwrap();
            std::fs::write(pod.repo().join("chapter1/work.py"), "image\n").unwrap();
            let out = pod.relocate();
            assert!(Pod::warnings(&out).is_empty(), "{out}");
            // The volume copy is authoritative and untouched; the link is back.
            assert_eq!(pod.link_target(), Some(pod.vol_repo()));
            assert_eq!(pod.read(&pod.vol_repo().join("chapter1/work.py")), "mine\n");
            assert_eq!(pod.read(&pod.repo().join("chapter1/work.py")), "mine\n");
            // The image's fresh checkout is moved aside — never deleted.
            let asides = pod.asides();
            assert_eq!(asides.len(), 1, "{asides:?}");
            assert_eq!(pod.read(&asides[0].join("chapter1/work.py")), "image\n");
            assert_eq!(pod.relocate(), "", "idempotent");
        }

        #[test]
        fn an_interrupted_run_is_finished_and_a_partial_copy_never_counts() {
            // A copy that died half-way (its partial dir left behind): redone from scratch.
            let pod = Pod::new("partial", true);
            std::fs::create_dir_all(pod.p("workspace/ARENA_materials.arena-partial/junk")).unwrap();
            pod.relocate();
            assert_eq!(pod.link_target(), Some(pod.vol_repo()));
            assert!(!pod.p("workspace/ARENA_materials.arena-partial").exists());
            assert!(!pod.vol_repo().join("junk").exists());
            // Died between moving the checkout aside and linking: the link is made.
            std::fs::remove_file(pod.repo()).unwrap();
            assert!(pod.relocate().contains("->"));
            assert_eq!(pod.link_target(), Some(pod.vol_repo()));
            // No repo anywhere (a non-arena image): nothing to do.
            let bare = Pod::new("bare", true);
            std::fs::remove_dir_all(bare.repo()).unwrap();
            assert_eq!(bare.relocate(), "");
            assert!(!bare.repo().exists() && !bare.vol_repo().exists());
        }

        #[test]
        fn anything_unexpected_is_a_warning_and_touches_nothing() {
            // A /workspace/ARENA_materials that isn't a checkout (not ours): left alone.
            let pod = Pod::new("notrepo", true);
            std::fs::create_dir_all(pod.vol_repo()).unwrap();
            std::fs::write(pod.vol_repo().join("theirs.txt"), "x").unwrap();
            let out = pod.relocate();
            assert!(Pod::warnings(&out).iter().any(|w| w.contains("isn't a git checkout")), "{out}");
            assert!(pod.repo().is_dir() && pod.link_target().is_none() && pod.asides().is_empty());
            assert_eq!(std::fs::read_dir(pod.vol_repo()).unwrap().count(), 1);

            // The repo path links somewhere else (or dangles): left as it is.
            let pod = Pod::new("dangling", true);
            std::fs::remove_dir_all(pod.repo()).unwrap();
            std::os::unix::fs::symlink(pod.vol_repo(), pod.repo()).unwrap(); // D missing
            let out = pod.relocate();
            assert!(Pod::warnings(&out).iter().any(|w| w.contains("not to a repo at")), "{out}");
            assert!(!pod.vol_repo().exists());

            // A participant's shell (or kernel) works inside the repo: not moved under it.
            let pod = Pod::new("busy", true);
            let mut sleeper = Command::new("sleep").arg("30").current_dir(pod.repo().join("chapter1")).spawn().unwrap();
            let out = pod.relocate();
            let _ = sleeper.kill();
            let _ = sleeper.wait();
            let w = Pod::warnings(&out);
            assert!(w.iter().any(|w| w.contains("in use") && w.contains(&sleeper.id().to_string())), "{out}");
            assert!(pod.repo().is_dir() && pod.link_target().is_none() && !pod.vol_repo().exists());
            // Idle again: moved.
            pod.relocate();
            assert_eq!(pod.link_target(), Some(pod.vol_repo()));
        }

        #[test]
        fn the_probe_reports_the_real_path_and_the_mount() {
            let pod = Pod::new("probe", true);
            let probe = |pod: &Pod| {
                let script = probe_script(&pod.repo().display().to_string(), &pod.p("workspace").display().to_string());
                super::super::parse_probe(&pod.sh(&script).0).expect("a parsable answer")
            };
            let before = probe(&pod);
            assert_eq!(before.real.as_deref(), Some(pod.repo().to_str().unwrap()));
            assert!(before.volume_mounted);
            assert_eq!(before.home.as_deref(), Some(pod.p("home").to_str().unwrap()));
            pod.relocate();
            let after = probe(&pod);
            assert_eq!(after.real.as_deref(), Some(pod.vol_repo().to_str().unwrap()), "symlink resolved");
            // No repo, no volume.
            let none = Pod::new("probe-none", false);
            std::fs::remove_dir_all(none.repo()).unwrap();
            let got = probe(&none);
            assert_eq!((got.real, got.volume_mounted), (None, false));
        }
    }
}
