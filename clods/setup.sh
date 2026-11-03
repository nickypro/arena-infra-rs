#!/usr/bin/env bash
# Set up this machine's Claude Code account profiles, one config dir per account, so
# CLAUDE_CONFIG_DIR picks the account. Everything lives in ~/.clods:
#
#   profiles/claude1 .. claudeN   profile N. Its token is in settings.json
#                                 (env.CLAUDE_CODE_OAUTH_TOKEN) and onboarding is
#                                 pre-completed, so there is no sign-in.
#   profiles/default              the profile plain `claude` uses in new shells, if any
#   clods.sh                      shell helpers, sourced from ~/.zshrc and ~/.bashrc
#
# Usage: setup.sh [options] [TOKENS_FILE] [N|EMAIL|none]...
#   TOKENS_FILE  one "email -> sk-ant-oat01-..." per line (default: tokens, next to this
#                script). Profile N is the Nth token, or pin it with a leading number:
#                "4 someone@x -> sk-ant-...". Append new accounts at the end (or pin them)
#                so N is the same account everywhere.
#   N|EMAIL      only set up these profiles; "none" sets up none (with --prune: remove them all)
#   --default N|EMAIL|none  the profile plain `claude` uses in new shells on this machine
#   --spread T   the default is account T in turn (counting from 0, around again past the
#                last): what push.sh --spread gives its Tth machine. Instead of --default.
#   --keep-default  a machine whose default still has a token keeps it: --default or --spread
#                only picks one for a machine with none (so a re-push never moves anyone)
#   --prune      take the token out of every profile this run doesn't set up (history stays)
#   --bundle     install nothing; print a self-contained script that does this same install
#                wherever it runs (scripts included). It contains the tokens. push.sh uses it.
#   --list       print "N email" for each token in TOKENS_FILE (no tokens) and exit
#   --summary    print one line instead of the usual ones, for scripts (push.sh -q, arena):
#                "clods: accounts=N default=K|none changed=0|1" (N: profiles with a token here)
#
# Re-running is safe: it refreshes tokens and leaves everything else in a profile alone. A run
# that has nothing to change writes nothing (changed=0).
set -euo pipefail

die() { echo "setup.sh: $*" >&2; exit 1; }

here=$(cd "$(dirname "$0")" && pwd)
clods="$HOME/.clods"
profiles="$clods/profiles"
# What a machine gets of the tool. Never the tokens file: profiles hold the tokens they need.
tool_files="setup.sh push.sh clods.sh README.md fleet.example .gitignore"

default="" spread="" prune="" keep="" summary="" mode=install
pos=()
while [ $# -gt 0 ]; do
  case "$1" in
    --default) [ $# -ge 2 ] || die "--default needs N, EMAIL or none"; default=$2; shift 2 ;;
    --default=*) default=${1#*=}; shift ;;
    --spread) [ $# -ge 2 ] || die "--spread needs a number"; spread=$2; shift 2 ;;
    --prune) prune=1; shift ;;
    --keep-default) keep=1; shift ;;
    --summary) summary=1; shift ;;
    --bundle) mode=bundle; shift ;;
    --list) mode=list; shift ;;
    -h|--help) sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p;}' "$0"; exit 0 ;;
    --) shift; pos+=("$@"); break ;;
    -*) die "unknown option $1 (see --help)" ;;
    *) pos+=("$1"); shift ;;
  esac
done
set -- ${pos[@]+"${pos[@]}"}

