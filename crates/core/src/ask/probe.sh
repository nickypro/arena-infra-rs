# arena ask probe (`arena pods ask`): READ-ONLY — it writes nothing anywhere.
#
#   env -i PATH=… HOME="$HOME" sh -c "$THIS" arena-ask-probe <repo> <nonce>
#
# The facts an operator's question needs that the dashboard's metrics probe and the jobs reads
# (sent in the same call, before this) don't give: the pod's clock and uptime, load, disks,
# logins and tmux sessions, the busiest processes, and the repo's working tree. Rendered and
# parsed by arena_core::ask::bundle. POSIX sh (dash on the images); every step that could hang
# runs under `timeout` where the pod has it, KILLed 2 s after its TERM (`-k 2`: a step that ignores
# TERM, or waits on a hung volume, would otherwise hold `timeout` forever). Every git — `busy_op`'s included — is `ro_git`
# (arena_core::metrics::RO_GIT_SH, rendered in below): no optional locks (the index is never
# refreshed, written or locked from under a participant's git), and nothing the repository
# configures is run — no fsmonitor hook, no clean/process filter, no gpg.program. No `git status`.
#
# Output, on stdout: `ARENA_ASK.<nonce> <key>=<value>` lines, and repeated
# `ARENA_ASK_<KIND>.<nonce> <fields>` lines (PS, WHO, TMUX, FILE). The nonce is the call's own
# (hex, else nothing is printed): a value that runs over a line — a path with a newline in it —
# can't make a line of ours without it. Never a file's contents, shell history or the
# environment: process arguments, login names and ttys (not where they come from), tmux session
# names, and file *names* only.
LC_ALL=C
export LC_ALL
GIT_OPTIONAL_LOCKS=0
export GIT_OPTIONAL_LOCKS
REPO=${1:-}
N=${2:-}
case "$N" in '' | *[!0-9a-f]*) exit 3 ;; esac
o() {
    printf 'ARENA_ASK.%s %s=%s\n' "$N" "$1" "$2"
}
num() {
    case "$1" in '' | *[!0-9]*) return 1 ;; *) return 0 ;; esac
}
t() {
    t_secs=$1
    shift
    if command -v timeout >/dev/null 2>&1; then timeout -k 2 "$t_secs" "$@"; else "$@"; fi
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
# This runs in the C locale (env -i), where `who` prints the time as `Oct 20 15:27` (one field
# more than ISO's `2026-10-20 15:27`): every field up to the `(source)` comment is kept, never it.
# The comment only says how the login came: `(tmux(8987).%0)` a tmux pane, `(:0)` or none a local
# one, anything else (an address, a host) over ssh — the first word of each line, before the user.
# Its times in UTC (`TZ=UTC`): in the pod's own zone each would be at that time's offset, and one
# label can't be right for a login from before a clock change.
if command -v who >/dev/null 2>&1; then
    if w=$(TZ=UTC; export TZ; t 5 who 2>/dev/null); then
        o who ok
        printf '%s\n' "$w" |
            awk -v t="ARENA_ASK_WHO.$N" 'NF >= 2 {
                via = "local"
                for (i = 2; i <= NF; i++) if (substr($i, 1, 1) == "(") { via = substr($i, 1, 6) == "(tmux(" ? "tmux" : (substr($i, 1, 2) == "(:" ? "local" : "ssh"); break }
                s = t " " via " " $1; for (i = 2; i <= NF && i <= 6 && substr($i, 1, 1) != "("; i++) s = s " " $i; print s }' |
            head -n 20
    else
        o who err
    fi
else
    o who absent
fi
# tmux, in the C locale, turns a tab in its -F output into `_` (tmux 3.4 on Ubuntu 24.04): the
# fields are `:`-separated, the name last (tmux allows no `:` in one); `-u` keeps a UTF-8 name.
# Asked only when its socket directory is there: `tmux list-sessions` makes that directory when
# it isn't (and then finds no server) — and with no directory no server has run as this user.
# Without a uid there's no directory to look for: unknown, never "no sessions".
if command -v tmux >/dev/null 2>&1; then
    uid=$(id -u 2>/dev/null)
    if [ -z "$uid" ]; then
        o tmux err
    elif [ ! -d "${TMUX_TMPDIR:-/tmp}/tmux-$uid" ]; then
        o tmux ok
    elif out=$(t 5 tmux -u list-sessions -F '#{session_windows}:#{session_attached}:#{session_activity}:#{session_name}' 2>&1); then
        o tmux ok
        printf '%s\n' "$out" | grep . | head -n 20 | sed -e "s/^/ARENA_ASK_TMUX.$N /"
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
        grep -v -e arena-ask-probe -e ' ps -eo pid=' | head -n "$2" | cut -c 1-1000 | sed -e "s/^ */ARENA_ASK_PS.$N /"
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
# @ARENA_RO_GIT_SH@
if [ -z "$REPO" ]; then
    o git none
