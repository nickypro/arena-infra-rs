#!/usr/bin/env bash
# clods' tests: setup.sh, push.sh and clods.sh run for real, each test in a scratch $HOME with
# fake tokens and a fake `claude`, `ssh` and `arena` on PATH. A "machine" is a directory under
# the test's hosts/: the fake ssh and arena run commands there with it as $HOME.
#
# Usage: clods/tests/run.sh [PATTERN]   (only the tests whose name contains PATTERN)
# clods.sh's tests run under bash and zsh; without zsh they run under bash only, unless
# CLODS_TEST_ZSH=1 (CI), which makes a missing zsh a failure.
# The shells' commands are in single quotes on purpose: they expand in the shell under test.
# shellcheck disable=SC2016
set -uo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/clods-tests.XXXXXX")
trap 'rm -rf "$work"' EXIT
only=${1:-}

# ---- assertions: each ends the test on failure ---------------------------------------------
fail() { echo "  ✗ $*" >&2; exit 1; }
eq() { [ "$1" = "$2" ] || fail "${3:-value}: expected [$2], got [$1]"; }
has() { case "$1" in *"$2"*) ;; *) fail "${3:-output} should contain [$2]; it was: [$1]" ;; esac; }
hasnt() { case "$1" in *"$2"*) fail "${3:-output} shouldn't contain [$2]; it was: [$1]" ;; esac; }
mode() { stat -c %a "$1"; }
exists() { [ -e "$1" ] || [ -L "$1" ] || fail "$1 should exist"; }
gone() { [ ! -e "$1" ] || fail "$1 shouldn't exist"; }
# The token in a profile's settings.json ("" when there's none).
token_of() {
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("env",{}).get("CLAUDE_CODE_OAUTH_TOKEN",""))' "$1/settings.json"
}

