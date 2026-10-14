#!/bin/bash
# arena bootstrap for a Shadeform VM: authorize the cohort keys for root (and the shadeform user),
# record arena's identity nonce. Rendered by arena; public data only.
set -u
S=""; [ "$(id -u)" = 0 ] || S="sudo -n"
KEYS='@KEYS@'
# Each cohort key ends up as one plain (keys-only, no options) line, after whatever else is
# there: any line already holding one of these keys is dropped first — cloud-init's
# `disable_root` puts the instance's key in root's file behind `command="echo 'Please login as
# …';exit 142"`, and sshd uses the first line that matches. Rerunning gives the same file.
PAT=()
while read -r _ b _; do [ -n "${b:-}" ] && PAT+=(-e " $b"); done <<< "$KEYS"
for u in root shadeform; do
  h=$(getent passwd "$u" | cut -d: -f6) || continue
  [ -n "$h" ] || continue
  f="$h/.ssh/authorized_keys"
  $S install -d -m 700 -o "$u" -g "$u" "$h/.ssh"
  $S touch "$f"
  if [ -n "$KEYS" ]; then
    { $S cat "$f" | grep -vF "${PAT[@]}"; printf '%s\n' "$KEYS"; } | awk 'NF && !seen[$0]++' | $S tee "$f.arena-new" >/dev/null
    # Replaced only by a file that holds the keys: a failed rewrite never empties it.
    if $S grep -qF -- "${PAT[1]}" "$f.arena-new"; then $S mv "$f.arena-new" "$f"; else $S rm -f "$f.arena-new"; fi
  fi
  $S chown "$u:$u" "$f"; $S chmod 600 "$f"
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
