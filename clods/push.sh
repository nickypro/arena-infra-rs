#!/usr/bin/env bash
# Put the Claude Code profiles (see setup.sh) on many machines, and pick each machine's default.
# Each machine also gets this folder's scripts in its own ~/.clods (never the tokens file).
#
# Usage: push.sh [options] MACHINE[=ACCOUNT]...
#        push.sh [options] [-f FLEET_FILE]      (with no machines: the fleet file next to this script)
#   MACHINE     anything `ssh` reaches (e.g. pod names, once `arena ssh-config` is installed),
#               or with --arena an arena pod selector (apple, arena9-wish, apple..mayor, all)
#   ACCOUNT     the machine's default: a profile number or an email; "none" takes every token
#               off it (with -a too)
#   FLEET_FILE  "MACHINE [ACCOUNT]" per line, "#" comments: the whole fleet in one place
#
#   -d, --default ACCOUNT  the default for machines given without one ("none": no default)
#   -s, --spread           machines given without one get accounts in turn: 1, 2, ..., N, 1, ...
#   -a, --all-tokens       every machine gets every account (switch with clodN); otherwise a
#                          machine gets only its default's token
#   -k, --keep-default     a machine that already has a default (with a token) keeps it: the
#                          defaults above only go to machines with none. With -a, a re-push
#                          then moves nobody.
#   -t, --tokens FILE      the tokens file (default: tokens, next to this script)
#       --arena            go through `arena pods cp` + `arena pods run` (binary: $ARENA, or arena)
#   -y, --yes              pass --yes to arena (it asks before each copy and run otherwise)
#   -q, --quiet            no plan; a line per machine: its accounts and default
#   -n, --dry-run          show who gets what, send nothing
#
# A push sets a machine's whole state: an account not sent to it loses its token there (its
# history stays), and its default becomes ACCOUNT. Re-push to rotate tokens or reassign machines.
#
#   push.sh arena9-wish=2 arena9-apple=someone@example.com
#   push.sh --spread arena9-apple arena9-bloom arena9-wish     # accounts 1, 2, 3
#   push.sh -a -d 3 box1 box2                                   # all accounts, default 3
#   push.sh -a -k --spread box1 box2 box3                       # all accounts; new boxes in turn
#   push.sh --arena -y apple..mayor=1 wish=2                    # through the arena CLI
set -euo pipefail

die() { echo "push.sh: $*" >&2; exit 1; }
umask 077   # the installers it writes hold tokens
here=$(cd "$(dirname "$0")" && pwd)
setup="$here/setup.sh"
[ -f "$setup" ] || die "needs setup.sh next to it ($setup)"

tokens="$here/tokens" fallback="" spread="" all="" keep="" arena="" yes="" quiet="" dry="" fleet=""
machines=() accounts=()
while [ $# -gt 0 ]; do
  case "$1" in
    -d|--default) [ $# -ge 2 ] || die "$1 needs an account"; fallback=$2; shift 2 ;;
    -t|--tokens) [ $# -ge 2 ] || die "$1 needs a file"; tokens=$2; shift 2 ;;
    -f) [ $# -ge 2 ] || die "-f needs a file"; fleet=$2; shift 2 ;;
    -s|--spread) spread=1; shift ;;
    -a|--all-tokens) all=1; shift ;;
    -k|--keep-default) keep=1; shift ;;
    --arena) arena=1; shift ;;
    -y|--yes) yes=1; shift ;;
    -q|--quiet) quiet=1; shift ;;
    -n|--dry-run) dry=1; shift ;;
    -h|--help) sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p;}' "$0"; exit 0 ;;
    -*) die "unknown option $1 (see --help)" ;;
    *=*) machines+=("${1%=*}"); accounts+=("${1##*=}"); shift ;;
    *) machines+=("$1"); accounts+=(""); shift ;;
  esac