# ---- a test's world ----------------------------------------------------------------------------
# T: its directory. HOME: $T/home. $T/clods: a copy of the tool (as cloned somewhere else).
# hosts/NAME: a machine for the fake ssh and arena. Fake bins log their argv in $T/*.log.
world() {
  T=$work/$1
  mkdir -p "$T/home" "$T/bin" "$T/hosts" "$T/clods" "$T/tmp"
  export HOME=$T/home SHELL=/bin/bash TMPDIR=$T/tmp
  unset CLAUDE_CONFIG_DIR CLAUDE_CODE_OAUTH_TOKEN ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN
  cp "$root"/setup.sh "$root"/push.sh "$root"/clods.sh "$root"/README.md "$root"/fleet.example "$root"/.gitignore "$T/clods/"
  touch "$HOME/.zshrc" "$HOME/.bashrc"
  cat > "$T/bin/claude" <<'EOF'
#!/usr/bin/env bash
[ "${1:-}" = --version ] && { echo "2.1.30 (Claude Code)"; exit 0; }
echo "fake-claude config=${CLAUDE_CONFIG_DIR:-} no_rc=${CLAUDE_NO_RC:-} args=$*"
EOF
  # ssh [-o OPT]... HOST CMD: CMD under bash in hosts/HOST, stdin passed on.
  cat > "$T/bin/ssh" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >> "$T/ssh.log"
while [ \$# -gt 0 ]; do case "\$1" in -o) shift 2 ;; -*) shift ;; *) break ;; esac; done
h=\$1; shift
[ -d "$T/hosts/\$h" ] || { echo "ssh: connect to host \$h port 22: Connection refused" >&2; exit 255; }
HOME="$T/hosts/\$h" TMPDIR="$T/tmp" exec bash -c "\$*"
EOF
  # arena pods cp|run [--yes] -t SEL ...: each pod in SEL (comma-separated) is hosts/POD.
  cat > "$T/bin/arena" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >> "$T/arena.log"
[ "\$1" = pods ] || exit 2
op=\$2; shift 2; sel=""
while [ \$# -gt 0 ]; do case "\$1" in --yes) shift ;; -t) sel=\$2; shift 2 ;; *) break ;; esac; done
[ -z "\${ARENA_FAIL:-}" ] || { echo "arena: \$ARENA_FAIL" >&2; exit 1; }
for h in \${sel//,/ }; do
  case \$op in
    cp) cp "\$1" "$T/hosts/\$h/\$2" ;;
    run) HOME="$T/hosts/\$h" TMPDIR="$T/tmp" bash -c "\$1" 2>&1 | sed "s|^|[\$h] |" ;;
  esac
done
EOF
  chmod +x "$T/bin/"*
  export PATH="$T/bin:$PATH"
}
tokens() { printf '%s\n' "$@" > "$T/clods/tokens"; chmod 600 "$T/clods/tokens"; }
host() { for h in "$@"; do mkdir -p "$T/hosts/$h"; touch "$T/hosts/$h/.zshrc" "$T/hosts/$h/.bashrc"; done; }
P() { echo "$HOME/.clods/profiles"; }
HP() { echo "$T/hosts/$1/.clods/profiles"; }
# Every file under $1 whose text holds a fake token, but the profiles' settings.json.
token_files() { grep -rl 'sk-ant-oat01-FAKE' "$1" 2>/dev/null | grep -v '/\.clods/profiles/claude[0-9]*/settings\.json$' || true; }

FOUR=("a@example.com -> sk-ant-oat01-FAKEaaaa" "b@example.com = sk-ant-oat01-FAKEbbbb" \
      "5 c@example.com sk-ant-oat01-FAKEcccc" "d@example.com -> sk-ant-oat01-FAKEdddd")

# ---- setup.sh ---------------------------------------------------------------------------------
test_numbering_and_pins() {
  tokens "# accounts" "${FOUR[@]}" "" "not a token line"
  out=$(bash "$T/clods/setup.sh" --list "$T/clods/tokens" 2>"$T/err")
  eq "$out" $'1 a@example.com\n2 b@example.com\n5 c@example.com\n6 d@example.com' "--list"
  has "$(cat "$T/err")" "skipping a line I can't parse: not a token line"
  tokens "a@x -> sk-ant-oat01-FAKEaaaa" "1 b@x -> sk-ant-oat01-FAKEbbbb"
  if bash "$T/clods/setup.sh" --list "$T/clods/tokens" 2>"$T/err"; then fail "a number twice should fail"; fi
  has "$(cat "$T/err")" "has profile 1 twice"
  tokens "0 a@x -> sk-ant-oat01-FAKEaaaa"
  if bash "$T/clods/setup.sh" --list "$T/clods/tokens" 2>/dev/null; then fail "profile 0 should fail"; fi
  # A line it can't parse is named without its token.
  tokens "a@x -> sk-ant-oat01-FAKEaaaa" "b@x -> sk-ant-oat01-FAKE!bad"
  bash "$T/clods/setup.sh" --list "$T/clods/tokens" 2>"$T/err" >/dev/null
  hasnt "$(cat "$T/err")" "FAKE!bad" "the warning"
}

test_setup_installs_profiles() {
  tokens "${FOUR[@]}"
  mkdir -p "$HOME/.claude/agents"; echo "notes" > "$HOME/.claude/CLAUDE.md"
  echo '{"theme": "dark"}' > "$HOME/.claude/settings.json"
  echo '{"projects": {"/w": {"hasTrustDialogAccepted": true}, "/x": {}}}' > "$HOME/.claude.json"
  out=$(bash "$T/clods/setup.sh" "$T/clods/tokens")
  has "$out" "clod5"
  for n in 1 2 5 6; do
    d=$(P)/claude$n
    eq "$(mode "$d")" 700 "claude$n's mode"
    eq "$(mode "$d/settings.json")" 600 "claude$n settings.json's mode"
  done
  eq "$(cat "$(P)/claude5/.account")" c@example.com
  eq "$(token_of "$(P)/claude5")" sk-ant-oat01-FAKEcccc
  eq "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["theme"])' "$(P)/claude1/settings.json")" dark "the copied settings"
  eq "$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["hasCompletedOnboarding"], sorted(d["projects"]))' "$(P)/claude1/.claude.json")" "True ['/w']"
  eq "$(readlink "$(P)/claude1/CLAUDE.md")" "$HOME/.claude/CLAUDE.md"
  exists "$(P)/claude1/agents"
  gone "$(P)/claude1/commands"
  gone "$(P)/default"
  eq "$(mode "$(P)")" 700 "profiles/"
  # The tool is in ~/.clods; never the tokens file.
  for f in setup.sh push.sh clods.sh README.md fleet.example .gitignore; do exists "$HOME/.clods/$f"; done
  gone "$HOME/.clods/tokens"
  eq "$(grep -c '\.clods/clods\.sh' "$HOME/.zshrc")" 1 "hook lines in .zshrc"
  eq "$(grep -c '\.clods/clods\.sh' "$HOME/.bashrc")" 1 "hook lines in .bashrc"
}

test_rerun_changes_nothing() {
  tokens "${FOUR[@]}"
  bash "$T/clods/setup.sh" --default 2 "$T/clods/tokens" >/dev/null
  (cd "$HOME" && find . -printf '%p %T@ %m\n' | sort) > "$T/before"
  sleep 1.1
  out=$(bash "$T/clods/setup.sh" --summary --default 2 "$T/clods/tokens")
  eq "$out" "clods: accounts=4 default=2 changed=0" "the summary"
  (cd "$HOME" && find . -printf '%p %T@ %m\n' | sort) > "$T/after"
  diff "$T/before" "$T/after" >&2 || fail "a re-run changed files"
  eq "$(grep -c '\.clods/clods\.sh' "$HOME/.zshrc")" 1 "hook lines after a re-run"
  # From ~/.clods itself too (the usual way, once it's there).
  out=$(bash "$HOME/.clods/setup.sh" --summary "$T/clods/tokens")
  eq "$out" "clods: accounts=4 default=2 changed=0" "the summary from ~/.clods"
}

test_default_and_none() {
  tokens "${FOUR[@]}"
  bash "$T/clods/setup.sh" --default 2 "$T/clods/tokens" >/dev/null
  eq "$(cat "$(P)/default")" 2
  bash "$T/clods/setup.sh" --default c@example.com "$T/clods/tokens" >/dev/null
  eq "$(cat "$(P)/default")" 5 "default by email"
  out=$(bash "$T/clods/setup.sh" --summary "$T/clods/tokens")
  eq "$out" "clods: accounts=4 default=5 changed=0" "no --default keeps it"
  out=$(bash "$T/clods/setup.sh" --summary --default none "$T/clods/tokens")
  eq "$out" "clods: accounts=4 default=none changed=1"
  gone "$(P)/default"
  if bash "$T/clods/setup.sh" --default 9 "$T/clods/tokens" >/dev/null 2>"$T/err"; then fail "--default 9 should fail"; fi
  has "$(cat "$T/err")" "no such profile"
}

test_spread_and_keep_default() {
  tokens "${FOUR[@]}"
  for t in 0 1 2 3 4 7; do
    rm -rf "$HOME/.clods"
    bash "$T/clods/setup.sh" --spread "$t" "$T/clods/tokens" >/dev/null
    want=$(echo "1 2 5 6" | cut -d' ' -f$((t % 4 + 1)))
    eq "$(cat "$(P)/default")" "$want" "--spread $t"
  done
  # Kept: a machine with a default keeps it, whatever the turn.
  out=$(bash "$T/clods/setup.sh" --summary --keep-default --spread 0 "$T/clods/tokens")
  eq "$out" "clods: accounts=4 default=6 changed=0" "--keep-default"
  out=$(bash "$T/clods/setup.sh" --summary --keep-default --default 1 "$T/clods/tokens")
  eq "$out" "clods: accounts=4 default=6 changed=0" "--keep-default with --default"
  # Its default's account taken off it: the turn picks one.
  tokens "${FOUR[@]:0:3}"
  out=$(bash "$T/clods/setup.sh" --summary --prune --keep-default --spread 1 "$T/clods/tokens")
  eq "$out" "clods: accounts=3 default=2 changed=1" "--keep-default after its account went"
  if bash "$T/clods/setup.sh" --spread 1 --default 2 "$T/clods/tokens" >/dev/null 2>&1; then fail "--spread with --default should fail"; fi
  if bash "$T/clods/setup.sh" --spread x "$T/clods/tokens" >/dev/null 2>&1; then fail "--spread x should fail"; fi
}

test_tokens_change() {
  tokens "${FOUR[@]}"
  bash "$T/clods/setup.sh" --default 2 "$T/clods/tokens" >/dev/null
  mkdir -p "$(P)/claude2/projects"; echo history > "$(P)/claude2/projects/s.jsonl"
  # A rotated token, and an account added at the end.
  tokens "a@example.com -> sk-ant-oat01-FAKEnewa" "${FOUR[@]:1}" "e@example.com -> sk-ant-oat01-FAKEeeee"
  out=$(bash "$T/clods/setup.sh" --summary "$T/clods/tokens")
  eq "$out" "clods: accounts=5 default=2 changed=1"
  eq "$(token_of "$(P)/claude1")" sk-ant-oat01-FAKEnewa "the rotated token"
  eq "$(cat "$(P)/claude7/.account")" e@example.com "the new account's number"
  # An account taken out of the file: kept until --prune, which keeps its history.
  tokens "${FOUR[0]}" "${FOUR[@]:2}" "e@example.com -> sk-ant-oat01-FAKEeeee"
  bash "$T/clods/setup.sh" "$T/clods/tokens" >/dev/null 2>"$T/err"
  has "$(cat "$T/err")" "claude2 has no token in"
  eq "$(token_of "$(P)/claude2")" sk-ant-oat01-FAKEbbbb "a token not pruned"
  out=$(bash "$T/clods/setup.sh" --prune "$T/clods/tokens")
  has "$out" "pruned claude2 (b@example.com)"
  has "$out" "default claude2 is gone"
  eq "$(token_of "$(P)/claude2")" "" "the pruned token"
  gone "$(P)/claude2/.account"
  gone "$(P)/default"
  eq "$(cat "$(P)/claude2/projects/s.jsonl")" history "the pruned profile's history"
}

test_bundle_installs_and_leaves_no_token() {
  tokens "${FOUR[@]}"
  host box
  bash "$T/clods/setup.sh" --bundle --prune --summary --spread 1 "$T/clods/tokens" 1 2 > "$T/bundle.sh"
  out=$(HOME=$T/hosts/box bash -s < "$T/bundle.sh")
  eq "$out" "clods: accounts=2 default=2 changed=1"
  eq "$(cat "$(HP box)/claude1/.account")" a@example.com
  gone "$(HP box)/claude5"
  eq "$(mode "$(HP box)/claude1/settings.json")" 600
  eq "$(mode "$(HP box)/claude1/.account")" 600 ".account's mode"
  eq "$(mode "$(HP box)/default")" 600 "default's mode"
  eq "$(token_files "$T/hosts/box")" "" "files with a token outside the profiles"
  eq "$(ls -A "$T/tmp")" "" "what the installer left in TMPDIR"
  out=$(HOME=$T/hosts/box bash -s < "$T/bundle.sh")
  eq "$out" "clods: accounts=2 default=2 changed=0" "the bundle again"
}

test_help() {
  has "$(bash "$T/clods/setup.sh" --help)" "--keep-default"
  has "$(bash "$T/clods/push.sh" --help)" "--keep-default"
  if bash "$T/clods/setup.sh" --nope 2>/dev/null; then fail "an unknown option should fail"; fi
}

# ---- push.sh ----------------------------------------------------------------------------------
test_push_spread() {
  tokens "${FOUR[@]:0:3}"
  host h1 h2 h3 h4
  out=$(bash "$T/clods/push.sh" -n --spread h1 h2 h3 h4)
  has "$out" "h4                           claude1 (a@example.com)"
  gone "$T/ssh.log"
  bash "$T/clods/push.sh" --spread h1 h2 h3 h4 > "$T/out"
  eq "$(cat "$(HP h2)/default")" 2
  eq "$(cat "$(HP h3)/default")" 5
  eq "$(cat "$(HP h4)/default")" 1
  # Without -a, each only its default's token.
  eq "$(cd "$(HP h2)" && ls -d claude*)" claude2 "h2's profiles"
  hasnt "$(cat "$T/ssh.log")" "sk-ant" "ssh's command lines"
  eq "$(token_files "$T/hosts")" "" "token files outside the profiles"
}

test_push_all_and_account_off() {
  tokens "${FOUR[@]:0:3}"
  host h1 h2
  bash "$T/clods/push.sh" -a --spread h1 h2 >/dev/null
  eq "$(cd "$(HP h1)" && find . -name .account | sort | tr '\n' ' ')" "./claude1/.account ./claude2/.account ./claude5/.account " "h1 with -a"
  eq "$(cat "$(HP h2)/default")" 2
  # A push sets the machine's whole state: h1 on account 5 alone.
  bash "$T/clods/push.sh" h1=5 >/dev/null
  eq "$(token_of "$(HP h1)/claude1")" "" "h1's account 1"
  gone "$(HP h1)/claude1/.account"
  eq "$(token_of "$(HP h1)/claude5")" sk-ant-oat01-FAKEcccc
  eq "$(cat "$(HP h1)/default")" 5
}

test_push_none_takes_everything_off() {
  tokens "${FOUR[@]:0:3}"
  host h1 h2
  bash "$T/clods/push.sh" -a -d 1 h1 h2 >/dev/null
  # Explicit none, with -a: every token off. `-d none` with -a: every account, no default.
  out=$(bash "$T/clods/push.sh" -n -a -d none h1=none h2)
  has "$out" "h1                           normal login                         none"
  bash "$T/clods/push.sh" -a -d none h1=none h2 >/dev/null
  for n in 1 2 5; do eq "$(token_of "$(HP h1)/claude$n")" "" "h1's claude$n"; done
  gone "$(HP h1)/default"
  eq "$(token_of "$(HP h2)/claude5")" sk-ant-oat01-FAKEcccc "h2's claude5"
  gone "$(HP h2)/default"
  # The fleet file's none too.
  printf 'h2 none\n' > "$T/clods/fleet"
  bash "$T/clods/push.sh" -a >/dev/null
  eq "$(token_of "$(HP h2)/claude1")" "" "h2's claude1 after the fleet's none"
}

test_push_keep_default_and_quiet() {
  tokens "${FOUR[@]:0:3}"
  host h1 h2 h3
  bash "$T/clods/push.sh" -a --spread h1 h2 >/dev/null
  # Another machine, the list in another order: -k moves nobody who has a default.
  out=$(bash "$T/clods/push.sh" -q -a -k --spread h3 h2 h1)
  eq "$out" $'h3: 3 account(s), default claude1\nh2: 3 account(s), default claude2, unchanged\nh1: 3 account(s), default claude1, unchanged' "-q -k"
}

test_push_unreachable() {
  tokens "${FOUR[@]:0:2}"
  host h1
  if bash "$T/clods/push.sh" -d 1 nowhere h1 > "$T/out" 2>&1; then fail "a failed push should fail"; fi
  has "$(cat "$T/out")" "[nowhere] ✗ failed"
  has "$(cat "$T/out")" "1 push(es) failed"
  eq "$(cat "$(HP h1)/default")" 1 "the reachable machine"
}

test_push_arena() {
  tokens "${FOUR[@]:0:3}"
  host p1 p2 p3
  bash "$T/clods/push.sh" --arena -y -a --spread p1 p2 p3 > "$T/out"
  eq "$(cat "$(HP p1)/default")" 1
  eq "$(cat "$(HP p3)/default")" 5
  # One copy and one run per group; the installer removed after it ran.
  eq "$(grep -c '^pods cp --yes -t p1 ' "$T/arena.log")" 1 "p1's copies"
  has "$(cat "$T/arena.log")" "pods run --yes -t p2 "
  hasnt "$(cat "$T/arena.log")" "sk-ant" "arena's command lines"
  gone "$T/hosts/p1/.claude-profiles-install.sh"
  eq "$(token_files "$T/hosts")" "" "token files outside the profiles"
  if ARENA_FAIL="pod not found" bash "$T/clods/push.sh" --arena -y -d 1 p1 > "$T/out" 2>&1; then fail "a failed arena push should fail"; fi
  has "$(cat "$T/out")" "✗ p1: failed"
}

# ---- clods.sh, under bash and zsh -------------------------------------------------------------
shells() {
  echo bash
  if command -v zsh >/dev/null; then echo zsh
  elif [ -n "${CLODS_TEST_ZSH:-}" ]; then fail "no zsh (CLODS_TEST_ZSH=1)"
  fi
}
# Run $2 in shell $1, as a new login of this machine would: $3 (an rc file's lines), then the
# hook. A script file, read line by line as an rc file is (an alias applies to the lines after).
in_shell() {
  printf '%s\n' "${3:-}" '. "$HOME/.clods/clods.sh"' "$2" > "$T/script"
  case $1 in
    bash) bash -O expand_aliases "$T/script" ;;
    zsh) zsh -f "$T/script" ;;
  esac
}

