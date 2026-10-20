# arena idle probe (the auto-reaper's look at a pod): READ-ONLY — it writes nothing anywhere.
#
#   env -i PATH=… HOME="$HOME" sh -c "$THIS" arena-idle-probe <nonce> <since> <repo> <provider> "$SSH_CONNECTION" <created>
#
# <since>: `@<epoch>` (scan files changed since then, pod clock) or `-<secs>` (that long before
# this probe started). <created>: the pod's creation (epoch, as its provider lists it; `-` or
# absent = unknown) — what the coverage checks reach back to, whatever reboots came since. Rendered and parsed by arena_core::reap::probe (probe_command /
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
CR=${6:-}
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

# --- The home, resolved, and the container's start -------------------------------------------
# HOME as the backup reads it: through any symlink (`pwd -P`) — a scan of the link itself sees
# nothing and would read as quiet. A home that is a link to ANOTHER filesystem
# (`/root -> /data/root`) is reported (`home=link`: can't tell); unresolvable, `home=err`.
H=${HOME:-/root}
while [ "$H" != / ] && [ "${H%/}" != "$H" ]; do H=${H%/}; done
Hr=$(cd "$H" 2>/dev/null && pwd -P)
if [ -z "$Hr" ]; then o home err; Hr=$H
elif [ "$Hr" != "$H" ]; then
    hp=${H%/*}
    [ -n "$hp" ] || hp=/
    d1=$(stat -c %d "$Hr" 2>/dev/null)
    d2=$(stat -c %d "$hp" 2>/dev/null)
    if num "$d1" && [ "$d1" = "$d2" ]; then o home ok; else o home link; fi
else o home ok
fi
hz=$(getconf CLK_TCK 2>/dev/null)
num "$hz" || hz=100
o hz "$hz"
st=$(awk '{ s = $0; sub(/^.*\) /, "", s); split(s, f, " "); print f[20] }' /proc/1/stat 2>/dev/null)
bt=$(awk '$1 == "btime" { print $2 }' /proc/stat 2>/dev/null)
# The container's start (epoch); unknown = 0, so "changed since it started" means "ever".
cs=0
if num "$st" && num "$bt"; then cs=$((bt + st / hz)); fi
# Since when data outside the backup is a veto (A; empty = unknown, and a check that needs it
# can't tell): the pod's creation — it survives reboots, and a VM's disk does too (Hetzner).
# Where a restart starts the container afresh from its image (RunPod, Vast), the later of that
# and the container's start (what came before went with the old container; the image's files
# were unpacked before it started). A creation in this pod's future is no anchor.
A=
if num "$CR" && [ "$CR" -le "$t0" ]; then
    case "$PROV" in
        hetzner) A=$CR ;;
        *) if [ "$cs" -gt "$CR" ]; then A=$cs; else A=$CR; fi ;;
    esac
elif [ "$PROV" != hetzner ] && [ "$cs" -gt 0 ]; then
    A=$cs
fi

# --- Files changed (ctime) since $S; and, never copied by a reap's backup, any regular file in a
# sweep folder at all (whenever it was written: the folder is never copied) but a placeholder —
# one pass over the resolved home, one filesystem, outside .git and ~/.arena.
marks=$( { timeout 60 find "$Hr" -xdev \( -name .git -o -path "$Hr/.arena" \) -prune -o \
    \( \( -path '*/TEMP_FOLDER_FOR_SWEEPS/*' -type f ! -name .gitkeep ! -name .gitignore ! -name .keep -printf 'U\n' \) , \( -newerct "@$S" -printf 'F\n' \) \) 2>/dev/null
    echo "RC $?"; } | awk '$1 == "RC" { print; next } !seen[$1]++ { print }')
rc=$(printf '%s\n' "$marks" | awk '$1 == "RC" { print $2 }')
judge_scan() {
    if printf '%s\n' "$marks" | grep -qx "$2"; then o "$1" hit
    elif [ "$rc" = 124 ]; then o "$1" timeout
    elif [ "$rc" = 0 ]; then o "$1" none
    else o "$1" err
    fi
}
judge_scan files F
judge_scan uncovered U
o scan_from "$S"

# --- Outside the home: what the reap's backup (the home, links copied as links) never reads ----
# The system, installed software and caches — never a participant's only copy (documented in
# docs/BACKUPS.md): where a link from the home may point, and what the roots scan passes over.
junk() {
    case "$1" in
        /usr | /usr/* | /etc | /etc/* | /bin | /bin/* | /sbin | /sbin/* | /lib | /lib/* | /lib32 | /lib32/* | /lib64 | /lib64/* \
        | /libx32 | /libx32/* | /proc | /proc/* | /sys | /sys/* | /dev | /dev/* | /run | /run/* | /boot | /boot/* | /snap | /snap/* \
        | /var/lib | /var/lib/* | /var/cache | /var/cache/* | /var/log | /var/log/* \
        | /opt/conda | /opt/conda/* | /opt/miniconda* | /opt/anaconda* | /opt/nvidia | /opt/nvidia/* | /opt/hpcx | /opt/hpcx/* \
        | */site-packages/* | */dist-packages/* | */__pycache__/* | */node_modules/* | */.cache/* | */.triton/* \
        | /tmp/systemd-private-* | /var/tmp/systemd-private-* | /tmp/snap-private-tmp* | /tmp/.X11-unix* | /tmp/.ICE-unix* \
        | /tmp/.font-unix* | /tmp/.XIM-unix* | /tmp/.Test-unix* | /tmp/tmux-* | /tmp/vscode-* | /tmp/.vscode-* \
        | /tmp/torchinductor_* | /tmp/hsperfdata_* | /tmp/.arena-* | /tmp/arena_deep.* | /tmp/cx | /tmp/cx/* | /tmp/pip-*) return 0 ;;
    esac
    return 1
}
# (a) A link in the home (3 levels down, outside .git) to somewhere else that isn't junk: what's
# there is copied as the link alone. A dangling one has nothing behind it.
links=none
lst=$( { timeout 15 find "$Hr" -xdev -maxdepth 3 \( -name .git -o -path "$Hr/.arena" \) -prune -o -type l -print 2>/dev/null; echo "RC $?"; } | head -n 501)
case "$(printf '%s\n' "$lst" | tail -n 1)" in
    "RC 0") ;;
    *) links=err ;;
esac
[ "$(printf '%s\n' "$lst" | grep -vc '^RC ')" -ge 500 ] && links=err
if [ "$links" = none ]; then
    while IFS= read -r l; do
        case "$l" in '' | "RC "*) continue ;; esac
        t=$(readlink -f -- "$l" 2>/dev/null) || continue
        [ -e "$t" ] || continue
        case "$t" in "$Hr" | "$Hr"/*) continue ;; esac
        junk "$t" && continue
        links=hit
        break
    done <<EOF_LINKS
$lst
EOF_LINKS
fi
o links "$links"
# (b) Data written since the pod was created (A) in the places a participant can write outside
# the home on its filesystem — regular files only (no sockets or fifos), junk passed over, and
# empty ones too: a 0-byte file holds no data to lose (uv leaves a lock per setuptools build,
# `/tmp/uv-setuptools-<hash>.lock`, mode 666, for good — on a VM, where the anchor is the
# creation, they vetoed forever; live W7 B-H0). A scan that runs out of time, or no anchor,
# can't tell.
ROOTS=${ARENA_PROBE_TEST_ROOTS-/data /scratch /srv /mnt /media /opt /var/tmp /tmp /home/*}
roots=none
if [ -z "$A" ]; then
    roots=err
else
    rdl=$(( $(date +%s) + 25 ))
    for r in $ROOTS; do
        [ -d "$r" ] || continue
        rr=$(cd "$r" 2>/dev/null && pwd -P) || { roots=err; continue; }
        case "$rr" in "$Hr" | "$Hr"/*) continue ;; esac
        [ "$rr" = /workspace ] && continue
        left=$(( rdl - $(date +%s) ))
        if [ "$left" -le 0 ]; then roots=timeout; break; fi
        e=$(timeout "$left" find "$rr" -xdev \( -path "$Hr" -o -path /workspace -o -name site-packages -o -name dist-packages \
            -o -name __pycache__ -o -name node_modules -o -name .cache -o -name .triton -o -path /opt/conda -o -path '/opt/miniconda*' \
            -o -path '/opt/anaconda*' -o -path /opt/nvidia -o -path /opt/hpcx -o -path '/tmp/systemd-private-*' -o -path '/var/tmp/systemd-private-*' \
            -o -path '/tmp/snap-private-tmp*' -o -path '/tmp/.X11-unix' -o -path '/tmp/.ICE-unix' -o -path '/tmp/.font-unix' \
            -o -path '/tmp/.XIM-unix' -o -path '/tmp/.Test-unix' -o -path '/tmp/tmux-*' -o -path '/tmp/vscode-*' -o -path '/tmp/.vscode-*' \
            -o -path '/tmp/torchinductor_*' -o -path '/tmp/hsperfdata_*' -o -path '/tmp/.arena-*' -o -path '/tmp/arena_deep.*' \
            -o -path /tmp/cx -o -path '/tmp/pip-*' \) -prune -o -type f ! -empty -newerct "@$A" -print -quit 2>/dev/null)
        rc=$?
        if [ -n "$e" ]; then roots=hit; break; fi
        if [ "$rc" = 124 ]; then roots=timeout; break; fi
        [ "$rc" = 0 ] || roots=err
    done
fi
o roots "$roots"

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
ANCS=" $(printf '%s' "$OUT" | awk -F'[= ]' '$2 == "anc" { printf "%s ", $4 }')"
for d in /proc/[0-9]*; do
    c=
    { IFS= read -r c; } 2>/dev/null <"$d/comm" || continue
    case "$c" in sshd | sshd-session | sshd-auth) ;; *) continue ;; esac
    if [ "$(n_of sshd)" -ge 100 ]; then o sshd_overflow 1; break; fi
    cmd=$({ tr '\000' ' ' <"$d/cmdline"; } 2>/dev/null | tr -c '[:print:]' '?' | cut -c1-200)
    o_n sshd "${d#/proc/} ${cmd:-sshd-exited}"
done
o sshd_n "$(n_of sshd)"

# --- Processes outside this probe's own session and ancestry: start time (jiffies since boot),
# a cksum of the command line, i(nteractive shell) or p, the name. Judged against the
# container's boot-time processes (arena_core::reap::judge) — a dormant `nohup python train.py`
# is a process too. A plain interactive shell (no script, no -c) does nothing by itself: what
# it runs is a process of its own.
ish() {
    set -f
    # shellcheck disable=SC2086
    set -- $1
    set +f
    a=${1##*/}
    a=${a#-}
    case "$a" in bash | sh | zsh | dash | fish | ksh | ash) ;; *) return 1 ;; esac
    shift
    for x in "$@"; do
        case "$x" in -l | -i | --login | -il | -li) ;; *) return 1 ;; esac
    done
    return 0
}
sid=$(awk '{ s = $0; sub(/^.*\) /, "", s); split(s, f, " "); print f[4] }' /proc/$$/stat 2>/dev/null)
# Terminals: processes on a pseudo-terminal (/dev/pts/*: majors 136–143) outside this probe's
# session — an ssh login, RunPod's web/proxy shell, a VS Code or Jupyter terminal — but a tmux
# pane's own shell (judged by the tmux fact). `-` when the list can't be read whole.
# The process list read: /proc, the pod's own (a container's PID namespace, or the VM's) — or a
# test's stand-in (ARENA_PROBE_TEST_PROCROOT; never on a pod: the probe runs under `env -i`).
# This probe's own session and ancestry always come from the real /proc.
PROCFS=${ARENA_PROBE_TEST_PROCROOT:-/proc}
ttys=-
if num "$sid"; then
    ttys=0
    o proc_read ok
    for d in "$PROCFS"/[0-9]*; do
        p=${d#"$PROCFS"/}
        line=
        { IFS= read -r line; } 2>/dev/null <"$d/stat" || continue
        rest=${line##*) }
        # shellcheck disable=SC2086
        set -- $rest
        [ "$4" = "$sid" ] && continue
        case "$ANCS" in *" $p "*) continue ;; esac
        pst=${20}
        num "$pst" || continue
        pp=$2
        tn=$5
        cmd=$({ tr '\000' ' ' <"$d/cmdline"; } 2>/dev/null)
        [ -n "$cmd" ] || continue
        if [ "$(n_of proc)" -ge 300 ]; then o proc_overflow 1; ttys=-; break; fi
        if num "$tn" && [ "$tn" != 0 ]; then
            maj=$(( (tn >> 8) & 4095 ))
            if [ "$maj" -ge 136 ] && [ "$maj" -le 143 ]; then
                pc=
                { IFS= read -r pc; } 2>/dev/null <"$PROCFS/$pp/comm"
                [ "$pc" = "tmux: server" ] || ttys=$((ttys + 1))
            fi
        fi
        crc=$(printf '%s' "$cmd" | cksum | awk '{ print $1 }')
        k=p
        ish "$cmd" && k=i
        c=
        { IFS= read -r c; } 2>/dev/null <"$d/comm"
        o_n proc "$pst $crc $k $(printf %s "${c:-?}" | tr -c '[:alnum:]._+(): -' '?' | cut -c1-24)"
    done
else
    o proc_read err
fi
o proc_n "$(n_of proc)"
o ttys "$ttys"
o self "$(printf %s "$SELF" | tr -c '0-9a-fA-F.: ' '?' | cut -c1-120)"

# --- Scheduled work no process shows yet: a user crontab line, an `at` job, a systemd timer
# someone added (a regular file — not a packaged one, which is a link, nor snapd's own). One
# that can't be read can't tell. (SYS: a test's stand-in root; empty on a pod.)
SYS=${ARENA_PROBE_TEST_SYSROOT:-}
s_hit=0
s_err=0
for d in "$SYS/var/spool/cron/crontabs" "$SYS/var/spool/cron"; do
    [ -d "$d" ] || continue
    if [ ! -r "$d" ] || [ ! -x "$d" ]; then s_err=1; continue; fi
    for f in "$d"/*; do
        [ -f "$f" ] || continue
        if [ ! -r "$f" ]; then s_err=1; continue; fi
        grep -qvE '^[[:space:]]*(#|$|[A-Za-z_][A-Za-z0-9_]*[[:space:]]*=)' "$f" 2>/dev/null && s_hit=1
    done
done
if [ -z "$SYS" ] && command -v atq >/dev/null 2>&1; then
    if q=$(timeout 5 atq 2>/dev/null); then [ -n "$q" ] && s_hit=1; else s_err=1; fi
elif [ -d "$SYS/var/spool/cron/atjobs" ]; then
    for f in "$SYS/var/spool/cron/atjobs"/*; do
        [ -f "$f" ] && case "${f##*/}" in .SEQ) ;; *) s_hit=1 ;; esac
    done
