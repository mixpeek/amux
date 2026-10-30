#!/bin/bash
# amux tmux-fleet shim. Installed by the amux server as `tmux` in
# ~/.amux/shims/tmux-fleet/ and put first on every tmux worker's PATH, so it
# covers commands a worker types and the scripts those commands run.
#
# Why: on 2026-10-17 a worker's demo reset script ran
# `TMUX_TMPDIR="$T" tmux kill-server`, meant for the demo's private server.
# Inside a pane tmux takes the server from $TMUX, which beats TMUX_TMPDIR, so it
# killed the fleet's server and every lane with it. It happened at 14:31 and
# again at 14:51. A guard on the typed command could not see it: the worker
# typed `reset.sh`.
#
# What it does:
# - A caller that sets TMUX_TMPDIR to somewhere other than the fleet's gets the
#   server it asked for: $TMUX is dropped for that call.
# - `kill-server`, and `kill-session -a`, aimed at the fleet's server are
#   refused and logged. Name a private server with -L or -S instead.
# Everything else goes straight to the real tmux.
set -u
shimdir=$(cd "$(dirname "$0")" && pwd -P)

# ANOTHER COPY OF AN AMUX SHIM IS NOT THE REAL BINARY (cpu RCA 20261030-065821).
# Skipping only this directory let two copies on one PATH (~/.amux/shims and a
# demo home's shims) each take the other for the real binary and exec it back
# and forth: one `pgrep` ran that way for 7h44m and the Mac allocated ~875
# pids a second. A candidate that is a script mentioning amux is a shim copy,
# whatever its age; real binaries are not scripts.
_amux_shim_copy() { # <path>
  local first=""
  IFS= read -r first < "$1" 2>/dev/null || return 1
  case "$first" in '#!'*) grep -qs 'amux' "$1" ;; *) return 1 ;; esac
}
# The real binary is the first tmux on PATH that is not in this directory.
real=""
IFS=: read -r -a dirs <<< "${PATH:-}"
for d in "${dirs[@]}"; do
  [ -n "$d" ] || continue
  dd=$(cd "$d" 2>/dev/null && pwd -P) || continue
  [ "$dd" = "$shimdir" ] && continue
  if [ -x "$d/tmux" ] && ! _amux_shim_copy "$d/tmux"; then real="$d/tmux"; break; fi
done
if [ -z "$real" ]; then
  echo "amux tmux-fleet shim: no real tmux found on PATH" >&2
  exit 127
fi

uid=$(id -u)
# /tmp and /private/tmp are one directory on macOS; compare resolved paths.
canon() {
  local d
  d=$(cd "$(dirname "$1")" 2>/dev/null && pwd -P) || { printf '%s' "$1"; return; }
  printf '%s/%s' "$d" "$(basename "$1")"
}

fleet=${AMUX_FLEET_TMUX_SOCKET:-}
[ -n "$fleet" ] || fleet=${TMUX:-}
fleet=${fleet%%,*}
[ -n "$fleet" ] && fleet=$(canon "$fleet")

# tmux's own options come before the command: -L name, -S path, and -c/-f/-T
# take a value; the rest are flags.
sock="" label="" i=1
while [ "$i" -le "$#" ]; do
  a=${!i}
  case "$a" in
    -S) i=$((i + 1)); sock=${!i:-} ;;
    -S*) sock=${a#-S} ;;
    -L) i=$((i + 1)); label=${!i:-} ;;
    -L*) label=${a#-L} ;;
    -c|-f|-T) i=$((i + 1)) ;;
    --) break ;;
    -*) ;;
    *) break ;;
  esac
  i=$((i + 1))
done

log() { # <verdict> <detail>
  local f="${AMUX_HOME:-$HOME/.amux}/logs/tmux-fleet-shim.log"
  mkdir -p "$(dirname "$f")" 2>/dev/null
  printf '%s session=%s verdict=%s %s\n' "$(date '+%F %T')" "${AMUX_SESSION:-unknown}" "$1" "$2" >> "$f" 2>/dev/null
}

# A caller-chosen TMUX_TMPDIR means a different server. Honour it.
if [ -z "$sock" ] && [ -z "$label" ] && [ -n "${TMUX:-}" ] && [ -n "${TMUX_TMPDIR:-}" ] && [ -n "$fleet" ] \
   && [ "$(canon "$TMUX_TMPDIR/tmux-$uid/default")" != "$fleet" ]; then
  unset TMUX
  log tmux_tmpdir_honoured "tmux_tmpdir=$TMUX_TMPDIR"
fi

if [ -n "$sock" ]; then target=$sock
elif [ -n "$label" ]; then target="${TMUX_TMPDIR:-/tmp}/tmux-$uid/$label"
elif [ -n "${TMUX:-}" ]; then target=${TMUX%%,*}
else target="${TMUX_TMPDIR:-/tmp}/tmux-$uid/default"
fi
target=$(canon "$target")

# tmux accepts a unique prefix of a command name, and `;` chains commands, so
# look at every argument rather than only the first command.
kills=""
for a in "$@"; do
  case "$a" in
    kill-ser*) kills="kill-server" ;;
    kill-ses*) [ -n "$kills" ] || kills="kill-session" ;;
  esac
done
if [ "$kills" = "kill-session" ]; then
  all=0
  for a in "$@"; do case "$a" in -a|-a*) all=1 ;; esac; done
  [ "$all" = 1 ] && kills="kill-session -a" || kills=""
fi

if [ -n "$kills" ] && [ -n "$fleet" ] && [ "$target" = "$fleet" ]; then
  log fleet_kill_refused "cmd=$(printf '%q ' "$@")"
  if [ -n "${AMUX_URL:-}" ]; then
    curl -sk --max-time 2 -X POST -H 'Content-Type: application/json' \
      -d "{\"kind\":\"tmux-fleet-shim\",\"verdict\":\"fleet_kill_refused\",\"measured\":true,\"n_considered\":1,\"session\":\"${AMUX_SESSION:-unknown}\",\"cmd\":\"$kills\"}" \
      "$AMUX_URL/api/client-debug" >/dev/null 2>&1 &
  fi
  cat >&2 <<MSG
amux: refused \`tmux $kills\`. It targets the amux fleet's tmux server
($fleet), which runs every worker. Running it would stop all of them.

Name your own server instead:
  tmux -L <name> kill-server          # a private socket by name
  tmux -S /path/to/socket kill-server # a private socket by path
TMUX_TMPDIR=<dir> tmux ... also works from a worker: this shim drops \$TMUX
for that call so your directory wins.
MSG
  exit 64
fi
exec "$real" "$@"
