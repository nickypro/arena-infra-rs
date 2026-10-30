#!/usr/bin/env bash
# `arena pods setup`'s VS Code warm-up (best-effort step; arena_core::vscode renders the call).
#
# Participants connect with VS Code Remote-SSH. On a fresh pod their first connect downloads
# the VS Code server and then every extension, on the pod — slow, and repeated on every new,
# replaced or restarted pod. This does it ahead of time:
#   1. in ~/.vscode-server/data/Machine/settings.json: python.defaultInterpreterPath = the first
#      interpreter given as an argument that exists, and on a machine short of RAM
#      (ARENA_VSCODE_LOW_MEMORY) memory-saving settings — Pylance's indexing off, diagnostics for
#      open files only, the env/caches/data excluded. Each added only where absent; every other
#      key and byte is kept (JSONC comments too), and a file it can't read the way VS Code does
#      is left alone.
#   2. the latest *stable* server for this CPU (x64 / arm64), in the layout Remote-SSH looks
#      for (exec-server mode, the default since VS Code 1.82):
#        ~/.vscode-server/code-<commit>                         the `code` CLI it runs first
#        ~/.vscode-server/cli/servers/Stable-<commit>/server/   the server itself
#        ~/.vscode-server/cli/servers/lru.json                  ["Stable-<commit>", …]
#      plus ~/.vscode-server/bin/<commit> -> that server, for the legacy (non-exec-server) mode.
#   3. the extensions (ARENA_VSCODE_EXTENSIONS, comma list) into ~/.vscode-server/extensions,
#      installed by that server. That dir is shared by every server version, so this is the
#      main win even when a participant's client is a different VS Code release.
#
# Contract with the caller (a best-effort step: it can't fail a pod's setup):
#   * exit 0 = all of it is in place (now or already); exit 3 = something couldn't be done,
#     and the LAST stderr line says what. Progress goes to stdout.
#   * idempotent: each part is skipped when already present (server + CLI for the latest
#     commit, each extension, each settings key). Downloads land in a temp dir on the same
#     filesystem and are verified (sha256 from the update API) and renamed into place only
#     when complete, so an interrupted run leaves nothing half-installed.
#   * bounded: everything shares ARENA_VSCODE_BUDGET seconds (each curl, the install).
#   * fail closed: an update-API answer that isn't exactly the expected shape (a 40-hex
#     commit, an https URL, a 64-hex sha256, the same commit for server and CLI) installs
#     nothing.
# Reads no stdin. Needs curl, tar, sha256sum (any Ubuntu image has them).
set -u
export LC_ALL=C

VS="$HOME/.vscode-server"
UPDATE_API="https://update.code.visualstudio.com/api/update"
budget=${ARENA_VSCODE_BUDGET:-240}
case $budget in '' | *[!0-9]*) budget=240 ;; esac
deadline=$((SECONDS + budget))
problems=""

have() { command -v "$1" >/dev/null 2>&1; }
note() { printf 'arena vscode: %s\n' "$1"; }
problem() {
  note "$1"
  problems="${problems:+$problems; }$1"
}
# `cap N`: seconds for the next bounded call — N, or whatever is left of the budget if
# less. Fails when under 5s remain (curl's --max-time 0 would mean "no limit").
cap() {
  local left=$((deadline - SECONDS))
  [ "$left" -ge 5 ] || return 1
  if [ "$left" -lt "$1" ]; then echo "$left"; else echo "$1"; fi
}
finish() {
  if [ -n "$problems" ]; then
    printf 'vscode warm-up incomplete: %s\n' "$problems" >&2
    exit 3
  fi
  note "done"
  exit 0
}

mkdir -p "$VS" || { problems="can't create $VS"; finish; }
tmp=$(mktemp -d "$VS/.arena-warmup.XXXXXX" 2>/dev/null) || { problems="mktemp failed in $VS"; finish; }
trap 'rm -rf "$tmp"' EXIT

