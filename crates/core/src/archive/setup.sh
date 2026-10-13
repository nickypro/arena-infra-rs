#!/bin/bash
# arena archive setup — run as root on the archive VM by `arena archive up` (base64 inside the
# ssh command, like the deep check). Idempotent: every run converges the VM to the same state.
# It never creates a filesystem or touches a partition table: the volumes come from Hetzner
# already ext4, and a device that isn't is refused, never "fixed".
#
# Inputs: the ARENA_* shell assignments arena renders in front of this script.
# Output: `key=value` lines; every `ok_<step>=0` comes with `why_<step>=…`, and the last line is
# `setup_done=1` (a report without it didn't finish). Exit 1 on any failure.
set -u
export DEBIAN_FRONTEND=noninteractive LC_ALL=C
PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

FAILED=0
say() { printf '%s=%s\n' "$1" "$(printf '%s' "$2" | tr '\n' ' ')"; }
ok() { say "ok_$1" 1; }
bad() { say "ok_$1" 0; say "why_$1" "$2"; FAILED=1; }
finish() { say setup_done 1; exit "$FAILED"; }

PUSH=/srv/arena/push
VIEW=/srv/arena/view
KEYS=/etc/ssh/arena-keys
DROPIN=/etc/ssh/sshd_config.d/10-arena-archive.conf
dev_of() { case "$1" in snap) printf '%s' "$ARENA_SNAP_DEV" ;; big) printf '%s' "$ARENA_BIG_DEV" ;; esac; }
# Write a file atomically (temp + rename in the same directory), root-owned, with a mode.
put() { t=$(mktemp "$1.XXXXXX") && printf '%s' "$2" > "$t" && chown root:root "$t" && chmod "$3" "$t" && mv -f "$t" "$1"; }

# --- 1. preflight ------------------------------------------------------------------------
os_id=$(. /etc/os-release 2>/dev/null; printf '%s' "${ID:-}")
if [ "$os_id" = ubuntu ]; then ok os; else bad os "not Ubuntu (${os_id:-unknown})"; finish; fi
if ! command -v rsync >/dev/null 2>&1; then
  apt-get update -qq >/dev/null 2>&1
  apt-get install -y -qq rsync >/dev/null 2>&1
fi
v=$(rsync --version 2>/dev/null | sed -n '1s/.*version \([0-9][0-9.]*\).*/\1/p')
say rsync_version "$v"
if [ -n "$v" ] && [ "$(printf '%s\n3.2.4\n' "$v" | sort -V | head -1)" = 3.2.4 ]; then ok rsync; else bad rsync "rsync ${v:-missing}, need 3.2.4 or newer"; fi
if command -v python3 >/dev/null 2>&1; then ok python3; else bad python3 "python3 missing (rrsync is a python script)"; fi
RRSYNC=$(command -v rrsync 2>/dev/null || true)
say rrsync "$RRSYNC"
if [ -n "$RRSYNC" ]; then ok rrsync; else bad rrsync "rrsync not found (Ubuntu's rsync package ships /usr/bin/rrsync; never fetched from elsewhere)"; fi
[ "$FAILED" = 0 ] || finish

# --- 2. users and the mountpoint guards ----------------------------------------------------
# arena-push: a real shell (sshd runs its forced rrsync command through it); backups: none.
# '*' = no password at all, without the '!' that sshd treats as a locked account.
u_ok=1
id -u arena-push >/dev/null 2>&1 || useradd --system --shell /bin/sh --home-dir "$PUSH" --no-create-home arena-push || u_ok=0
id -u backups >/dev/null 2>&1 || useradd --system --shell /usr/sbin/nologin --home-dir / --no-create-home backups || u_ok=0
usermod -s /bin/sh -d "$PUSH" -p '*' arena-push >/dev/null 2>&1 || u_ok=0
usermod -s /usr/sbin/nologin -d / -p '*' backups >/dev/null 2>&1 || u_ok=0
if [ "$u_ok" = 1 ]; then ok users; else bad users "creating or updating arena-push/backups failed"; fi