tokens="${1:-$here/tokens}"
[ $# -gt 0 ] && shift

[ -r "$tokens" ] || die "can't read tokens file $tokens"
if [ -n "$(find "$tokens" \( -perm -004 -o -perm -040 \))" ]; then
  echo "setup.sh: warning: $tokens is readable by other users; chmod 600 it" >&2
fi

# Parse "[N] email -> token" (also "email = token", or just whitespace between them).
re='^[[:space:]]*([0-9]+[[:space:]]+)?([^[:space:]=>]+)[[:space:]]*(->|=)?[[:space:]]*(sk-ant-[A-Za-z0-9_-]+)[[:space:]]*$'
nums=() emails=() toks=()
n=0
while IFS= read -r line <&3 || [ -n "$line" ]; do
  case "$line" in '#'*) continue ;; esac
  if [[ $line =~ $re ]]; then
    if [ -n "${BASH_REMATCH[1]}" ]; then n=$((10#${BASH_REMATCH[1]%%[[:space:]]*})); else n=$((n + 1)); fi
    [ "$n" -gt 0 ] || die "profile numbers start at 1 ($tokens: ${BASH_REMATCH[2]})"
    case " ${nums[*]-} " in *" $n "*) die "$tokens has profile $n twice (${BASH_REMATCH[2]})" ;; esac
    nums+=("$n") emails+=("${BASH_REMATCH[2]}") toks+=("${BASH_REMATCH[4]}")
  elif ! [[ $line =~ ^[[:space:]]*$ ]]; then
    echo "setup.sh: skipping a line I can't parse: ${line%%sk-ant-*}..." >&2
  fi
done 3< "$tokens"

if [ "$mode" = list ]; then
  for i in ${nums[@]+"${!nums[@]}"}; do printf '%s %s\n' "${nums[$i]}" "${emails[$i]}"; done
  exit 0
fi

# Which entries this run sets up: all of them, or the N|EMAIL arguments ("none": no entries).
sel=()
if [ $# -eq 0 ]; then
  sel=(${nums[@]+"${!nums[@]}"})
else
  for w in "$@"; do
    [ "$w" = none ] && continue
    found=""
    for i in ${nums[@]+"${!nums[@]}"}; do
      if [ "$w" = "${nums[$i]}" ] || [ "$w" = "${emails[$i]}" ]; then
        case " ${sel[*]-} " in *" $i "*) ;; *) sel+=("$i") ;; esac
        found=1
      fi
    done
    [ -n "$found" ] || die "no token for $w in $tokens (see: setup.sh --list)"
  done
