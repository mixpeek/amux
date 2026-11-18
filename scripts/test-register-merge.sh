#!/usr/bin/env bash
# AMUX-3117 regression: `amux register <existing>` must MERGE, not truncate.
#
# It used to `cat >` the session .env down to CC_NAME/CC_DIR/CC_FLAGS, silently
# wiping every other CC_* key. CC_DESC and CC_TAGS were lost that way, and
# because re-register is the only path to set flags (the PATCH-flags gap,
# AMUX-3115), the destructive write sat on the happy path. This pins that a
# re-register preserves the keys it does not manage while updating the ones it
# does. CLI-side .env write => no server log could catch a regression; this test
# is the guard.
set -euo pipefail
cd "$(dirname "$0")/.."
AMUX_BIN="${AMUX_BIN:-./amux}"
PASS=0; FAIL=0
check() { if [ "$2" = "$3" ]; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); echo "FAIL: $1 (expected '$2' got '$3')"; fi; }
has() { if grep -qF "$2" "$1"; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); echo "FAIL: $3 (missing '$2' in $1)"; fi; }

TMP=$(mktemp -d)
export CC_HOME="$TMP"
SESS="$TMP/sessions"; mkdir -p "$SESS"

# A pre-existing worker carrying config that register does NOT manage.
cat > "$SESS/wtest.env" <<'EOF'
# amux session: wtest
CC_NAME="wtest"
CC_DIR="/old/dir"
CC_FLAGS="--model sonnet"
CC_DESC="important description"
CC_TAGS=["alpha","beta"]
CC_ARCHIVED="0"
CC_MCP="chrome"
EOF

# Re-register to change the model + dir (the exact gtm-engine workflow).
CC_HOME="$TMP" "$AMUX_BIN" register wtest --model opus --dir "$TMP" >/dev/null 2>&1
F="$SESS/wtest.env"

# Unmanaged keys survive VERBATIM (values, including JSON, intact).
has "$F" 'CC_DESC="important description"' "CC_DESC preserved"
has "$F" 'CC_TAGS=["alpha","beta"]'        "CC_TAGS preserved (JSON value intact)"
has "$F" 'CC_ARCHIVED="0"'                 "CC_ARCHIVED preserved"
has "$F" 'CC_MCP="chrome"'                 "CC_MCP preserved"
# Managed keys are updated.
has "$F" 'CC_FLAGS="--model opus"'         "CC_FLAGS updated to the new model"
has "$F" "CC_DIR=\"$TMP\""                 "CC_DIR updated"
# And no duplication of a managed key (merge must exclude, not append).
check "CC_DESC appears exactly once" "1" "$(grep -c '^CC_DESC=' "$F")"
check "CC_FLAGS appears exactly once" "1" "$(grep -c '^CC_FLAGS=' "$F")"

# A successful register exits 0, with and without flags. With no flags the verb's
# last line was `[[ -n "$CC_FLAGS_STR" ]] && echo ...`, a false test that made a
# successful register exit 1, so an agent checking the exit code read success as
# failure (found by the amux Claude Code plugin's smoke test, 2026-11-18).
set +e
CC_HOME="$TMP" "$AMUX_BIN" register wnew --dir "$TMP" >/dev/null 2>&1; rc_plain=$?
CC_HOME="$TMP" "$AMUX_BIN" register wnew2 --dir "$TMP" --provider codex >/dev/null 2>&1; rc_prov=$?
CC_HOME="$TMP" "$AMUX_BIN" register wnew3 --model opus --dir "$TMP" >/dev/null 2>&1; rc_flags=$?
set -e
check "register with no flags exits 0" "0" "$rc_plain"
check "register with only --provider exits 0" "0" "$rc_prov"
check "register with flags exits 0" "0" "$rc_flags"

rm -rf "$TMP"
echo "register merge: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
