#!/usr/bin/env bash
# AEAB-52 — the e2e harness must actually SET the seam it depends on.
#
# The Rust side is unit-tested in lib.rs (self_adopt_enabled, both directions).
# This pins the other half, which is the half that silently rots: a seam nobody
# sets is decoration, and the ethos file's first rule is about exactly that —
# mcp.json shipped six servers that reached 0 of 101 sessions because the flag
# enabling them was never passed.
#
# Two properties, and the second is the one a careless edit breaks:
#   1. serve-head.sh exports AMUX_NO_SELF_ADOPT=1
#   2. it does so BEFORE it exec's cargo — an export after the exec line is
#      unreachable, and would look completely correct in a diff.
#
# Exit 0 = all pass, 1 = a failure. Wired into .github/workflows/checks.yml.
set -uo pipefail
cd "$(dirname "$0")/.."
SERVE=e2e/serve-head.sh
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   — $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL — $1"; }

if [ ! -f "$SERVE" ]; then
  echo "  FAIL — $SERVE not found"; exit 1
fi

# 1. the export exists at all
if grep -qE '^[[:space:]]*export[[:space:]]+AMUX_NO_SELF_ADOPT=1[[:space:]]*$' "$SERVE"; then
  ok "serve-head.sh exports AMUX_NO_SELF_ADOPT=1"
else
  bad "serve-head.sh does NOT export AMUX_NO_SELF_ADOPT=1 — the e2e servers will hot-swap mid-suite"
fi

# 2. it is reachable. `exec` replaces the process, so anything after the FIRST
#    unconditional exec never runs. Compare line numbers rather than trusting
#    that the file reads top-to-bottom the way it looks.
exp_line=$(grep -nE '^[[:space:]]*export[[:space:]]+AMUX_NO_SELF_ADOPT=1' "$SERVE" | head -1 | cut -d: -f1)
exec_line=$(grep -nE '^[[:space:]]*exec[[:space:]]+cargo' "$SERVE" | tail -1 | cut -d: -f1)
if [ -n "$exp_line" ] && [ -n "$exec_line" ]; then
  if [ "$exp_line" -lt "$exec_line" ]; then
    ok "the export (line $exp_line) precedes the exec (line $exec_line), so it reaches the server"
  else
    bad "the export is at line $exp_line, AFTER the exec at line $exec_line — it never runs"
  fi
else
  bad "could not locate both the export and the exec (export=${exp_line:-none} exec=${exec_line:-none})"
fi

# 3. the Rust side reads the same name. Two spellings of one seam is the drift
#    this repo keeps finding; a rename on one side alone would leave the harness
#    setting a variable nothing consults, and every symptom would look identical.
if grep -q 'AMUX_NO_SELF_ADOPT' crates/amux-server/src/lib.rs; then
  ok "the server reads AMUX_NO_SELF_ADOPT under the same name"
else
  bad "crates/amux-server/src/lib.rs does not mention AMUX_NO_SELF_ADOPT — the two halves disagree"
fi

echo
echo "e2e-no-self-adopt: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
