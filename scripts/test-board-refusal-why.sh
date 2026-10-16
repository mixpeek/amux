#!/usr/bin/env bash
# test-board-refusal-why.sh: a refused board move prints the server's `why`
# (how to satisfy the gate), and the one-line verdict stays LAST for `| tail -1`.
# 2026-10-16: the evidence gate printed only "requires evidence of what was run",
# and 70 closes in one triage were refused for want of backticks the server's
# `why` named. Runs the shipped function, extracted from ./amux.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
fn=$(awk '/^_board_move_finish\(\) \{/{p=1} p{print} p&&/^\}/{exit}' "$HERE/amux")
[ -n "$fn" ] || { echo "FAIL: _board_move_finish not found in ./amux"; exit 1; }
eval "$fn"
resp='{"error":"done requires evidence of what was run","code":"done_evidence_has_no_artifact","why":"Name the artifact: a command in backticks, a repo path, a URL, a commit sha, or a #PR."}'
err=$( { _board_move_finish 1 "$resp" AMUX-1 done || true; } 2>&1 >/dev/null )
check "the server's why is printed"     yes "$(printf '%s' "$err" | grep -qF 'why: Name the artifact: a command in backticks' && echo yes || echo no)"
check "the verdict is the last line"    yes "$(printf '%s\n' "$err" | tail -1 | grep -qF 'AMUX-1 was NOT moved to done: done requires evidence of what was run [done_evidence_has_no_artifact]' && echo yes || echo no)"
err=$( { _board_move_finish 1 '{"error":"holding limit","code":"wip"}' AMUX-2 todo || true; } 2>&1 >/dev/null )
check "no why line when the server gave none" 1 "$(printf '%s\n' "$err" | grep -c .)"
if [ "$fails" = 0 ]; then echo "PASS: board-refusal-why — all checks passed"; else echo "board-refusal-why: $fails check(s) FAILED"; exit 1; fi