fi
[ ${#sel[@]} -gt 0 ] || [ -n "$prune" ] || die "no profiles to set up (no tokens in $tokens?)"
if [ -n "$spread" ]; then
  [ -z "$default" ] || die "--spread and --default: give one of them"
  case "$spread" in ''|*[!0-9]*) die "--spread needs a number (got $spread)" ;; esac
  [ ${#sel[@]} -gt 0 ] || die "--spread: no profiles to pick from"
  default=${nums[${sel[$((10#$spread % ${#sel[@]}))]}]}
fi

if [ "$mode" = bundle ]; then
  flags=""
  if [ -n "$prune" ]; then flags="$flags --prune"; fi
  if [ -n "$keep" ]; then flags="$flags --keep-default"; fi
  if [ -n "$summary" ]; then flags="$flags --summary"; fi
  if [ -n "$default" ]; then flags="$flags --default $(printf '%q' "$default")"; fi
  # Everything sits inside main(), so a `bash -s` reading this from a pipe has it all before
  # anything runs. The heredoc end is random: setup.sh, embedded below, holds this template.
  eof=$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')
  cat <<EOF
#!/usr/bin/env bash
# Installs Claude Code profiles into ~/.clods (see its README.md). Made by setup.sh --bundle
# on ${HOSTNAME:-?} at $(date -u +%Y-%m-%dT%H:%MZ). It contains account tokens: delete it after use.
main() {
set -euo pipefail
umask 077
clod_tmp=\$(mktemp -d)
trap 'rm -rf "\$clod_tmp"' EXIT
EOF
  for f in $tool_files; do
    [ -f "$here/$f" ] || continue
    printf "cat > \"\$clod_tmp/%s\" <<'__CLOD_%s__'\n%s\n__CLOD_%s__\n" "$f" "$eof" "$(cat "$here/$f")" "$eof"
  done
  printf "cat > \"\$clod_tmp/tokens\" <<'__CLOD_%s__'\n" "$eof"
  for i in ${sel[@]+"${sel[@]}"}; do printf '%s %s -> %s\n' "${nums[$i]}" "${emails[$i]}" "${toks[$i]}"; done
  printf '__CLOD_%s__\n' "$eof"
  cat <<EOF
bash "\$clod_tmp/setup.sh"$flags "\$clod_tmp/tokens" $([ ${#sel[@]} -gt 0 ] || echo none) </dev/null
}
main "\$@"
EOF
  exit 0
fi

command -v python3 >/dev/null || die "needs python3 (to edit the JSON config files)"
# What this run changed on disk (--summary says it): a re-run with nothing new writes nothing.
changed=0
say() { if [ -z "$summary" ]; then printf '%s\n' "$*"; fi; }
[ -d "$profiles" ] || changed=1
mkdir -p "$profiles"
chmod 700 "$profiles"
# Run from anywhere else (a bundle, a clone), put the tool in ~/.clods so this machine has it.
if [ "$here" != "$(cd "$clods" && pwd)" ]; then
  for f in $tool_files; do
    if [ -f "$here/$f" ] && ! cmp -s "$here/$f" "$clods/$f"; then cp "$here/$f" "$clods/$f"; changed=1; fi
  done
  chmod 755 "$clods"/*.sh
fi

# Writes the token into settings.json and marks onboarding done in .claude.json (or, with an
# empty token, takes it out). The token comes in via the environment so it never shows in `ps`.
# Prints "changed" when it changed a file.
write_profile() {
  CLOD_TOKEN="$2" python3 - "$1" <<'PY'
import json, os, sys

profile = sys.argv[1]
token = os.environ["CLOD_TOKEN"]
home = os.path.expanduser("~")
changed = False

def load(path):
    try:
        with open(path) as f:
            return json.load(f)
    except FileNotFoundError:
        return None

def save(path, data):
    global changed
    tmp = path + ".tmp"
    with os.fdopen(os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "w") as f:
        json.dump(data, f, indent=2)
        f.write("\n")
    os.replace(tmp, path)
    changed = True

def done():
    if changed:
        print("changed")
    sys.exit(0)

path = os.path.join(profile, "settings.json")
settings = load(path)
if not token:
    if settings and settings.get("env", {}).pop("CLAUDE_CODE_OAUTH_TOKEN", None):
        save(path, settings)
    done()

# A new profile starts from this machine's normal settings (theme, permission prefs...).
if settings is None:
    settings = load(os.path.join(home, ".claude", "settings.json")) or {}
env = settings.setdefault("env", {})
if env.get("CLAUDE_CODE_OAUTH_TOKEN") != token:
    env["CLAUDE_CODE_OAUTH_TOKEN"] = token
    save(path, settings)
if os.stat(path).st_mode & 0o777 != 0o600:
    os.chmod(path, 0o600)
    changed = True

# A new profile also inherits which folders the normal login already trusts.
path = os.path.join(profile, ".claude.json")
state = load(path)
if state is None:
    main = load(os.path.join(home, ".claude.json")) or {}
    state = {"projects": {p: {"hasTrustDialogAccepted": True}
                          for p, v in main.get("projects", {}).items()
                          if v.get("hasTrustDialogAccepted")}}
if not state.get("hasCompletedOnboarding"):
    state["hasCompletedOnboarding"] = True
    save(path, state)
done()
PY
}

# Writes the line $2 to the file $1, only when the file says something else.
put() {
  if [ "$(cat "$1" 2>/dev/null)" != "$2" ]; then printf '%s\n' "$2" > "$1"; changed=1; fi
}

kept=" "
for i in ${sel[@]+"${sel[@]}"}; do
  n=${nums[$i]} email=${emails[$i]}
  dir="$profiles/claude$n"
  if [ ! -d "$dir" ]; then mkdir -p "$dir"; changed=1; fi
  chmod 700 "$dir"
  put "$dir/.account" "$email"
  w=$(write_profile "$dir" "${toks[$i]}")
  if [ -n "$w" ]; then changed=1; fi
  # Share the hand-written parts of the normal config.
  for f in CLAUDE.md agents commands; do
    if [ -e "$HOME/.claude/$f" ] && [ ! -e "$dir/$f" ] && [ ! -L "$dir/$f" ]; then
      ln -s "$HOME/.claude/$f" "$dir/$f"
      changed=1
    fi
  done
  kept="$kept$n "
  say "$(printf '  clod%-3s %-32s %s' "$n" "$email" "$dir")"
done

shopt -s nullglob
for dir in "$profiles"/claude*; do
  k=${dir##*/claude}
  case "$k" in ''|*[!0-9]*) continue ;; esac
  [ -f "$dir/.account" ] || continue
  case "$kept" in *" $k "*) continue ;; esac
  if [ -n "$prune" ]; then
    w=$(write_profile "$dir" "")
    say "  pruned claude$k ($(cat "$dir/.account")): token removed, history kept"
    rm -f "$dir/.account"
    changed=1
  else
    case " ${nums[*]-} " in
      *" $k "*) ;;
      *) echo "setup.sh: note: $dir has no token in $tokens any more (--prune takes it out)" >&2 ;;
    esac
  fi
done

# The machine's default, if it still has a token: --keep-default keeps it.
had=""
if [ -f "$profiles/default" ] && [ -f "$profiles/claude$(cat "$profiles/default")/.account" ]; then
  had=$(cat "$profiles/default")
fi
want=$default
if [ -n "$keep" ] && [ -n "$had" ]; then want=""; fi
case "$want" in
  '') ;;
  none)
    if [ -e "$profiles/default" ]; then rm -f "$profiles/default"; changed=1; fi
    ;;
  *)
    k=""
    for dir in "$profiles"/claude*; do
      [ -f "$dir/.account" ] || continue
      if [ "$want" = "${dir##*/claude}" ] || [ "$want" = "$(cat "$dir/.account")" ]; then k=${dir##*/claude}; fi
    done
    [ -n "$k" ] || die "--default $want: there's no such profile on this machine"
    put "$profiles/default" "$k"
    ;;
esac
if [ -f "$profiles/default" ] && [ ! -f "$profiles/claude$(cat "$profiles/default")/.account" ]; then
  say "  default claude$(cat "$profiles/default") is gone, so plain claude is back on the normal login"
  rm -f "$profiles/default"
  changed=1
fi
if [ -f "$profiles/default" ]; then
  k=$(cat "$profiles/default")
  say "  default: claude$k ($(cat "$profiles/claude$k/.account")) for plain claude in new shells"
fi

hook="if [ -f \"$clods/clods.sh\" ]; then . \"$clods/clods.sh\"; fi"
hooked=""
add_hook() {
  if [ -f "$1" ] && grep -qF "$clods/clods.sh" "$1"; then hooked="$hooked $1"; return; fi
  printf '\n# Claude Code account profiles: clod1..clodN, clods (see %s)\n%s\n' "$clods/README.md" "$hook" >> "$1"
  hooked="$hooked $1"
  changed=1
}
for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
  if [ -f "$rc" ]; then add_hook "$rc"; fi
done
if [ -z "$hooked" ]; then
  case "${SHELL:-}" in *zsh) add_hook "$HOME/.zshrc" ;; *) add_hook "$HOME/.bashrc" ;; esac
fi
say "  hooked into:$hooked (new shells; or now: . $clods/clods.sh)"

if [ -n "$summary" ]; then
  accounts=0
  for dir in "$profiles"/claude*; do
    if [ -f "$dir/.account" ]; then accounts=$((accounts + 1)); fi
  done
  printf 'clods: accounts=%s default=%s changed=%s\n' "$accounts" "$(cat "$profiles/default" 2>/dev/null || echo none)" "$changed"
fi
