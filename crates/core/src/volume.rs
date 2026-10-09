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

/// A file the container runtime creates when it creates the container — its mtime is when the
/// container disk was last reset (Docker writes `/.dockerenv` into each new container's init
/// layer; a restart of the same container keeps it). Anything in the image's checkout changed
/// after that time is work done since the reset, not the image's.
pub const CONTAINER_MARK: &str = "/.dockerenv";

/// The relocation's own budget (its setup step's timeout): a first move copies the whole repo
/// onto the volume — and, on an overlay root, a second time to put the original aside.
pub const RELOCATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// How long a relocation waits for another one (a timed-out setup's copy still running on the
/// pod) before it leaves the move to it.
const LOCK_WAIT_SECS: u64 = 120;

/// `REPO_ON_VOLUME`: setup's move of the repo onto the volume, on unless `0`/`false`/`no`/`off`;
/// absent or empty = on. An escape hatch for an operator who'd rather keep the repo where the
/// image has it (setup then neither moves it nor links a volume copy back after a reset). Anything
/// else is an error rather than a guess.
pub fn relocation_enabled(raw: Option<&str>) -> crate::error::Result<bool> {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("" | "1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(other) => Err(crate::error::Error::Config(format!("REPO_ON_VOLUME must be 1 or 0 (got `{other}`)"))),
    }
}

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

/// Where the relocation runs and its limits: [`RelocationSite::pod`] on a real pod; a temp dir
/// with a fake mount, mark and tiny limits in tests.
#[derive(Debug, Clone)]
pub struct RelocationSite {
    /// The volume's mount point (`/workspace`).
    pub volume: String,
    /// The lock file (container-local).
    pub lock: String,
    /// [`CONTAINER_MARK`].
    pub container_mark: String,
    /// The copy onto the volume gives up after this many seconds (`timeout`, when the pod has
    /// it), so the step ends inside its own budget instead of being cut off mid-move.
    pub copy_secs: u64,
    /// How long to wait for another setup's relocation.
    pub lock_wait_secs: u64,
}

impl RelocationSite {
    /// A real pod, the step's budget `budget`: the lock wait, then up to two copies of the repo
    /// (onto the volume, then — on an overlay root — the original aside), with a minute spare.
    pub fn pod(budget: std::time::Duration) -> Self {
        let copy_secs = (budget.as_secs().saturating_sub(LOCK_WAIT_SECS + 60) / 2).max(10);
        Self {
            volume: VOLUME_MOUNT_PATH.into(),
            lock: LOCK_FILE.into(),
            container_mark: CONTAINER_MARK.into(),
            copy_secs,
            lock_wait_secs: LOCK_WAIT_SECS,
        }
    }
}

/// The relocation script itself, for `repo` at `site` — parameters so tests can run it against
/// a temp dir with a fake mount. Callers on a pod use [`relocation_command`]. `None` as for
/// [`volume_repo_path`] (computed under the site's volume here).
///
/// Cases (`R` = the configured repo path, `D` = `<volume>/<name>`, `A` = `R.arena-aside-<UTC
/// time>`), all under one lock so two setups can't interleave (bounded: a timed-out setup's copy
/// can still be running on the pod after its ssh client is gone — the next one waits for it a
/// while, then leaves the move to it):
/// - volume not mounted → nothing (unchanged behaviour without a volume);
/// - `R` already links to `D` (a git checkout) → nothing (idempotent);
/// - `R` a directory, `D` absent (a fresh volume) → a move in an order where every step can be
///   undone and `D` only appears once `R` is out of the way (a `D` next to a real `R` is never
///   created — the state the reset case below must not mistake for "the container was reset"):
///   refused while the repo is in use (a process's working directory or open file inside it) or
///   while the volume or the container disk lacks the room; `cp -a R D.arena-partial` (time-
///   bounded); refused if anything in `R` changed during the copy (ctime against a mark made
///   just before it); `mv R A` (a rename, except on an overlay root that can't rename a
///   directory from the image — there `mv` copies it, and a save into `R` meanwhile would land
///   in neither copy); undone if anything was modified during that (mtimes, which `mv`'s copy
///   keeps, against the mark); `mv D.arena-partial D`; `ln -s D R`. Any failure puts `R` back
///   and drops the copy;
/// - `R` a directory, `D` a git checkout → the container was reset and the volume kept the work:
///   the volume copy is authoritative and `R`, the image's fresh checkout, is moved aside (never
///   deleted, `D` never overwritten) and the link recreated — but ONLY when `R` provably is that
///   untouched checkout: `git status` clean AND nothing in its work tree changed since the
///   container was created ([`CONTAINER_MARK`]; no mark → can't tell → refused). Otherwise —
///   participants worked in `R` before setup ran (a provider restart, a stop/start), or work
///   was left there by an earlier failure — both are left alone with a warning saying what to
///   reconcile: moving `R` aside would hide that work in a dir no pull backs up;
/// - `R` and `D` both missing, an aside dir there → a move interrupted between its renames:
///   the newest aside is put back at `R` and the move redone;
/// - `R` missing, `D` a checkout (an interrupted earlier run) → link it;
/// - anything else (`D` not a checkout, `R` a link elsewhere or dangling, `R` a file) → a
///   warning ([`WARNING_PREFIX`]) and nothing touched: the repo stays where it is and the
///   restart gate stays closed.
///
/// Never fails setup on its own: every branch exits 0, problems are warnings. Plain POSIX sh
/// (it runs under `sh -c`, whatever the login shell is) plus `find -lname/-cnewer`, `du`, `df`
/// (GNU/BusyBox-free fallbacks are refusals, never guesses).
pub fn relocation_script(repo: &str, site: &RelocationSite) -> Option<String> {
    let repo = repo.trim_end_matches('/');
    let volume = site.volume.trim_end_matches('/');
    let name = volume_repo_path(repo)?.rsplit('/').next()?.to_string();
    if !volume.starts_with('/') || repo == volume || repo.starts_with(&format!("{volume}/")) {
        return None;
    }
    let script = r#"R=@R@ V=@V@ D=@D@ L=@L@ C=@C@
P="$D.arena-partial" M="$L.mark" published=
warn() { printf '%s%s\n' "@WARN@" "$*"; }
@ISMOUNT@
ismount "$V" 2>/dev/null || exit 0
if ( : >>"$L" ) 2>/dev/null; then
  exec 9>>"$L"
  if command -v flock >/dev/null 2>&1 && ! flock -w @WAIT@ 9; then
    warn "another setup is still moving $R onto the $V volume - left to it; re-run setup once it is done"
    exit 0
  fi
fi
# (never an existing name: `mv` would move the repo INTO it)
A="$R.arena-aside-$(date -u +%Y%m%dT%H%M%SZ)"
if [ -e "$A" ] || [ -L "$A" ]; then A="$A-$$"; fi
glob() { printf '%s' "$1" | sed 's/[][*?\\]/\\&/g'; }
# The pids (not this one) with their working directory or an open file at or under $R (or
# its real path $RP); "?" when find can't answer (no -lname) - the caller refuses then.
busy() {
  find /proc/self/cwd -maxdepth 0 -lname '/*' 2>/dev/null | grep -q . || { echo '?'; return; }
  rg=$(glob "$R") pg=$(glob "$RP")
  find /proc/[0-9]*/cwd /proc/[0-9]*/fd -maxdepth 1 \( -lname "$rg" -o -lname "$rg/*" -o -lname "$pg" -o -lname "$pg/*" \) -print 2>/dev/null |
    sed -n 's#^/proc/\([0-9][0-9]*\)/.*#\1#p' | sort -u | grep -vx "$$" | tr '\n' ' ' | sed 's/ $//'
}
# Why $R isn't the image's untouched checkout (empty = it is). Untracked entries are left to
# the -cnewer scan below: the image itself ships untracked nested clones (arena-env:9.1 has 4
# exercise dirs that are their own git repos), so `status --porcelain` counting them would
# never call a pristine checkout untouched — and the volume copy would never be linked back
# after a reset. Anything a participant creates, edits or deletes after the container started
# is newer than $C and caught there (deletions via the parent dir's ctime).
changed() {
  [ -e "$C" ] || { echo "no $C to tell when the container was created"; return; }
  s=$(git -C "$R" --no-optional-locks status --porcelain --untracked-files=no 2>/dev/null) || { echo "a git status that fails"; return; }
  [ -z "$s" ] || { echo "uncommitted changes"; return; }
  rg=$(glob "$R")
  n=$(find "$R" -path "$rg/.git" -prune -o -cnewer "$C" -print 2>/dev/null) || { echo "files that can't be checked"; return; }
  [ -z "$n" ] || echo "files changed since the container was created"
}
# The paths under $1 (relative to it, sorted) modified after the mark $M: a move that changes
# this list had something written into the repo while it ran (files dated in the future, if
# any, are on both sides of the comparison).
after_mark() { find "$1" -newer "$M" -print 2>/dev/null | awk -v p="$1" '{ print substr($0, length(p) + 1) }' | sort; }
# Free KB on the filesystem holding $1.
free_kb() { df -Pk "$1" 2>/dev/null | awk 'NR == 2 { print $4 }'; }
num() { case "$1" in ''|*[!0-9]*) return 1;; esac; }
# Put a half-done fresh move back: $R from the aside, then (only then) drop our copy — the
# one at $D only if this run put it there.
undo() {
  if mv "$A" "$R"; then
    if [ -n "$published" ]; then mv "$D" "$P"; fi
    rm -rf "$P"
  else
    warn "couldn't put $A back at $R - the repo is at $A (and a copy at $D or $P): move it back by hand"
  fi
}
if [ ! -e "$R" ] && [ ! -L "$R" ] && [ ! -e "$D" ] && [ ! -L "$D" ]; then
  last=$(ls -1d "$R".arena-aside-* 2>/dev/null | tail -n 1)
  if [ -n "$last" ] && [ -d "$last" ] && [ ! -L "$last" ]; then
    mv "$last" "$R" || { warn "couldn't put $last back at $R - move it back by hand"; exit 0; }
    rm -rf "$P"
    printf 'arena-volume: %s put back at %s (an interrupted move)\n' "$last" "$R"
  fi