test_clods_sh() {
  tokens "${FOUR[@]:0:3}"
  bash "$T/clods/setup.sh" --default 2 "$T/clods/tokens" >/dev/null
  for sh in $(shells); do
    eq "$(in_shell "$sh" 'echo "$CLAUDE_CONFIG_DIR"')" "$(P)/claude2" "$sh: a new shell's profile"
    out=$(in_shell "$sh" clods)
    has "$out" "* clod2   b@example.com" "$sh: clods"
    has "$out" "(default)" "$sh: clods"
    eq "$(in_shell "$sh" 'clod5 hi')" "fake-claude config=$(P)/claude5 no_rc= args=hi" "$sh: clod5"
    eq "$(in_shell "$sh" 'claude hi')" "fake-claude config=$(P)/claude2 no_rc= args=hi" "$sh: plain claude"
    has "$(in_shell "$sh" 'clods default a@example.com')" "claude1 (a@example.com)" "$sh: default by email"
    eq "$(cat "$(P)/default")" 1 "$sh: the default file"
    eq "$(in_shell "$sh" 'clods default none >/dev/null; echo "[${CLAUDE_CONFIG_DIR:-}]"')" "[]" "$sh: default none"
    gone "$(P)/default"
    eq "$(in_shell "$sh" 'echo "[${CLAUDE_CONFIG_DIR:-}]"; clods | tail -1')" $'[]\n* plain claude: your normal login' "$sh: no default"
    if in_shell "$sh" 'clods default 9' 2>/dev/null; then fail "$sh: clods default 9 should fail"; fi
    in_shell "$sh" 'clods default 2' >/dev/null
  done
}

