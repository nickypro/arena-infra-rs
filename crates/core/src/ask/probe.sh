# arena ask probe (`arena pods ask`): READ-ONLY — it writes nothing anywhere.
#
#   env -i PATH=… HOME="$HOME" sh -c "$THIS" arena-ask-probe <repo>
#
# The facts an operator's question needs that the dashboard's metrics probe and the jobs reads
# (sent in the same call, before this) don't give: the pod's clock and uptime, load, disks,
# logins and tmux sessions, the busiest processes, and the repo's working tree. Rendered and
# parsed by arena_core::ask::bundle. POSIX sh (dash on the images); every step that could hang
# runs under `timeout` where the pod has it. git runs with GIT_OPTIONAL_LOCKS=0, so `git status`
# never refreshes (writes) the index or takes its lock from under a participant's git.
#
# Output, on stdout: `ARENA_ASK <key>=<value>` lines, and repeated `ARENA_ASK_<KIND> <fields>`
# lines (PS, WHO, TMUX, FILE). Never a file's contents, shell history or the environment:
# process arguments, login names and ttys (not where they come from), tmux session names, and
# file *names* only.
LC_ALL=C
export LC_ALL
GIT_OPTIONAL_LOCKS=0
export GIT_OPTIONAL_LOCKS
REPO=${1:-}
o() {
    printf 'ARENA_ASK %s=%s\n' "$1" "$2"
}
num() {
    case "$1" in '' | *[!0-9]*) return 1 ;; *) return 0 ;; esac
}
t() {
    t_secs=$1
    shift
    if command -v timeout >/dev/null 2>&1; then timeout "$t_secs" "$@"; else "$@"; fi
}

# --- Clock, uptime, load ------------------------------------------------------------------------
o now "$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)"
up=$(cut -d' ' -f1 /proc/uptime 2>/dev/null)
up=${up%%.*}
o uptime "$up"
o load "$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null)"
o cpus "$(getconf _NPROCESSORS_ONLN 2>/dev/null)"
# PID 1: its name and age — what "up" means in a container, whose /proc/uptime is the host's.
o pid1 "$(head -c 40 /proc/1/comm 2>/dev/null | tr -cd 'A-Za-z0-9._:/-')"
hz=$(getconf CLK_TCK 2>/dev/null)
st=$(sed -e 's/^.*) //' /proc/1/stat 2>/dev/null | cut -d' ' -f20)
if num "$up" && num "$hz" && num "$st" && [ "$hz" -gt 0 ]; then o pid1_age $((up - st / hz)); else o pid1_age -; fi
if command -v nvidia-smi >/dev/null 2>&1; then o nvsmi yes; else o nvsmi no; fi

# --- Disks: used and total KB, and the mount ------------------------------------------------------
dfl() {
    t 10 df -kP "$1" 2>/dev/null | awk 'NR == 2 { print $3 " " $2 " " $6 }'
}
o df_root "$(dfl /)"
o home "$HOME"
o df_home "$(dfl "$HOME")"
if [ -d /workspace ]; then o df_workspace "$(dfl /workspace)"; else o df_workspace none; fi
o home_kb "$(t 10 du -skx "$HOME" 2>/dev/null | cut -f1)"

# --- Logins and tmux ------------------------------------------------------------------------------
if command -v who >/dev/null 2>&1; then
    if w=$(t 5 who 2>/dev/null); then
        o who ok
        printf '%s\n' "$w" | awk 'NF >= 2 { print "ARENA_ASK_WHO " $1 " " $2 " " $3 " " $4 }' | head -n 20
    else
        o who err
    fi
else
    o who absent
fi
if command -v tmux >/dev/null 2>&1; then
    if out=$(t 5 tmux list-sessions -F '#{session_name}	#{session_windows}	#{session_attached}	#{session_activity}' 2>&1); then
        o tmux ok
        printf '%s\n' "$out" | grep . | head -n 20 | sed -e 's/^/ARENA_ASK_TMUX /'
    else
        case "$out" in
            *"no server running"* | *"error connecting to"* | *"No such file or directory"*) o tmux ok ;;
            *) o tmux err ;;
        esac
    fi
else
    o tmux absent
fi

# --- Processes: the busiest by CPU (lifetime average, as ps reports it), then by memory -----------
pss() {
    t 10 ps -eo pid=,user=,pcpu=,pmem=,etimes=,args= --sort="$1" 2>/dev/null |
        grep -v -e arena-ask-probe -e ' ps -eo pid=' | head -n "$2" | cut -c 1-1000 | sed -e 's/^ */ARENA_ASK_PS /'
}
if t 10 ps -eo pid=,etimes= --sort=-pcpu >/dev/null 2>&1; then
    o ps ok
    pss -pcpu 12
    pss -rss 6
else
    o ps unavailable
fi

# --- The repo ------------------------------------------------------------------------------------
# @ARENA_BUSY_OP_SH@
if [ -z "$REPO" ]; then
    o git none
elif ! cd "$REPO" 2>/dev/null; then
    o git absent
elif ! command -v git >/dev/null 2>&1; then
    o git nogit
elif ! top=$(git rev-parse --show-toplevel 2>/dev/null) || ! cd "$top" 2>/dev/null; then
    o git notrepo
else
    o git ok
    o real "$(pwd -P)"
    o branch "$(git rev-parse --abbrev-ref HEAD 2>/dev/null)"
    gd=$(git rev-parse --absolute-git-dir 2>/dev/null)
    op=$(busy_op "$gd" 2>/dev/null)
    if [ -z "$op" ] && { [ -d "$gd/rebase-merge" ] || [ -d "$gd/rebase-apply" ]; }; then op=rebase; fi
    if [ -z "$op" ] && [ -f "$gd/BISECT_LOG" ]; then op=bisect; fi
    o op "${op:--}"
    if s=$(t 20 git status --porcelain=v1 --untracked-files=normal 2>/dev/null); then
        o status ok
        o modified "$(printf '%s\n' "$s" | grep . | grep -c -v '^??')"
        o untracked "$(printf '%s\n' "$s" | grep -c '^??')"
        printf '%s\n' "$s" | grep . | head -n 20 | sed -e 's/^/ARENA_ASK_FILE /'
    else
        o status err
    fi
    o last "$(git log -1 --format='%ct %s' 2>/dev/null | head -c 300)"
    o upstream "$(git rev-parse --abbrev-ref '@{u}' 2>/dev/null)"
    o stashes "$(git stash list 2>/dev/null | wc -l | tr -cd '0-9')"
fi
o done 1
