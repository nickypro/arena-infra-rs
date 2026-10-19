#!/usr/bin/env bash
# `arena pods setup`'s VS Code warm-up (best-effort step; arena_core::vscode renders the call).
#
# Participants connect with VS Code Remote-SSH. On a fresh pod their first connect downloads
# the VS Code server and then every extension, on the pod — slow, and repeated on every new,
# replaced or restarted pod. This does it ahead of time:
#   1. python.defaultInterpreterPath = the first interpreter given as an argument that exists,
#      in ~/.vscode-server/data/Machine/settings.json — added only when the key is absent;
#      every other key is kept, and a file that isn't plain JSON (comments) is left alone.
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
#     commit, each extension, the settings key). Downloads land in a temp dir on the same
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

# ---- 1. default interpreter (local, no network: first, so it lands even offline) ----
py=""
for c in "$@"; do
  case $c in "~/"*) c="$HOME/${c#"~/"}" ;; esac
  if [ -x "$c" ] && [ ! -d "$c" ]; then
    py=$c
    break
  fi
done
settings="$VS/data/Machine/settings.json"
if [ -z "$py" ]; then
  problem "no arena python found (looked for: $*)"
else
  cat >"$tmp/merge_settings.py" <<'PYEOF'
"""Add python.defaultInterpreterPath to VS Code's machine settings unless it's already set.
argv: <settings.json> <interpreter>. Keeps every other key; refuses (exit 4) a file that
isn't a plain JSON object rather than rewrite something it can't read faithfully."""
import json
import os
import sys

KEY = "python.defaultInterpreterPath"
path, interpreter = sys.argv[1], sys.argv[2]
try:
    with open(path, encoding="utf-8") as f:
        text = f.read()
except FileNotFoundError:
    text = ""
data = {}
if text.strip():
    try:
        data = json.loads(text)
    except ValueError:
        print(f"{path} is not plain JSON (comments?) - left alone", file=sys.stderr)
        sys.exit(4)
    if not isinstance(data, dict):
        print(f"{path} is not a JSON object - left alone", file=sys.stderr)
        sys.exit(4)
if KEY in data:
    print(f"kept {KEY}={data[KEY]}")
    sys.exit(0)
data[KEY] = interpreter
os.makedirs(os.path.dirname(path), exist_ok=True)
tmp = path + ".arena-tmp"
with open(tmp, "w", encoding="utf-8") as f:
    json.dump(data, f, indent=4)
    f.write("\n")
os.replace(tmp, path)
print(f"set {KEY}={interpreter}")
PYEOF
  if t=$(cap 30); then
    if have timeout; then
      out=$(timeout -k 2 "$t" "$py" -I "$tmp/merge_settings.py" "$settings" "$py" 2>&1)
    else
      out=$("$py" -I "$tmp/merge_settings.py" "$settings" "$py" 2>&1)
    fi
    rc=$?
    if [ "$rc" -eq 0 ]; then
      note "$out"
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
