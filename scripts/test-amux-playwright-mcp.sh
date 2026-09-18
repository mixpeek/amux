#!/usr/bin/env bash
# Regression coverage for the localhost default and explicit host override.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/amux-playwright-mcp-test.XXXXXX")
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/home"

cat > "$TMP/bin/npx" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$CAPTURE_FILE"
EOF
chmod +x "$TMP/bin/npx"

run_case() {
  local expected=$1
  local args="$TMP/$expected.args" log="$TMP/$expected.log"
  shift
  CAPTURE_FILE="$args" HOME="$TMP/home" PATH="$TMP/bin:$PATH" "$@" \
    2> "$log"
  local host
  host=$(awk '$0 == "--host" { getline; print; exit }' "$args")
  [ "$host" = "$expected" ]
  grep -Fx "browser-security playwright_mcp_host=$expected" "$log" >/dev/null
}

run_case 127.0.0.1 "$ROOT/scripts/amux-playwright-mcp.sh" lane-8931
run_case 10.0.0.5 env AMUX_PLAYWRIGHT_MCP_HOST=10.0.0.5 \
  "$ROOT/scripts/amux-playwright-mcp.sh" lane-8931

echo "ok: Playwright MCP defaults to localhost and honors explicit override"
