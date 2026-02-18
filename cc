#!/usr/bin/env bash
# cc — phone-friendly Claude Code session manager built on tmux
# Usage: cc [command] [args...]

set -euo pipefail

CC_HOME="${CC_HOME:-$HOME/.cc}"
CC_SESSIONS="$CC_HOME/sessions"
CC_DEFAULTS="$CC_HOME/defaults.env"
CC_VERSION="0.2.0"

# ── Colors (disabled when not a tty or TERM is dumb) ────────────────────────
if [[ -t 1 ]] && [[ "${TERM:-dumb}" != "dumb" ]]; then
  BOLD=$'\033[1m' DIM=$'\033[2m' RESET=$'\033[0m'
  RED=$'\033[31m' GREEN=$'\033[32m' YELLOW=$'\033[33m'
  BLUE=$'\033[34m' CYAN=$'\033[36m'
else
  BOLD='' DIM='' RESET='' RED='' GREEN='' YELLOW='' BLUE='' CYAN=''
fi

# ── Helpers ──────────────────────────────────────────────────────────────────

die() { echo "${RED}error:${RESET} $*" >&2; exit 1; }

need_tmux() {
  command -v tmux &>/dev/null || die "tmux is required. Install with: brew install tmux"
}

ensure_dirs() {
  mkdir -p "$CC_SESSIONS"
  [[ -f "$CC_DEFAULTS" ]] || touch "$CC_DEFAULTS"
}

