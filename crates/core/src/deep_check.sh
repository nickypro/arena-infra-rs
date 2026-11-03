#!/usr/bin/env bash
# `arena pods test --deep`: gather one pod's health facts and print them as `key=value`.
# Its first argument is the mode: `deep` (everything), `keys` (`pods test --keys`: only the keys'
# facts, in a few seconds) or `course` (`pods test --course`: what a participant uses beside the
# machine, no GPU work). Then, for the course facts (`deep` and `course` only; without them none
# are gathered): the course repo's path, the course env's candidates (`:`-separated, most likely
# first), the deploy key's path (`GIT_SSH_KEY_REMOTE`, may be empty), `1` to make the one Claude
# call that uses the machine's account (`pods test --claude-call`; else `0`), and the cohort repo
# (`owner/name`, may be empty) the dry-run push goes to.
#
# Contract with the parser (arena_core::health::parse_deep):
#   * stdout carries ONLY `key=value` lines: one fact per line, single-line values. The
#     first is `deep_check=<version>` (anything before it, e.g. shell-rc chatter, is
#     ignored) and the last is `deep_check_end=1` (absent = the run was cut off).
#   * a fact that couldn't be measured says so (`missing`, `unknown`, `error: …`); a key
#     that never appears also reads as unknown.
#   * every sub-step is bounded with `timeout` and the network probe runs alongside the
#     GPU checks, so a run ends in under ~2 minutes even on a sick host (worst case ≈
#     10+10+10 nvidia-smi, 5 driver file, 75 python, 5+5 df; the caller's budget is
#     DEEP_CHECK_TIMEOUT). The keys' calls run alongside too (15s each), and so do all of the
#     course's probes, from the start: the repo's metadata and the dry-run push (25s + 20s),
#     Claude Code (10s + 25s), the notebook kernel (50s), uv (5s).
#   * no secret is ever printed: a key or token is reported by its fingerprint only.
# Read-only apart from its own temp dir (removed on exit): the course repo's git metadata is read
# (never its files, never its index written), the push is a dry run, the kernel runs with its
# runtime and IPython dirs in the temp dir and is stopped before the end. Needs no TTY, reads no
# stdin. The caller runs it inside the participants' login env (conda env active), so `python`
# is the interpreter participants actually use, and the keys are those their shell exports.
# No tracing, whatever the login environment says (an exported SHELLOPTS=xtrace, a `set -x`
# left on an image): a trace would print the keys this script expands.
set +xv
set -u
export LC_ALL=C
mode=${1:-deep}
course=0
if [ "$#" -ge 3 ] && { [ "$mode" = deep ] || [ "$mode" = course ]; }; then course=1; fi
course_repo=${2:-}
course_envs=${3:-}
course_key=${4:-}
claude_call=${5:-0}
course_origin=${6:-}

echo "deep_check=3"