# ---- 1. machine settings (local, no network: first, so they land even offline) ----
# python.defaultInterpreterPath = the first interpreter argument that exists; and on a machine
# short of RAM the memory-saving settings (merge_settings.py's LOW_MEMORY): Pylance and the
# kernels ran a cx33 (8 GB) out of memory. ARENA_VSCODE_LOW_MEMORY says which machines:
# `always` (a CPU VM: the kernels' models sit in the same RAM as VS Code), a number N = only one
# with less than N GiB of RAM (MemTotal, or a container's own cgroup limit when that's lower: a
# container sees its host's MemTotal), unset or 0 = none.
# (ARENA_MEMINFO, ARENA_CGROUP, ARENA_PROC_CGROUP, and ARENA_SETTINGS_RACE in the merge: a test's
# stand-ins for /proc/meminfo, /sys/fs/cgroup, /proc/self/cgroup and VS Code; never set on a pod.)
py=""
for c in "$@"; do
  case $c in "~/"*) c="$HOME/${c#"~/"}" ;; esac
  if [ -x "$c" ] && [ ! -d "$c" ]; then
    py=$c
    break
  fi
done
# `ram_kb`: this machine's RAM in kB, as above; fails when /proc/meminfo can't be read. The
# memory limits that apply: this process's cgroup's and every one above it (a limit holds for all
# below it), v2 (`0::<path>` in /proc/self/cgroup) and v1's memory controller, each looked for
# under the cgroup mount — where a container with a cgroup namespace of its own sees its own at
# the top (`/`), and one sharing the host's sees it at its path (`/docker/<id>`, …). The lowest wins.
ram_kb() {
  local kb lim f cg=${ARENA_CGROUP:-/sys/fs/cgroup}
  kb=$(awk '$1 == "MemTotal:" { printf "%.0f\n", $2; exit }' "${ARENA_MEMINFO:-/proc/meminfo}" 2>/dev/null)
  case $kb in '' | *[!0-9]*) return 1 ;; esac
  while IFS= read -r f; do
    lim=$(awk 'NR == 1 && $1 ~ /^[0-9]+$/ { printf "%.0f\n", $1 / 1024 }' "$f" 2>/dev/null)
    case $lim in '' | *[!0-9]*) continue ;; esac
    [ "$lim" -lt "$kb" ] && kb=$lim
  done < <(
    printf '%s\n' "$cg/memory.max" "$cg/memory/memory.limit_in_bytes"
    awk -v cg="$cg" '{
      path = $0; sub(/^[^:]*:[^:]*:/, "", path)
      if ($0 ~ /^0::/) { base = cg; file = "memory.max" }
      else if ($0 ~ /^[0-9]+:([^:]*,)?memory(,[^:]*)?:/) { base = cg "/memory"; file = "memory.limit_in_bytes" }
      else next
      while (path != "" && path != "/") { print base path "/" file; sub(/\/[^\/]*$/, "", path) }
    }' "${ARENA_PROC_CGROUP:-/proc/self/cgroup}" 2>/dev/null
  )
  echo "$kb"
}
gib() { awk -v k="$1" 'BEGIN { printf "%.1f", k / 1048576 }'; }
lowmem=0
want=${ARENA_VSCODE_LOW_MEMORY:-0}
case $want in
  0) ;;
  always)
    lowmem=1
    note "memory-saving settings: on (every machine of this kind)"
    ;;
  '' | *[!0-9]*) problem "ARENA_VSCODE_LOW_MEMORY must be always or a number of GiB, not '$want' - no memory-saving settings" ;;
  *)
    if ! kb=$(ram_kb); then
      problem "couldn't read this machine's RAM - no memory-saving settings"
    elif [ "$kb" -lt $((want * 1048576)) ]; then
      lowmem=1
      note "memory-saving settings: on ($(gib "$kb") GiB of RAM, under $want)"
    else
      note "memory-saving settings: not needed ($(gib "$kb") GiB of RAM)"
    fi
    ;;
esac
settings="$VS/data/Machine/settings.json"
# The merge runs on the arena python; with none found, the system's for the memory-saving part.
runner=$py
if [ -z "$py" ]; then
  problem "no arena python found (looked for: $*)"
  [ "$lowmem" = 1 ] && runner=$(command -v python3 2>/dev/null)
fi
if [ -n "$py" ] || [ "$lowmem" = 1 ]; then
  cat >"$tmp/merge_settings.py" <<'PYEOF'
