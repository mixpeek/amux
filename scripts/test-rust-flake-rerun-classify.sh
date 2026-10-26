#!/usr/bin/env bash
# Cells for scripts/rust-flake-rerun-classify.sh. Logs come from fixtures.
set -euo pipefail
cd "$(dirname "$0")/.."
FIX=$(mktemp -d); trap 'rm -rf "$FIX"' EXIT
printf 'error writing file `target/debug/deps/x` to disk\nCaused by:\n  failed to unpack `target/debug/deps/x` into `/w/target/debug/deps/x`\nCaused by:\n  incomplete frame\n' > "$FIX/101"
printf 'thread main panicked at tests/x.rs:4\ntest result: FAILED. 1 failed\n' > "$FIX/102"
printf 'error[E0425]: cannot find value `x`\n' > "$FIX/103"
export FLAKE_LOG_CMD="cat '$FIX'/\$1"
fails=0
cell() { # label, expected exit, expected verdict fragment, input
  local out code
  code=0; out=$(printf '%b' "$4" | scripts/rust-flake-rerun-classify.sh) || code=$?
  if [ "$code" = "$2" ] && printf '%s' "$out" | grep -q -- "$3"; then echo "  ok   $1"
  else echo "  FAIL $1 (exit $code: $out)"; fails=$((fails + 1)); fi
}
cell "e2e only is re-run" 0 "flake_rerun_allowed" '1\te2e-shards (16)\n2\te2e\n'
cell "a truncated test archive plus its aggregate is re-run" 0 "archive_unpack" '101\ttest-shards (3)\n9\ttest\n'
cell "a failing unit test is not re-run" 1 "jobs=test-shards (4);" '102\ttest-shards (4)\n9\ttest\n'
cell "a compile error is not re-run" 1 "non_flake_failure" '103\tclippy\n'
cell "an archive flake beside a real failure is not re-run" 1 "non_flake_failure" '101\ttest-shards (3)\n102\ttest-shards (4)\n9\ttest\n'
cell "the aggregate alone is not re-run" 1 "aggregate_only" '9\ttest\n'
cell "no failed jobs is not re-run" 1 "no_failed_jobs" ''
[ "$fails" -eq 0 ] && echo "PASS: rust-flake-rerun-classify" || { echo "FAIL: $fails cell(s)"; exit 1; }