# One fact per line: newlines/tabs flattened, clipped to 300 chars — and every secret the
# shell exports that this script handles scrubbed out of it, whatever printed it (a belt to the
# braces there).
kv() {
  local v=$2 s
  for s in "${OPENROUTER_API_KEY:-}" "${HF_TOKEN:-}" "${WANDB_API_KEY:-}" "${ANTHROPIC_API_KEY:-}" "${ANTHROPIC_AUTH_TOKEN:-}" "${CLAUDE_CODE_OAUTH_TOKEN:-}"; do
    [ "${#s}" -lt 8 ] || v=${v//"$s"/[secret]}
  done
  v=$(printf '%s' "$v" | tr '\r\n\t' '   ' | cut -c1-300)
  printf '%s=%s\n' "$1" "$v"
}
have() { command -v "$1" >/dev/null 2>&1; }
# `bounded <secs> cmd…`: TERM at <secs>, KILL 2s later; exit 124/137 = timed out.
bounded() {
  local secs=$1
  shift
  if have timeout; then timeout -k 2 "$secs" "$@"; else "$@"; fi
}
first_line() { printf '%s\n' "$1" | sed -n '/[^[:space:]]/{p;q;}'; }
trim() {
  local s=$1
  s=${s#"${s%%[![:space:]]*}"}
  s=${s%"${s##*[![:space:]]}"}
  printf '%s' "$s"
}

work=$(mktemp -d /tmp/arena_deep.XXXXXX 2>/dev/null) || work=""
if [ -z "$work" ]; then
  kv error "mktemp failed (is /tmp full or read-only?)"
  echo "deep_check_end=1"
  exit 0
fi
# The notebook kernel the course probe starts (its own session: `kernel_stop`) goes with the temp
# dir, however the run ends — a timeout's TERM included.
trap 'kernel_stop; rm -rf "$work"' EXIT
trap 'exit 1' HUP INT TERM
kernel_stop() {
  local p
  p=$(cat "$work/kernel.pid" 2>/dev/null)
  case $p in
    '' | *[!0-9]*) return 0 ;;
  esac
  kill -9 -- "-$p" 2>/dev/null
  kill -9 "$p" 2>/dev/null
  rm -f "$work/kernel.pid"
  return 0
}

# ---- keys: do the participant's OpenRouter key, Hugging Face token and W&B key work? ----
# `$OPENROUTER_API_KEY`, `$HF_TOKEN` and `$WANDB_API_KEY` as the login shell exports them, each
# asked of its service once, in the background: OpenRouter's `GET /api/v1/key` (free, spends
# nothing: the key's cap, what's left of it, whether it's disabled; a bad key gets 401), Hugging
# Face's `whoami-v2`, and W&B's GraphQL `viewer` (free; a bad key gets 200 with no viewer). The
# secret never leaves this script: curl reads it on stdin (`-K -`), so it is on no command line
# (`ps` shows those), and only its fingerprint is printed — the first 12 hex of its SHA-256,
# which the control box compares with its keys CSV and config — never the key, nor OpenRouter's
# redacted label of it. ARENA_TEST_OPENROUTER_URL / ARENA_TEST_HF_URL / ARENA_TEST_WANDB_URL:
# tests only (a local stub server).
OR_KEY_URL=${ARENA_TEST_OPENROUTER_URL:-https://openrouter.ai/api/v1/key}
HF_WHOAMI_URL=${ARENA_TEST_HF_URL:-https://huggingface.co/api/whoami-v2}
WANDB_URL=${ARENA_TEST_WANDB_URL:-${WANDB_BASE_URL:-https://api.wandb.ai}/graphql}
or_key=${OPENROUTER_API_KEY:-}
hf_key=${HF_TOKEN:-}
wb_key=${WANDB_API_KEY:-}

# `secret_probe <tag> <name> <secret> <url> [curl args…]`: one bounded call carrying the secret
# as curl's config option `name` (`header` for a Bearer token, `user` for basic auth), the
# option handed to curl on stdin (`printf` is a builtin: no process carries it either). `-q`
# first: no ~/.curlrc, whose `trace-ascii` or `verbose` would log it. Leaves $work/<tag>.code (the
# HTTP status), .body, .err and .rc.
secret_probe() {
  local tag=$1 name=$2 s=${3//\\/\\\\} url=$4
  shift 4
  s=${s//\"/\\\"}
  printf '%s = "%s"\n' "$name" "$s" |
    bounded 15 curl -q -sS --connect-timeout 5 --max-time 10 -K - -o "$work/$tag.body" -w '%{http_code}' "$@" "$url" \
      >"$work/$tag.code" 2>"$work/$tag.err"
  echo "$?" >"$work/$tag.rc"
}

# `key_probe <tag> <secret> <url>`: a GET with the secret as its Bearer token.
key_probe() {
  secret_probe "$1" header "Authorization: Bearer $2" "$3"
}

# A service's or curl's message, safe to print: the secrets out, then anything shaped like a
# key, a redacted label of one or a bearer token (`sk-…`, `hf_…`, `Bearer …`) — before it is
# clipped, so no part of a secret survives the cut — then clipped to 160 characters.
scrub() {
  local v=$1 s
  for s in "$or_key" "$hf_key" "$wb_key" "${ANTHROPIC_API_KEY:-}" "${ANTHROPIC_AUTH_TOKEN:-}" "${CLAUDE_CODE_OAUTH_TOKEN:-}"; do
    [ "${#s}" -lt 8 ] || v=${v//"$s"/[secret]}
  done
  printf '%s' "$v" | tr '\r\n\t' '   ' |
    sed -E 's/[Bb]earer[[:space:]]+[^[:space:]"]+/Bearer [secret]/g; s/(^|[^A-Za-z0-9])(sk|hf)[-_][A-Za-z0-9._~-]+/\1[secret]/g' |
    cut -c1-160
}

# The first 12 hex of the secret's SHA-256 (it goes to the hasher on stdin); nothing when no
# hasher is installed.
fingerprint() {
  local h=""
  if have sha256sum; then
    h=$(printf '%s' "$1" | sha256sum 2>/dev/null)
  elif have shasum; then
    h=$(printf '%s' "$1" | shasum -a 256 2>/dev/null)
  elif have openssl; then
    h=$(printf '%s' "$1" | openssl dgst -sha256 -r 2>/dev/null)
  fi
  h=${h%% *}
  case $h in
    [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]*) printf '%s' "${h:0:12}" ;;
  esac
}

# One scalar of a small JSON answer (`"name": value`, the first one): a string's text, a
# number, `true`/`false`, `none` for null; nothing when absent. For OpenRouter's flat `data`
# object and an error's `message`, not JSON at large.
json_value() {
  head -c 16384 "$2" 2>/dev/null | tr '\r\n\t' '   ' |
    grep -oE "\"$1\"[[:space:]]*:[[:space:]]*(\"[^\"]*\"|-?[0-9][0-9.eE+-]*|true|false|null)" | head -n 1 |
    sed -E 's/^"[^"]*"[[:space:]]*:[[:space:]]*//; s/^"(.*)"$/\1/; s/^null$/none/'
}

keys_pids=""
keys_start() {
  if [ -n "$or_key" ] && have curl; then
    (key_probe openrouter "$or_key" "$OR_KEY_URL") </dev/null &
    keys_pids="$keys_pids $!"
  fi
  if [ -n "$hf_key" ] && have curl; then
    (key_probe hf "$hf_key" "$HF_WHOAMI_URL") </dev/null &
    keys_pids="$keys_pids $!"
  fi
  if [ -n "$wb_key" ] && have curl; then
    (secret_probe wandb user "api:$wb_key" "$WANDB_URL" -H 'Content-Type: application/json' \
      --data '{"query":"query { viewer { id } }"}') </dev/null &
    keys_pids="$keys_pids $!"
  fi
}

# `key_facts <tag> <secret>`: `<tag>.key=missing|set`, its fingerprint, and what its service
# answered (`.curl_exit`, `.http`, `.error` — curl's, or the answer's `message`/`error`).
key_facts() {
  local rc code msg
  if [ -z "$2" ]; then
    kv "$1.key" missing
    return 1
  fi
  kv "$1.key" set
  kv "$1.fingerprint" "$(fingerprint "$2")"
  if ! have curl; then
    kv "$1.curl_exit" missing
    return 1
  fi
  rc=$(cat "$work/$1.rc" 2>/dev/null)
  code=$(cut -c1-3 "$work/$1.code" 2>/dev/null)
  kv "$1.curl_exit" "${rc:-unknown}"
  kv "$1.http" "${code:-unknown}"
  if [ "${rc:-1}" != 0 ]; then
    msg=$(first_line "$(cat "$work/$1.err" 2>/dev/null)")
    kv "$1.error" "$(scrub "${msg:-curl exit ${rc:-unknown}}")"
    return 1
  fi
  case $code in
    2??) return 0 ;;
  esac
  msg=$(json_value message "$work/$1.body")
  [ -n "$msg" ] || msg=$(json_value error "$work/$1.body")
  [ -z "$msg" ] || kv "$1.error" "$(scrub "$msg")"
  return 1
}

keys_report() {
  local p f v
  for p in $keys_pids; do
    wait "$p" 2>/dev/null
  done
  if key_facts openrouter "$or_key"; then
    for f in limit limit_remaining limit_reset usage disabled; do
      v=$(json_value "$f" "$work/openrouter.body")
      [ -z "$v" ] || kv "openrouter.$f" "$v"
    done
  fi
  key_facts hf "$hf_key"
  # W&B answers a key it doesn't know with 200 and `"viewer": null`.
  if key_facts wandb "$wb_key"; then
    v=$(json_value viewer "$work/wandb.body")
    if [ "$v" = none ]; then
      kv wandb.viewer no
    elif [ -n "$(json_value id "$work/wandb.body")" ]; then
      kv wandb.viewer yes
    else
      # Neither: a GraphQL error (HTTP 200 too), an empty answer — not a verdict on the key.
      kv wandb.viewer unknown
      v=$(json_value message "$work/wandb.body")
      kv wandb.error "$(scrub "${v:-no viewer in the answer}")"
    fi
  fi
}

# ---- the course: what a participant uses beside the machine (`deep` and `course` modes) ----
# The machine's identity (what `pods test` compares with what the proxy port reaches), the
# course repo's git metadata, a dry-run push with the deploy key, Claude Code, the notebook
# kernel of the course env, and uv. The repo is read through git's metadata only — refs, config,
# the operation markers, never a file of the work — with no optional locks, so the participant's
# own git never meets a lock of ours. `ARENA_TEST_ANTHROPIC_URL`: tests only.
# @ARENA_IDENTITY_SH@
# @ARENA_ORIGIN_SH@
# @ARENA_BUSY_OP_SH@
export GIT_OPTIONAL_LOCKS=0
ANTHROPIC_URL=${ARENA_TEST_ANTHROPIC_URL:-https://api.anthropic.com}
course_pids=""

# `in_repo git …`: a git command in the course repo, bounded (fsmonitor hooks off: none runs).
in_repo() {
  (cd "$course_repo" 2>/dev/null && bounded 5 git -c core.fsmonitor=false "$@")
}

# The repo's metadata, then the dry-run push, in one background job (git can be slow on a big
# checkout or a network-mounted one): `repo.*` lines into $work/repo.facts, and the push's files.
repo_job() {
  local gd branch op urls u keys=""
  {
    printf 'repo.path=%s\n' "$course_repo"
    if [ ! -e "$course_repo" ]; then
      echo "repo.state=missing"
      return 0
    fi
    if ! have git; then
      echo "repo.state=error: no git on PATH"
      return 0
    fi
    if ! gd=$(in_repo rev-parse --absolute-git-dir 2>"$work/git.err"); then
      printf 'repo.state=error: %s\n' "$(first_line "$(cat "$work/git.err" 2>/dev/null)")"
      return 0
    fi
    echo "repo.state=ok"
    branch=$(in_repo symbolic-ref -q --short HEAD 2>/dev/null)
    if [ -n "$branch" ]; then
      printf 'repo.branch=%s\n' "$branch"
    else
      printf 'repo.detached=%s\n' "$(in_repo rev-parse --short HEAD 2>/dev/null)"
    fi
    op=$(cd "$course_repo" 2>/dev/null && busy_op "$gd" 2>/dev/null | head -n 1)
    printf 'repo.op=%s\n' "${op:-none}"
    urls=$({ in_repo remote get-url --all origin; in_repo remote get-url --push --all origin; } 2>/dev/null)
    u=$(first_line "$urls")
    printf 'repo.origin=%s\n' "$(origin_shown "${u:-}")"
    while IFS= read -r u; do
      [ -z "$u" ] || keys="$keys $(origin_key "$u")"
    done <<URLS
$urls
URLS
    printf 'repo.origin_keys=%s\n' "$(trim "$keys")"
    printf '%s' "$branch" >"$work/repo.branch"
    : >"$work/repo.ok"
  } >"$work/repo.facts"
  push_probe
}

# A dry run of the push the backup makes, with its deploy key, to the cohort repo's SSH URL —
# never origin's own URL, which could carry a credential to a command line or a credential
# helper: GitHub answers a push for the key (a read-only key, or one not on the repo, is refused)
# and nothing is sent. No hook runs (`--no-verify`, no hooks path), no credential helper, and a
# host key ssh hasn't seen goes into the temp dir's known-hosts file, never the machine's.
push_probe() {
  local key=$course_key ref s
  [ -e "$work/repo.ok" ] || return 0
  if [ -z "$course_origin" ]; then
    echo none >"$work/push.key"
    return 0
  fi
  case $key in "~/"*) key=$HOME/${key#"~/"} ;; esac
  if [ -n "$key" ] && [ ! -r "$key" ]; then
    echo missing >"$work/push.key"
    return 0
  fi
  ref=$(cat "$work/repo.branch" 2>/dev/null)
  s="ssh -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new"
  s="$s -o 'UserKnownHostsFile=$work/known_hosts $HOME/.ssh/known_hosts'"
  [ -z "$key" ] || s="$s -i '$key' -o IdentitiesOnly=yes"
  (
    cd "$course_repo" &&
      GIT_SSH_COMMAND=$s GIT_TERMINAL_PROMPT=0 bounded 20 git -c core.hooksPath=/dev/null -c core.fsmonitor=false \
        -c credential.helper= push --dry-run --no-verify --porcelain "git@github.com:$course_origin.git" \
        "HEAD:refs/heads/${ref:-arena-push-check}" >"$work/push.out" 2>"$work/push.err"
    echo "$?" >"$work/push.rc"
  )
}

push_report() {
  local rc msg
  [ -e "$work/repo.ok" ] || return 0
  case $(cat "$work/push.key" 2>/dev/null) in
    missing)
      kv push.key missing
      return 0
      ;;
    none)
      kv push.key none
      return 0
      ;;
  esac
  kv push.key "$([ -n "$course_key" ] && echo set || echo default)"
  rc=$(cat "$work/push.rc" 2>/dev/null)
  kv push.exit "${rc:-unknown}"
  if [ "${rc:-1}" != 0 ]; then
    # GitHub's own words first (`ERROR: …`, `… denied …`), else ssh's or git's first line.
    msg=$(grep -m 1 -iE 'error|denied|not found|read only|could not|timed out|refused' "$work/push.err" 2>/dev/null)
    [ -n "$msg" ] || msg=$(first_line "$(cat "$work/push.err" 2>/dev/null)")
    kv push.error "$(scrub "${msg:-git push exit ${rc:-unknown}}")"
  fi
}

# Claude Code as root's login shell finds it: `claude --version`, and which auth it would use
# (the python below; kinds and fingerprints only), asked of its service where that is free.
claude_start() {
  local py
  py=$(command -v python3 2>/dev/null || command -v python 2>/dev/null)
  (
    if have claude; then
      bounded 10 claude --version >"$work/claude.ver" 2>"$work/claude.err"
      echo "$?" >"$work/claude.rc"
    fi
    if [ -n "$py" ]; then
      cat >"$work/claude_auth.py" <<'CLAUDEPY'
@ARENA_CLAUDE_PY@
CLAUDEPY
      ARENA_ANTHROPIC_URL=$ANTHROPIC_URL ARENA_OPENROUTER_URL=$OR_KEY_URL \
        bounded 25 "$py" "$work/claude_auth.py" "$claude_call" >"$work/claude.auth" 2>"$work/claude.pyerr"
    else
      echo "claude.auth=unknown" >"$work/claude.auth"
      echo "claude.auth_error=no python3 to read Claude Code's settings" >>"$work/claude.auth"
    fi
  ) </dev/null &
  course_pids="$course_pids $!"
}

# Only the python's own `claude.*` facts, through `kv` (one line each, clipped, scrubbed).
pass_facts() {
  local line
  while IFS= read -r line; do
    case $line in
      "$2".*=*) kv "${line%%=*}" "${line#*=}" ;;
    esac
  done <"$1"
}

claude_report() {
  local rc
  if ! have claude; then
    kv claude.path missing
  else
    kv claude.path "$(command -v claude)"
    rc=$(cat "$work/claude.rc" 2>/dev/null)
    if [ "${rc:-1}" = 0 ]; then
      kv claude.version "$(first_line "$(cat "$work/claude.ver" 2>/dev/null)")"
    elif [ "$rc" = 124 ] || [ "$rc" = 137 ]; then
      kv claude.version_error "timed out after 10s"
    else
      kv claude.version_error "$(scrub "$(first_line "$(cat "$work/claude.err" "$work/claude.ver" 2>/dev/null)")") (exit ${rc:-unknown})"
    fi
  fi
  [ ! -f "$work/claude.auth" ] || pass_facts "$work/claude.auth" claude
}

# The notebook kernel of the course env: the first candidate with a `bin/python`, its Jupyter's
# kernels for that python, and one of them started, `import torch` run in it, and stopped (the
# python below). Its runtime files and IPython's go into the temp dir; its process group is
# killed at the end whatever happened (`kernel_stop`).
kernel_env=""
kernel_start() {
  local c rest=$course_envs
  while [ -n "$rest" ]; do
    c=${rest%%:*}
    if [ "$c" = "$rest" ]; then rest=""; else rest=${rest#*:}; fi
    case $c in "~/"*) c=$HOME/${c#"~/"} ;; esac
    if [ -n "$c" ] && [ -x "$c/bin/python" ]; then
      kernel_env=$c
      break
    fi
  done
  [ -n "$kernel_env" ] || return 0
  mkdir -p "$work/jupyter-runtime" "$work/ipython"
  cat >"$work/kernel.py" <<'KERNELPY'
@ARENA_KERNEL_PY@
KERNELPY
  (
    JUPYTER_RUNTIME_DIR=$work/jupyter-runtime IPYTHONDIR=$work/ipython PYTHONDONTWRITEBYTECODE=1 \
      bounded 50 "$kernel_env/bin/python" "$work/kernel.py" "$kernel_env/bin/python" "$work/kernel.pid" \
      >"$work/kernel.out" 2>"$work/kernel.err"
    echo "$?" >"$work/kernel.rc"
  ) </dev/null &
  course_pids="$course_pids $!"
}

kernel_report() {
  local rc
  kernel_stop
  if [ -z "$kernel_env" ]; then
    kv kernel.env none
    return 0
  fi
  kv kernel.env "$kernel_env"
  pass_facts "$work/kernel.out" kernel
  rc=$(cat "$work/kernel.rc" 2>/dev/null)
  case $rc in
    0) ;;
    124 | 137) kv kernel.error "the kernel check timed out after 50s" ;;
    *) kv kernel.error "$(scrub "$(tail -n 1 "$work/kernel.err" 2>/dev/null)") (exit ${rc:-unknown})" ;;
  esac
}

# Every course probe starts here, in the background, alongside the machine's checks.
course_start() {
  [ "$course" = 1 ] || return 0
  (repo_job) </dev/null &
  course_pids="$course_pids $!"
  claude_start
  kernel_start
  (
    u=$(command -v uv 2>/dev/null)
    printf 'uv.path=%s\n' "${u:-missing}"
    [ -z "$u" ] || printf 'uv.version=%s\n' "$(first_line "$(bounded 5 uv --version 2>/dev/null)")"
  ) </dev/null >"$work/uv.facts" &
  course_pids="$course_pids $!"
}

course_report() {
  local p
  [ "$course" = 1 ] || return 0
  arena_identity
  for p in $course_pids; do
    wait "$p" 2>/dev/null
  done
  if [ -s "$work/repo.facts" ]; then
    pass_facts "$work/repo.facts" repo
  else
    kv repo.path "$course_repo"
    kv repo.state "error: the repo check didn't finish"
  fi
  push_report
  claude_report
  kernel_report
  [ ! -s "$work/uv.facts" ] || pass_facts "$work/uv.facts" uv
}

if [ "$mode" = keys ]; then
  keys_start
  keys_report
  echo "deep_check_end=1"
  exit 0
fi
if [ "$mode" = course ]; then
  course_start
  course_report
  echo "deep_check_end=1"
  exit 0
fi

# ---- network, in the background: a 32 MiB byte range of a public Hugging Face file ----
# No token is sent (the file is public), so this never spends the shared HF rate limit.
HF_URL="https://huggingface.co/openai-community/gpt2/resolve/main/model.safetensors"
HF_RANGE_BYTES=33554432
net_pid=""
if have curl; then
  (
    bounded 40 curl -sS -L --connect-timeout 10 --max-time 30 -r "0-$((HF_RANGE_BYTES - 1))" \
      -o /dev/null -w '%{http_code} %{size_download} %{time_total}' "$HF_URL" \
      >"$work/net.out" 2>"$work/net.err"
    echo "$?" >"$work/net.rc"
  ) </dev/null &
  net_pid=$!
fi
keys_start
course_start

# ---- host: load is the *host's* (containers share the kernel), so is uptime ----
if read -r l1 l5 l15 _rest 2>/dev/null </proc/loadavg; then
  kv load1 "$l1"
  kv load5 "$l5"
  kv load15 "$l15"
else
  kv load1 unknown
fi
cpus=$(grep -c '^processor' /proc/cpuinfo 2>/dev/null)
kv cpus "${cpus:-unknown}"
kv nproc "$(nproc 2>/dev/null || echo unknown)"
up=$(cut -d' ' -f1 /proc/uptime 2>/dev/null)
up=${up%%.*}
kv uptime_secs "${up:-unknown}"

# ---- nvidia-smi: driver + GPUs as the machine sees them ----
if have nvidia-smi; then
  out=$(bounded 10 nvidia-smi 2>&1)
  rc=$?
  if [ "$rc" -eq 0 ]; then
    kv smi ok
    cuda=$(printf '%s\n' "$out" | sed -n 's/.*CUDA Version: *\([0-9][0-9.]*\).*/\1/p' | head -n 1)
    kv smi_cuda "${cuda:-unknown}"
    q=$(bounded 10 nvidia-smi --query-gpu=index,name,driver_version,memory.total,memory.used \
      --format=csv,noheader,nounits 2>&1)
    rc=$?
    if [ "$rc" -eq 0 ]; then
      n=0
      while IFS=, read -r idx name drv mtot mused _rest; do
        idx=$(trim "$idx")
        case $idx in '' | *[!0-9]*) continue ;; esac
        kv "gpu.$idx.name" "$(trim "$name")"
        kv "gpu.$idx.driver" "$(trim "$drv")"
        kv "gpu.$idx.mem_total_mib" "$(trim "$mtot")"
        kv "gpu.$idx.mem_used_mib" "$(trim "$mused")"
        n=$((n + 1))
      done <<GPUS
$q
GPUS
      kv smi_gpus "$n"
    else
      msg=$(first_line "$q")
      kv smi_query "error: ${msg:-exit $rc}"
    fi
    # MIG: the pod gets a slice ("  MIG 1g.34gb  Device 0: (UUID: MIG-…)" under its parent
    # in `nvidia-smi -L`); --query-gpu names only the parent card.
    lst=$(bounded 10 nvidia-smi -L 2>/dev/null)
    mig=$(printf '%s\n' "$lst" | grep -c '^[[:space:]]*MIG [^[:space:]]')
    if [ "${mig:-0}" -gt 0 ] 2>/dev/null; then
      kv mig_devices "$mig"
      kv mig_profile "$(printf '%s\n' "$lst" | sed -n 's/^[[:space:]]*MIG \([^[:space:]]*\).*/\1/p' | head -n 1)"
    fi
  elif [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
    kv smi "error: nvidia-smi timed out after 10s"
  else
    msg=$(first_line "$out")
    kv smi "error: ${msg:-exit $rc}"
  fi
else
  kv smi missing
fi

# ---- the driver as its kernel module reports it ----
# A container usually sees this file even where NVML is blocked (a MIG slice's "Insufficient
# Permissions", "No devices were found"), so it stands in for nvidia-smi's driver version.
# The `NVRM version:` line's first dotted number. ARENA_TEST_NVIDIA_VERSION_FILE: tests only
# (a fake file — the test machines have no GPU).
nvver=${ARENA_TEST_NVIDIA_VERSION_FILE:-/proc/driver/nvidia/version}
if [ -r "$nvver" ]; then
  v=$(bounded 5 head -c 4096 "$nvver" 2>/dev/null | sed -n 's/^NVRM version:\(.*\)$/\1/p' | head -n 1 |
    grep -oE '(^|[[:space:]])[0-9]+(\.[0-9]+)+([[:space:]]|$)' | head -n 1)
  v=$(trim "$v")
  kv driver_proc "${v:-unknown}"
else
  kv driver_proc missing
fi

# ---- torch / CUDA (python, written to a file: the NCCL test's spawned workers re-import it) ----
PY=$(command -v python 2>/dev/null || command -v python3 2>/dev/null)
if [ -z "$PY" ]; then
  kv python missing
else
  kv python "$PY"
  cat >"$work/deep_check.py" <<'PYEOF'
"""arena pods test --deep: the torch/CUDA half. Prints only key=value lines (contract in
the bash header). Everything runs under the __main__ guard because the NCCL test spawns
worker processes that re-import this file."""
import datetime
import queue
import socket
import time
import warnings

NCCL_BUDGET = 30.0  # seconds for the whole NCCL test: spawn + init + all_reduce
MAX_NCCL_RANKS = 8


def one(v, n=300):
    return " ".join(str(v).split())[:n]


def kv(k, v):
    print(f"{k}={one(v)}", flush=True)  # flushed, so a later hang keeps earlier facts


def err(e):
    return f"error: {type(e).__name__}: {e}"


def fact(k, get):
    """A fact only torch can give when nvidia-smi can't answer (a MIG slice): printed when
    `get()` works, left out when it raises (an older torch, a slice's limits) — never failing
    the run or the checks after it."""
    try:
        kv(k, get())
    except BaseException:
        pass


def cuda_version(v):
    """A CUDA version as the driver API encodes it (1000 * major + 10 * minor) as major.minor:
    13020 -> 13.2. Raises on anything that isn't one (0, negative, not a number)."""
    v = int(v)
    if v <= 0:
        raise ValueError(v)
    return f"{v // 1000}.{v % 1000 // 10}"


def driver_cuda(torch):
    """The newest CUDA the driver supports: nvidia-smi's "CUDA Version", for when nvidia-smi
    can't run. Asked of the driver library itself (cuDriverGetVersion, which needs no
    context); torch's private reading of it only as a second try - torch 2.14 has none (live,
    a B300 MIG slice: never known). Raises when neither answers, so `fact` leaves it out."""
    try:
        import ctypes

        v = ctypes.c_int()
        if ctypes.CDLL("libcuda.so.1").cuDriverGetVersion(ctypes.byref(v)) == 0:
            return cuda_version(v.value)
    except BaseException:
        pass
    return cuda_version(torch._C._cuda_getDriverVersion())


def tensor_check(torch, i):
    """A small exact integer op plus a matmul (cuBLAS), synchronized, on GPU i."""
    dev = f"cuda:{i}"
    n = 1 << 20
    x = torch.arange(n, device=dev, dtype=torch.int64)
    total = int((x * 2 + 1).sum().item())
    a = torch.randn(256, 256, device=dev)
    m = float((a @ a).abs().sum().item())
    torch.cuda.synchronize(i)
    if total != n * n:
        return f"wrong result: sum {total} != {n * n}"
    if m != m or m == float("inf"):
        return "wrong result: matmul is not finite"
    return "ok"


def peer_check(torch, i, j):
    """GPU i -> GPU j copy must arrive intact (bad hosts return garbage, not errors)."""
    x = torch.randn(4 << 20, device=f"cuda:{i}")  # 16 MiB of float32
    y = x.to(f"cuda:{j}")
    torch.cuda.synchronize(i)
    torch.cuda.synchronize(j)
    return "ok" if torch.equal(y.cpu(), x.cpu()) else "mismatch"


def nccl_worker(rank, world, port, results):
    try:
        import torch
        import torch.distributed as dist

        torch.cuda.set_device(rank)
        dist.init_process_group(
            "nccl",
            init_method=f"tcp://127.0.0.1:{port}",
            rank=rank,
            world_size=world,
            timeout=datetime.timedelta(seconds=NCCL_BUDGET),
        )
        t = torch.full((1024,), float(rank + 1), device=f"cuda:{rank}")
        dist.all_reduce(t)
        torch.cuda.synchronize(rank)
        want = world * (world + 1) / 2
        got = t.cpu()
        ok = bool((got == want).all())
        results.put((rank, "ok" if ok else f"wrong result {got[0].item()} != {want}"))
        dist.destroy_process_group()
    except BaseException as e:  # report it; never leave the parent waiting
        results.put((rank, err(e)))


def nccl_check(world):
    import multiprocessing as mp

    ctx = mp.get_context("spawn")  # CUDA is already initialised here, so no fork
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    results = ctx.Queue()
    procs = [
        ctx.Process(target=nccl_worker, args=(r, world, port, results), daemon=True)
        for r in range(world)
    ]
    for p in procs:
        p.start()
    deadline = time.monotonic() + NCCL_BUDGET
    got = {}
    while len(got) < world and time.monotonic() < deadline:
        try:
            rank, res = results.get(timeout=0.5)
            got[rank] = res
        except queue.Empty:
            if not any(p.is_alive() for p in procs):
                break
    while True:  # anything a worker put just before exiting
        try:
            rank, res = results.get_nowait()
            got[rank] = res
        except queue.Empty:
            break
    for p in procs:
        if p.is_alive():
            p.kill()
        p.join(timeout=2)
    bad = [f"rank {r}: {got[r]}" for r in sorted(got) if got[r] != "ok"]
    if bad:
        return bad[0]
    missing = [r for r in range(world) if r not in got]
    if missing:
        died = [f"rank {r} died (exit {procs[r].exitcode})" for r in missing
                if procs[r].exitcode not in (None, -9)]
        if died:
            return "error: " + ", ".join(died)
        ranks = ", ".join(str(r) for r in missing)
        return f"error: timeout after {NCCL_BUDGET:.0f}s (no result from rank {ranks})"
    return "ok"


def main():
    try:
        import torch
    except BaseException as e:
        kv("torch", err(e))
        return
    kv("torch", "ok")
    kv("torch_version", torch.__version__)
    kv("torch_cuda", torch.version.cuda or "none")

    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        try:
            available = bool(torch.cuda.is_available())
        except BaseException:
            available = False
    kv("cuda_available", "true" if available else "false")
    if not available:
        # is_available() only warns; init() raises the real reason (cuInit 999, an old
        # driver, no driver at all).
        reason = ""
        try:
            torch.cuda.init()
        except BaseException as e:
            reason = err(e)
        if not reason:
            msgs = [str(w.message) for w in caught]
            reason = msgs[-1] if msgs else "torch.cuda.is_available() is False"
        kv("cuda_error", reason)
    try:
        n = int(torch.cuda.device_count())
        kv("device_count", n)
    except BaseException as e:
        kv("device_count_error", err(e))
        n = 0
    if not available:
        return
    fact("torch_driver_cuda", lambda: driver_cuda(torch))

    mib = 1 << 20
    for i in range(n):
        # Name and memory as torch sees them (under MIG it names the slice): what labels and
        # the nvidia-smi check fall back on when nvidia-smi can't list the GPUs.
        fact(f"tgpu.{i}.name", lambda: torch.cuda.get_device_name(i))
        fact(f"tgpu.{i}.mem_total_mib", lambda: torch.cuda.get_device_properties(i).total_memory // mib)
        try:
            res = tensor_check(torch, i)
        except BaseException as e:
            res = err(e)
        kv(f"tensor.{i}", res)
        fact(f"tgpu.{i}.mem_free_mib", lambda: torch.cuda.mem_get_info(i)[0] // mib)

    if n < 2:
        kv("peer", f"skipped ({n} GPU)")
        kv("nccl", f"skipped ({n} GPU)")
        return
    for i in range(n):  # a ring, so every GPU sends and receives once
        j = (i + 1) % n
        try:
            res = peer_check(torch, i, j)
        except BaseException as e:
            res = err(e)
        kv(f"peer.{i}-{j}", res)
    world = min(n, MAX_NCCL_RANKS)
    kv("nccl_ranks", world)
    try:
        res = nccl_check(world)
    except BaseException as e:
        res = err(e)
    kv("nccl", res)


if __name__ == "__main__":
    main()
    kv("py_done", 1)
PYEOF
  (cd "$work" && bounded 75 "$PY" deep_check.py) </dev/null 2>"$work/py.err"
  rc=$?
  kv py_exit "$rc"
  if [ "$rc" -ne 0 ]; then
    kv py_stderr "$(tail -n 3 "$work/py.err" 2>/dev/null)"
  fi
fi

# ---- disk: / and the (often network-mounted, so bounded) /workspace ----
avail_kb() { bounded 5 df -P -k "$1" 2>/dev/null | awk 'NR == 2 { print $4 }'; }
v=$(avail_kb /)
kv disk.root_avail_kb "${v:-unknown}"
if [ -d /workspace ]; then
  v=$(avail_kb /workspace)
  kv disk.workspace_avail_kb "${v:-unknown}"
fi

# ---- VS Code Remote-SSH pre-install (setup's warm-up; informational, local reads only) ----
# The newest installed server (exec-server layout) and how many; each extension in the
# shared dir as `vscode.ext.<id>=<version>` (one line each: a joined list would be clipped);
# the machine-settings default interpreter.
vs="$HOME/.vscode-server"
newest="" count=0
for d in "$vs"/cli/servers/Stable-*/server; do
  [ -x "$d/bin/code-server" ] || continue
  count=$((count + 1))
  if [ -z "$newest" ] || [ "$d" -nt "$newest" ]; then newest=$d; fi
done
commit=${newest%/server}
commit=${commit##*/Stable-}
kv vscode.server "${commit:-none}"
kv vscode.servers "$count"
n=0
for d in "$vs"/extensions/*/; do
  [ -d "$d" ] || continue
  [ "$n" -lt 60 ] || break
  b=${d%/}
  b=${b##*/}
  idver=$(printf '%s\n' "$b" | sed -nE 's/^([A-Za-z0-9_-]+\.[A-Za-z0-9_-]+)-([0-9]+\.[0-9][^ ]*)$/\1 \2/p')
  [ -n "$idver" ] || continue
  kv "vscode.ext.$(printf '%s' "${idver% *}" | tr '[:upper:]' '[:lower:]')" "${idver#* }"
  n=$((n + 1))
done
v=$(tr -d '\r\n' <"$vs/data/Machine/settings.json" 2>/dev/null |
  grep -oE '"python\.defaultInterpreterPath"[[:space:]]*:[[:space:]]*"[^"]*"' | head -n 1 |
  sed -E 's/.*"([^"]*)"$/\1/')
kv vscode.python "${v:-unset}"

# ---- network result (the probe has been running all along; bounded at 40s) ----
if [ -n "$net_pid" ]; then
  wait "$net_pid" 2>/dev/null
  rc=$(cat "$work/net.rc" 2>/dev/null)
  kv net.curl_exit "${rc:-unknown}"
  code="" bytes="" secs=""
  read -r code bytes secs _rest 2>/dev/null <"$work/net.out"
  kv net.http "${code:-unknown}"
  kv net.bytes "${bytes:-unknown}"
  kv net.secs "${secs:-unknown}"
  if [ "${rc:-1}" != 0 ]; then
    msg=$(first_line "$(cat "$work/net.err" 2>/dev/null)")
    kv net.error "${msg:-curl exit ${rc:-unknown}}"
  fi
else
  kv net.curl_exit missing
fi

# ---- the keys' results (their calls have been running alongside; 15s each at most) ----
keys_report

# ---- the course's (its probes too: the push 20s, Claude 25s, the kernel 50s at most) ----
course_report

echo "deep_check_end=1"
