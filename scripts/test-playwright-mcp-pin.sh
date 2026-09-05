#!/usr/bin/env bash
# The playwright-mcp wrapper must name a VERSION, never `@latest` (AMUX-3989).
#
# WHY. `npx -y @playwright/mcp@latest` puts an upstream release on this fleet
# with no amux commit behind it: nothing to review, nothing to bisect, and the
# moment of change is whenever some lane's npx cache happened to expire.
# Measured 2026-08-31: the cache held 0.0.68, which is what the fleet was
# running, while `@latest` resolved to 0.0.79. Browser lifecycle is exactly
# where an unreviewed upstream change is expensive — see epic AMUX-3988.
#
# Reads the SHIPPED script. No network, no npx, no browser: this cell must give
# the same answer on a laptop and on a CI runner, which is the property three
# separate tests failed to have today (AMUX-3962, AMUX-3969, AMUX-3974).
set -uo pipefail
cd "$(dirname "$0")/.."
W="${PLAYWRIGHT_MCP_WRAPPER:-$(pwd)/scripts/amux-playwright-mcp.sh}"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
no(){ FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

echo "playwright-mcp pin (AMUX-3989)"

[ -f "$W" ] || { no "wrapper not found" "$W"; printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"; exit 1; }

# 1. No floating tag, in any form npm accepts.
floating="$(grep -nE '@playwright/mcp@(latest|next|beta|canary)' "$W" || true)"
[ -z "$floating" ] \
  && ok "no floating dist-tag for @playwright/mcp" \
  || no "@playwright/mcp must be pinned to a version" "$floating"

# 2. A CONCRETE VERSION IS PRESENT. Cell 1 alone passes if the dependency is
#    deleted entirely, which would be a different bug reading as a fix.
ver="$(grep -oE 'PLAYWRIGHT_MCP_VERSION="\$\{AMUX_PLAYWRIGHT_MCP_VERSION:-[0-9]+\.[0-9]+\.[0-9]+\}"' "$W" || true)"
[ -n "$ver" ] \
  && ok "a concrete semver default is present ($(printf '%s' "$ver" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+'))" \
  || no "the wrapper must default to a concrete x.y.z" \
        "found: $(grep -n 'PLAYWRIGHT_MCP_VERSION' "$W" || echo '(nothing)')"

# 3. And the launch actually USES it. A pinned constant the exec line ignores is
#    decoration — the ethos-rule-1 shape, one file down.
[ -n "$(grep -nE 'npx .*@playwright/mcp@\$\{PLAYWRIGHT_MCP_VERSION\}' "$W" || true)" ] \
  && ok "the exec line launches the pinned version, not a literal" \
  || no "the pinned variable must be what npx receives" \
        "$(grep -n 'exec npx' "$W" || echo '(no exec npx line)')"

# 4. The override still exists, so a lane can test an upgrade without editing
#    a file every other lane shares.
grep -q 'AMUX_PLAYWRIGHT_MCP_VERSION' "$W" \
  && ok "AMUX_PLAYWRIGHT_MCP_VERSION can override it for a one-off test" \
  || no "pinning must not remove the escape" "no env override found"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