"""Merge arena's keys into VS Code's machine settings.json, never clobbering what's there.
argv: <settings.json> <interpreter, or ''> <memory-saving: 1|0>

Adds python.defaultInterpreterPath (when an interpreter is given) and, with memory-saving 1,
LOW_MEMORY below. Each only where absent: a key already set (by the participant, VS Code, an
operator, an earlier run) is kept as it is; to an exclude map already there only the patterns it
doesn't name are added (one set to false stays false). VS Code writes and reads the file as JSONC
(comments, trailing commas): the additions are inserted into the text, so every byte already
there stays. A file that isn't JSONC VS Code would read the same way, or isn't an object at the
top, is left alone (exit 4); so is one whose edited text doesn't parse back to exactly the old
settings plus the additions, and one that changed while this ran. Written through a temp file
beside it, swapped into place (put(); mode kept; a symlink's target)."""
import copy
import errno
import json
import os
import re
import sys

INTERPRETER = "python.defaultInterpreterPath"
# The memory-saving settings (a small machine: Pylance + the kernels ran a cx33 out of RAM).
# Pylance: no background index of every package in the env (torch, jax, ... - GBs), diagnostics
# for open files only, and not the env, caches, data or the course's infrastructure copies
# (`**/.*` is pyright's own default, which a list of our own replaces). The watcher, search and
# explorer: not the env, caches or data. `.git` stays watched (VS Code's default already skips
# its objects): the Source Control view follows the backups' commits through it.
LOW_MEMORY = [
    ("python.analysis.indexing", False),
    ("python.analysis.diagnosticMode", "openFilesOnly"),
    ("python.analysis.exclude", ["**/.venv", "**/.git", "**/.cache", "**/__pycache__", "**/data",
                                 "**/infrastructure", "**/node_modules", "**/.*"]),
    ("files.watcherExclude", {"**/.venv/**": True, "**/.cache/**": True, "**/__pycache__/**": True,
                              "**/data/**": True, "**/infrastructure/**": True}),
    ("search.exclude", {"**/.venv": True, "**/.cache": True, "**/__pycache__": True, "**/data": True,
                        "**/infrastructure": True}),
    ("files.exclude", {"**/__pycache__": True}),
]


class Bad(Exception):
    pass


# JSONC as VS Code's own scanner reads it: a `//` comment ends at a CR as well as an LF (a CR-only
# line break must not hide the setting after it). Anything else it might read differently - other
# whitespace, a control character in a string - is refused, never guessed.
TOKEN = re.compile(
    r'[ \t\r\n]+|//[^\r\n]*|/\*.*?\*/'
    r'|"(?:[^"\\\x00-\x1f]|\\(?:["\\/bfnrt]|u[0-9a-fA-F]{4}))*"'
    r'|-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?(?![0-9A-Za-z_.])'
    r'|(?:true|false|null)(?![0-9A-Za-z_])|[{}\[\],:]',
    re.S,
)


class Node:
    """A parsed value and where it is: [start, end) in the text; for an object its members
    (key, key offset, Node), the offset of its `}` and whether a comma ends its members."""

    def __init__(self, value, start, end, members=None, trailing=False):
        self.value, self.start, self.end = value, start, end
        self.members, self.trailing = members, trailing


def parse(text):
    """The top-level Node of JSONC `text` (None: no value at all), or Bad."""
    toks, i = [], 0
    while i < len(text):
        m = TOKEN.match(text, i)
        if not m:
            raise Bad("unexpected %r at offset %d" % (text[i:i + 12], i))
        t = m.group()
        if not (t[0] in " \t\r\n" or t.startswith("//") or t.startswith("/*")):
            toks.append((t, m.start(), m.end()))
        i = m.end()
    pos = 0

    def tok():
        if pos >= len(toks):
            raise Bad("ends early")
        return toks[pos]

    def value():
        nonlocal pos
        t, s, e = tok()
        pos += 1
        if t in ("{", "["):
            obj, close = t == "{", "}" if t == "{" else "]"
            members, items, d, trailing = [], [], {}, False
            while tok()[0] != close:
                if obj:
                    k, ks, _ = tok()
                    if k[0] != '"':
                        raise Bad("a key isn't a string at offset %d" % ks)
                    pos += 1
                    if tok()[0] != ":":
                        raise Bad("no ':' after a key at offset %d" % ks)
                    pos += 1
                    v = value()
                    members.append((json.loads(k), ks, v))
                    d[json.loads(k)] = v.value
                else:
                    items.append(value().value)
                trailing = False
                if tok()[0] == ",":
                    pos += 1
                    trailing = True
                elif tok()[0] != close:
                    raise Bad("expected ',' or '%s' at offset %d" % (close, tok()[1]))
            _, c, _ = tok()
            pos += 1
            if obj:
                return Node(d, s, c + 1, members, trailing)
            return Node(items, s, c + 1)
        if t[0] in '"-0123456789tfn':
            return Node(json.loads(t), s, e)
        raise Bad("unexpected %r at offset %d" % (t, s))

    if not toks:
        return None
    top = value()
    if pos != len(toks):
        raise Bad("more after the settings object, at offset %d" % toks[pos][1])
    return top


