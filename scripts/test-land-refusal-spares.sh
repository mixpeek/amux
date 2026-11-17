#!/usr/bin/env bash
# test-land-refusal-spares.sh [amux]: an anonymous batch refusal counts against
# a waiter only when it names a file that waiter's commits touched, or names no
# file. gates' 13-lane batch at 01:08Z on 2026-10-03 was refused by a defect
# already on origin (server/scripts/ops/golden_known_item.py, named on an
# indented line after a blank one) and 12 innocent tickets went solo.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
F="$(mktemp "${TMPDIR:-/tmp}/rs.XXXXXX")"
{ sed -n '/^_grep_all() {/,/^}/p' "$AM"; sed -n '/^_land_refusal_spares() {/,/^}/p' "$AM"; } > "$F"
# shellcheck disable=SC1090
. "$F"; rm -f "$F"
T="$(mktemp -d "${TMPDIR:-/tmp}/rs.XXXXXX")"; cd "$T" || exit 1
git init -q; git config user.email t@t; git config user.name t
mkdir -p server/scripts/ops server/scripts/ci server/api; echo a > server/api/x.py; echo b > server/scripts/ops/golden_known_item.py; echo e > server/scripts/ci/check_gcloudignore_allowlist.py
git add -A; git commit -qm base; base="$(git rev-parse HEAD)"
echo c >> server/api/x.py; git commit -qm innocent -- server/api/x.py; innocent="$(git rev-parse HEAD)"
git checkout -q "$base"; echo d >> server/scripts/ops/golden_known_item.py; git commit -qm culprit -- server/scripts/ops/golden_known_item.py; culprit="$(git rev-parse HEAD)"
cat > "$T/hook.log" <<'EOF'
pre-push: gating candidate commit abc on https://github.com/mixpeek/mixpeek.git
✗ 1 of 54 spec schemas are defined ONLY in files the `generate-openapi` hook does not watch.
  Editing them changes openapi.json and fires no hook.

       1 schema(s)  server/scripts/ops/golden_known_item.py

  Current pattern: ^server/(api|shared)/.*\.py$
EOF
printf 'qdrant-refs: ✗ REFUSED\n' > "$T/nofile.log"
# The pre-commit shape (02:07Z 2026-10-03): the check named only by its path
# inside the hook's temp tree, under a "....Failed" line.
cat > "$T/precommit.log" <<EOF2
trim trailing whitespace.................................Passed
Guard against missing .gcloudignore-ci allowlist entries.....Failed
- hook id: check-gcloudignore-allowlist
- exit code: 1
Traceback (most recent call last):
  File "$T/zb-pp-tree.Rg/server/scripts/ci/check_gcloudignore_allowlist.py", line 54, in <module>
ModuleNotFoundError: No module named 'scripts.lib'
Check shell script syntax (bash -n)......................Passed
precommit-prepush-stage: ✗ a pre-push-stage hook FAILED on the pushed files (above).
EOF2
git checkout -q "$base"; echo f >> server/scripts/ci/check_gcloudignore_allowlist.py; git commit -qm checktoucher -- server/scripts/ci/check_gcloudignore_allowlist.py; checktoucher="$(git rev-parse HEAD)"
res() { echo "excluded: the batch was refused by the push hook (full output $1); landing on your own turn"; }
fail=0
_land_refusal_spares "$(res "$T/hook.log")" "$innocent" "$base" && echo "ok   a waiter that did not touch the named file is spared" || { echo "FAIL innocent waiter counted"; fail=1; }
_land_refusal_spares "$(res "$T/hook.log")" "$culprit" "$base" && { echo "FAIL the waiter that touched the named file was spared"; fail=1; } || echo "ok   the waiter that touched the named file is counted"
_land_refusal_spares "$(res "$T/nofile.log")" "$innocent" "$base" && { echo "FAIL a refusal naming no file spared a waiter"; fail=1; } || echo "ok   a refusal naming no file still counts"
_land_refusal_spares "excluded: no output path here" "$innocent" "$base" && { echo "FAIL an unreadable refusal spared a waiter"; fail=1; } || echo "ok   no readable output still counts"
_land_refusal_spares "$(res "$T/precommit.log")" "$innocent" "$base" && echo "ok   pre-commit refusal: a waiter that did not touch the failing check is spared" || { echo "FAIL pre-commit refusal counted against an innocent waiter"; fail=1; }
_land_refusal_spares "$(res "$T/precommit.log")" "$checktoucher" "$base" && { echo "FAIL the waiter that changed the failing check was spared"; fail=1; } || echo "ok   pre-commit refusal: the waiter that changed the failing check is counted"
cd / && rm -rf -- "${T:?}"
exit $fail
