# arena idle probe (the auto-reaper's look at a pod): READ-ONLY — it writes nothing anywhere.
#
#   env -i PATH=… HOME="$HOME" sh -c "$THIS" arena-idle-probe <nonce> <since> <repo> <provider> "$SSH_CONNECTION"
#
# <since>: `@<epoch>` (scan files changed since then, pod clock) or `-<secs>` (that long before
# this probe started). Rendered and parsed by arena_core::reap::probe (probe_command /
# parse_probe). POSIX sh (dash on the images). Every step that can hang runs under `timeout`.
#
# Output, all on stdout, in this order:
#   ARENA_PROBE_BEGIN <nonce>
#   ARENA_PROBE <key>=<value>          (one per fact; repeated facts carry an index 1..n and a
#                                       `<key>_n=<n>` count line, so a lost or doubled line shows)
#   ARENA_PROBE sum=<cksum of every line above, from the first fact on> <bytes>
#   ARENA_PROBE_END <nonce>
# Nothing is printed until the end: a probe cut short prints no END (and no facts).
#
# Sampling order, so the probe doesn't measure itself: CPU + network counters, a 10 s window,
# the counters again; then 5 GPU samples a second apart; then the file scan, sockets,
# processes, tmux/screen, markers and identity.
LC_ALL=C
export LC_ALL
N=$1
SINCE=$2
REPO=$3
PROV=$4
SELF=$5
case "$N" in '' | *[!A-Za-z0-9]*) exit 2 ;; esac
command -v timeout >/dev/null 2>&1 || exit 3
command -v cksum >/dev/null 2>&1 || exit 3
WIN=${ARENA_PROBE_TEST_WINDOW:-10}
GPU_T=${ARENA_PROBE_TEST_GPU_TIMEOUT:-8}
OUT=
o() {
    OUT="${OUT}ARENA_PROBE $1=$2
"
}
# Repeated facts: `o_n key value` numbers them; `n_of key` is how many so far.
o_n() {
    eval "c=\${CNT_$1:-0}"
    c=$((c + 1))
    eval "CNT_$1=$c"
    o "$1" "$c $2"
}
n_of() {
    eval "printf %s \"\${CNT_$1:-0}\""
}
num() {
    case "$1" in '' | *[!0-9]*) return 1 ;; *) return 0 ;; esac
}

t0=$(date +%s)
num "$t0" || exit 3
case "$SINCE" in
    @*) S=${SINCE#@} ;;
    -*) B=${SINCE#-}; num "$B" || exit 2; S=$((t0 - B)) ;;
    *) exit 2 ;;
esac
num "$S" || exit 2

# --- CPU and network, around a quiet window -------------------------------------------------
cpu() {
    if [ -r /sys/fs/cgroup/cpu.stat ]; then
        v=$(awk '$1 == "usage_usec" { print $2; exit }' /sys/fs/cgroup/cpu.stat 2>/dev/null)
        if num "$v"; then echo "cg2 $v"; return; fi
    fi
    for f in /sys/fs/cgroup/cpuacct/cpuacct.usage /sys/fs/cgroup/cpu,cpuacct/cpuacct.usage /sys/fs/cgroup/cpuacct,cpu/cpuacct.usage; do
        if [ -r "$f" ]; then
            v=$(cat "$f" 2>/dev/null)
            if num "$v"; then echo "cg1 $((v / 1000))"; return; fi
        fi
    done
    hz=$(getconf CLK_TCK 2>/dev/null)
    num "$hz" || hz=100
    v=$(awk -v hz="$hz" '$1 == "cpu" { printf "%.0f\n", ($2 + $3 + $4 + $7 + $8 + $9) * 1000000 / hz; exit }' /proc/stat 2>/dev/null)
    if num "$v"; then echo "proc $v"; else echo "- -"; fi
}
net() {
    v=$(awk 'NR > 2 { l = $0; sub(/^ */, "", l); i = index(l, ":"); if (i == 0) next; if (substr(l, 1, i - 1) == "lo") next;
            split(substr(l, i + 1), f, " "); s += f[1] + f[9]; n++ } END { if (n) printf "%.0f\n", s }' /proc/net/dev 2>/dev/null)
    if num "$v"; then echo "$v"; else echo "-"; fi
}
o v 1
o t0 "$t0"
o cpu0 "$(cpu)"
o net0 "$(net)"
sleep "$WIN"
o t1 "$(date +%s)"
o cpu1 "$(cpu)"
o net1 "$(net)"