def line_indent(text, at):
    start = text.rfind("\n", 0, at) + 1
    return re.match(r"[ \t]*", text[start:]).group()


def own_line(text, at):
    """The indent before `at` when nothing else precedes it on its line, else None."""
    lead = text[text.rfind("\n", 0, at) + 1:at]
    return lead if lead.strip(" \t") == "" else None


def merge(text, interpreter, low):
    """(new text or None when nothing to add, what was added, what was kept), or Bad."""
    top = parse(text)
    if top is not None and not isinstance(top.value, dict):
        raise Bad("its top level isn't an object")
    have = top.value if top is not None else {}
    nl = "\r\n" if "\r\n" in text else "\n"
    unit = "    "
    if top is not None and top.members:
        first = own_line(text, top.members[0][1])
        if first:
            unit = first[len(line_indent(text, top.start)):] or first

    def configured(key):
        # The key, or a part of it written as an object ({"python": {"analysis": ...}}).
        parts = key.split(".")
        return any(".".join(parts[:n]) in have for n in range(1, len(parts) + 1))

    wanted = ([(INTERPRETER, interpreter)] if interpreter else []) + (LOW_MEMORY if low else [])
    add_top, add_in, added, kept = [], [], [], []
    for key, val in wanted:
        if not configured(key):
            add_top.append((key, val))
            added.append(key)
        elif isinstance(val, dict) and isinstance(have.get(key), dict):
            missing = [(k, v) for k, v in val.items() if k not in have[key]]
            if missing:
                add_in.append((key, missing))
                added.append("%s (+%d)" % (key, len(missing)))
            else:
                kept.append(key)
        else:
            kept.append(key + ("=%s" % json.dumps(have[key]) if key == INTERPRETER and key in have else ""))
    if not add_top and not add_in:
        return None, added, kept

    def render(pairs, indent):
        out = []
        for k, v in pairs:
            body = json.dumps(v, indent=unit, ensure_ascii=False).replace("\n", nl + indent)
            out.append(nl + indent + json.dumps(k, ensure_ascii=False) + ": " + body)
        return ",".join(out)

    if top is None:
        lead = text if not text or text.endswith("\n") else text + nl
        new = lead + "{" + render(add_top, unit) + nl + "}" + nl
    else:
        edits = []

        def add_to(obj, pairs):
            if obj.members:
                indent = own_line(text, obj.members[0][1])
                if indent is None:
                    indent = line_indent(text, obj.start) + unit
                if not obj.trailing:
                    edits.append((obj.members[-1][2].end, ","))
            else:
                indent = line_indent(text, obj.start) + unit
            close = obj.end - 1
            at = close
            while at > obj.start + 1 and text[at - 1] in " \t\r\n":
                at -= 1
            tail = "" if "\n" in text[at:close] else nl + line_indent(text, obj.start)
            edits.append((at, render(pairs, indent) + tail))

        for key, missing in add_in:
            node = [m for m in top.members if m[0] == key][-1][2]
            add_to(node, missing)
        if add_top:
            add_to(top, add_top)
        edits.sort(key=lambda e: e[0])
        out, last = [], 0
        for at, piece in edits:
            out.append(text[last:at])
            out.append(piece)
            last = at
        out.append(text[last:])
        new = "".join(out)

    want = copy.deepcopy(have)
    want.update(dict(add_top))
    for key, missing in add_in:
        want[key].update(dict(missing))
    back = parse(new)
    if back is None or back.value != want:
        raise Bad("the merged text didn't read back as the settings plus arena's")
    return new, added, kept


def left_alone(path, why):
    print("%s %s - left alone" % (path, why), file=sys.stderr)
    sys.exit(4)