install -d -o root -g root -m 0755 /srv/arena "$PUSH" "$VIEW"
# While nothing is mounted on it, a mountpoint is a root-owned, immutable, empty directory: a
# push that arrives with the volume missing fails (EPERM) instead of filling the root disk.
guard() {
  if mountpoint -q "$1"; then echo mounted; return; fi
  [ -d "$1" ] || mkdir -p "$1"
  chattr -i "$1" 2>/dev/null
  chown root:root "$1" && chmod 0755 "$1" && chattr +i "$1" 2>/dev/null
  if lsattr -d "$1" 2>/dev/null | awk '{print $1}' | grep -q i; then echo 1; else echo 0; fi
}
for t in snap big; do
  for side in push view; do
    if [ "$side" = push ]; then mp=$PUSH/$t; else mp=$VIEW/$t; fi
    g=$(guard "$mp")
    say "guard_${side}_$t" "$g"
    [ "$g" = 0 ] && bad "guard_${side}_$t" "$mp is not a root-owned immutable mountpoint"
  done
done

# --- 3. the volumes: ext4 or nothing, mounted at fixed paths, read-only binds ----------------
dev_ok=1
for t in snap big; do
  dev=$(dev_of "$t")
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    [ -b "$dev" ] && break
    udevadm settle 2>/dev/null
    sleep 2
  done
  if [ ! -b "$dev" ]; then bad "dev_$t" "$dev is not a block device (volume not attached?)"; dev_ok=0; continue; fi
  ok "dev_$t"
  fs=$(blkid -o value -s TYPE "$dev" 2>/dev/null)
  if [ "$fs" = ext4 ]; then ok "fs_$t"; else bad "fs_$t" "$dev is ${fs:-not formatted} — the archive only mounts ext4 and never formats a volume"; dev_ok=0; fi
done

if [ "$dev_ok" = 1 ]; then
  t=$(mktemp /etc/fstab.arena.XXXXXX)
  if awk '$0=="# BEGIN arena-archive"{skip=1} !skip{print} $0=="# END arena-archive"{skip=0}' /etc/fstab > "$t" \
     && printf '%s\n' "$ARENA_FSTAB_BLOCK" >> "$t" && chmod 0644 "$t" && mv -f "$t" /etc/fstab; then
    ok fstab
  else
    rm -f "$t"
    bad fstab "rewriting /etc/fstab failed"
  fi
  systemctl daemon-reload >/dev/null 2>&1
  for t in snap big; do
    dev=$(dev_of "$t"); mp=$PUSH/$t; vp=$VIEW/$t
    want=$(readlink -f "$dev")
    err=""
    mountpoint -q "$mp" || err=$(mount "$mp" 2>&1)
    src=$(findmnt -no SOURCE --mountpoint "$mp" 2>/dev/null | head -1)
    fst=$(findmnt -no FSTYPE --mountpoint "$mp" 2>/dev/null | head -1)
    if [ -n "$src" ] && [ "$(readlink -f "$src")" = "$want" ] && [ "$fst" = ext4 ]; then
      ok "mount_$t"
      # The volume's root belongs to the push user (not recursive): it writes there, nowhere else.
      if chown arena-push:arena-push "$mp" && chmod 0755 "$mp"; then ok "owner_$t"; else bad "owner_$t" "chown $mp failed"; fi
    else
      bad "mount_$t" "$mp has ${src:-nothing} ${fst} mounted, want $want ext4 ${err}"
      continue
    fi
    # The read-only bind; re-done when it binds something else (the bare mountpoint after a boot
    # without the volume) or isn't read-only.
    bsrc=$(findmnt -no SOURCE --mountpoint "$vp" 2>/dev/null | head -1 | sed 's/\[.*\]$//')
    if mountpoint -q "$vp" && [ "$(readlink -f "$bsrc")" != "$want" ]; then umount "$vp" 2>/dev/null; fi
    mountpoint -q "$vp" || mount "$vp" >/dev/null 2>&1
    bopts=$(findmnt -no OPTIONS --mountpoint "$vp" 2>/dev/null | head -1)
    case ",$bopts," in *,ro,*) ;; *) mountpoint -q "$vp" && mount -o remount,bind,ro "$vp" >/dev/null 2>&1 ;; esac
    bsrc=$(findmnt -no SOURCE --mountpoint "$vp" 2>/dev/null | head -1 | sed 's/\[.*\]$//')
    bopts=$(findmnt -no OPTIONS --mountpoint "$vp" 2>/dev/null | head -1)
    case ",$bopts," in *,ro,*) ro=1 ;; *) ro=0 ;; esac
    if [ -n "$bsrc" ] && [ "$(readlink -f "$bsrc")" = "$want" ] && [ "$ro" = 1 ]; then ok "bind_$t"; else bad "bind_$t" "$vp: ${bsrc:-nothing} (${bopts:-not mounted}), want $want read-only"; fi
  done
