#!/usr/bin/env bash
# `arena pods test --deep`: gather one pod's health facts and print them as `key=value`.
#
# Contract with the parser (arena_core::health::parse_deep):
#   * stdout carries ONLY `key=value` lines: one fact per line, single-line values. The
#     first is `deep_check=<version>` (anything before it, e.g. shell-rc chatter, is
#     ignored) and the last is `deep_check_end=1` (absent = the run was cut off).
#   * a fact that couldn't be measured says so (`missing`, `unknown`, `error: …`); a key
#     that never appears also reads as unknown.
#   * every sub-step is bounded with `timeout` and the network probe runs alongside the
#     GPU checks, so a run ends in under ~2 minutes even on a sick host (worst case ≈
#     10+10 nvidia-smi, 75 python, 5+5 df; the caller's budget is DEEP_CHECK_TIMEOUT).
# Read-only apart from its own temp dir (removed on exit). Needs no TTY, reads no stdin.
# The caller runs it inside the participants' login env (conda env active), so `python`
# is the interpreter participants actually use.
set -u
export LC_ALL=C

echo "deep_check=1"

# One fact per line: newlines/tabs flattened, clipped to 300 chars.
kv() {
  local v
  v=$(printf '%s' "$2" | tr '\r\n\t' '   ' | cut -c1-300)
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
trap 'rm -rf "$work"' EXIT

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
  elif [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
    kv smi "error: nvidia-smi timed out after 10s"
  else
    msg=$(first_line "$out")
    kv smi "error: ${msg:-exit $rc}"
  fi
else
  kv smi missing
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

    for i in range(n):
        try:
            res = tensor_check(torch, i)
        except BaseException as e:
            res = err(e)
        kv(f"tensor.{i}", res)

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

echo "deep_check_end=1"
