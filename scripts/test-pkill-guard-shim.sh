#!/usr/bin/env bash
# Tests for scripts/shims/pkill-guard-shim.sh. The "real" pkill and pgrep here
# are fakes on PATH after the shim: pkill records its arguments, pgrep prints
# FAKE_N lines. Nothing on this machine is ever signalled.
set -euo pipefail
cd "$(dirname "$0")/.."
SHIM=scripts/shims/pkill-guard-shim.sh
T=$(mktemp -d "${TMPDIR:-/tmp}/test-pkill-guard.XXXXXX")
trap 'rm -rf -- "${T:?}"' EXIT
pass=0; fail=0
ok()  { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; }

mkdir -p "$T/shims" "$T/real" "$T/home"
cp "$SHIM" "$T/shims/pkill"; cp "$SHIM" "$T/shims/pgrep"; chmod +x "$T/shims/pkill" "$T/shims/pgrep"
cat > "$T/real/pkill" <<'SH'
#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_LOG"
SH
cat > "$T/real/pgrep" <<'SH'
#!/bin/sh
printf 'pgrep %s\n' "$*" >> "$FAKE_LOG"
i=0; while [ "$i" -lt "${FAKE_N:-0}" ]; do echo $((1000 + i)); i=$((i + 1)); done
SH
chmod +x "$T/real/pkill" "$T/real/pgrep"

run() { # <n matches> <tool> <args...>: rc in $T/rc, stderr in $T/err
  local n=$1; shift
  : > "$T/calls"
  rc=0
  env -u AMUX_URL PATH="$T/shims:$T/real:/usr/bin:/bin" AMUX_HOME="$T/home" FAKE_LOG="$T/calls" FAKE_N="$n" \
    AMUX_SESSION=test "$@" > "$T/out" 2> "$T/err" || rc=$?
  echo "$rc" > "$T/rc"
}
killed() { grep -v '^pgrep ' "$T/calls" || true; }

# The 2026-10-21 command: options after the pattern.
run 226 pkill -f "explore.js" -P 1
[ "$(cat "$T/rc")" = 64 ] && [ -z "$(killed)" ] && ok "pkill -f pat -P 1 is refused and kills nothing" || bad "misordered pkill refused"
grep -q 'options must come BEFORE the pattern' "$T/err" && ok "the refusal explains BSD option order" || bad "refusal text"
grep -q 'verdict=option_after_pattern' "$T/home/logs/pkill-guard.log" && ok "the refusal is logged with its verdict" || bad "refusal logged"

run 3 pgrep -f "run_ux.sh" -P 1
[ "$(cat "$T/rc")" = 64 ] && ok "a misordered pgrep is refused too (it feeds kill)" || bad "misordered pgrep"

# Correct order passes straight through.
run 2 pkill -P 1 -f "explore.js"
[ "$(cat "$T/rc")" = 0 ] && [ "$(killed)" = "-P 1 -f explore.js" ] && ok "options-first pkill reaches the real pkill unchanged" || bad "ordered pkill passes: $(killed)"
run 2 pkill -TERM -U 501 -f "explore.js"
[ "$(cat "$T/rc")" = 0 ] && grep -q '^pgrep -U 501 -f explore.js$' "$T/calls" && ok "a leading signal is dropped for the count, -U 501 kept" || bad "signal strip: $(cat "$T/calls")"
run 2 pkill -9 explore
[ "$(cat "$T/rc")" = 0 ] && [ "$(killed)" = "-9 explore" ] && ok "pkill -9 pattern passes" || bad "pkill -9 passes"
run 0 pgrep -lf "explore.js"
[ "$(cat "$T/rc")" = 0 ] && ok "a plain pgrep passes" || bad "plain pgrep"

# Too broad: refused unless asked for.
run 21 pkill -f "node"
[ "$(cat "$T/rc")" = 64 ] && [ -z "$(killed)" ] && grep -q 'matches 21 processes' "$T/err" && ok "a pkill matching 21 (> 20) is refused with the count" || bad "too-broad pkill"
grep -q 'verdict=pkill_too_broad' "$T/home/logs/pkill-guard.log" && ok "too-broad refusal is logged" || bad "too-broad logged"
run 20 pkill -f "node"
[ "$(cat "$T/rc")" = 0 ] && ok "exactly the ceiling (20) passes" || bad "ceiling passes"
run 50 env AMUX_PKILL_ALLOW_MANY=1 pkill -f "node"
[ "$(cat "$T/rc")" = 0 ] && [ "$(killed)" = "-f node" ] && ok "AMUX_PKILL_ALLOW_MANY=1 allows a deliberate wide kill" || bad "allow-many"
run 30 env AMUX_PKILL_MAX=40 pkill -f "node"
[ "$(cat "$T/rc")" = 0 ] && ok "AMUX_PKILL_MAX raises the ceiling" || bad "max knob"

echo "pkill-guard-shim: $pass passed, $fail failed"
[ "$fail" = 0 ]
