#!/bin/bash
# What a push must pass before it deploys, inside AMUX_FAST_GATE_S (60).
# Syntax of what changed, then the shell tests that name a changed script.
# A test that outruns the budget is named and does not fail the gate:
# checks.yml runs it in full.
set -uo pipefail
before=${1:-}; sha=${2:-HEAD}
budget=${AMUX_FAST_GATE_S:-60}
git cat-file -e "$before^{commit}" 2>/dev/null || before="$sha~1"
if ! touched=$(git diff --name-only --diff-filter=d "$before" "$sha" 2>/dev/null); then
  echo "fast gate: no diff for $before..$sha (verdict=fast_gate_unmeasured)"
  exit 0
fi
n=$(printf '%s\n' "$touched" | grep -c .)
fail=0
while IFS= read -r f; do
  [ -n "$f" ] || continue
  case "$f" in
    *.mjs|*.js) node --check "$f" || { echo "SYNTAX $f"; fail=1; } ;;
    *.sh|amux) bash -n "$f" || { echo "SYNTAX $f"; fail=1; } ;;
    *.py) python3 -c 'import ast,sys; ast.parse(open(sys.argv[1]).read())' "$f" || { echo "SYNTAX $f"; fail=1; } ;;
    *.json) python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$f" || { echo "SYNTAX $f"; fail=1; } ;;
  esac
done <<< "$touched"
tests=""
for t in scripts/test-*.sh; do
  grep -q "$t" .github/workflows/checks.yml || continue
  for f in $touched; do
    case "$f" in scripts/*) ;; *) continue ;; esac
    if [ "$f" = "$t" ] || grep -q "$(basename "$f")" "$t"; then tests+="$t"$'\n'; break; fi
  done
done
ran=0 timed=0
out=$(mktemp -d)
if [ -n "$tests" ]; then
  # One job per CPU, each given the whole budget.
  # Each test is the last argument ($3), not an -I{} substitution: BSD xargs
  # (macOS, where the builder runs this gate) refuses an -I command over 255
  # bytes and ran nothing.
  printf '%s' "$tests" | xargs -P "$(getconf _NPROCESSORS_ONLN)" -n 1 bash -c '
    t=$3; log="$1/$(basename "$t").log"
    # No GNU timeout on macOS (the builder runs this gate there): Perl alarm,
    # which ends the test with SIGALRM (142) where timeout says 124.
    if command -v timeout >/dev/null 2>&1; then timeout "$2" "$t"; else perl -e "alarm shift; exec @ARGV or exit 127" "$2" "$t"; fi \
      > "$log" 2>&1; echo $? > "$log.rc"' _ "$out" "$budget"
  for rc in "$out"/*.rc; do
    ran=$((ran+1)); log=${rc%.rc}
    case "$(cat "$rc")" in
      0) ;;
      124|142) timed=$((timed+1)); echo "over ${budget}s: $(basename "$log" .log) (checks.yml runs it)" ;;
      *) fail=1; echo "FAILED $(basename "$log" .log)"; tail -15 "$log" ;;
    esac
  done
fi
rm -rf -- "${out:?}"
echo "fast gate: $n file(s) checked, $ran script test(s) selected, $timed over budget, failed=$fail (verdict=fast_gate_done)"
exit "$fail"
