#!/usr/bin/env bash
set -euo pipefail
: "${AMUX_LIFECYCLE_BINARY:?Run python3 scripts/lifecycle/run.py browser first}"
test -x "$AMUX_LIFECYCLE_BINARY"
for key in ${!AMUX_@}; do
  case "$key" in
    AMUX_HOME|AMUX_RS_PORT|AMUX_RS_NO_LOOPBACK_BYPASS|AMUX_LIFECYCLE_BINARY) ;;
    *) unset "$key" ;;
  esac
done
unset TMUX TMUX_PANE
export AMUX_ISOLATED=1 AMUX_NO_SELF_ADOPT=1
export AMUX_FILES_ROOT="$AMUX_HOME/workspace"
mkdir -p "$AMUX_FILES_ROOT"
exec "$AMUX_LIFECYCLE_BINARY"
