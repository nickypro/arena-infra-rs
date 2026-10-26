#!/usr/bin/env bash
# Login banner, shared by containers and bare VMs.
MOTD_DIR="${ARENA_MOTD_DIR:-/etc/update-motd.d}"
mkdir -p "$MOTD_DIR" || exit 1
for item in 10-help-text 50-motd-news 60-unminimize; do
    [ ! -f "$MOTD_DIR/$item" ] || chmod -x "$MOTD_DIR/$item"
done
cat > "$MOTD_DIR/20-arena" <<'BANNER'
#!/bin/sh
if [ -x /usr/bin/figlet ]; then /usr/bin/figlet ARENA; fi
# The machine's name can change after a snapshot is taken.
[ ! -r "$HOME/.name" ] || . "$HOME/.name"
printf 'You are on ARENA machine %s\n\n' "${MACHINE_NAME:-$(hostname)}"
BANNER
chmod +x "$MOTD_DIR/20-arena"