fi

# --- 4. key files and the README -------------------------------------------------------------
# Root-owned, outside both users' homes: neither can change who may log in as it.
f_ok=1
install -d -o root -g root -m 0755 "$KEYS" || f_ok=0
put "$KEYS/arena-push" "${ARENA_PUSH_KEY_TEMPLATE//@RRSYNC@/$RRSYNC}" 0644 || f_ok=0
put "$KEYS/backups" "$ARENA_BACKUPS_KEYS" 0644 || f_ok=0
put "$VIEW/README.txt" "$ARENA_README" 0644 || f_ok=0
if [ "$f_ok" = 1 ]; then ok files; else bad files "writing the key files or README failed"; fi

# --- 5. sshd: installed only once `sshd -t` and the per-user `sshd -T` checks pass -----------
sshd_ok=1; why=""
if ! grep -Eq '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/\*\.conf' /etc/ssh/sshd_config; then
  sshd_ok=0; why="/etc/ssh/sshd_config doesn't Include /etc/ssh/sshd_config.d/*.conf"
fi
check() {
  u=$1; shift
  out=$(sshd -T -C "user=$u,host=archive-check,addr=192.0.2.1" 2>&1) || { why="sshd -T for $u failed: $(printf '%s' "$out" | tail -1)"; return 1; }
  for want in "$@"; do
    printf '%s\n' "$out" | grep -qixF -- "$want" || { why="sshd -T for user $u lacks \"$want\""; return 1; }
  done
}
if [ "$sshd_ok" = 1 ]; then
  install -d -m 0755 /etc/ssh/sshd_config.d
  prev=""
  if [ -f "$DROPIN" ]; then prev=$(mktemp /root/.arena-sshd-prev.XXXXXX) && cp -p "$DROPIN" "$prev"; fi
  if ! put "$DROPIN" "$ARENA_SSHD_DROPIN" 0644; then
    sshd_ok=0; why="writing $DROPIN failed"
  elif ! err=$(sshd -t 2>&1); then
    sshd_ok=0; why="sshd -t: $(printf '%s' "$err" | tail -1)"
  elif ! check backups "chrootdirectory /srv/arena/view" "forcecommand internal-sftp -R -d /" "permittty no" \
         "authorizedkeysfile /etc/ssh/arena-keys/%u" "disableforwarding yes" "passwordauthentication no"; then
    sshd_ok=0
  elif ! check arena-push "authorizedkeysfile /etc/ssh/arena-keys/%u" "permittty no" "disableforwarding yes" "chrootdirectory none"; then
    sshd_ok=0
  elif ! check root "chrootdirectory none" "passwordauthentication no" "forcecommand none"; then
    sshd_ok=0
  fi
  if [ "$sshd_ok" = 1 ]; then
    rm -f "$prev"
    if systemctl is-active --quiet ssh.service; then
      if systemctl reload ssh.service >/dev/null 2>&1; then say sshd_reload ok; else say sshd_reload failed; fi
    elif systemctl is-active --quiet sshd.service; then
      if systemctl reload sshd.service >/dev/null 2>&1; then say sshd_reload ok; else say sshd_reload failed; fi
    else
      say sshd_reload "no running ssh.service (socket-activated: each new connection reads the config)"
    fi
  elif [ -n "$prev" ]; then
    mv -f "$prev" "$DROPIN"
  else
    rm -f "$DROPIN"
  fi
fi
if [ "$sshd_ok" = 1 ]; then ok sshd; else bad sshd "$why — config unchanged"; fi

install -d -m 0755 /var/lib/arena-archive && printf '%s\n' "$ARENA_SETUP_VERSION" > /var/lib/arena-archive/setup-version
say setup_version "$ARENA_SETUP_VERSION"
finish