# Resolve session name from partial match or alias
resolve_session() {
  local query="$1"
  # Exact match first
  if [[ -f "$CC_SESSIONS/$query.env" ]]; then
    echo "$query"
    return
  fi
  # Prefix match
  local matches=()
  for f in "$CC_SESSIONS"/*.env; do
    [[ -f "$f" ]] || continue
    local name
    name=$(basename "$f" .env)
    if [[ "$name" == "$query"* ]]; then
      matches+=("$name")
    fi
  done
  if [[ ${#matches[@]} -eq 1 ]]; then
    echo "${matches[0]}"
    return
  elif [[ ${#matches[@]} -gt 1 ]]; then
    die "ambiguous: '$query' matches ${matches[*]}"
  fi
  # Number lookup (for dashboard shortcuts)
  if [[ "$query" =~ ^[0-9]+$ ]]; then
    local i=1
    for f in "$CC_SESSIONS"/*.env; do
      [[ -f "$f" ]] || continue
      if [[ "$i" -eq "$query" ]]; then
        basename "$f" .env
        return
      fi
      ((i++))
    done
  fi
  die "session '$query' not found"
}

# Load session config into env vars
load_session() {
  local name="$1"
  local file="$CC_SESSIONS/$name.env"
  [[ -f "$file" ]] || die "session '$name' not found"
  # shellcheck disable=SC1090
  source "$file"
}

# Load global defaults
load_defaults() {
  [[ -f "$CC_DEFAULTS" ]] && source "$CC_DEFAULTS"
}

# Get tmux session name (prefixed to avoid conflicts)
tmux_name() { echo "cc-$1"; }

# Check if tmux session is alive
is_running() {
  tmux has-session -t "$(tmux_name "$1")" 2>/dev/null
}

# Parse Claude Code flags from args, split into CC_FLAGS array and remaining args
# Sets: CC_FLAGS_STR (for storage), CC_DIR, CC_EXTRA_ARGS
parse_claude_flags() {
  CC_FLAGS=()
  CC_DIR=""
  CC_EXTRA_ARGS=()

  while [[ $# -gt 0 ]]; do
    case "$1" in
      # Flags that take a value
      --model|--permission-mode|--system-prompt|--append-system-prompt|\
      --mcp-config|--allowedTools|--allowed-tools|--disallowedTools|--disallowed-tools|\
      --add-dir|--effort|--settings|--setting-sources|--tools|\
      --plugin-dir|--agents|--agent|--betas|--debug-file|--json-schema|\
      --fallback-model|--max-budget-usd|--session-id|--file|--strict-mcp-config)
        CC_FLAGS+=("$1" "$2")
        shift 2
        ;;
      # Boolean flags
      --dangerously-skip-permissions|--allow-dangerously-skip-permissions|\
      --yolo|--verbose|--chrome|--no-chrome|--debug|\
      --disable-slash-commands|--no-session-persistence|--ide|--fork-session)
        # --yolo is an alias for --dangerously-skip-permissions
        if [[ "$1" == "--yolo" ]]; then
          CC_FLAGS+=("--dangerously-skip-permissions")
        else
          CC_FLAGS+=("$1")
        fi
        shift
        ;;
      # Working directory — resolve to absolute path
      --dir)
        CC_DIR="$(cd "$2" 2>/dev/null && pwd || echo "$2")"
        shift 2
        ;;
      # Everything else goes to extra args
      *)
        CC_EXTRA_ARGS+=("$1")
        shift
        ;;
    esac
  done
  CC_FLAGS_STR="${CC_FLAGS[*]:-}"
}

# ── Commands ─────────────────────────────────────────────────────────────────

cmd_register() {
  local name="${1:-}"
  [[ -n "$name" ]] || die "usage: cc register <name> [--model opus] [--yolo] [--dir /path] [claude flags...]"
  shift

  parse_claude_flags "$@"

  local file="$CC_SESSIONS/$name.env"
  cat > "$file" <<EOF
# cc session: $name
# registered: $(date -Iseconds)
CC_NAME="$name"
CC_DIR="${CC_DIR:-$(pwd)}"
CC_FLAGS="${CC_FLAGS_STR}"
EOF

  echo "${GREEN}registered${RESET} ${BOLD}$name${RESET} → ${CC_DIR:-$(pwd)}"
  [[ -n "$CC_FLAGS_STR" ]] && echo "  flags: $CC_FLAGS_STR"
}

cmd_start() {
  local name
  name=$(resolve_session "${1:-}")
  shift 2>/dev/null || true
  need_tmux
  load_defaults
  load_session "$name"

  local tname
  tname=$(tmux_name "$name")

  if is_running "$name"; then
    echo "${YELLOW}already running:${RESET} $name — attaching..."
    tmux attach-session -t "$tname"
    return
  fi

  local dir="${CC_DIR:-$HOME}"
  [[ -d "$dir" ]] || die "directory not found: $dir"

  # Build claude command
  local cmd="claude"
  # Apply global defaults
  if [[ -n "${CC_DEFAULT_FLAGS:-}" ]]; then
    cmd="$cmd $CC_DEFAULT_FLAGS"
  fi
  # Apply session flags
  if [[ -n "${CC_FLAGS:-}" ]]; then
    cmd="$cmd $CC_FLAGS"
  fi
  # Apply any extra inline args
  if [[ $# -gt 0 ]]; then
    cmd="$cmd $*"
  fi

  # Create tmux session and attach
  tmux new-session -d -s "$tname" -n "$name" -c "$dir" "$cmd"
  # Lock the window name so Claude can't overwrite it with escape sequences
  tmux set-option -t "$tname" allow-rename off 2>/dev/null
  tmux set-window-option -t "$tname" automatic-rename off 2>/dev/null
  echo "${GREEN}started${RESET} ${BOLD}$name${RESET} in $dir"
  echo "  ${DIM}$cmd${RESET}"
  tmux attach-session -t "$tname"
}

cmd_exec() {
  # Register + start in one step
  local name="${1:-}"
  [[ -n "$name" ]] || die "usage: cc exec <name> [--model opus] [--yolo] [--dir /path] [claude flags...]"
  shift

  parse_claude_flags "$@"

  # Register if not exists, or update
  local file="$CC_SESSIONS/$name.env"
  cat > "$file" <<EOF
# cc session: $name
# registered: $(date -Iseconds)
CC_NAME="$name"
CC_DIR="${CC_DIR:-$(pwd)}"
CC_FLAGS="${CC_FLAGS_STR}"
EOF

  # Start
  cmd_start "$name" "${CC_EXTRA_ARGS[@]+"${CC_EXTRA_ARGS[@]}"}"
}

cmd_attach() {
  local name
  name=$(resolve_session "${1:-}")
  need_tmux
  local tname
  tname=$(tmux_name "$name")
  is_running "$name" || die "session '$name' is not running. Start with: cc start $name"
  tmux attach-session -t "$tname"
}

cmd_peek() {
  local name
  name=$(resolve_session "${1:-}")
  local lines="${2:-30}"
  need_tmux

  local tname
  tname=$(tmux_name "$name")
  is_running "$name" || die "session '$name' is not running"

  echo "${BOLD}── $name ──${RESET}"
  tmux capture-pane -t "$tname" -p -S "-$lines" 2>/dev/null || echo "${DIM}(no output)${RESET}"
  echo "${BOLD}── end ──${RESET}"
}

cmd_send() {
  local name
  name=$(resolve_session "${1:-}")
  shift
  need_tmux

  local tname
  tname=$(tmux_name "$name")
  is_running "$name" || die "session '$name' is not running"

  # Check for --keys flag
  if [[ "${1:-}" == "--keys" ]]; then
    shift
    tmux send-keys -t "$tname" "$@"
    echo "${GREEN}sent keys${RESET} to $name"
  else
    local text="$*"
    [[ -n "$text" ]] || die "usage: cc send <name> <text>  or  cc send <name> --keys 'C-c'"
    tmux send-keys -t "$tname" "$text" Enter
    echo "${GREEN}sent${RESET} to $name: $text"
  fi
}

cmd_stop() {
  local name
  name=$(resolve_session "${1:-}")
  need_tmux

  local tname
  tname=$(tmux_name "$name")
  if is_running "$name"; then
    tmux kill-session -t "$tname"
    echo "${RED}stopped${RESET} $name"
  else
    echo "${DIM}$name is not running${RESET}"
  fi
}

cmd_rm() {
  local name
  name=$(resolve_session "${1:-}")

  # Stop if running
  if is_running "$name" 2>/dev/null; then
    cmd_stop "$name"
  fi

  rm -f "$CC_SESSIONS/$name.env"
  echo "${RED}removed${RESET} $name"
}

cmd_info() {
  local name
  name=$(resolve_session "${1:-}")
  load_session "$name"

  echo "${BOLD}$name${RESET}"
  echo "  dir:   ${CC_DIR:-"(not set)"}"
  echo "  flags: ${CC_FLAGS:-"(none)"}"
  if is_running "$name" 2>/dev/null; then
    echo "  status: ${GREEN}running${RESET}"
  else
    echo "  status: ${DIM}stopped${RESET}"
  fi
}

cmd_ls() {
  local i=1
  local has_any=false

  for f in "$CC_SESSIONS"/*.env; do
    [[ -f "$f" ]] || continue
    has_any=true
    local name
    name=$(basename "$f" .env)
    load_session "$name"

    local status_icon status_color
    if is_running "$name" 2>/dev/null; then
      status_icon="*"
      status_color="$GREEN"
    else
      status_icon=" "
      status_color="$DIM"
    fi

    # Build flag summary
    local flag_summary=""
    if [[ "${CC_FLAGS:-}" == *"--dangerously-skip-permissions"* ]]; then
      flag_summary+=" ${YELLOW}YOLO${RESET}"
    fi
    local model_match
    if model_match=$(echo "${CC_FLAGS:-}" | sed -n 's/.*--model \([^ ]*\).*/\1/p' 2>/dev/null) && [[ -n "$model_match" ]]; then
      flag_summary+=" ${CYAN}$model_match${RESET}"
    fi

    # Preview last output line if running
    local preview=""
    if is_running "$name" 2>/dev/null; then
      preview=$(tmux capture-pane -t "$(tmux_name "$name")" -p -S -3 2>/dev/null | grep -v '^$' | tail -1 | cut -c1-60)
      [[ -n "$preview" ]] && preview=" ${DIM}│ $preview${RESET}"
    fi

    printf " %s${status_color}%s${RESET} ${BOLD}%-16s${RESET} %s%s%s\n" \
      "$i" "$status_icon" "$name" "${DIM}${CC_DIR:-}${RESET}" "$flag_summary" "$preview"
    ((i++))
  done

  if ! $has_any; then
    echo "${DIM}No sessions registered. Run: cc register <name>${RESET}"
  fi
}