fi
if [ -L "$R" ]; then
  T=$(readlink "$R")
  if [ "$T" = "$D" ] && [ -d "$D/.git" ]; then exit 0; fi
  warn "$R links to $T, not to a repo at $D - left as it is"
  exit 0
fi
if [ -d "$R" ]; then
  RP=$(cd "$R" 2>/dev/null && pwd -P) || RP="$R"
  if [ -e "$D" ] || [ -L "$D" ]; then
    if [ -L "$D" ] || [ ! -d "$D/.git" ]; then
      warn "$D exists but isn't a git checkout - left alone; the repo stays on the container disk at $R"
      exit 0
    fi
    why=$(changed)
    if [ -n "$why" ]; then
      warn "both $R ($why) and the volume copy $D exist - neither touched: work in $R is on the container disk. Copy what's needed from $R into $D, move $R away, then re-run setup"
      exit 0
    fi
    b=$(busy)
    if [ -n "$b" ]; then
      warn "the volume copy $D wasn't linked back: $R is in use (pid $b) - re-run setup when it is idle"
      exit 0
    fi
    need=$(du -sk "$R" 2>/dev/null | awk 'NR == 1 { print $1 }') onr=$(free_kb "$(dirname "$R")")
    if ! num "$need" || ! num "$onr" || [ $((need + need / 10 + 102400)) -gt "$onr" ]; then
      warn "no room on the container disk to move $R aside (${need:-?} KB, ${onr:-?} KB free) - the volume copy $D isn't linked"
      exit 0
    fi
    if ! ( : >"$M" ) 2>/dev/null; then
      warn "couldn't write $M to time the move - the volume copy $D isn't linked"
      exit 0
    fi
    pre=$(after_mark "$R")
    mv "$R" "$A" || { warn "couldn't move $R aside - the volume copy $D isn't linked"; exit 0; }
    if [ "$(after_mark "$A")" != "$pre" ]; then
      mv "$A" "$R"
      warn "$R was written to while it was moved aside - put back, the volume copy $D isn't linked; re-run setup when it is idle"
      exit 0
    fi
    ln -s "$D" "$R" || { mv "$A" "$R"; warn "couldn't link $R to $D - the repo stays on the container disk"; exit 0; }
    printf 'arena-volume: %s -> %s (what was at %s is at %s)\n' "$R" "$D" "$R" "$A"
    exit 0
  fi
  b=$(busy)
  if [ -n "$b" ]; then
    warn "repo not moved onto the $V volume: in use (pid $b) - re-run setup when it is idle"
    exit 0
  fi
  need=$(du -sk "$R" 2>/dev/null | awk 'NR == 1 { print $1 }') onv=$(free_kb "$V") onr=$(free_kb "$(dirname "$R")")
  if ! num "$need" || ! num "$onv" || ! num "$onr"; then
    warn "couldn't measure $R or the free space - the repo stays on the container disk"
    exit 0
  fi
  want=$((need + need / 10 + 102400))
  if [ "$want" -gt "$onv" ]; then
    warn "no room on the $V volume for $R ($need KB, $onv KB free) - the repo stays on the container disk"
    exit 0
  fi
  if [ "$want" -gt "$onr" ]; then
    warn "no room on the container disk to move $R aside ($need KB, $onr KB free) - the repo stays on the container disk"
    exit 0
  fi
  rm -rf "$P"
  if ! ( : >"$M" ) 2>/dev/null; then
    warn "couldn't write $M to time the copy - the repo stays on the container disk"
    exit 0
  fi
  if command -v timeout >/dev/null 2>&1; then BOUND="timeout @COPY@"; else BOUND=; fi
  if ! $BOUND cp -a "$R" "$P"; then
    rm -rf "$P"
    warn "couldn't copy $R onto the $V volume within @COPY@ s (full? too big?) - the repo stays on the container disk"
    exit 0
  fi
  if ! n=$(find "$R" -cnewer "$M" -print 2>/dev/null) || [ -n "$n" ]; then
    rm -rf "$P"
    warn "$R changed while it was copied (in use?) - the repo stays on the container disk; re-run setup when it is idle"
    exit 0
  fi
  pre=$(after_mark "$R")
  if ! mv "$R" "$A"; then
    if [ -d "$R" ]; then rm -rf "$P"; fi
    warn "couldn't move $R aside - the repo stays on the container disk$([ -e "$A" ] && printf ' (a partial copy of it is at %s)' "$A")"
    exit 0
  fi
  if [ "$(after_mark "$A")" != "$pre" ]; then
    undo
    warn "$R changed while it was moved (in use?) - put back; re-run setup when it is idle"
    exit 0
  fi
  if ! mv "$P" "$D"; then
    undo
    warn "couldn't put the copy at $D - the repo stays on the container disk"
    exit 0
  fi
  published=1
  if ! ln -s "$D" "$R"; then
    undo
    warn "couldn't link $R to $D - the repo stays on the container disk"
    exit 0
  fi
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
            .replace("@L@", &shell_quote(&site.lock))
            .replace("@C@", &shell_quote(&site.container_mark))
            .replace("@WAIT@", &site.lock_wait_secs.to_string())
            .replace("@COPY@", &site.copy_secs.to_string())
            .replace("@WARN@", WARNING_PREFIX)
            .replace("@ISMOUNT@", ISMOUNT_SH),
    )
}

/// The relocation as setup runs it (its own step, before the repo update, within `budget`):
/// [`relocation_script`] for the real `/workspace`, under `sh -c` so the login shell's dialect
/// can't change it. `None` when the configured repo path can't be relocated (see
/// [`volume_repo_path`]).
pub fn relocation_command(repo: &str, budget: std::time::Duration) -> Option<String> {
    relocation_script(repo, &RelocationSite::pod(budget)).map(|s| format!("sh -c {}", shell_quote(&s)))
}

/// What the dry-run shows for the relocation step (its command is a long script).
pub fn relocation_summary(repo: &str) -> String {
    let dest = volume_repo_path(repo).unwrap_or_default();
    format!(
        "if {VOLUME_MOUNT_PATH} is a mounted volume: move {repo} onto it as {dest} (copy, check nothing changed, \
         keep the original as {repo}.arena-aside-<time>, link {repo} -> {dest}); after a reset, link the volume copy \
         back only if {repo} is the image's untouched checkout; anything else is a warning, never a failed setup"
    )
}

/// The config step's guard before it touches the repo: wait (bounded) for a relocation still
/// running on the pod — a timed-out relocation step's copy goes on after its ssh client is gone
/// — so the update doesn't act on a checkout that is half-way onto the volume.
pub fn relocation_wait_command() -> String {
    format!(
        "if [ -e {l} ] && command -v flock >/dev/null 2>&1; then flock -w {LOCK_WAIT_SECS} {l} true 2>/dev/null || \
         echo '{WARNING_PREFIX}another setup is still moving the repo onto the volume - the update ran anyway'; fi",
        l = shell_quote(LOCK_FILE),
    )
}

/// The read-only probe script for `repo`, against the volume at `volume` and the container mark
/// at `mark` (parameters for tests). Prints `arena-repo-home=$HOME`, `arena-repo-real=<repo with
/// every symlink resolved>` (only when it is a directory), `arena-repo-mount=yes|no`, and
/// `arena-repo-container=<UNIX time the container was created>` (only when the mark is there).
pub fn probe_script(repo: &str, volume: &str, mark: &str) -> String {
    format!(
        "{ISMOUNT_SH}\nR={r}; V={v}; C={c}\nprintf 'arena-repo-home=%s\\n' \"$HOME\"\n\
         if [ -d \"$R\" ]; then printf 'arena-repo-real=%s\\n' \"$(readlink -f \"$R\")\"; fi\n\
         if ismount \"$V\" 2>/dev/null; then echo arena-repo-mount=yes; else echo arena-repo-mount=no; fi\n\
         if [ -e \"$C\" ]; then t=$(date -r \"$C\" +%s 2>/dev/null) && printf 'arena-repo-container=%s\\n' \"$t\"; fi\n",
        r = shell_quote(repo.trim_end_matches('/')),
        v = shell_quote(volume),
        c = shell_quote(mark),
    )
}

/// [`probe_script`] for the real `/workspace` and [`CONTAINER_MARK`], as one `sh -c` command.
pub fn probe_command(repo: &str) -> String {
    format!("sh -c {}", shell_quote(&probe_script(repo, VOLUME_MOUNT_PATH, CONTAINER_MARK)))
}

