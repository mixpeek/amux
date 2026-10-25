#!/bin/bash
# amux pkill/pgrep guard. Installed by the amux server as `pkill` and `pgrep` in
# ~/.amux/shims/tmux-fleet/, the directory already first on every worker's PATH,
# so it covers what a worker types, its subagents, and the scripts they run.
#
# Why: macOS pkill and pgrep are BSD. Options must come BEFORE the pattern;
# anything after the first pattern is read as another pattern. On 2026-10-21 at
# 10:54:12 a worker ran `pkill -f "explore.js" -P 1` (meant: explore.js children
# of launchd). BSD read "-P" and "1" as two more patterns, and "1" appears in
# nearly every command line: 226 processes matched, and the SIGTERM took down
# every Claude worker (the fleet relaunched at 10:55), LaunchAgents, widgets,
# and left 36 crashpad handlers crashing as apps restarted helpers. A test
# cleanup did the same on 2026-10-19 (184 processes). A note in one lane's
# memory did not reach the next lane; a shim on PATH does.
#
# What it does:
# - Refuses an option after the pattern, for pkill and pgrep (a misordered
#   pgrep feeding `kill` is the same accident). Put options first.
# - Refuses a pkill that would signal more than AMUX_PKILL_MAX processes
#   (default 20), counted with the real pgrep on the same arguments.
#   AMUX_PKILL_ALLOW_MANY=1 on the command allows a deliberate wide kill.
# Everything else goes straight to the real binary.
set -u
shimdir=$(cd "$(dirname "$0")" && pwd -P)
me=$(basename "$0")

real=""
IFS=: read -r -a dirs <<< "${PATH:-}"
for d in "${dirs[@]}"; do
  [ -n "$d" ] || continue
  dd=$(cd "$d" 2>/dev/null && pwd -P) || continue
  [ "$dd" = "$shimdir" ] && continue
  if [ -x "$d/$me" ]; then real="$d/$me"; break; fi
done
if [ -z "$real" ]; then
  echo "amux $me guard: no real $me found on PATH" >&2
  exit 127
fi

log() { # <verdict> <detail>
  local f="${AMUX_HOME:-$HOME/.amux}/logs/pkill-guard.log"
  mkdir -p "$(dirname "$f")" 2>/dev/null
  printf '%s session=%s verdict=%s %s\n' "$(date '+%F %T')" "${AMUX_SESSION:-unknown}" "$1" "$2" >> "$f" 2>/dev/null
  if [ -n "${AMUX_URL:-}" ]; then
    curl -sk --max-time 2 -X POST -H 'Content-Type: application/json' \
      -d "{\"kind\":\"pkill-guard\",\"verdict\":\"$1\",\"measured\":true,\"n_considered\":1,\"session\":\"${AMUX_SESSION:-unknown}\",\"tool\":\"$me\"}" \
      "$AMUX_URL/api/client-debug" >/dev/null 2>&1 &
  fi
}

# BSD pkill/pgrep options that take a value. pkill also takes a signal as
# -SIGNAL (-9, -TERM, -KILL), which is a flag-shaped first argument.
first="" i=1 n=$#
while [ "$i" -le "$n" ]; do
  a=${!i}
  if [ -n "$first" ]; then
    case "$a" in
      -*) log option_after_pattern "cmd=$(printf '%q ' "$me" "$@")"
          cat >&2 <<MSG
amux: refused \`$me $*\`.
On macOS, $me options must come BEFORE the pattern. "$a" comes after the
pattern "$first", so BSD $me reads it, and anything after it, as MORE
patterns to match. "-P 1" that way matches every process with a 1 in its
command line: on 2026-10-21 that killed every worker on this machine.

Put the options first:
  $me ${a} ... -f "$first"
MSG
          exit 64 ;;
    esac
  else
    case "$a" in
      --) i=$((i + 1)); first=${!i:-}; [ -n "$first" ] || break ;;
      -F|-G|-g|-P|-t|-U|-u|-s|-J|-c) i=$((i + 1)) ;;
      -*) ;;
      *) first=$a ;;
    esac
  fi
  i=$((i + 1))
done

if [ "$me" = pkill ] && [ "${AMUX_PKILL_ALLOW_MANY:-0}" != 1 ]; then
  max=${AMUX_PKILL_MAX:-20}
  # Same arguments to the real pgrep, minus pkill's own: the signal, which
  # BSD only accepts as the FIRST argument (-9, -TERM, -SIGTERM), and -I
  # (interactive), which pgrep rejects. Other uppercase options (-U, -P, -G)
  # take values and must stay.
  args=() k=0
  for a in "$@"; do
    k=$((k + 1))
    if [ "$k" = 1 ]; then
      case "$a" in -[0-9]*|-SIG*|-[A-Z][A-Z]*) continue ;; esac
    fi
    [ "$a" = -I ] && continue
    args+=("$a")
  done
  pgrep_real=""
  for d in "${dirs[@]}"; do
    [ -n "$d" ] || continue
    dd=$(cd "$d" 2>/dev/null && pwd -P) || continue
    [ "$dd" = "$shimdir" ] && continue
    if [ -x "$d/pgrep" ]; then pgrep_real="$d/pgrep"; break; fi
  done
  if [ -n "$pgrep_real" ] && [ "${#args[@]}" -gt 0 ]; then
    count=$("$pgrep_real" "${args[@]}" 2>/dev/null | wc -l | tr -d ' ')
    if [ "${count:-0}" -gt "$max" ]; then
      log pkill_too_broad "matched=$count max=$max cmd=$(printf '%q ' "$me" "$@")"
      cat >&2 <<MSG
amux: refused \`pkill $*\`: it matches $count processes (the ceiling is $max).
A pattern that wide almost always matches far more than you meant; on
2026-10-21 one took down every worker on this machine. See what it matches:
  pgrep -l ${args[*]}
Narrow it (options first, an exact pattern), or kill the pids you started.
If you really mean it: AMUX_PKILL_ALLOW_MANY=1 pkill ...
MSG
      exit 64
    fi
  fi
fi
exec "$real" "$@"
