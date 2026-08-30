#!/bin/bash
# Basic test suite for amux-remote CLI
# Tests configuration loading, error handling, and command parsing

set -euo pipefail

# Color codes for output
PASS=$'\033[32m✓\033[0m'
FAIL=$'\033[31m✗\033[0m'
SKIP=$'\033[33m⊘\033[0m'

passed=0
failed=0
skipped=0

assert_success() {
  local name="$1"
  local cmd="$2"

  if eval "$cmd" >/dev/null 2>&1; then
    echo "$PASS $name"
    ((passed++))
  else
    echo "$FAIL $name"
    ((failed++))
  fi
}

assert_failure() {
  local name="$1"
  local cmd="$2"

  if ! eval "$cmd" >/dev/null 2>&1; then
    echo "$PASS $name"
    ((passed++))
  else
    echo "$FAIL $name"
    ((failed++))
  fi
}

assert_contains() {
  local name="$1"
  local cmd="$2"
  local expected="$3"

  if output=$(eval "$cmd" 2>&1) && echo "$output" | grep -q "$expected"; then
    echo "$PASS $name"
    ((passed++))
  else
    echo "$FAIL $name (expected: '$expected', got: '$output')"
    ((failed++))
  fi
}

# ── Test: Configuration Loading ──────────────────────────────────────────────

echo "=== Configuration Loading Tests ==="

# Test 1: Missing required config
assert_failure \
  "Fails without AMUX_URL" \
  "unset AMUX_URL; unset AMUX_TOKEN; $(cd /home/syseng/src/amux && ./amux-remote ls)"

# Test 2: Config file loading
TEST_HOME=$(mktemp -d)
trap "rm -rf $TEST_HOME" EXIT

cat > "$TEST_HOME/.amux/remote.env" << 'EOF'
AMUX_URL=https://example.com:8824
AMUX_TOKEN=test-token-123
EOF

assert_success \
  "Loads config from ~/.amux/remote.env" \
  "HOME=$TEST_HOME $(cd /home/syseng/src/amux && ./amux-remote url)"

# Test 3: Environment override
assert_success \
  "Environment vars override config file" \
  "AMUX_URL=https://override.com:8824 AMUX_TOKEN=override-token HOME=$TEST_HOME $(cd /home/syseng/src/amux && ./amux-remote url)"

# ── Test: Command Parsing ────────────────────────────────────────────────────

echo ""
echo "=== Command Parsing Tests ==="

AMUX_URL="https://example.com:8824"
AMUX_TOKEN="test-token"
export AMUX_URL AMUX_TOKEN

# Test 4: Help command
assert_contains \
  "Help command shows usage" \
  "$(cd /home/syseng/src/amux && ./amux-remote help)" \
  "Commands"

# Test 5: URL command
assert_contains \
  "URL command returns server URL" \
  "$(cd /home/syseng/src/amux && ./amux-remote url)" \
  "https://example.com:8824"

# Test 6: Missing command
assert_failure \
  "Unknown command fails" \
  "$(cd /home/syseng/src/amux && ./amux-remote nonexistent-command)"

# ── Test: Error Messages ─────────────────────────────────────────────────────

echo ""
echo "=== Error Handling Tests ==="

# Test 7: Missing session name
assert_failure \
  "Peek without session name fails" \
  "AMUX_URL=https://example.com:8824 AMUX_TOKEN=token $(cd /home/syseng/src/amux && ./amux-remote peek)"

# Test 8: Missing send text
assert_failure \
  "Send without text fails" \
  "AMUX_URL=https://example.com:8824 AMUX_TOKEN=token $(cd /home/syseng/src/amux && ./amux-remote send session-name)"

# ── Test: iTerm2 Detection ───────────────────────────────────────────────────

echo ""
echo "=== iTerm2 Detection Tests ==="

# Test 9: iTerm2 auto-detection (should work in any terminal)
assert_success \
  "iTerm2 detection doesn't crash" \
  "AMUX_URL=https://example.com:8824 AMUX_TOKEN=token $(cd /home/syseng/src/amux && ./amux-remote url)"

# Test 10: Force CC mode
assert_success \
  "Force CC mode doesn't crash" \
  "AMUX_CC=1 AMUX_URL=https://example.com:8824 AMUX_TOKEN=token $(cd /home/syseng/src/amux && ./amux-remote url)"

# ── Test: SSH Host Derivation ────────────────────────────────────────────────

echo ""
echo "=== SSH Host Tests ==="

# Test 11: SSH host from URL
assert_contains \
  "Derives SSH host from AMUX_URL" \
  "AMUX_URL=https://example.com:8824 AMUX_TOKEN=token AMUX_SSH_HOST=override.com $(cd /home/syseng/src/amux && ./amux-remote url)" \
  "example.com"

# ── Test: Environment Variable Handling ──────────────────────────────────────

echo ""
echo "=== Environment Variable Tests ==="

# Test 12: AMUX_SSH_USER override
AMUX_URL="https://example.com:8824"
AMUX_TOKEN="token"
AMUX_SSH_USER="testuser"
export AMUX_URL AMUX_TOKEN AMUX_SSH_USER

assert_success \
  "AMUX_SSH_USER is respected" \
  "$(cd /home/syseng/src/amux && ./amux-remote url)"

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "=== Test Summary ==="
echo "Passed:  $passed"
echo "Failed:  $failed"
echo "Skipped: $skipped"
echo "Total:   $((passed + failed + skipped))"

if [[ $failed -eq 0 ]]; then
  echo ""
  echo "All tests passed!"
  exit 0
else
  echo ""
  echo "$failed test(s) failed"
  exit 1
fi
