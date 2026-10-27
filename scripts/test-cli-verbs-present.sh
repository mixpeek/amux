#!/usr/bin/env bash
# Verbs the docs and access ladder name must exist in the shipped CLI.
#
# 2026-09-28: 2da26297 (amux order --url) rewrote `amux` from a stale copy and
# deleted `amux computer` (rung 3 of the ~/.claude/CLAUDE.md access ladder)
# and `amux browser profiles` (AMUX-5307). Nothing failed. Lanes found it four
# days later as "unknown command" and filed it as a failed rung (gs12-spend,
# mixpeek-override, 2026-10-02). A whole-file write is silent, so pin the
# verbs here: each needs its dispatch entry, its function, and a usage that runs.
set -euo pipefail
cd "$(dirname "$0")/.." || exit 1
FAILED=0; CELLS=0
cell() { # <label> <command...>
  local label="$1"; shift
  CELLS=$((CELLS + 1))
  if "$@" >/dev/null 2>&1; then echo "  ok    $label"; else echo "  FAIL  $label"; FAILED=$((FAILED + 1)); fi
}
cell "dispatch: computer|cu"            grep -q '^    computer|cu)' amux
cell "function: cmd_computer"           grep -q '^cmd_computer() {' amux
cell "help computer routes to the verb" grep -q 'cmd_computer help' amux
cell "amux help computer prints usage"  bash -c 'bash amux help computer 2>&1 | grep -q "amux computer start"'
cell "browser: profiles subcommand"     grep -q '^    profiles|ls|list)' amux
cell "main help names amux computer"    grep -q 'amux computer start|status' amux
cell "dispatch: order"                  grep -q '^    order)' amux
cell "dispatch: cdp (access rung 2)"    grep -q '^    cdp)' amux
cell "dispatch: land"                   grep -q '^    land)' amux
cell "dispatch: watch"                  grep -q '^    watch)' amux
cell "amux watch --help prints usage"   bash -c 'AMUX_REPO_DIR="$PWD" bash amux watch --help 2>&1 | grep -q "add"'
cell "land --help names --adopt"        bash -c 'bash amux land --help 2>&1 | grep -q -- "--adopt"'
echo
if [ "$FAILED" -eq 0 ]; then echo "PASS ($CELLS outcome cells)"; exit 0; fi
echo "FAIL ($FAILED of $CELLS outcome cells)"; exit 1