# --- GPU: 5 samples a second apart, then the compute processes -------------------------------
if command -v nvidia-smi >/dev/null 2>&1; then
    o gpu_smi present
    k=1
    while [ "$k" -le 5 ]; do
        out=$(timeout "$GPU_T" nvidia-smi --query-gpu=utilization.gpu,memory.used --format=csv,noheader,nounits 2>/dev/null)
        rc=$?
        o_n gpu_rc "$k $rc"
        lines=$(printf '%s\n' "$out" | head -n 16)
        while IFS= read -r l; do
            [ -n "$l" ] && o_n gpu "$k $(printf %s "$l" | tr -c '0-9A-Za-z ,./-' '?' | cut -c1-40)"
        done <<EOF_GPU
$lines
EOF_GPU
        [ "$rc" = 0 ] || break
        [ "$k" = 5 ] || sleep 1
        k=$((k + 1))
    done
    out=$(timeout "$GPU_T" nvidia-smi --query-compute-apps=pid --format=csv,noheader 2>/dev/null)
    if [ $? = 0 ]; then o gpu_procs "$(printf '%s\n' "$out" | grep -c '[0-9]')"; else o gpu_procs -; fi
else
    o gpu_smi absent
fi
o gpu_rc_n "$(n_of gpu_rc)"
o gpu_n "$(n_of gpu)"

# --- Files changed (ctime) since $S, outside .git and ~/.arena; one filesystem -----------------
H=${HOME:-/root}
f=$(timeout 60 find "$H" -xdev \( -name .git -o -path "$H/.arena" \) -prune -o -newerct "@$S" -print -quit 2>/dev/null)
rc=$?
if [ -n "$f" ]; then o files hit
elif [ "$rc" = 124 ]; then o files timeout
elif [ "$rc" = 0 ]; then o files none
else o files err
fi
o scan_from "$S"

# --- Sockets: ESTABLISHED (01) and LISTEN (0A), capped ----------------------------------------
if [ -r /proc/net/tcp ]; then
    o tcp_read ok
    lines=$(for t in /proc/net/tcp /proc/net/tcp6; do
        [ -r "$t" ] && awk 'FNR > 1 && ($4 == "01" || $4 == "0A") { print $4, $2, $3 }' "$t" 2>/dev/null
    done)
    total=$(printf '%s\n' "$lines" | grep -c .)
    capped=$(printf '%s\n' "$lines" | grep . | head -n 500)
    while IFS= read -r l; do
        [ -n "$l" ] && o_n tcp "$l"
    done <<EOF_TCP
$capped
EOF_TCP
    if [ "$total" -gt 500 ]; then o tcp_overflow 1; else o tcp_overflow 0; fi
else
    o tcp_read err
fi
o tcp_n "$(n_of tcp)"

# --- SSH sessions: sshd processes, and this probe's own ancestors -----------------------------
p=$$
i=0
while num "$p" && [ "$p" -gt 1 ] && [ "$i" -lt 64 ]; do
    o_n anc "$p"
    p=$(awk '{ s = $0; sub(/^.*\) /, "", s); split(s, f, " "); print f[2] }' "/proc/$p/stat" 2>/dev/null)
    i=$((i + 1))
done
o anc_n "$(n_of anc)"
for d in /proc/[0-9]*; do
    c=
    { IFS= read -r c; } 2>/dev/null <"$d/comm" || continue
    case "$c" in sshd | sshd-session | sshd-auth) ;; *) continue ;; esac
    if [ "$(n_of sshd)" -ge 100 ]; then o sshd_overflow 1; break; fi
    cmd=$({ tr '\000' ' ' <"$d/cmdline"; } 2>/dev/null | tr -c '[:print:]' '?' | cut -c1-200)
    o_n sshd "${d#/proc/} ${cmd:-sshd-exited}"
done
o sshd_n "$(n_of sshd)"
o self "$(printf %s "$SELF" | tr -c '0-9a-fA-F.: ' '?' | cut -c1-120)"

# --- Logins, tmux, screen ---------------------------------------------------------------------
if command -v who >/dev/null 2>&1; then
    if w=$(timeout 5 who 2>/dev/null); then o who "$(printf '%s' "$w" | grep -c .)"; else o who -; fi
else
    o who absent
