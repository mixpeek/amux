#!/usr/bin/env bash
set -euo pipefail
: "${AMUX_LIFECYCLE_BINARY:?Run python3 scripts/lifecycle/run.py browser first}"
test -x "$AMUX_LIFECYCLE_BINARY"
for key in ${!AMUX_@}; do
  case "$key" in
    AMUX_HOME|AMUX_RS_PORT|AMUX_RS_NO_LOOPBACK_BYPASS|AMUX_LIFECYCLE_BINARY|AMUX_LIFECYCLE_ASSET_MANIFEST|AMUX_LIFECYCLE_OUTPUT) ;;
    *) unset "$key" ;;
  esac
done
unset TMUX TMUX_PANE
export AMUX_ISOLATED=1 AMUX_NO_SELF_ADOPT=1
export AMUX_FILES_ROOT="$AMUX_HOME/workspace"
mkdir -p "$AMUX_FILES_ROOT"
if [[ -n "${AMUX_LIFECYCLE_ASSET_MANIFEST:-}" ]]; then
  "$AMUX_LIFECYCLE_BINARY" &
  lifecycle_server_pid=$!
  trap 'kill "$lifecycle_server_pid" 2>/dev/null || true' EXIT INT TERM
  python3 "$(dirname "$0")/../../scripts/lifecycle/check_assets.py"
  wait "$lifecycle_server_pid"
else
  exec "$AMUX_LIFECYCLE_BINARY"
fi