/// What a pod said about its repo ([`probe_command`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoProbe {
    /// The login home (`$HOME`), when absolute.
    pub home: Option<String>,
    /// The repo's real path (symlinks resolved); `None` = no repo directory there.
    pub real: Option<String>,
    /// Whether `/workspace` is a real mount on the pod.
    pub volume_mounted: bool,
    /// When the pod's container (its disk) was created — the last reset (UNIX seconds);
    /// `None` = unknown. `pods restore` compares backups against it.
    pub container_created: Option<u64>,
}

/// Parse [`probe_command`]'s output. `None` (= "the pod didn't say") unless the mount line is
/// there with `yes`/`no` — fail closed on anything unexpected. A `real` that isn't a clean
/// absolute path is dropped (unknown), never guessed at; so is a container time that isn't a
/// number.
pub fn parse_probe(stdout: &str) -> Option<RepoProbe> {
    let (mut home, mut real, mut mount, mut created) = (None, None, None, None);
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
        } else if let Some(v) = line.strip_prefix("arena-repo-container=") {
            created = v.parse::<u64>().ok();
        }
    }
    let clean = |p: String| (p.starts_with('/') && !p.split('/').any(|c| c == "..")).then_some(p);
    Some(RepoProbe {
        home: home.and_then(clean),
        real: real.and_then(clean),
        volume_mounted: mount?,
        container_created: created,
    })
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

/// The extra rsync job a pull needs when the repo inside it isn't a plain directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPull {
    /// Where the repo's tree goes inside the backup (relative: `ARENA_materials`, or
    /// `root/ARENA_materials` for a `--remote-path /root` pull) — also the main job's exclude
    /// (anchored, `/ARENA_materials`), so its link isn't copied as well.
    pub rel: String,
    /// The repo job's rsync source path on the pod (a directory, trailing `/` = its contents).
    pub source: String,
    /// The pull's own source IS the repo (`--remote-path ARENA_materials`, no trailing slash):
    /// the repo job replaces the main job instead of joining it.
    pub whole: bool,
}

impl RepoPull {
    /// The main job's exclude for the link (anchored at the transfer root).
    pub fn home_exclude(&self) -> String {
        format!("/{}", self.rel)
    }
}

/// The pod's login home when it didn't say: `/root`, else `/home/<user>`.
fn default_home(user: &str) -> String {
    if user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{user}")
    }
}

/// Where a pull's remote path (`--remote-path` / `BACKUP_REMOTE_PATH`) points on the pod, as
/// rsync reads it: `(absolute dir, contents)` — `contents` = a trailing slash or `.` (or the
/// empty path, the home): the dir's contents land in the backup; without one the dir itself lands
/// there, as `<dest>/<its name>/`. Relative paths and `~`/`~/…` are the home's (rsync passes a
/// leading `~` to the pod's shell unescaped). `None` = can't tell (`..`, `$VAR` — which rsync
/// escapes, so the pod's shell never expands it —, a glob, `~user`): never guessed at.
pub fn resolve_remote_path(remote_path: &str, home: &str) -> Option<(String, bool)> {
    let home = home.trim_end_matches('/');
    if remote_path.is_empty() {
        return Some((home.to_string(), true));
    }
    // rsync names a source by its last component: `.` has none of its own, so it's the contents.
    let contents = remote_path.ends_with('/') || remote_path == "." || remote_path.ends_with("/.");
    let (base, rest) = if remote_path == "~" {
        (home, "")
    } else if let Some(rest) = remote_path.strip_prefix("~/") {
        (home, rest)
    } else if let Some(rest) = remote_path.strip_prefix('/') {
        ("", rest)
    } else {
        (home, remote_path)
    };
    if rest.contains(['$', '`', '*', '?', '[', '{', '\\', '~']) {
        return None;
    }
    let mut abs = base.to_string();
    for c in rest.split('/').filter(|c| !c.is_empty() && *c != ".") {
        if c == ".." {
            return None;
        }
        abs.push('/');
        abs.push_str(c);
    }
    if abs.is_empty() {
        abs.push('/');
    }
    Some((abs, contents))
}

/// Plan the repo job for a home pull of a pod whose login user is `user`, with the configured
/// `repo`, given the pod's `probe` (`None` = the probe failed): [`repo_pull_from`] with the
/// empty remote path. Pure.
pub fn repo_pull(repo: &str, user: &str, probe: Option<&RepoProbe>) -> Option<RepoPull> {
    repo_pull_from("", repo, user, probe).ok().flatten()
}