def read(target):
    try:
        with open(target, "rb") as f:
            return f.read()
    except FileNotFoundError:
        return None


def exchange(a, b):
    """Swap two paths in one step (Linux's renameat2 RENAME_EXCHANGE): True if done, False where
    the system or filesystem can't, OSError for anything else."""
    try:
        import ctypes
        fn = ctypes.CDLL(None, use_errno=True).renameat2
    except (ImportError, OSError, AttributeError):
        return False
    fn.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    at_fdcwd, rename_exchange = -100, 2
    if fn(at_fdcwd, os.fsencode(a), at_fdcwd, os.fsencode(b), rename_exchange) == 0:
        return True
    err = ctypes.get_errno()
    if err in (errno.EINVAL, errno.ENOSYS, errno.EOPNOTSUPP):
        return False
    raise OSError(err, os.strerror(err))


def put(path, tmp, target, raw, ours):
    """tmp (the merged text) into target's place - unless target no longer holds `raw` (VS Code
    saved a participant's change after it was read): then theirs stays. The swap trades the two
    in one step, so what comes out of target's place is exactly what was there at that moment;
    anything but `raw` goes straight back (and if VS Code saved again meanwhile, its newest
    stays). Where a swap can't be done: compare, then rename (a moment's window). A file that
    wasn't there is created only if it still isn't (a hard link fails if it is)."""
    race = os.environ.get("ARENA_SETTINGS_RACE")  # a test's stand-in for VS Code saving just now
    if race:
        with open(race, "rb") as f, open(target, "wb") as out:
            out.write(f.read())
    changed = "changed while arena merged its settings (re-run setup)"
    if raw is None:
        try:
            os.link(tmp, target)
            return
        except FileExistsError:
            left_alone(path, changed)
        except OSError:
            pass
    elif exchange(tmp, target):
        if read(tmp) == raw:
            return
        try:
            back = exchange(tmp, target)
        except OSError:
            back = False
        if not back or read(tmp) != ours:
            os.replace(tmp, target)
        left_alone(path, changed)
    if read(target) != raw:
        left_alone(path, changed)
    os.replace(tmp, target)


def main():
    path, interpreter, low = sys.argv[1], sys.argv[2], sys.argv[3] == "1"
    target = os.path.realpath(path)
    try:
        raw = read(target)
    except OSError as e:
        left_alone(path, "can't be read (%s)" % e.strerror)
    try:
        text = (raw or b"").decode("utf-8")
    except UnicodeDecodeError:
        left_alone(path, "isn't UTF-8")
    bom = text.startswith("\ufeff")
    try:
        new, added, kept = merge(text[1:] if bom else text, interpreter, low)
    except (Bad, RecursionError, ValueError) as e:
        left_alone(path, "isn't JSON(C) VS Code would read the same way (%s)" % e)
    if kept:
        print("kept " + ", ".join(kept))
    if new is None:
        return
    tmp = "%s.arena-%d" % (target, os.getpid())
    data = ("\ufeff" if bom else "").encode("utf-8") + new.encode("utf-8")
    try:
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(tmp, "wb") as f:
            f.write(data)
        if raw is not None:
            os.chmod(tmp, os.stat(target).st_mode & 0o7777)
        put(path, tmp, target, raw, data)
    except OSError as e:
        left_alone(path, "couldn't be written (%s)" % e.strerror)
    finally:
        if os.path.exists(tmp):
            os.remove(tmp)
    print("added " + ", ".join(added))


main()
PYEOF
  if [ -z "$runner" ]; then
    problem "settings: no python3 to merge them with"
  elif t=$(cap 30); then
    if have timeout; then
      out=$(timeout -k 2 "$t" "$runner" -I "$tmp/merge_settings.py" "$settings" "$py" "$lowmem" 2>&1)
    else
      out=$("$runner" -I "$tmp/merge_settings.py" "$settings" "$py" "$lowmem" 2>&1)
    fi
    rc=$?
    if [ "$rc" -eq 0 ]; then
      [ -z "$out" ] || note "settings: $out"
    else
      problem "settings: ${out:-exit $rc}"
    fi
  else
    problem "out of time before the settings"
  fi
fi