cmd_dashboard() {
  need_tmux
  clear

  echo "${BOLD}${BLUE}  cc${RESET} ${DIM}v${CC_VERSION}${RESET}"
  echo ""
  cmd_ls
  echo ""
  echo "${DIM}Commands: [n]umber to attach, p[n] to peek, s[n] to send, q to quit${RESET}"
  echo ""

  while true; do
    read -rp "${CYAN}cc>${RESET} " input || break
    case "$input" in
      q|quit|exit)
        break
        ;;
      ls|list)
        cmd_ls
        ;;
      p[0-9]*)
        local num="${input#p}"
        local name
        name=$(resolve_session "$num" 2>/dev/null) && cmd_peek "$name" || echo "${RED}invalid${RESET}"
        ;;
      s[0-9]*)
        # s1 fix the bug → send "fix the bug" to session 1
        local rest="${input#s}"
        local num="${rest%% *}"
        local text="${rest#* }"
        [[ "$text" == "$num" ]] && text=""
        local name
        if name=$(resolve_session "$num" 2>/dev/null); then
          if [[ -n "$text" ]]; then
            cmd_send "$name" "$text"
          else
            read -rp "  msg> " text
            [[ -n "$text" ]] && cmd_send "$name" "$text"
          fi
        else
          echo "${RED}invalid${RESET}"
        fi
        ;;
      [0-9]*)
        local name
        name=$(resolve_session "$input" 2>/dev/null) && cmd_attach "$name" || echo "${RED}invalid${RESET}"
        ;;
      "")
        ;;
      *)
        # Try as session name
        local name
        if name=$(resolve_session "$input" 2>/dev/null); then
          cmd_attach "$name"
        else
          echo "${RED}unknown:${RESET} $input"
        fi
        ;;
    esac
  done
}