fi
for f in "$SYS"/etc/systemd/system/*.timer "$SYS"/etc/systemd/system/*/*.timer "$SYS"/run/systemd/transient/*.timer \
    "$Hr"/.config/systemd/user/*.timer "$Hr"/.config/systemd/user/*/*.timer; do
    [ -f "$f" ] && [ ! -L "$f" ] || continue
    case "${f##*/}" in snap.*) continue ;; esac
    s_hit=1
done
if [ "$s_hit" = 1 ]; then o sched hit; elif [ "$s_err" = 1 ]; then o sched err; else o sched none; fi

# --- Logins, tmux, screen ---------------------------------------------------------------------
# `who` without the utmp entries tmux (libutempter: host `tmux(<pid>).%<pane>`) and screen
# (host `:<n>…`) write for their own windows: those are judged by the tmux/screen facts below
# (a pane running only a shell isn't use), not as logins.
if command -v who >/dev/null 2>&1; then
    if w=$(timeout 5 who 2>/dev/null); then
        o who "$(printf '%s\n' "$w" | grep -v -E '[(](tmux[(][0-9]+[)][.]%[0-9]+|:[^)]*)[)][[:space:]]*$' | grep -c .)"
    else o who -; fi
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
if [ -e "$Hr/.arena/keep" ]; then o keep 1; else o keep 0; fi
# /workspace (RunPod's volume) — resolved through a link; inside the home it's the home's.
Wr=
if [ -e /workspace ]; then Wr=$(cd /workspace 2>/dev/null && pwd -P); fi
if [ ! -e /workspace ] && [ ! -L /workspace ]; then
    o workspace absent