# ---- 2. the server + CLI for the latest stable commit ----
case "$(uname -m)" in
  x86_64 | amd64) arch=x64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) arch="" ;;
esac

# `field NAME VALUE_REGEX JSON`: one top-level string field's value, only if the whole
# value matches VALUE_REGEX (so anything unexpected reads as absent). `\/` is unescaped
# first: JSON may escape slashes in the URL.
field() {
  printf '%s' "$3" | tr -d '\r\n' | sed 's#\\/#/#g' |
    grep -oE "\"$1\"[[:space:]]*:[[:space:]]*\"$2\"" | head -n 1 |
    sed -E "s/^\"$1\"[[:space:]]*:[[:space:]]*\"(.*)\"\$/\\1/"
}
# `latest PLATFORM`: sets rel_commit / rel_version / rel_url / rel_sha from the update API's
# answer for PLATFORM (e.g. server-linux-x64); fails unless every one has its exact shape.
latest() {
  local t json
  rel_commit="" rel_version="" rel_url="" rel_sha=""
  t=$(cap 30) || { why="out of time"; return 1; }
  json=$(curl -fsSL --connect-timeout 10 --max-time "$t" "$UPDATE_API/$1/stable/latest" 2>"$tmp/curl.err") || {
    why="update API for $1: $(head -n 1 "$tmp/curl.err" 2>/dev/null)"
    return 1
  }
  rel_commit=$(field version '[0-9a-f]{40}' "$json")
  rel_version=$(field productVersion '[0-9]+(\.[0-9]+)*' "$json")
  rel_url=$(field url 'https://[^"[:space:]\\]+' "$json")
  rel_sha=$(field sha256hash '[0-9a-f]{64}' "$json")
  if [ -z "$rel_commit" ] || [ -z "$rel_url" ] || [ -z "$rel_sha" ]; then
    why="update API for $1 answered in an unexpected shape - installing nothing"
    return 1
  fi
}
# `fetch URL SHA256 OUT`: download within the budget; keep it only if the checksum matches.
fetch() {
  local t
  t=$(cap 180) || { why="out of time"; return 1; }
  curl -fsSL --connect-timeout 10 --max-time "$t" -o "$3" "$1" 2>"$tmp/curl.err" || {
    why="download failed: $(head -n 1 "$tmp/curl.err" 2>/dev/null)"
    return 1
  }
  have sha256sum || { why="no sha256sum to verify the download"; return 1; }
  [ "$(sha256sum "$3" | cut -d ' ' -f 1)" = "$2" ] || { why="checksum mismatch for $1"; return 1; }
}

commit=""
server_dir=""
why=""
if [ -z "$arch" ]; then
  problem "unsupported CPU $(uname -m) (VS Code server ships for x64/arm64)"
elif ! have curl || ! have tar; then
  problem "curl/tar missing"
elif ! latest "server-linux-$arch"; then
  problem "$why"