fi
if command -v tmux >/dev/null 2>&1; then
    out=$(timeout 5 tmux list-panes -a -F '#{pane_current_command}' 2>&1)
    rc=$?
    if [ "$rc" = 0 ]; then
        o tmux ok
        o tmux_panes "$(printf '%s\n' "$out" | grep -c .)"
        lines=$(printf '%s\n' "$out" | grep . | head -n 50)
        while IFS= read -r l; do
            [ -n "$l" ] && o_n pane "$(printf %s "$l" | tr -c '[:alnum:]._+-' '?' | cut -c1-40)"
        done <<EOF_PANE
$lines
EOF_PANE
    else
        case "$out" in
            *"no server running"* | *"error connecting to"*) o tmux ok; o tmux_panes 0 ;;
            *) o tmux err ;;
        esac
    fi
else
    o tmux absent
fi
o pane_n "$(n_of pane)"
if command -v screen >/dev/null 2>&1; then
    out=$(timeout 5 screen -ls 2>&1)
    case "$out" in
        *"No Sockets found"*) o screen 0 ;;
        *"There is a screen on"* | *"There are screens on"*)
            n=$(printf '%s\n' "$out" | grep -c '^[[:space:]][0-9]')
            [ "$n" -gt 0 ] || n=1
            o screen "$n" ;;
        *) o screen err ;;
    esac
else
    o screen absent
fi

# --- Markers, scope, identity -----------------------------------------------------------------
if [ -e "$H/.arena/keep" ]; then o keep 1; else o keep 0; fi
hz=$(getconf CLK_TCK 2>/dev/null)
num "$hz" || hz=100
st=$(awk '{ s = $0; sub(/^.*\) /, "", s); split(s, f, " "); print f[20] }' /proc/1/stat 2>/dev/null)
bt=$(awk '$1 == "btime" { print $2 }' /proc/stat 2>/dev/null)
if [ ! -e /workspace ]; then
    o workspace absent
elif awk '$2 == "/workspace" { f = 1 } END { exit !f }' /proc/self/mounts 2>/dev/null; then
    e=$(timeout 20 find /workspace -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)
    rc=$?
    if [ -n "$e" ]; then o workspace has_data; elif [ "$rc" = 0 ]; then o workspace clean; else o workspace err; fi
elif num "$st" && num "$bt"; then
    cs=$((bt + st / hz))
    e=$(timeout 20 find /workspace -xdev -mindepth 1 -newerct "@$cs" -print -quit 2>/dev/null)
    rc=$?
    if [ -n "$e" ]; then o workspace has_data; elif [ "$rc" = 0 ]; then o workspace clean; else o workspace err; fi
else
    o workspace err
fi
h=$(cd "$H" 2>/dev/null && pwd -P)
if [ -z "$REPO" ] || [ ! -e "$REPO" ]; then
    o repo absent
else
    r=$(cd "$REPO" 2>/dev/null && pwd -P)
    if [ -z "$r" ] || [ -z "$h" ]; then
        o repo err
    else
        case "$r/" in "$h"/*) o repo under_home ;; *) o repo outside_home ;; esac
    fi
fi
if [ -n "$h" ] && m=$(awk -v h="$h/" 'index($2, h) == 1 { n++ } END { print n + 0 }' /proc/self/mounts 2>/dev/null); then
    o home_mounts "$m"
else
    o home_mounts -
fi
case "$PROV" in
    runpod) id=$({ tr '\000' '\n' </proc/1/environ; } 2>/dev/null | sed -n 's/^RUNPOD_POD_ID=//p' | head -n 1) ;;
    hetzner) id=$(cat /var/lib/cloud/data/instance-id 2>/dev/null) || id=$(timeout 3 curl -s -m 2 http://169.254.169.254/hetzner/v1/metadata/instance-id 2>/dev/null) ;;
    *) id= ;;
esac
o id "$(printf %s "$id" | tr -cd 'A-Za-z0-9._-' | cut -c1-64)"
o boot_id "$({ tr -cd 'a-f0-9-' </proc/sys/kernel/random/boot_id; } 2>/dev/null)"
o pid1_start "$(printf %s "$st" | tr -cd '0-9')"
o now "$(date +%s)"

echo "ARENA_PROBE_BEGIN $N"
printf '%s' "$OUT"
printf 'ARENA_PROBE sum=%s\n' "$(printf '%s' "$OUT" | cksum | awk '{ print $1 " " $2 }')"
echo "ARENA_PROBE_END $N"
