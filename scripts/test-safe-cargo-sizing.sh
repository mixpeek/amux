#!/usr/bin/env bash
# test-safe-cargo-sizing.sh: safe-cargo.sh sizes each build's memory cap to the
# host (70% of RAM, never above max(12 GB, RAM/6)), and lint-blame names a
# budget stop instead of calling it an unattributable lint failure
# (2026-11-17: a 12.9 GB workspace check on a 96 GB Mac was killed at 12 GB).
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
fail=0; ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; fail=1; }
cap() { AMUX_CARGO_HOST_MB="$1" bash "$ROOT/scripts/safe-cargo.sh" --version 2>&1 >/dev/null | sed -n 's/.*each capped at \([0-9]*\) MB.*/\1/p' | head -1; }
[ "$(cap 8192)" = 5734 ] && ok "an 8 GB host: 70% of RAM" || bad "8 GB host capped at $(cap 8192)"
[ "$(cap 16384)" = 11468 ] && ok "a 16 GB host: 70%, under 12 GB" || bad "16 GB host capped at $(cap 16384)"
[ "$(cap 32768)" = 12288 ] && ok "a 32 GB host: still the 12 GB floor of the ceiling" || bad "32 GB host capped at $(cap 32768)"
[ "$(cap 98304)" = 16384 ] && ok "a 96 GB host: a sixth of RAM, above the 12.9 GB check that was killed" || bad "96 GB host capped at $(cap 98304)"
out="$(CLIPPY_OUT='{"event": "cargo_budget_stopped", "measured": true, "reason": "memory", "peak_rss_bytes": 12927959040}' AMUX_CARGO_MAX_RSS_MB=12288 python3 "$ROOT/scripts/git-hooks/lint-blame.py")"
printf '%s' "$out" | grep -q "no lint failed. The cargo budget stopped the build (reason=memory, peak 12329 MB, cap 12288 MB)" && ok "lint-blame names a memory stop with its numbers" || bad "lint-blame: $out"
out="$(CLIPPY_OUT='error: something with no span' python3 "$ROOT/scripts/git-hooks/lint-blame.py")"
printf '%s' "$out" | grep -q "could not attribute this failure" && ok "an unattributable lint failure still says so" || bad "plain: $out"
[ "$fail" = 0 ] && echo "test-safe-cargo-sizing: all checks passed" || { echo "test-safe-cargo-sizing: FAILED"; exit 1; }