cmd_defaults() {
  local subcmd="${1:-show}"
  case "$subcmd" in
    show)
      if [[ -s "$CC_DEFAULTS" ]]; then
        echo "${BOLD}Global defaults:${RESET}"
        cat "$CC_DEFAULTS"
      else
        echo "${DIM}No defaults set. Run: cc defaults edit${RESET}"
      fi
      ;;
    edit)
      "${EDITOR:-vi}" "$CC_DEFAULTS"
      ;;
    reset)
      : > "$CC_DEFAULTS"
      echo "${GREEN}defaults reset${RESET}"
      ;;
    *)
      die "usage: cc defaults [show|edit|reset]"
      ;;
  esac
}

cmd_stop_all() {
  for f in "$CC_SESSIONS"/*.env; do
    [[ -f "$f" ]] || continue
    local name
    name=$(basename "$f" .env)
    if is_running "$name" 2>/dev/null; then
      cmd_stop "$name"
    fi
  done
}

cmd_start_all() {
  for f in "$CC_SESSIONS"/*.env; do
    [[ -f "$f" ]] || continue
    local name
    name=$(basename "$f" .env)
    if ! is_running "$name" 2>/dev/null; then
      cmd_start "$name"
    fi
  done
}

cmd_serve() {
  local port="${1:-8822}"
  # Resolve cc-server.py relative to the real location of this script
  local script_dir
  script_dir="$(cd "$(dirname "$(readlink -f "$0" 2>/dev/null || realpath "$0" 2>/dev/null || echo "$0")")" && pwd)"
  local server="$script_dir/cc-server.py"
  [[ -f "$server" ]] || die "cc-server.py not found at $script_dir"
  command -v python3 &>/dev/null || die "python3 is required for cc serve"
  exec python3 "$server" "$port"
}

cmd_help() {
  cat <<EOF
${BOLD}cc${RESET} — phone-friendly Claude Code session manager ${DIM}v${CC_VERSION}${RESET}

${BOLD}Session management:${RESET}
  cc register <name> [flags]   Register a session with Claude Code flags
  cc start <name>              Start a registered session in tmux
  cc exec <name> [flags]       Register + start in one step
  cc stop <name>               Stop a running session
  cc rm <name>                 Remove a session (stops if running)
  cc start-all                 Start all registered sessions
  cc stop-all                  Stop all running sessions

${BOLD}Interaction:${RESET}
  cc attach <name>             Attach to a running session (detach: Ctrl-b d)
  cc peek <name> [lines]       View last N lines without attaching (default: 30)
  cc send <name> <text>        Send text + Enter to a session
  cc send <name> --keys 'C-c'  Send raw key sequence (e.g., Ctrl-C)

${BOLD}Info:${RESET}
  cc ls                        List all sessions with status and previews
  cc info <name>               Show session config details
  cc defaults [show|edit|reset] Manage global default flags

${BOLD}Dashboard:${RESET}
  cc                           Interactive dashboard (optimized for phone)

${BOLD}Web Dashboard:${RESET}
  cc serve [port]              Start web dashboard (default: 8822)

${BOLD}Claude Code flags (on register/exec):${RESET}
  --model <model>              Model: opus, sonnet, haiku, or full ID
  --yolo                       Alias for --dangerously-skip-permissions
  --dangerously-skip-permissions  Bypass all permission checks
  --allow-dangerously-skip-permissions  Enable skip-permissions as option
  --permission-mode <mode>     acceptEdits, bypassPermissions, default, plan, etc.
  --system-prompt <prompt>     Override system prompt
  --append-system-prompt <p>   Append to system prompt
  --mcp-config <path>          MCP server config file
  --allowedTools <tools>       Allowed tools list
  --disallowedTools <tools>    Disallowed tools list
  --effort <level>             low, medium, high
  --dir <path>                 Working directory (default: cwd)
  --verbose                    Verbose mode
  --chrome / --no-chrome       Browser automation toggle
  ... and all other claude CLI flags pass through

${BOLD}Session names support fuzzy matching:${RESET}
  cc peek api                  Matches 'api', 'api-server', etc.
  cc 3                         Attach to session #3 from cc ls

${BOLD}Examples:${RESET}
  cc register api --model opus --yolo --dir ~/Dev/myapp
  cc exec frontend --append-system-prompt "React + Tailwind"
  cc send api "fix the auth bug in login.ts"
  cc send api --keys 'C-c'
  cc peek api 50
EOF
}

# ── Main dispatch ────────────────────────────────────────────────────────────

ensure_dirs

case "${1:-}" in
  register|reg)     shift; cmd_register "$@" ;;
  start)            shift; cmd_start "$@" ;;
  exec|run)         shift; cmd_exec "$@" ;;
  attach|a)         shift; cmd_attach "$@" ;;
  peek|p)           shift; cmd_peek "$@" ;;
  send)             shift; cmd_send "$@" ;;
  stop|kill)        shift; cmd_stop "$@" ;;
  rm|remove|del)    shift; cmd_rm "$@" ;;
  ls|list)          shift; cmd_ls "$@" ;;
  info)             shift; cmd_info "$@" ;;
  defaults|config)  shift; cmd_defaults "$@" ;;
  start-all)        cmd_start_all ;;
  stop-all)         cmd_stop_all ;;
  serve|web)        shift; cmd_serve "$@" ;;
  help|--help|-h)   cmd_help ;;
  version|--version|-v) echo "cc v${CC_VERSION}" ;;
  "")               cmd_dashboard ;;
  *)
    # Try as session name shortcut
    name=$(resolve_session "$1" 2>/dev/null) && cmd_attach "$name" || {
      echo "${RED}unknown command:${RESET} $1"
      echo "Run ${BOLD}cc help${RESET} for usage"
      exit 1
    }
    ;;
esac
