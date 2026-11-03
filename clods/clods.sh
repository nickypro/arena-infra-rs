# Claude Code account profiles: shell helpers. ~/.zshrc and ~/.bashrc source this (setup.sh
# adds the line).
#
#   clod1 .. clodN [args]   run claude as profile N (= CLAUDE_CONFIG_DIR=~/.clods/profiles/claudeN claude)
#   clods                   list profiles; * marks the one plain `claude` uses in this shell
#   clods default N|EMAIL|none  pick the profile plain `claude` uses here and in new shells
#
# CLAUDE_CONFIG_DIR alone is enough, from any shell or tool: the token is in the profile's
# settings.json. These helpers add shortcuts, the notice, and the machine's default.

_CLODS_PROFILES="$HOME/.clods/profiles"

# New shells start on this machine's default profile, unless CLAUDE_CONFIG_DIR is already set.
if [ -z "${CLAUDE_CONFIG_DIR:-}" ] && [ -f "$_CLODS_PROFILES/default" ] &&
   [ -f "$_CLODS_PROFILES/claude$(cat "$_CLODS_PROFILES/default")/.account" ]; then
  CLAUDE_CONFIG_DIR="$_CLODS_PROFILES/claude$(cat "$_CLODS_PROFILES/default")"
  export CLAUDE_CONFIG_DIR
fi

# Says on stderr which account `claude` is about to use.
_clod_notice() {
  [ -t 2 ] || return 0
  local d="${CLAUDE_CONFIG_DIR%/}" msg
  if [ -n "$d" ] && [ -f "$d/.account" ]; then
    msg="profile ${d##*/} ($(cat "$d/.account")), whose token overrides any /login"
  elif [ -n "$d" ]; then
    msg="CLAUDE_CONFIG_DIR=$d"
  elif [ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]; then
    msg="CLAUDE_CODE_OAUTH_TOKEN from the environment, which overrides your login"
  else
    msg="your normal login (~/.claude)"
  fi
  printf '\033[2mclaude: %s\033[0m\n' "$msg" >&2
  if [ -n "${ANTHROPIC_API_KEY:-}${ANTHROPIC_AUTH_TOKEN:-}" ]; then
    printf 'claude: warning: ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN is set and takes precedence over that\n' >&2
  fi
}

# Wrap whatever `claude` already is (e.g. a function defined earlier in ~/.zshrc), once.
if typeset -f claude >/dev/null 2>&1 && ! typeset -f claude | grep -q _clod_notice; then
  eval "$(typeset -f claude | sed '1s/^claude /_clod_prev_claude /')"
fi

# `function NAME {`, not `NAME() {`: a `claude` alias in the rc file would be expanded in the
# latter, and the whole file would fail to load (bash and zsh). The alias still works: it runs
# this function.
function claude {
  _clod_notice
  if typeset -f _clod_prev_claude >/dev/null 2>&1; then
    # Remote Control needs a full login; profile tokens are inference-only.
    if [ -n "${CLAUDE_CONFIG_DIR:-}" ] && [ -f "$CLAUDE_CONFIG_DIR/.account" ]; then
      CLAUDE_NO_RC=1 _clod_prev_claude "$@"
    else
      _clod_prev_claude "$@"
    fi
  else
    command claude "$@"
  fi
}

function clods {
  [ -n "${ZSH_VERSION:-}" ] && setopt local_options null_glob
  local d n cur="${CLAUDE_CONFIG_DIR%/}" def="" mark
  [ -f "$_CLODS_PROFILES/default" ] && def="$_CLODS_PROFILES/claude$(cat "$_CLODS_PROFILES/default")"
  case "${1:-}" in
    '') ;;
    default)
      case "${2:-}" in
        none)
          rm -f "$_CLODS_PROFILES/default"
          case "$cur" in "$_CLODS_PROFILES"/claude*) unset CLAUDE_CONFIG_DIR ;; esac
          echo "plain claude uses your normal login here and in new shells" ;;
        '') echo "usage: clods default N|EMAIL|none" >&2; return 2 ;;
        *)
          # A number, or the email of a profile here (as setup.sh --default takes it).
          n=""
          for d in "$_CLODS_PROFILES"/claude*; do
            [ -f "$d/.account" ] || continue
            if [ "$2" = "${d##*/claude}" ] || [ "$2" = "$(cat "$d/.account")" ]; then n=${d##*/claude}; fi
          done
          [ -n "$n" ] || { echo "clods: there's no profile $2 here" >&2; return 1; }
          printf '%s\n' "$n" > "$_CLODS_PROFILES/default"
          export CLAUDE_CONFIG_DIR="$_CLODS_PROFILES/claude$n"
          echo "plain claude uses claude$n ($(cat "$CLAUDE_CONFIG_DIR/.account")) here and in new shells" ;;
      esac
      return ;;
    *) echo "usage: clods [default N|EMAIL|none]" >&2; return 2 ;;
  esac
  for d in "$_CLODS_PROFILES"/claude*; do
    [ -f "$d/.account" ] || continue
    if [ "$d" = "$cur" ]; then mark='*'; else mark=' '; fi
    printf '%s clod%-3s %-32s %s%s\n' "$mark" "${d##*/claude}" "$(cat "$d/.account")" "$d" \
      "$([ "$d" = "$def" ] && echo '  (default)')"
  done
  [ -n "$cur" ] || echo "* plain claude: your normal login"
}

_clod_define() {
  [ -n "${ZSH_VERSION:-}" ] && setopt local_options null_glob
  local d n
  for d in "$_CLODS_PROFILES"/claude*; do
    n=${d##*/claude}
    case "$n" in ''|*[!0-9]*) continue ;; esac
    [ -f "$d/.account" ] || continue
    eval "function clod$n { ( export CLAUDE_CONFIG_DIR='$d'; claude \"\$@\" ); }"
  done
}
_clod_define
