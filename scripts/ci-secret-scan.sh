#!/bin/bash
# The repo-wide secret scan: checks.yml and gate.yml both run it.
set -euo pipefail
PATTERNS=(
  "sk-ant-api[0-9a-zA-Z-]{20,}" "AKIA[0-9A-Z]{16}" "ghp_[A-Za-z0-9]{36}"
  "sk_(test|live)_[A-Za-z0-9]{20,}"
  "(R2|AWS)_SECRET(_ACCESS)?_KEY[[:space:]]*=[[:space:]]*[A-Za-z0-9/+]{32,}"
  "CLERK_SECRET_KEY[[:space:]]*=[[:space:]]*[A-Za-z0-9_-]{20,}"
  "xox[baprs]-[A-Za-z0-9-]{10,}" "glpat-[A-Za-z0-9_-]{20,}"
)
# Self-test FIRST: a planted secret must be caught, or a green run
# here means nothing (the scanner that missed AC-239 was also green).
# Built at RUNTIME, never written literally in this file — a literal
# here would be found by the scan below and fail the repo on its own
# self-test, forever. (Caught before shipping: the first cut did
# exactly that.)
printf 'CLERK_SECRET_KEY=sk_%s_%s\n' "test" "$(printf 'A%.0s' $(seq 28))" > /tmp/_probe
grep -Eq -e "sk_(test|live)_[A-Za-z0-9]{20,}" /tmp/_probe \
  || { echo "SELF-TEST FAILED: scanner cannot catch a planted secret"; exit 1; }
rm -f /tmp/_probe
fail=0
for p in "${PATTERNS[@]}"; do
  if out=$(grep -REn -e "$p" --include='*.py' --include='*.sh' --include='*.yml' \
            --include='*.yaml' --include='*.json' --include='*.ts' --include='*.js' \
            --include='*.rs' --include='*.toml' . 2>/dev/null); then
    echo "SECRET PATTERN MATCHED: $p"; echo "$out" | head -3 | cut -c1-120; fail=1
  fi
done
[ "$fail" -eq 0 ] && echo "secret scan clean" || exit 1