/// Plan the repo job for a pull of `remote_path` (empty = the home) from a pod whose login user
/// is `user`, with the configured `repo`, given the pod's `probe` (`None` = it didn't answer).
/// rsync `-a` copies a symlink as a symlink, so wherever the repo's link sits inside the pull,
/// the backup would hold only the link. Pure.
///
/// - the repo isn't inside the pull (elsewhere, or the pull is a dir inside the repo — the pod
///   resolves a link along the way) → `None`;
/// - the pull names the repo with a trailing slash → `None` (the pod's rsync follows the link);
/// - the pod answered: no repo there, or its real path IS the configured one (a plain directory
///   — the main job copies it as before) → `None`; a different real path (a symlink, e.g. onto
///   the volume) → pull that path into the repo's place;
/// - no answer → pull `<repo>/` (home-relative in a home pull): the trailing slash makes the
///   pod's rsync follow a symlink there, and a plain directory comes over the same — so the
///   repo's tree is captured whichever it is (a missing repo then fails that job, loudly).
///
/// `Err` (with why) when the remote path can't be judged ([`resolve_remote_path`]): the caller
/// pulls it as given and warns if the repo is a link.
pub fn repo_pull_from(remote_path: &str, repo: &str, user: &str, probe: Option<&RepoProbe>) -> Result<Option<RepoPull>, String> {
    let repo = repo.trim_end_matches('/');
    let home = probe.and_then(|p| p.home.clone()).unwrap_or_else(|| default_home(user));
    let Some((root, contents)) = resolve_remote_path(remote_path, &home) else {
        return Err(format!("can't tell where the remote path `{remote_path}` is on the pod"));
    };
    if !repo.starts_with('/') || repo.split('/').skip(1).any(|c| c.is_empty() || c == "." || c == "..") {
        return Ok(None);
    }
    let name = |p: &str| p.rsplit('/').next().unwrap_or_default().to_string();
    let (rel, whole) = if repo == root {
        if contents {
            return Ok(None);
        }
        (name(&root), true)
    } else {
        let Some(inner) = repo.strip_prefix(&format!("{}/", root.trim_end_matches('/'))) else {
            return Ok(None);
        };
        (if contents { inner.to_string() } else { format!("{}/{inner}", name(&root)) }, false)
    };
    if rel.is_empty() {
        return Ok(None);
    }
    let source = match probe {
        None if remote_path.is_empty() => format!("{rel}/"),
        None => format!("{repo}/"),
        Some(RepoProbe { real: None, .. }) => return Ok(None),
        Some(RepoProbe { real: Some(real), .. }) if real.trim_end_matches('/') == repo => return Ok(None),
        Some(RepoProbe { real: Some(real), .. }) => format!("{}/", real.trim_end_matches('/')),
    };
    Ok(Some(RepoPull { rel, source, whole }))
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

/// A tier's jobs for one pod: the main job into `dest` (`pc` as given) — and, when the repo is
/// split out ([`repo_pull_from`]), the main job excludes the link and a repo job pulls the real
/// tree into `<dest><rel>/` with the same filters, so the backup's layout is the same as for a
/// repo that is a plain directory (and `pods restore` puts it back through the link). A split
/// that is the whole pull (`whole`) is just the repo job.
pub fn pull_jobs(pc: &crate::pull::PullConfig, dest: &str, split: Option<&RepoPull>) -> Vec<PullJob> {
    let Some(split) = split else {
        return vec![PullJob { repo: false, dest: dest.to_string(), config: pc.clone() }];
    };
    let repo = PullJob {
        repo: true,
        dest: format!("{}/{}/", dest.trim_end_matches('/'), split.rel),
        config: crate::pull::PullConfig { remote_path: split.source.clone(), ..pc.clone() },
    };
    if split.whole {
        return vec![repo];
    }
    let mut home = pc.clone();
    home.excludes.push(split.home_exclude());
    vec![PullJob { repo: false, dest: dest.to_string(), config: home }, repo]
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
    use crate::pull::{pod_to_pod_command, pod_to_pod_command_into};
    let Some(rel) = rel else {
        return pod_to_pod_command(dest_ip, dest_port, dest_user, remote_key, pc);
    };
    let mut home = pc.clone();
    home.excludes.push(format!("/{rel}"));
    let tree = tree_config(pc, rel);
    format!(
        "{} && if [ -d \"$HOME\"/{} ]; then {}; fi",
        pod_to_pod_command(dest_ip, dest_port, dest_user, remote_key, &home),
        shell_quote(rel),
        pod_to_pod_command_into(dest_ip, dest_port, dest_user, remote_key, &tree, &format!("{rel}/")),
    )
}

/// The pushes of a via-local copy's staging dir (`stage`, trailing `/`) onto the destination:
/// `(local source, config)` per rsync. With the repo at `rel`, the home push ALWAYS leaves
/// `<rel>` out — as the direct copy does — and the repo goes up as its own push into `<rel>/`
/// (resolved through a link on the destination) only when it was staged as a directory
/// (`stage_has_repo`). A plain push would replace the destination's link with a directory on
/// its container disk; and when the source's repo is missing or a dangling link (staged as a
/// link), a mirroring home push would replace the destination's whole repo with that link —
/// its files moved aside, then tidied away (review finding). Then the destination's repo is
/// left alone, as the direct copy leaves it. Pure.
pub fn push_jobs(
    pc: &crate::pull::PullConfig,
    stage: &str,
    rel: Option<&str>,
    stage_has_repo: bool,
) -> Vec<(String, crate::pull::PullConfig)> {
    let stage = format!("{}/", stage.trim_end_matches('/'));
    let Some(rel) = rel else {
        return vec![(stage, pc.clone())];
    };
    let mut home = pc.clone();
    home.excludes.push(format!("/{rel}"));
    let mut jobs = vec![(stage.clone(), home)];
    if stage_has_repo {
        jobs.push((format!("{stage}{rel}/"), tree_config(pc, rel)));
    }
    jobs
}

/// The config for carrying the repo's tree into `<rel>/` on its own, from the home copy's:
/// the same filters, and — for a replica — its own `--backup-dir` beside the repo's real
/// directory ([`crate::replica::tree_backup_dir`]). A `rel` that can't name one gets no
/// mirror at all: a deletion is never made without somewhere to move the file to.
fn tree_config(pc: &crate::pull::PullConfig, rel: &str) -> crate::pull::PullConfig {
    use crate::pull::Mirror;
    let mirror = match &pc.mirror {
        Mirror::Replica { backup_dir } => match crate::replica::tree_backup_dir(backup_dir, rel) {
            Some(backup_dir) => Mirror::Replica { backup_dir },
            None => Mirror::Accumulate,
        },
        other => other.clone(),
    };
    crate::pull::PullConfig { remote_path: format!("{rel}/"), mirror, ..pc.clone() }
}

/// Single-quote for a POSIX `sh -c` string (`'\''` escaping).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A fake `ssh` for driving a *real* local rsync in tests: it drops ssh's options and the
/// host, then runs the remote command (rsync's `--server` side) inside `$FAKE_HOME`, which is
/// also its `$HOME` (a remote `~/` expands there) — so a pull/push "to a pod" lands in a temp
/// dir, through exactly the argv the CLI builds.
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
             shift\ncd \"$FAKE_HOME\" && HOME=\"$FAKE_HOME\" exec sh -c \"$*\"\n",
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
    fn the_relocation_is_on_unless_turned_off() {
        for (raw, want) in [(None, Some(true)), (Some(""), Some(true)), (Some("1"), Some(true)), (Some(" ON "), Some(true)), (Some("0"), Some(false)), (Some("off"), Some(false)), (Some("maybe"), None)] {
            assert_eq!(relocation_enabled(raw).ok(), want, "{raw:?}");
        }
        assert!(relocation_enabled(Some("2")).unwrap_err().to_string().contains("REPO_ON_VOLUME"));
    }

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
            assert_eq!(relocation_command(repo, RELOCATE_TIMEOUT).is_some(), want.is_some(), "{repo}");
        }
    }

    #[test]
    fn relocation_command_is_quoted_sh_with_the_lock_and_never_deletes_the_checkouts() {
        let c = relocation_command("/root/it's", RELOCATE_TIMEOUT).unwrap();
        assert!(c.starts_with("sh -c '"), "{c}");
        let site = RelocationSite::pod(RELOCATE_TIMEOUT);
        // 900 s: a 120 s lock wait, a minute spare, two copies of 360 s at most.
        assert_eq!((site.copy_secs, site.lock_wait_secs, site.container_mark.as_str()), (360, 120, "/.dockerenv"));
        assert_eq!(RelocationSite::pod(std::time::Duration::from_secs(5)).copy_secs, 10, "never zero");
        let s = relocation_script("/root/it's", &site).unwrap();
        assert!(
            s.starts_with(r"R='/root/it'\''s' V='/workspace' D='/workspace/it'\''s' L='/tmp/.arena-volume.lock' C='/.dockerenv'"),
            "{s}"
        );
        assert!(s.contains("flock -w 120 9") && s.contains("mountpoint -q") && s.contains("timeout 360"), "{s}");
        // The only removals are of its own copy (`$P`, or `$D` renamed back to `$P` by `undo`,
        // only after `$R` is back) — never R, D or an aside dir.
        for line in s.lines().filter(|l| l.contains("rm ")) {
            assert!(line.contains(r#"rm -rf "$P""#), "unexpected removal: {line}");
        }
        assert!(!s.contains("--delete") && !s.contains("rm -rf \"$R") && !s.contains("rm -rf \"$D\""), "{s}");
        // The config step's guard waits on the same lock, bounded.
        let w = relocation_wait_command();
        assert!(w.contains("flock -w 120 '/tmp/.arena-volume.lock' true") && w.contains(WARNING_PREFIX), "{w}");
        assert!(relocation_summary("/root/ARENA_materials").contains("/workspace/ARENA_materials"));
    }

    #[test]
    fn probe_parses_or_fails_closed() {
        let p = |s: &str| parse_probe(s);
        assert_eq!(
            p("arena-repo-home=/root\narena-repo-real=/workspace/ARENA_materials\narena-repo-mount=yes\narena-repo-container=1791400000\n"),
            Some(RepoProbe {
                home: Some("/root".into()),
                real: Some("/workspace/ARENA_materials".into()),
                volume_mounted: true,
                container_created: Some(1_791_400_000),
            })
        );
        // No repo directory: no real line; no container mark: no time.
        assert_eq!(
            p("arena-repo-home=/root\r\narena-repo-mount=no\r\n"),
            Some(RepoProbe { home: Some("/root".into()), real: None, volume_mounted: false, container_created: None })
        );
        // A container time that isn't a number is unknown.
        assert_eq!(p("arena-repo-mount=no\narena-repo-container=soon\n").unwrap().container_created, None);
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
            ..Default::default()
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
        let probe = |real: Option<&str>| RepoProbe { home: Some("/root".into()), real: real.map(String::from), volume_mounted: true, ..Default::default() };
        let split = |rel: &str, source: &str| Some(RepoPull { rel: rel.into(), source: source.into(), whole: false });
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
        let other = RepoProbe { home: Some("/data/home".into()), real: Some("/workspace/r".into()), volume_mounted: true, ..Default::default() };
        assert_eq!(repo_pull("/data/home/r", "root", Some(&other)), split("r", "/workspace/r/"));
        assert_eq!(split("ARENA_materials", "x").unwrap().home_exclude(), "/ARENA_materials");
    }

    #[test]
    fn remote_paths_resolve_as_rsync_reads_them_or_not_at_all() {
        // (remote path, home, resolved)
        let cases: &[(&str, &str, Option<(&str, bool)>)] = &[
            ("", "/root", Some(("/root", true))),
            ("~", "/root", Some(("/root", false))),
            ("~/", "/root/", Some(("/root", true))),
            ("~/ARENA_materials", "/root", Some(("/root/ARENA_materials", false))),
            ("ARENA_materials/", "/root", Some(("/root/ARENA_materials", true))),
            ("./ARENA_materials//results/", "/root", Some(("/root/ARENA_materials/results", true))),
            ("/root/", "/root", Some(("/root", true))),
            ("/root", "/root", Some(("/root", false))),
            ("/", "/root", Some(("/", true))),
            (".", "/home/u", Some(("/home/u", true))),
            ("ARENA_materials/.", "/root", Some(("/root/ARENA_materials", true))),
            // rsync escapes `$`, so the pod's shell never expands it; globs and `..` aren't judged
            ("$HOME/", "/root", None),
            ("${HOME}", "/root", None),
            ("~other/x", "/root", None),
            ("ARENA_*", "/root", None),
            ("x/../y", "/root", None),
        ];
        for (path, home, want) in cases {
            assert_eq!(resolve_remote_path(path, home), want.map(|(a, c)| (a.to_string(), c)), "{path:?}");
        }
    }

    /// `--remote-path` / `BACKUP_REMOTE_PATH` pulls get the same split as a home pull wherever
    /// the repo's link sits inside them.
    #[test]
    fn a_remote_path_pull_splits_the_repo_wherever_it_sits() {
        const CFG: &str = "/root/ARENA_materials";
        let linked = RepoProbe { home: Some("/root".into()), real: Some("/workspace/ARENA_materials".into()), volume_mounted: true, ..Default::default() };
        let plain = RepoProbe { real: Some(CFG.into()), ..linked.clone() };
        let pull = |rel: &str, source: &str, whole: bool| Ok(Some(RepoPull { rel: rel.into(), source: source.into(), whole }));
        // (remote path, probe, plan)
        let cases: Vec<(&str, Option<&RepoProbe>, Result<Option<RepoPull>, String>)> = vec![
            // the home, however it's spelled with its contents: the home split
            ("~/", Some(&linked), pull("ARENA_materials", "/workspace/ARENA_materials/", false)),
            ("/root/", Some(&linked), pull("ARENA_materials", "/workspace/ARENA_materials/", false)),
            (".", Some(&linked), pull("ARENA_materials", "/workspace/ARENA_materials/", false)),
            // the home dir itself (no slash): it lands as <dest>/root/, the repo with it
            ("/root", Some(&linked), pull("root/ARENA_materials", "/workspace/ARENA_materials/", false)),
            // the repo itself without a slash: rsync would copy the link alone
            ("ARENA_materials", Some(&linked), pull("ARENA_materials", "/workspace/ARENA_materials/", true)),
            ("/root/ARENA_materials", None, pull("ARENA_materials", "/root/ARENA_materials/", true)),
            // …with one: the pod's rsync follows the link
            ("ARENA_materials/", Some(&linked), Ok(None)),
            // inside the repo (the pod resolves the link on the way), or elsewhere
            ("ARENA_materials/results", Some(&linked), Ok(None)),
            ("/workspace/", Some(&linked), Ok(None)),
            ("/data", None, Ok(None)),
            // a plain directory: nothing to split
            ("/root", Some(&plain), Ok(None)),
            ("ARENA_materials", Some(&plain), Ok(None)),
            // the probe failed: through the configured path
            ("~/", None, pull("ARENA_materials", "/root/ARENA_materials/", false)),
        ];
        for (path, probe, want) in cases {
            assert_eq!(repo_pull_from(path, CFG, "root", probe), want, "{path:?} {probe:?}");
        }
        assert!(repo_pull_from("$HOME/", CFG, "root", Some(&linked)).unwrap_err().contains("can't tell"));
        // The whole-repo plan is one job, into <dest>/<name>/.
        let whole = RepoPull { rel: "ARENA_materials".into(), source: "/workspace/ARENA_materials/".into(), whole: true };
        let jobs = pull_jobs(&crate::pull::PullConfig::small_tier("50M"), "/b/w1d1/devtest-a/", Some(&whole));
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].repo);
        assert_eq!((jobs[0].dest.as_str(), jobs[0].config.remote_path.as_str()), ("/b/w1d1/devtest-a/ARENA_materials/", "/workspace/ARENA_materials/"));
    }

    #[test]
    fn a_split_pull_excludes_the_link_and_pulls_the_tree_into_its_place() {
        use crate::pull::PullConfig;
        let pc = PullConfig::small_tier("50M");
        let one = pull_jobs(&pc, "/b/w1d1/devtest-a/", None);
        assert_eq!(one.len(), 1);
        assert_eq!((one[0].repo, one[0].dest.as_str(), one[0].config.excludes.clone()), (false, "/b/w1d1/devtest-a/", pc.excludes.clone()));
        let split = RepoPull { rel: "ARENA_materials".into(), source: "/workspace/ARENA_materials/".into(), whole: false };
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
        let probe = RepoProbe {
            home: Some(home.display().to_string()),
            real: Some(vol.display().to_string()),
            volume_mounted: true,
            ..Default::default()
        };
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
        let blind = RepoPull { rel: "ARENA_materials".into(), source: "ARENA_materials/".into(), whole: false };
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
        assert_eq!(one[0].1.excludes.last().map(String::as_str), Some("/ARENA_materials"), "never the repo's place, staged or not");
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
            ..Default::default()
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

    /// A replica sync (live findings #7 and #20) through both copies, real rsync, the repo a
    /// link onto each pod's volume: a file the participant deleted on the original is gone
    /// from the new pod — in the home and in the repo — but MOVED aside, not destroyed: the
    /// home's into `~/arena-sync-kept/<stamp>/`, the repo's beside its real directory
    /// (on the volume, so the move is a rename). The tidy-up then finds both folders through
    /// the link. Never `--delete` on the source: it's only ever read.
    #[cfg(unix)]
    #[test]
    fn a_replica_copy_moves_what_it_deletes_beside_each_tree() {
        use crate::pull::{push_rsync_args, PullConfig};
        use crate::replica::{home_mirror, parse_settle, settle_command, SettleMode, KEPT_DIR};
        if !fake_ssh::have_rsync() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("arena-volume-replica-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let put = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        let exists = |rel: &str| root.join(rel).exists();
        let pod = |name: &str| {
            put(&format!("{name}/ws/ARENA_materials/chapter1/work.py"), "work\n");
            std::fs::create_dir_all(root.join(format!("{name}/home"))).unwrap();
            std::os::unix::fs::symlink(root.join(format!("{name}/ws/ARENA_materials")), root.join(format!("{name}/home/ARENA_materials"))).unwrap();
        };
        pod("old");
        put("old/home/notes.txt", "notes\n");
        let pc = PullConfig { mirror: home_mirror("S1"), ..PullConfig::replication() };
        let source_before: Vec<_> = walk(&root.join("old"));

        // Direct, run on the "source".
        pod("new");
        put("new/ws/ARENA_materials/deleted_on_original.py", "old copy\n");
        put("new/home/gone.txt", "old copy\n");
        put("new/home/.name", "export MACHINE_NAME='a-new'\n");
        let bin = fake_ssh::install_as_ssh(&root);
        let cmd = pod_to_pod_copy("10.0.0.2", 22, "root", "/k", &pc, Some("ARENA_materials"));
        assert!(cmd.contains("--delete") && cmd.contains("ConnectTimeout="), "{cmd}");
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
        assert!(!exists("new/ws/ARENA_materials/deleted_on_original.py") && !exists("new/home/gone.txt"));
        assert!(exists(&format!("new/ws/{KEPT_DIR}/S1/ARENA_materials/deleted_on_original.py")), "beside the real repo, on the volume");
        assert!(exists(&format!("new/home/{KEPT_DIR}/S1/gone.txt")));
        assert!(exists("new/home/ARENA_materials/chapter1/work.py") && exists("new/home/notes.txt"));
        assert_eq!(std::fs::read_to_string(root.join("new/home/.name")).unwrap(), "export MACHINE_NAME='a-new'\n", "~/.name stays the pod's own");
        assert_eq!(walk(&root.join("old")), source_before, "the source is only read");
        // The tidy-up finds both folders (no stamp: it keeps everything and says so), and
        // brings the repo's off the volume into the home's.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(settle_command(None, "S1", Some("ARENA_materials"), SettleMode::Keep))
            .env("HOME", root.join("new/home"))
            .output()
            .unwrap();
        let s = parse_settle(&String::from_utf8_lossy(&out.stdout)).unwrap();
        assert_eq!((s.kept, s.dirs.len()), (2, 1), "{s:?}");
        assert!(exists(&format!("new/home/{KEPT_DIR}/S1/ARENA_materials/deleted_on_original.py")));
        assert!(!exists(&format!("new/ws/{KEPT_DIR}")));

        // Via local staging: the push leg, the same way.
        pod("via");
        put("via/ws/ARENA_materials/deleted_on_original.py", "old copy\n");
        let stage = format!("{}/", root.join("stage").display());
        put("stage/notes.txt", "notes\n");
        put("stage/ARENA_materials/chapter1/work.py", "work\n");
        let fake = fake_ssh::install(&root);
        let target = crate::ssh::SshTarget { user: "root".into(), host: "10.0.0.2".into(), port: 22, key_paths: vec![], connect_timeout_secs: 10 };
        for (src, cfg) in push_jobs(&pc, &stage, Some("ARENA_materials"), true) {
            fake_ssh::rsync(&fake_ssh::swap_transport(&push_rsync_args(&target, &cfg, &src), &fake), &root.join("via/home"));
        }
        assert!(!exists("via/ws/ARENA_materials/deleted_on_original.py"));
        assert!(exists(&format!("via/ws/{KEPT_DIR}/S1/ARENA_materials/deleted_on_original.py")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Review finding: when the source's repo is missing or a dangling link (so it was staged
    /// as a link, not a tree), the via-local home push mirrored (`--delete`) WITHOUT leaving the
    /// repo's place out: the destination's repo directory was replaced by that link, its files
    /// moved aside and then tidied away. The direct copy skips the tree then (`[ -d ]`); the
    /// via-local one now leaves the destination's repo alone the same way.
    #[cfg(unix)]
    #[test]
    fn a_via_local_replica_never_replaces_the_destinations_repo_with_a_link() {
        use crate::pull::{push_rsync_args, PullConfig};
        if !fake_ssh::have_rsync() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("arena-volume-dangling-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let put = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        put("stage/notes.txt", "notes\n");
        std::os::unix::fs::symlink("/workspace/ARENA_materials_missing", root.join("stage/ARENA_materials")).unwrap();
        put("dest/ARENA_materials/solution.py", "the participant's work\n");
        let pc = PullConfig { mirror: crate::replica::home_mirror("S2"), ..PullConfig::replication() };
        let stage = format!("{}/", root.join("stage").display());
        let staged_repo = std::fs::symlink_metadata(root.join("stage/ARENA_materials")).is_ok_and(|m| m.is_dir());
        assert!(!staged_repo);
        let fake = fake_ssh::install(&root);
        let target = crate::ssh::SshTarget { user: "root".into(), host: "10.0.0.2".into(), port: 22, key_paths: vec![], connect_timeout_secs: 10 };
        for (src, cfg) in push_jobs(&pc, &stage, Some("ARENA_materials"), staged_repo) {
            fake_ssh::rsync(&fake_ssh::swap_transport(&push_rsync_args(&target, &cfg, &src), &fake), &root.join("dest"));
        }
        let repo = std::fs::symlink_metadata(root.join("dest/ARENA_materials")).unwrap();
        assert!(repo.is_dir(), "still the destination's own directory, not the source's dangling link");
        assert_eq!(std::fs::read_to_string(root.join("dest/ARENA_materials/solution.py")).unwrap(), "the participant's work\n");
        assert_eq!(std::fs::read_to_string(root.join("dest/notes.txt")).unwrap(), "notes\n", "the rest of the home still goes");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every file under `dir` with its contents, sorted — to show a tree is untouched.
    #[cfg(unix)]
    fn walk(dir: &std::path::Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let ft = e.file_type().unwrap();
                if ft.is_dir() {
                    stack.push(e.path());
                } else if ft.is_file() {
                    out.push((e.path().display().to_string(), std::fs::read_to_string(e.path()).unwrap_or_default()));
                }
            }
        }
        out.sort();
        out
    }

    /// `--remote-path` / `BACKUP_REMOTE_PATH` pulls through the real rsync (fake transport):
    /// the documented `~/`, the home spelled absolutely, the home dir itself, and the repo named
    /// without a trailing slash all used to back up the link alone; with the split they hold
    /// the tree, where the main job would have put it.
    #[cfg(unix)]
    #[test]
    fn a_remote_path_pull_holds_the_tree_not_the_link() {
        use crate::pull::{rsync_args, PullConfig};
        if !fake_ssh::have_rsync() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("arena-volume-rpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, vol) = (root.join("pod/root"), root.join("pod/workspace/ARENA_materials"));
        std::fs::create_dir_all(vol.join("chapter1")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(vol.join("chapter1/work.py"), "mine\n").unwrap();
        std::fs::write(home.join("notes.txt"), "notes\n").unwrap();
        std::os::unix::fs::symlink(&vol, home.join("ARENA_materials")).unwrap();
        let fake = fake_ssh::install(&root);
        let target = crate::ssh::SshTarget { user: "root".into(), host: "10.0.0.1".into(), port: 22, key_paths: vec![], connect_timeout_secs: 10 };
        let repo = home.join("ARENA_materials").display().to_string();
        let probe = RepoProbe { home: Some(home.display().to_string()), real: Some(vol.display().to_string()), volume_mounted: true, ..Default::default() };
        let home_abs = home.display().to_string();
        // (remote path, where the work lands in the backup)
        let cases = [
            ("~/".to_string(), "ARENA_materials/chapter1/work.py"),
            (format!("{home_abs}/"), "ARENA_materials/chapter1/work.py"),
            (home_abs.clone(), "root/ARENA_materials/chapter1/work.py"),
            ("ARENA_materials".to_string(), "ARENA_materials/chapter1/work.py"),
        ];
        for (i, (remote_path, lands)) in cases.iter().enumerate() {
            let pc = PullConfig { remote_path: remote_path.clone(), ..PullConfig::small_tier("50M") };
            let run = |dest: &std::path::Path, split: Option<&RepoPull>| {
                for job in pull_jobs(&pc, &format!("{}/", dest.display()), split) {
                    std::fs::create_dir_all(&job.dest).unwrap();
                    fake_ssh::rsync(&fake_ssh::swap_transport(&rsync_args(&target, &job.config, &job.dest), &fake), &home);
                }
            };
            // As given: the link alone.
            let old = root.join(format!("old{i}"));
            run(&old, None);
            let link = old.join(lands.split("/chapter1").next().unwrap());
            assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "{remote_path}: {}", link.display());
            // Split: the tree, in the same place.
            let split = repo_pull_from(remote_path, &repo, "root", Some(&probe)).unwrap().expect(remote_path);
            let new = root.join(format!("new{i}"));
            run(&new, Some(&split));
            assert_eq!(std::fs::read_to_string(new.join(lands)).unwrap(), "mine\n", "{remote_path}");
            let dir = new.join(lands.split("/chapter1").next().unwrap());
            assert!(std::fs::symlink_metadata(&dir).unwrap().is_dir(), "{remote_path}: a real directory");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The rendered relocation, run by a real `sh` against a temp-dir "pod": a fake home whose
    /// checkout is a real git repo, a fake `/workspace`, a stub `mountpoint` on PATH that says
    /// whether that dir is mounted, and a container mark made after the image's files. Stubs
    /// for `df`, `cp`, `mv` and `ln` play the failures. Each case is a state a real pod can be in.
    #[cfg(unix)]
    mod sim {
        use super::super::{probe_script, relocation_script, RelocationSite, WARNING_PREFIX};
        use std::path::{Path, PathBuf};
        use std::process::Command;

        struct Pod {
            root: PathBuf,
            copy_secs: u64,
        }

        impl Drop for Pod {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        fn have_git() -> bool {
            Command::new("git").arg("--version").output().is_ok()
        }

        /// Never the operator's git config; a fixed identity.
        fn isolated(cmd: &mut Command, home: &Path) {
            cmd.env("HOME", home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE");
        }

        impl Pod {
            /// A pod with the image's checkout at `home/ARENA_materials` (a git repo, committed
            /// and clean, `chapter1/work.py` saying `image`, `*.pt` ignored), an empty
            /// `workspace/`, the volume mounted or not, and the container mark made after all
            /// that. `None` without git.
            fn new(tag: &str, mounted: bool) -> Option<Self> {
                use std::os::unix::fs::PermissionsExt;
                if !have_git() {
                    eprintln!("git not installed — skipping");
                    return None;
                }
                let root = std::env::temp_dir().join(format!("arena-volume-sim-{}-{tag}", std::process::id()));
                let _ = std::fs::remove_dir_all(&root);
                for d in ["bin", "home", "workspace"] {
                    std::fs::create_dir_all(root.join(d)).unwrap();
                }
                let stub = root.join("bin/mountpoint");
                let ws = root.join("workspace");
                std::fs::write(&stub, format!("#!/bin/sh\n[ -e '{}' ] && [ \"$2\" = '{}' ]\n", root.join("mounted").display(), ws.display())).unwrap();
                std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
                let pod = Pod { root, copy_secs: 60 };
                pod.image_checkout();
                if mounted {
                    std::fs::write(pod.root.join("mounted"), "").unwrap();
                }
                Some(pod)
            }

            /// (Re)create the image's checkout at the repo path, then the container mark — as a
            /// fresh container from the image has them.
            fn image_checkout(&self) {
                let r = self.repo();
                std::fs::create_dir_all(r.join("chapter1")).unwrap();
                std::fs::write(r.join("chapter1/work.py"), "image\n").unwrap();
                std::fs::write(r.join(".gitignore"), "*.pt\n").unwrap();
                for args in [&["init", "-q", "-b", "main"][..], &["add", "-A"], &["commit", "-q", "-m", "image"]] {
                    self.git(&r, args);
                }
                // Like the real image (arena-env:9.1): an exercise dir that is its own git
                // clone, untracked in the parent checkout — present before the container starts.
                let nested = r.join("chapter4/shutdown_avoidance");
                std::fs::create_dir_all(&nested).unwrap();
                std::fs::write(nested.join("env.py"), "upstream\n").unwrap();
                for args in [&["init", "-q", "-b", "main"][..], &["add", "-A"], &["commit", "-q", "-m", "upstream"]] {
                    self.git(&nested, args);
                }
                std::fs::write(self.mark(), "").unwrap();
            }

            fn git(&self, dir: &Path, args: &[&str]) -> String {
                let mut c = Command::new("git");
                c.arg("-C").arg(dir).args(args);
                isolated(&mut c, &self.p("home"));
                let out = c.output().unwrap();
                assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }

            /// A stub for `name` first on PATH (`body` is the script after the shebang).
            fn stub(&self, name: &str, body: &str) {
                use std::os::unix::fs::PermissionsExt;
                let p = self.p(&format!("bin/{name}"));
                std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }

            fn unstub(&self, name: &str) {
                std::fs::remove_file(self.p(&format!("bin/{name}"))).unwrap();
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
            fn mark(&self) -> PathBuf {
                self.p("dockerenv")
            }

            fn sh(&self, script: &str) -> (String, String) {
                let path = format!("{}:{}", self.p("bin").display(), std::env::var("PATH").unwrap_or_default());
                let mut c = Command::new("sh");
                c.arg("-c").arg(script).env("PATH", path).current_dir(self.p("home"));
                isolated(&mut c, &self.p("home"));
                let out = c.output().unwrap();
                let (stdout, stderr) =
                    (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
                assert!(out.status.success(), "exit {:?}\n{stdout}\n{stderr}", out.status.code());
                (stdout, stderr)
            }

            fn site(&self) -> RelocationSite {
                RelocationSite {
                    volume: self.p("workspace").display().to_string(),
                    lock: self.p("lock").display().to_string(),
                    container_mark: self.mark().display().to_string(),
                    copy_secs: self.copy_secs,
                    lock_wait_secs: 1,
                }
            }

            /// Run setup's relocation; returns stdout.
            fn relocate(&self) -> String {
                let script = relocation_script(&self.repo().display().to_string(), &self.site()).unwrap();
                self.sh(&script).0
            }

            fn read(&self, path: &Path) -> String {
                std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            }

            fn write(&self, path: &Path, body: &str) {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, body).unwrap();
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

            /// No partial copy left on the volume.
            fn no_partial(&self) -> bool {
                !self.p("workspace/ARENA_materials.arena-partial").exists()
            }

            /// The repo is still the plain directory on the container disk, nothing on the volume.
            fn untouched(&self) -> bool {
                self.link_target().is_none() && self.repo().is_dir() && !self.vol_repo().exists() && self.no_partial()
            }

            /// The pod after a reset: the home is the image again (fresh checkout, new container
            /// mark), the volume kept.
            fn reset(&self) {
                std::thread::sleep(std::time::Duration::from_millis(30));
                std::fs::remove_dir_all(self.p("home")).unwrap();
                std::fs::create_dir_all(self.p("home")).unwrap();
                self.image_checkout();
            }

            /// Time passes past the mark's (coarse) clock tick, as it does between a reset and
            /// a participant's first save.
            fn later(&self) {
                std::thread::sleep(std::time::Duration::from_millis(30));
            }

            fn warnings(out: &str) -> Vec<&str> {
                out.lines().filter_map(|l| l.strip_prefix(WARNING_PREFIX)).collect()
            }
        }

        #[test]
        fn no_volume_changes_nothing() {
            let Some(pod) = Pod::new("novol", false) else { return };
            assert_eq!(pod.relocate(), "");
            assert!(pod.untouched() && pod.asides().is_empty());
        }

        #[test]
        fn a_fresh_volume_gets_the_repo_and_the_path_becomes_a_link() {
            let Some(pod) = Pod::new("fresh", true) else { return };
            let out = pod.relocate();
            assert!(Pod::warnings(&out).is_empty(), "{out}");
            assert_eq!(pod.link_target(), Some(pod.vol_repo()));
            // The whole tree, .git included, is on the volume — and reachable through the link.
            assert_eq!(pod.read(&pod.vol_repo().join("chapter1/work.py")), "image\n");
            assert!(pod.vol_repo().join(".git/HEAD").is_file());
            assert_eq!(pod.read(&pod.repo().join("chapter1/work.py")), "image\n");
            assert_eq!(
                pod.git(&pod.repo(), &["status", "--porcelain", "--untracked-files=no"]),
                "",
                "a working checkout through the link"
            );
            // The container-disk checkout was moved aside, not deleted; no partial copy left.
            let asides = pod.asides();
            assert_eq!(asides.len(), 1, "{asides:?}");
            assert_eq!(pod.read(&asides[0].join("chapter1/work.py")), "image\n");
            assert!(pod.no_partial());
            // Idempotent: a second setup changes nothing.
            assert_eq!(pod.relocate(), "");
            assert_eq!((pod.link_target(), pod.asides().len()), (Some(pod.vol_repo()), 1));
        }

        #[test]
        fn after_a_reset_the_volume_copy_wins_over_the_untouched_image_checkout() {
            let Some(pod) = Pod::new("restarted", true) else { return };
            pod.relocate();
            // The participant works (through the link, i.e. on the volume)…
            pod.write(&pod.repo().join("chapter1/work.py"), "mine\n");
            // …then the container is reset: the home is the image again, the volume kept.
            pod.reset();
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

        /// After a reset participants can work in the image's checkout before setup runs (a
        /// provider restart, a stop + start): that checkout isn't "the image's" any more, so the
        /// volume copy is NOT linked over it — any kind of change, git-visible or not.
        #[test]
        fn after_a_reset_work_in_the_image_checkout_is_never_moved_aside() {
            type Work = fn(&Pod);
            let cases: &[(&str, Work, &str)] = &[
                ("edit", |pod| pod.write(&pod.repo().join("chapter1/work.py"), "new work\n"), "uncommitted changes"),
                ("untracked", |pod| pod.write(&pod.repo().join("chapter1/exercise_solution.py"), "new\n"), "changed since the container was created"),
                // work inside an untracked nested clone (the image ships some) — also caught by the mark.
                ("nested", |pod| pod.write(&pod.repo().join("chapter4/shutdown_avoidance/mine.py"), "work\n"), "changed since the container was created"),
                // git doesn't see an ignored checkpoint — the container mark does.
                ("ignored", |pod| pod.write(&pod.repo().join("chapter1/model.pt"), "weights\n"), "changed since the container was created"),
                // committed (the */15 autocommit can run before setup): clean, but newer than the mark.
                ("committed", |pod| {
                    pod.write(&pod.repo().join("chapter1/work.py"), "committed work\n");
                    pod.git(&pod.repo(), &["commit", "-qam", "autocommit"]);
                }, "changed since the container was created"),
                // no mark: can't tell what the image had — refused.
                ("nomark", |pod| std::fs::remove_file(pod.mark()).unwrap(), "no "),
            ];
            for (tag, work, why) in cases {
                let Some(pod) = Pod::new(&format!("reset-{tag}"), true) else { return };
                pod.relocate();
                pod.write(&pod.repo().join("chapter1/work.py"), "on the volume\n");
                pod.reset();
                pod.later();
                work(&pod);
                let before = std::fs::read_dir(pod.repo().join("chapter1")).unwrap().count();
                let out = pod.relocate();
                let w = Pod::warnings(&out);
                assert!(w.iter().any(|w| w.contains(why) && w.contains("neither touched")), "{tag}: {out}");
                // Nothing moved: the participants' checkout stays where they work, the volume
                // copy as it was, no aside.
                assert!(pod.link_target().is_none() && pod.repo().is_dir() && pod.asides().is_empty(), "{tag}");
                assert_eq!(std::fs::read_dir(pod.repo().join("chapter1")).unwrap().count(), before, "{tag}");
                assert_eq!(pod.read(&pod.vol_repo().join("chapter1/work.py")), "on the volume\n", "{tag}");
            }
        }

        /// The reviewer's sequence: the old order published the volume copy before moving the
        /// checkout aside, so a failed `mv` left a stale volume copy next to the live checkout —
        /// and the re-run it asked for moved the participants' newer work aside. Now nothing is
        /// published until the checkout is out of the way.
        #[test]
        fn a_failed_move_aside_publishes_nothing_and_the_rerun_takes_the_newer_work() {
            let Some(pod) = Pod::new("mvfail", true) else { return };
            pod.stub("mv", r#"case "$2" in *.arena-aside-*) echo "mv: cannot move: No space left on device" >&2; exit 1;; esac; PATH=/usr/bin:/bin exec mv "$@""#);
            let out = pod.relocate();
            assert!(Pod::warnings(&out).iter().any(|w| w.contains("couldn't move")), "{out}");
            assert!(pod.untouched(), "no volume copy published, the copy dropped");
            pod.unstub("mv");
            // The participants keep working in the checkout…
            pod.write(&pod.repo().join("chapter1/exercise_solution.py"), "newer work\n");
            // …and the re-run moves THAT onto the volume.
            let out = pod.relocate();
            assert!(Pod::warnings(&out).is_empty(), "{out}");
            assert_eq!(pod.link_target(), Some(pod.vol_repo()));
            assert_eq!(pod.read(&pod.vol_repo().join("chapter1/exercise_solution.py")), "newer work\n");
        }

        /// Every later failure of the move puts the checkout back and drops the copy.
        #[test]
        fn a_failure_after_the_move_aside_puts_the_checkout_back() {
            let cases: &[(&str, &str, &str)] = &[
                ("ln", "ln", "exit 1"),
                ("publish", "mv", r#"case "$1" in *.arena-partial) exit 1;; esac; PATH=/usr/bin:/bin exec mv "$@""#),
            ];
            for (tag, tool, body) in cases {
                let Some(pod) = Pod::new(&format!("undo-{tag}"), true) else { return };
                pod.stub(tool, body);
                let out = pod.relocate();
                assert!(Pod::warnings(&out).iter().any(|w| w.contains("the repo stays on the container disk")), "{tag}: {out}");
                assert!(pod.untouched(), "{tag}");
                assert!(pod.asides().is_empty(), "{tag}: the checkout went back");
                assert_eq!(pod.read(&pod.repo().join("chapter1/work.py")), "image\n", "{tag}");
            }
        }

        /// Writes during the copy or the move: not moved (a save would otherwise land in the
        /// copy that is about to be hidden, or in neither).
        #[test]
        fn a_repo_written_while_it_moves_is_left_where_it_is() {
            // During the copy: `cp` copies, then a "participant" saves into the checkout.
            let Some(pod) = Pod::new("cpwrite", true) else { return };
            let file = pod.repo().join("chapter1/work.py");
            pod.stub("cp", &format!("PATH=/usr/bin:/bin cp \"$@\" && sleep 0.05 && echo saved >> '{}'", file.display()));
            let out = pod.relocate();
            assert!(Pod::warnings(&out).iter().any(|w| w.contains("changed while it was copied")), "{out}");
            assert!(pod.untouched() && pod.asides().is_empty());
            assert_eq!(pod.read(&file), "image\nsaved\n");
            // During the move aside: the save lands in what is being moved — put back.
            let Some(pod) = Pod::new("mvwrite", true) else { return };
            let file = pod.repo().join("chapter1/work.py");
            pod.stub(
                "mv",
                &format!(
                    "case \"$2\" in *.arena-aside-*) sleep 0.05; echo saved >> '{}';; esac; PATH=/usr/bin:/bin exec mv \"$@\"",
                    file.display()
                ),
            );
            let out = pod.relocate();
            assert!(Pod::warnings(&out).iter().any(|w| w.contains("changed while it was moved")), "{out}");
            assert!(pod.untouched() && pod.asides().is_empty(), "put back");
            assert_eq!(pod.read(&file), "image\nsaved\n");
            // After a reset, the same while the image's checkout is moved aside: put back, the
            // volume copy left as it was and not linked.
            let Some(pod) = Pod::new("resetmvwrite", true) else { return };
            pod.relocate();
            pod.write(&pod.repo().join("chapter1/work.py"), "on the volume\n");
            pod.reset();
            let file = pod.repo().join("chapter1/work.py");
            pod.stub(
                "mv",
                &format!(
                    "case \"$2\" in *.arena-aside-*) sleep 0.05; echo saved >> '{}';; esac; PATH=/usr/bin:/bin exec mv \"$@\"",
                    file.display()
                ),
            );
            let out = pod.relocate();
            assert!(Pod::warnings(&out).iter().any(|w| w.contains("written to while it was moved aside")), "{out}");
            assert!(pod.link_target().is_none() && pod.repo().is_dir() && pod.asides().is_empty(), "put back");
            assert_eq!(pod.read(&file), "image\nsaved\n");
            assert_eq!(pod.read(&pod.vol_repo().join("chapter1/work.py")), "on the volume\n");
        }

        #[test]
        fn an_interrupted_run_is_finished_and_a_partial_copy_never_counts() {
            // A copy that died half-way (its partial dir left behind): redone from scratch.
            let Some(pod) = Pod::new("partial", true) else { return };
            std::fs::create_dir_all(pod.p("workspace/ARENA_materials.arena-partial/junk")).unwrap();
            pod.relocate();
            assert_eq!(pod.link_target(), Some(pod.vol_repo()));
            assert!(pod.no_partial());
            assert!(!pod.vol_repo().join("junk").exists());
            // Died between moving the checkout aside and linking: the link is made.
            std::fs::remove_file(pod.repo()).unwrap();
            assert!(pod.relocate().contains("->"));
            assert_eq!(pod.link_target(), Some(pod.vol_repo()));
            // Died between moving the checkout aside and publishing the copy: the checkout is put
            // back and the move redone.
            let Some(pod) = Pod::new("between", true) else { return };
            let aside = pod.p("home/ARENA_materials.arena-aside-20261008T000000Z");
            std::fs::rename(pod.repo(), &aside).unwrap();
            std::fs::create_dir_all(pod.p("workspace/ARENA_materials.arena-partial")).unwrap();
            let out = pod.relocate();
            assert!(out.contains("put back at") && Pod::warnings(&out).is_empty(), "{out}");
            assert_eq!(pod.link_target(), Some(pod.vol_repo()));
            assert_eq!(pod.read(&pod.vol_repo().join("chapter1/work.py")), "image\n");
            assert!(pod.no_partial() && !aside.exists());
            // No repo anywhere (a non-arena image): nothing to do.
            let Some(bare) = Pod::new("bare", true) else { return };
            std::fs::remove_dir_all(bare.repo()).unwrap();
            assert_eq!(bare.relocate(), "");
            assert!(!bare.repo().exists() && !bare.vol_repo().exists());
        }

        #[test]
        fn anything_unexpected_is_a_warning_and_touches_nothing() {
            // A /workspace/ARENA_materials that isn't a checkout (not ours): left alone.
            let Some(pod) = Pod::new("notrepo", true) else { return };
            std::fs::create_dir_all(pod.vol_repo()).unwrap();
            std::fs::write(pod.vol_repo().join("theirs.txt"), "x").unwrap();
            let out = pod.relocate();
            assert!(Pod::warnings(&out).iter().any(|w| w.contains("isn't a git checkout")), "{out}");
            assert!(pod.repo().is_dir() && pod.link_target().is_none() && pod.asides().is_empty());
            assert_eq!(std::fs::read_dir(pod.vol_repo()).unwrap().count(), 1);

            // The repo path links somewhere else (or dangles): left as it is.
            let Some(pod) = Pod::new("dangling", true) else { return };
            std::fs::remove_dir_all(pod.repo()).unwrap();
            std::os::unix::fs::symlink(pod.vol_repo(), pod.repo()).unwrap(); // D missing
            let out = pod.relocate();
            assert!(Pod::warnings(&out).iter().any(|w| w.contains("not to a repo at")), "{out}");
            assert!(!pod.vol_repo().exists());

            // Not enough room on the volume, or on the container disk for the aside.
            for (tag, free_on) in [("novolroom", "workspace"), ("nodiskroom", "home")] {
                let Some(pod) = Pod::new(tag, true) else { return };
                pod.stub(
                    "df",
                    &format!("case \"$2\" in *{free_on}) n=10;; *) n=99999999;; esac; echo 'Filesystem 1024-blocks Used Available Capacity Mounted on'; echo \"x 1 1 $n 1% /\""),
                );
                let out = pod.relocate();
                assert!(Pod::warnings(&out).iter().any(|w| w.contains("no room")), "{tag}: {out}");
                assert!(pod.untouched(), "{tag}");
            }

            // A copy over its time limit: stopped, dropped.
            let Some(mut pod) = Pod::new("slow", true) else { return };
            pod.copy_secs = 1;
            pod.stub("cp", "exec sleep 5");
            let out = pod.relocate();
            assert!(Pod::warnings(&out).iter().any(|w| w.contains("within 1 s")), "{out}");
            assert!(pod.untouched());
        }

        /// In use = a process's working directory OR an open file inside the repo (a logger, a
        /// training run started from the home with its log in the repo, a kernel) — on a fresh
        /// volume and after a reset alike.
        #[test]
        fn a_repo_in_use_is_not_moved_from_under_its_users() {
            for (tag, by_fd) in [("cwd", false), ("fd", true)] {
                for reset in [false, true] {
                    let Some(pod) = Pod::new(&format!("busy-{tag}-{reset}"), true) else { return };
                    if reset {
                        pod.relocate();
                        pod.reset();
                    }
                    let mut user = if by_fd {
                        // Started from the home: its cwd isn't in the repo, its open file is (a
                        // log it appends to on a fresh pod; a file it reads after the reset —
                        // anything it wrote there would already count as changed work).
                        let (redirect, file) = if reset { ("3<", "chapter1/work.py") } else { ("3>>", "chapter1/train.log") };
                        Command::new("sh")
                            .arg("-c")
                            .arg(format!("exec {redirect}\"$1\"; exec sleep 30"))
                            .arg("sh")
                            .arg(pod.repo().join(file))
                            .current_dir(pod.p("home"))
                            .spawn()
                            .unwrap()
                    } else {
                        Command::new("sleep").arg("30").current_dir(pod.repo().join("chapter1")).spawn().unwrap()
                    };
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    let out = pod.relocate();
                    let _ = user.kill();
                    let _ = user.wait();
                    let w = Pod::warnings(&out);
                    assert!(w.iter().any(|w| w.contains("in use") && w.contains(&user.id().to_string())), "{tag} reset={reset}: {out}");
                    assert!(pod.link_target().is_none() && pod.repo().is_dir(), "{tag} reset={reset}");
                    // Idle again: moved / linked.
                    let out = pod.relocate();
                    assert_eq!(pod.link_target(), Some(pod.vol_repo()), "{tag} reset={reset}: {out}");
                }
            }
        }

        /// A relocation still running elsewhere (a timed-out setup's copy) holds the lock: the
        /// next one waits a bounded while, then leaves the move to it.
        #[test]
        fn a_relocation_already_running_is_left_to_finish() {
            if Command::new("flock").arg("--version").output().is_err() {
                eprintln!("flock not installed — skipping");
                return;
            }
            let Some(pod) = Pod::new("locked", true) else { return };
            let mut holder = Command::new("flock").arg(pod.p("lock")).arg("sleep").arg("10").spawn().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
            let started = std::time::Instant::now();
            let out = pod.relocate();
            let _ = holder.kill();
            let _ = holder.wait();
            assert!(Pod::warnings(&out).iter().any(|w| w.contains("still moving")), "{out}");
            assert!(started.elapsed() < std::time::Duration::from_secs(8), "bounded wait");
            assert!(pod.untouched());
        }

        #[test]
        fn the_probe_reports_the_real_path_the_mount_and_the_container_time() {
            let Some(pod) = Pod::new("probe", true) else { return };
            let probe = |pod: &Pod| {
                let script = probe_script(
                    &pod.repo().display().to_string(),
                    &pod.p("workspace").display().to_string(),
                    &pod.mark().display().to_string(),
                );
                super::super::parse_probe(&pod.sh(&script).0).expect("a parsable answer")
            };
            let before = probe(&pod);
            assert_eq!(before.real.as_deref(), Some(pod.repo().to_str().unwrap()));
            assert!(before.volume_mounted);
            assert_eq!(before.home.as_deref(), Some(pod.p("home").to_str().unwrap()));
            let mtime = std::fs::metadata(pod.mark()).unwrap().modified().unwrap();
            let secs = mtime.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            assert_eq!(before.container_created, Some(secs));
            pod.relocate();
            let after = probe(&pod);
            assert_eq!(after.real.as_deref(), Some(pod.vol_repo().to_str().unwrap()), "symlink resolved");
            // No repo, no volume, no mark.
            let Some(none) = Pod::new("probe-none", false) else { return };
            std::fs::remove_dir_all(none.repo()).unwrap();
            std::fs::remove_file(none.mark()).unwrap();
            let got = probe(&none);
            assert_eq!((got.real, got.volume_mounted, got.container_created), (None, false, None));
        }
    }
}