test_clods_sh_wraps_claude() {
  tokens "${FOUR[@]:0:2}"
  bash "$T/clods/setup.sh" --default 1 "$T/clods/tokens" >/dev/null
  for sh in $(shells); do
    # A claude function from the rc file: wrapped, and told a profile can't do Remote Control.
    eq "$(in_shell "$sh" 'claude x' 'claude() { echo "mine"; command claude "$@"; }')" \
       $'mine\n'"fake-claude config=$(P)/claude1 no_rc=1 args=x" "$sh: a claude function"
    # Sourced twice, it wraps once.
    eq "$(in_shell "$sh" '. "$HOME/.clods/clods.sh"; claude x | head -1' 'claude() { echo "mine"; command claude "$@"; }')" mine "$sh: sourced twice"
    # A claude alias: the file still loads, and the alias still runs.
    eq "$(in_shell "$sh" 'claude x; type clods >/dev/null && echo loaded' "alias claude='claude --flag'")" \
       "fake-claude config=$(P)/claude1 no_rc= args=--flag x"$'\nloaded' "$sh: a claude alias"
  done
}

# ---- run them ---------------------------------------------------------------------------------
pass=0 failed=()
for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
  case "$t" in *"$only"*) ;; *) continue ;; esac
  # Not `if ( … )`: set -e is ignored inside a condition, and each assertion relies on it too.
  ( set -e; world "$t"; "$t" ) > "$work/$t.log" 2>&1
  rc=$?
  if [ "$rc" -eq 0 ]; then
    pass=$((pass + 1)); echo "✓ ${t#test_}"
  else
    failed+=("$t"); echo "✗ ${t#test_}"; sed 's/^/    /' "$work/$t.log"
  fi
done
echo "$pass passed, ${#failed[@]} failed"
[ ${#failed[@]} -eq 0 ]