else
  commit=$rel_commit version=$rel_version srv_url=$rel_url srv_sha=$rel_sha
  server_dir="$VS/cli/servers/Stable-$commit/server"
  cli_bin="$VS/code-$commit"
  label="VS Code ${version:-?} (${commit:0:7}, $arch)"
  if [ -x "$server_dir/bin/code-server" ]; then
    note "server $label already present"
  elif [ -e "$server_dir" ]; then
    # Not ours to finish (Remote-SSH may be installing it right now): leave it be.
    problem "$server_dir exists but is incomplete - left alone"
  elif ! fetch "$srv_url" "$srv_sha" "$tmp/server.tgz"; then
    problem "server: $why"
  elif ! { mkdir "$tmp/server" && tar -xzf "$tmp/server.tgz" -C "$tmp/server" --strip-components=1; }; then
    problem "server: couldn't unpack the download"
  elif [ ! -x "$tmp/server/bin/code-server" ]; then
    problem "server: the download has no bin/code-server"
  elif ! { mkdir -p "${server_dir%/server}" && mv -T "$tmp/server" "$server_dir"; }; then
    problem "server: couldn't move it into $server_dir"
  else
    note "installed server $label"
  fi

  # The CLI Remote-SSH starts first; same commit as the server, or nothing.
  if [ -x "$cli_bin" ]; then
    :
  elif ! latest "cli-alpine-$arch"; then
    problem "cli: $why"
  elif [ "$rel_commit" != "$commit" ]; then
    problem "cli: update API offers $rel_commit, server $commit (mid-release?) - skipped"
  elif ! fetch "$rel_url" "$rel_sha" "$tmp/cli.tgz"; then
    problem "cli: $why"
  elif ! { mkdir "$tmp/cli" && tar -xzf "$tmp/cli.tgz" -C "$tmp/cli"; } || [ ! -f "$tmp/cli/code" ]; then
    problem "cli: the download has no \`code\` binary"
  elif ! { chmod 755 "$tmp/cli/code" && mv -T "$tmp/cli/code" "$cli_bin"; }; then
    problem "cli: couldn't move it to $cli_bin"
  else
    note "installed CLI code-${commit:0:7}"
  fi

  if [ -x "$server_dir/bin/code-server" ]; then
    # lru.json: the CLI's list of installed servers, most recent first. Add ours if absent;
    # a file in any other shape than a flat JSON list of strings is left alone.
    lru="$VS/cli/servers/lru.json"
    entry="\"Stable-$commit\""
    cur=$(tr -d '\r\n' <"$lru" 2>/dev/null | sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//')
    new=""
    case $cur in
      *"$entry"*) ;;
      '' | '[]') new="[$entry]" ;;
      '["'*'"]') new="[$entry,${cur#\[}" ;;
      *) problem "$lru is not a JSON list - left alone" ;;
    esac
    if [ -n "$new" ]; then
      printf '%s' "$new" >"$lru.arena-tmp" && mv -f "$lru.arena-tmp" "$lru" || problem "couldn't write $lru"
    fi
    # Legacy (non-exec-server) Remote-SSH looks in ~/.vscode-server/bin/<commit>.
    if [ ! -e "$VS/bin/$commit" ] && [ ! -L "$VS/bin/$commit" ]; then
      mkdir -p "$VS/bin" && ln -s "../cli/servers/Stable-$commit/server" "$VS/bin/$commit" ||
        problem "couldn't link $VS/bin/$commit"
    fi
  fi
fi

# ---- 3. extensions ----
ext_dir="$VS/extensions"
# Installed = a `<id>-<version>` dir (version starts with a digit, so `ms-toolsai.jupyter`
# isn't satisfied by `ms-toolsai.jupyter-keymap-…`). Ids compare case-insensitively.
has_ext() {
  [ -n "$(find "$ext_dir" -mindepth 1 -maxdepth 1 -type d -iname "$1-[0-9]*" -print -quit 2>/dev/null)" ]
}
missing=()
IFS=',' read -r -a wanted <<<"${ARENA_VSCODE_EXTENSIONS:-}"
for id in "${wanted[@]}"; do
  [ -n "$id" ] || continue
  has_ext "$id" || missing+=("$id")
done
if [ "${#missing[@]}" -eq 0 ]; then
  [ "${#wanted[@]}" -eq 0 ] || note "extensions already present: ${ARENA_VSCODE_EXTENSIONS}"
elif [ -z "$server_dir" ] || [ ! -x "$server_dir/bin/code-server" ]; then
  problem "extensions not installed (no server for the latest release to install them with): ${missing[*]}"
elif ! t=$(cap 600); then
  problem "out of time before installing extensions: ${missing[*]}"
else
  args=()
  for id in "${missing[@]}"; do args+=(--install-extension "$id"); done
  mkdir -p "$ext_dir"
  if have timeout; then
    timeout -k 5 "$t" "$server_dir/bin/code-server" --accept-server-license-terms \
      --extensions-dir "$ext_dir" "${args[@]}" </dev/null >"$tmp/ext.log" 2>&1
  else
    "$server_dir/bin/code-server" --accept-server-license-terms \
      --extensions-dir "$ext_dir" "${args[@]}" </dev/null >"$tmp/ext.log" 2>&1
  fi
  rc=$?
  still=()
  for id in "${missing[@]}"; do has_ext "$id" || still+=("$id"); done
  if [ "${#still[@]}" -eq 0 ]; then
    note "installed extensions: ${missing[*]}"
  else
    last=$(grep -v '^[[:space:]]*$' "$tmp/ext.log" 2>/dev/null | tail -n 1 | cut -c1-160)
    problem "extensions not installed: ${still[*]} (exit $rc${last:+: $last})"
  fi
fi

finish
