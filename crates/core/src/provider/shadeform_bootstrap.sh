#!/bin/bash
# arena bootstrap for a Shadeform VM: authorize the cohort keys for root (and the shadeform user),
# record arena's identity nonce. Rendered by arena; public data only.
set -u
S=""; [ "$(id -u)" = 0 ] || S="sudo -n"
KEYS='@KEYS@'
for u in root shadeform; do
  h=$(getent passwd "$u" | cut -d: -f6) || continue
  [ -n "$h" ] || continue
  $S install -d -m 700 -o "$u" -g "$u" "$h/.ssh"
  [ -z "$KEYS" ] || printf '%s\n' "$KEYS" | $S tee -a "$h/.ssh/authorized_keys" >/dev/null
  $S touch "$h/.ssh/authorized_keys"
  $S sort -u -o "$h/.ssh/authorized_keys" "$h/.ssh/authorized_keys"
  $S chown "$u:$u" "$h/.ssh/authorized_keys"; $S chmod 600 "$h/.ssh/authorized_keys"
done
$S install -d -m 755 /var/lib/arena
printf '%s\n' '@NONCE@' | $S tee /var/lib/arena/shadeform-nonce >/dev/null
printf '%s\n' '@NAME@'  | $S tee /var/lib/arena/name >/dev/null
# Keys-only root login, in case the image forbids root.
case "$($S sshd -T 2>/dev/null | awk '/^permitrootlogin/{print $2}')" in
  yes|prohibit-password|without-password) ;;
  *) $S install -d -m 755 /etc/ssh/sshd_config.d
     printf 'PermitRootLogin prohibit-password\n' | $S tee /etc/ssh/sshd_config.d/10-arena-root.conf >/dev/null
     $S systemctl reload ssh 2>/dev/null || $S systemctl reload sshd 2>/dev/null ;;
esac
