#!/bin/bash
# arena archive probe — run as root on the archive VM by `arena archive status`. READ-ONLY: it
# changes nothing, reads only a few fixed files and the mount table, and never walks the backups
# (nothing that runs as root follows a participant's symlink).
#
# Inputs: ARENA_SNAP_DEV / ARENA_BIG_DEV (the volumes' device paths, to resolve them here).
# Output: `key=value` lines, ending with `probe_done=1` (a report without it is unknown).
set -u
export LC_ALL=C
PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
say() { printf '%s=%s\n' "$1" "$(printf '%s' "$2" | tr '\n' ' ')"; }
b64() { if [ -f "$1" ] && [ ! -L "$1" ]; then base64 -w0 "$1" 2>/dev/null; fi; }

for t in snap big; do
  case "$t" in snap) dev=$ARENA_SNAP_DEV ;; big) dev=$ARENA_BIG_DEV ;; esac
  mp=/srv/arena/push/$t
  vp=/srv/arena/view/$t
  say "dev_$t" "$(readlink -f "$dev" 2>/dev/null)"
  if mountpoint -q "$mp"; then
    say "mounted_$t" 1
    say "source_$t" "$(readlink -f "$(findmnt -no SOURCE --mountpoint "$mp" | head -1)")"
    say "fstype_$t" "$(findmnt -no FSTYPE --mountpoint "$mp" | head -1)"
    say "owner_$t" "$(stat -c %U "$mp" 2>/dev/null)"
    read -r total used _ <<EOF
$(df -P -B1 "$mp" | awk 'NR==2{print $2, $3, $4}')
EOF
    say "bytes_total_$t" "${total:-}"
    say "bytes_used_$t" "${used:-}"
    read -r itotal iused _ <<EOF
$(df -P -i "$mp" | awk 'NR==2{print $2, $3, $4}')
EOF
    say "inodes_total_$t" "${itotal:-}"
    say "inodes_used_$t" "${iused:-}"
  else
    say "mounted_$t" 0
  fi
  if mountpoint -q "$vp"; then
    say "bind_$t" 1
    say "bind_source_$t" "$(readlink -f "$(findmnt -no SOURCE --mountpoint "$vp" | head -1 | sed 's/\[.*\]$//')")"
    o=$(findmnt -no OPTIONS --mountpoint "$vp" | head -1)
    case ",$o," in *,ro,*) say "bind_ro_$t" 1 ;; *) say "bind_ro_$t" 0 ;; esac
  else
    say "bind_$t" 0
  fi
done

say root_keys_b64 "$(b64 /root/.ssh/authorized_keys)"
say push_keys_b64 "$(b64 /etc/ssh/arena-keys/arena-push)"
say backups_keys_b64 "$(b64 /etc/ssh/arena-keys/backups)"
# Private keys where none belong (the archive must hold no outbound credential); host keys
# live in /etc/ssh itself and aren't counted.
n=0
for f in /root/.ssh/* /home/*/.ssh/* /etc/ssh/arena-keys/*; do
  [ -f "$f" ] && [ ! -L "$f" ] || continue
  grep -q -- '-----BEGIN .*PRIVATE KEY-----' "$f" 2>/dev/null && n=$((n + 1))
done
say private_keys "$n"
say reboot_required "$([ -e /var/run/reboot-required ] && echo 1 || echo 0)"
say ssh_service "$(systemctl is-active ssh.service 2>/dev/null)"
say ssh_socket "$(systemctl is-active ssh.socket 2>/dev/null)"
say unattended_upgrades "$(systemctl is-active unattended-upgrades.service 2>/dev/null)"
say rsync_version "$(rsync --version 2>/dev/null | sed -n '1s/.*version \([0-9][0-9.]*\).*/\1/p')"
say rrsync "$(command -v rrsync 2>/dev/null)"
say setup_version "$(cat /var/lib/arena-archive/setup-version 2>/dev/null)"
say sshd_dropin "$([ -f /etc/ssh/sshd_config.d/10-arena-archive.conf ] && echo 1 || echo 0)"
say root_avail "$(df -P -B1 / | awk 'NR==2{print $4}')"
say probe_done 1