elif ! cd "$REPO" 2>/dev/null; then
    o git absent
elif ! command -v git >/dev/null 2>&1; then
    o git nogit
else
    # From here every `git` is `ro_git`, busy_op's `git ls-files -u` (an index read: an
    # fsmonitor hook would run) and ro_setup's config read included; `ro_git` itself reaches the
    # binary. Each within 10 s, the whole repo section through (a hung /workspace volume costs
    # this section, never the lines above it). A fact a git couldn't read — it timed out (124,
    # 137) or failed, a fatal error other than the absence asked about included — is said
    # empty, unknown: never "not a checkout", "none" or 0.
    git() { ro_git "$@"; }
    # Whether git, asked `$2…` again, says what `$1` (an ERE, case aside) matches at the start of
    # a line of its stderr — read in the C locale (LC_ALL above), so in git's own English. Asked
    # only after an exit 128, which is every fatal error of git's, not only the absence it means
    # here: a checkout git refuses (dubious ownership), one it can't read (permissions, corrupt
    # metadata). So only the absence's own message says absent; any other is unknown. Asked
    # again because one call can't give its stdout and its stderr apart in POSIX sh without a
    # file, and this probe writes none.
    git_says() {
        gs_re=$1
        shift
        git "$@" 2>&1 >/dev/null | head -c 4000 | grep -Eiq "^($gs_re)"
    }
    ro_timeout=10
    top=$(git rev-parse --show-toplevel 2>/dev/null)
    rc=$?
    if [ "$rc" -ne 0 ] || ! cd "$top" 2>/dev/null; then
        # Not a checkout only when git says so: 128 and "not a git repository".
        if [ "$rc" -eq 128 ] && git_says 'fatal: not a git repository' rev-parse --show-toplevel; then o git notrepo; else o git err; fi
    else
        o git ok
        o real "$(pwd -P)"
        o branch "$(git rev-parse --abbrev-ref HEAD 2>/dev/null)"
        gd=$(git rev-parse --absolute-git-dir 2>/dev/null)
        # busy_op takes an index it can't read for one with nothing unmerged: read it first.
        if [ -z "$gd" ] || ! git ls-files -u >/dev/null 2>&1; then
            o op ""
        else
            op=$(busy_op "$gd" 2>/dev/null)
            if [ -z "$op" ] && { [ -d "$gd/rebase-merge" ] || [ -d "$gd/rebase-apply" ]; }; then op=rebase; fi
            if [ -z "$op" ] && [ -f "$gd/BISECT_LOG" ]; then op=bisect; fi
            o op "${op:--}"
        fi
        # The working tree from plumbing (`ro_changes`: a file only touched may count as
        # modified).
        if ro_setup && s=$(ro_changes 2>/dev/null); then
            o status ok
            o modified "$(printf '%s\n' "$s" | grep . | grep -c -v '^??')"
            o untracked "$(printf '%s\n' "$s" | grep -c '^??')"
            printf '%s\n' "$s" | grep . | head -n 20 | sed -e "s/^/ARENA_ASK_FILE.$N /"
        else
            o status err
        fi
        o last "$(git log -1 --format='%ct %s' 2>/dev/null | head -c 300)"
        # No upstream — `-`, none — only when git says so: 128 and "no upstream configured", "no
        # such branch" (an unborn one) or "HEAD does not point to a branch" (a detached HEAD).
        if up=$(git rev-parse --abbrev-ref '@{u}' 2>/dev/null); then
            o upstream "$up"
        elif [ $? -eq 128 ] && git_says 'fatal: (no upstream configured|no such branch|HEAD does not point to a branch)' rev-parse --abbrev-ref '@{u}'; then
            o upstream -
        else
            o upstream ""
        fi
        if st=$(git stash list 2>/dev/null); then o stashes "$(printf '%s\n' "$st" | grep -c .)"; else o stashes ""; fi
    fi
    ro_timeout=
fi
o done 1
