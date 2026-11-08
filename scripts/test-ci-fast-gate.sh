#!/bin/bash
# The deploy gate must fail on what it claims to check (ethos rule 7).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
FIX=$(mktemp -d); trap 'rm -rf -- "${FIX:?}"' EXIT
cd "$FIX" || exit 1
git init -q .; git config user.email t@t; git config user.name t
mkdir -p scripts .github/workflows
cat > scripts/test-widget.sh <<'T'
#!/bin/bash
# exercises widget.sh
[ ! -e fail-me ]
T
chmod +x scripts/test-widget.sh
echo 'run: scripts/test-widget.sh' > .github/workflows/checks.yml
echo 'true' > scripts/widget.sh; echo 'ok' > a.txt
step() { git add .; git commit -qm "$1"; }
step base
fails=0
run() { # <label> <want rc> <expected output>
  rc=0; out=$(bash "$HERE/ci-fast-gate.sh" HEAD~1 HEAD 2>&1) || rc=$?
  if [ "$rc" = "$2" ] && printf '%s' "$out" | grep -qF -- "$3"; then echo "  ok   $1"; else echo "  FAIL $1 (rc=$rc): $out"; fails=$((fails+1)); fi
}
echo ok2 > a.txt; step plain
run "an unrelated change selects no test" 0 '0 script test(s) selected'
echo 'if' > scripts/widget.sh; step badsh
run "bad shell fails" 1 'SYNTAX scripts/widget.sh'
echo 'true' > scripts/widget.sh; step good
run "a changed script runs the test naming it" 0 '1 script test(s) selected'
touch fail-me; echo '# x' >> scripts/widget.sh; step red
run "a red selected test fails" 1 'FAILED test-widget.sh'
rm fail-me; echo '{' > b.json; step badjson
run "bad JSON fails" 1 'SYNTAX b.json'
rm b.json; printf '#!/bin/bash\nsleep 30\n' > scripts/test-widget.sh; step slow
AMUX_FAST_GATE_S=1 run "a test over budget is named, not failed" 0 'over 1s: test-widget.sh'
[ "$fails" = 0 ] && { echo "PASS: ci-fast-gate"; exit 0; }
echo "ci-fast-gate: $fails FAILED"; exit 1
