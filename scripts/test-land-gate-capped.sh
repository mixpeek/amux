#!/bin/bash
# The land gate runs capped and low priority, and says so (Mac cpu RCA
# 20261101-184920). Runs the function the CLI ships, not a paraphrase.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
FIX=$(mktemp -d); trap 'rm -rf -- "${FIX:?}"' EXIT
sed -n '/^land_gate_run() {$/,/^}$/p' "$HERE/amux" > "$FIX/fn.sh"
grep -q 'land_gate_run python3 "$_lg_dir/land_gate.py"' "$HERE/amux"
# shellcheck disable=SC1091
. "$FIX/fn.sh"
out=$(land_gate_run bash -c 'echo "$CARGO_BUILD_JOBS $NEXTEST_TEST_THREADS $(ps -o ni= -p $$ | tr -d " ")"' 2>"$FIX/err")
base=$(ps -o ni= -p $$ | tr -d ' ')
[ "$out" = "12 8 $((base + 10))" ] || { echo "FAIL defaults: $out"; exit 1; }
grep -q 'verdict=land_gate_capped' "$FIX/err" || { echo "FAIL no verdict"; exit 1; }
out=$(AMUX_LAND_BUILD_JOBS=4 AMUX_LAND_TEST_THREADS=2 land_gate_run bash -c 'echo "$CARGO_BUILD_JOBS $NEXTEST_TEST_THREADS"' 2>/dev/null)
[ "$out" = "4 2" ] || { echo "FAIL override: $out"; exit 1; }
echo "PASS: land-gate-capped"