elif [ -z "$Wr" ]; then
    o workspace err
elif [ "$Wr" = "$Hr" ] || [ "${Wr#"$Hr"/}" != "$Wr" ]; then
    o workspace absent
elif awk -v w="$Wr" '$2 == w { f = 1 } END { exit !f }' /proc/self/mounts 2>/dev/null; then
    e=$(timeout 20 find "$Wr" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)
    rc=$?
    if [ -n "$e" ]; then o workspace has_data; elif [ "$rc" = 0 ]; then o workspace clean; else o workspace err; fi
elif [ -z "$A" ]; then
    o workspace err
else
    e=$(timeout 20 find "$Wr" -xdev -mindepth 1 -newerct "@$A" -print -quit 2>/dev/null)
    rc=$?
    if [ -n "$e" ]; then o workspace has_data; elif [ "$rc" = 0 ]; then o workspace clean; else o workspace err; fi
fi
# Any other writable mount outside the home and the system (a Hetzner volume, a network
# share): data written there since the pod was created (A) is outside the backup.
cands=$(awk -v h="$Hr" -v w="$Wr" '
    BEGIN { n = split("proc sysfs devpts devtmpfs tmpfs cgroup cgroup2 mqueue securityfs debugfs tracefs pstore bpf fusectl configfs hugetlbfs autofs binfmt_misc nsfs ramfs rpc_pipefs efivarfs selinuxfs squashfs overlay nfsd", a, " "); for (i = 1; i <= n; i++) sys[a[i]] = 1 }
    { mp = $2; opt = "," $4 "," }
    sys[$3] { next }
    opt !~ /,rw,/ { next }
    mp == "/" || mp == "/workspace" || mp == w { next }
    mp ~ "^/(proc|sys|dev|run|etc|boot|usr|lib|lib32|lib64|libx32|bin|sbin|snap|var/lib/docker|var/snap)(/|$)" { next }
    h != "" && (mp == h || index(mp, h "/") == 1) { next }
    { print mp }' /proc/self/mounts 2>/dev/null) || cands=ERR
mounts=clean
if [ "$cands" = ERR ]; then
    mounts=err
elif [ -z "$A" ] && [ -n "$cands" ]; then
    mounts=err
else
    while IFS= read -r mp; do
        [ -n "$mp" ] || continue
        case "$mp" in *\\*) mounts=err; continue ;; esac
        [ -d "$mp" ] || continue
        e=$(timeout 10 find "$mp" -xdev -mindepth 1 -newerct "@$A" -print -quit 2>/dev/null)
        rc=$?
        if [ -n "$e" ]; then mounts=has_data; break; fi
        [ "$rc" = 0 ] || mounts=err
    done <<EOF_MOUNTS
$cands
EOF_MOUNTS
fi
o mounts "$mounts"
h=$Hr
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