done
if [ ${#machines[@]} -eq 0 ] && [ -z "$fleet" ] && [ -f "$here/fleet" ]; then fleet="$here/fleet"; fi
if [ -n "$fleet" ]; then
  [ -r "$fleet" ] || die "can't read $fleet"
  while read -r m a _ <&3 || [ -n "${m:-}" ]; do
    case "${m:-}" in ''|'#'*) continue ;; esac
    machines+=("$m"); accounts+=("${a:-}")
  done 3< "$fleet"
fi
[ ${#machines[@]} -gt 0 ] || die "no machines given, and no $here/fleet (see --help)"

# The accounts, as "N email" (setup.sh parses the tokens file; no token leaves it here).
list=$(bash "$setup" --list "$tokens") || exit 1
[ -n "$list" ] || die "no tokens in $tokens"
read -ra nums <<< "$(printf '%s\n' "$list" | cut -d' ' -f1 | tr '\n' ' ')"

account() {  # account N|EMAIL|none -> N or none
  [ "$1" = none ] && { echo none; return; }
  printf '%s\n' "$list" | awk -v a="$1" '$1 == a || $2 == a { print $1; exit }' | grep . ||
    die "no account $1 in $tokens (have: $(printf '%s\n' "$list" | tr '\n' ' '))"
}
email() { printf '%s\n' "$list" | awk -v n="$1" '$1 == n { print $2 }'; }

# Settle each machine's default and the accounts it gets.
defaults=() sends=()
turn=0
for i in "${!machines[@]}"; do
  a=${accounts[$i]}
  if [ -z "$a" ] && [ -n "$spread" ]; then
    a=${nums[$((turn % ${#nums[@]}))]}
    turn=$((turn + 1))
  fi
  [ -n "$a" ] || a=$fallback
  [ -n "$a" ] || die "no account for ${machines[$i]}: give ${machines[$i]}=N, -d N or --spread"
  a=$(account "$a")
  defaults+=("$a")
  # MACHINE=none (or "none" in the fleet file) takes every token off, -a or not; `-d none` only
  # means no default.
  if [ "${accounts[$i]}" = none ]; then sends+=("none")
  elif [ -n "$all" ]; then sends+=("${nums[*]}")
  elif [ "$a" = none ]; then sends+=("none")
  else sends+=("$a"); fi
done

if [ -z "$quiet" ]; then
  printf '%-28s %-36s %s\n' MACHINE "DEFAULT${keep:+ (if it has none)}" ACCOUNTS
  for i in "${!machines[@]}"; do
    d=${defaults[$i]}
    [ "$d" = none ] && shown="normal login" || shown="claude$d ($(email "$d"))"
    printf '%-28s %-36s %s\n' "${machines[$i]}" "$shown" "${sends[$i]}"
  done
fi
[ -z "$dry" ] || exit 0

# The installer for machine $1 (setup.sh --bundle: the scripts and its tokens).
bundle() {
  local flags=(--bundle --prune --default "${defaults[$1]}")
  if [ -n "$keep" ]; then flags+=(--keep-default); fi
  if [ -n "$quiet" ]; then flags+=(--summary); fi
  # shellcheck disable=SC2086 # sends is a list of profile numbers
  bash "$setup" "${flags[@]}" "$tokens" ${sends[$1]}
}

# Under -q, a machine's output as one line: setup.sh's summary in words; anything else as it is.
summarize() {
  local m=$1 line acc def chg
  while IFS= read -r line; do
    case "$line" in
      'clods: accounts='*)
        acc=${line#*accounts=} def=${line#*default=} chg=${line#*changed=}
        acc=${acc%% *} def=${def%% *} chg=${chg%% *}
        [ "$def" = none ] && def="normal login" || def="claude$def"
        printf '%s: %s account(s), default %s%s\n' "$m" "$acc" "$def" "$([ "$chg" = 0 ] && echo ', unchanged')"
        ;;
      *) printf '[%s] %s\n' "$m" "$line" ;;
    esac
  done
}

fails=0
if [ -z "$arena" ]; then
  # One ssh per machine, the installer on its stdin: nothing with a token is left behind.
  for i in "${!machines[@]}"; do
    m=${machines[$i]}
    [ -n "$quiet" ] || echo "== $m"
    if bundle "$i" | ssh -o BatchMode=yes -o ConnectTimeout=15 "$m" 'bash -s' 2>&1 |
       if [ -n "$quiet" ]; then summarize "$m"; else sed "s|^|[$m] |"; fi; then
      :
    else
      echo "[$m] ✗ failed"
      fails=$((fails + 1))
    fi
  done
else
  # Through arena: one copy + one run per group of machines that get the same thing.
  ARENA=${ARENA:-arena}
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  # A name of this run's own: two pushes at once never run (or remove) each other's installer.
  remote=.claude-profiles-install-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n').sh
  run="f=\$HOME/$remote; bash \"\$f\"; rc=\$?; rm -f \"\$f\"; exit \$rc"
  done_keys=" "
  for i in "${!machines[@]}"; do
    key="${defaults[$i]}:${sends[$i]// /,}"
    case "$done_keys" in *" $key "*) continue ;; esac
    done_keys="$done_keys$key "
    sel=""
    for j in "${!machines[@]}"; do
      [ "${defaults[$j]}:${sends[$j]// /,}" = "$key" ] && sel="${sel:+$sel,}${machines[$j]}"
    done
    [ -n "$quiet" ] || echo "== $sel"
    b="$tmp/$remote"
    bundle "$i" > "$b"
    if "$ARENA" pods cp ${yes:+--yes} -t "$sel" "$b" "$remote" &&
       "$ARENA" pods run ${yes:+--yes} -t "$sel" "$run"; then
      :
    else
      echo "✗ $sel: failed (a copied installer is removed by the run step; if that didn't run," \
           "remove ~/$remote there: it holds tokens)"
      fails=$((fails + 1))
    fi
    rm -f "$b"
  done
fi

[ "$fails" -eq 0 ] || die "$fails push(es) failed"
[ -n "$quiet" ] || echo "done. On each machine: clods lists its accounts; new shells use its default."
