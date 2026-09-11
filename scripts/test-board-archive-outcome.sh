#!/usr/bin/env bash
# AF-701: the server gates archiving a `needsyou` card on either a status
# change away from needsyou or an `archive_outcome` string, but until now the
# CLI's `archive` verb had no way to send one -- so the sanctioned path
# through the new gate was a raw, unattributed PATCH (ethos rule 6). This also
# pins that `--authorized-by` and `--archive-outcome` compose: a needsyou card
# owned by another lane can need both flags on one call.
#
# CLI-side write (the PATCH body is built in the shell), so runs the REAL
# shipped verb against a MOCK curl, same pattern as test-board-shepherd.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
AMUX_BIN="${AMUX_BIN:-./amux}"
PASS=0; FAIL=0
has()  { if grep -qF -- "$2" "$1"; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); echo "FAIL: $3 (missing '$2')"; fi; }
lacks() { if grep -qF -- "$2" "$1"; then FAIL=$((FAIL+1)); echo "FAIL: $3 (unexpectedly has '$2')"; else PASS=$((PASS+1)); fi; }

TMP=$(mktemp -d)
mkdir -p "$TMP/bin"
cat > "$TMP/bin/curl" <<'MOCK'
#!/usr/bin/env bash
body=""; is_patch=0; args=("$@")
for ((i=0;i<${#args[@]};i++)); do
  case "${args[i]}" in
    -X) [[ "${args[$((i+1))]}" == "PATCH" ]] && is_patch=1 ;;
    -d|--data|--data-binary) body="${args[$((i+1))]}"; [[ "$body" == "@-" ]] && body="$(cat)" ;;
  esac
done
if [[ $is_patch -eq 1 ]]; then
  printf '%s\n' "$body" >> "$CAPTURE"
  echo '{"ok":true,"id":"TEST-1","archived":1}'
else
  echo '{"item":{"id":"TEST-1","status":"needsyou","type":"code"}}'
fi
MOCK
chmod +x "$TMP/bin/curl"
export PATH="$TMP/bin:$PATH"
export AMUX_API="https://localhost:9999"   # never contacted -- curl is mocked
export AMUX_SESSION="wtest" AMUX_WORKER="wtest"

# 1. Plain archive sends only the archived field, no stray keys from flags
#    nobody passed.
export CAPTURE="$TMP/c1"; : > "$CAPTURE"
"$AMUX_BIN" board archive TEST-1 >/dev/null 2>&1 || true
has "$TMP/c1" '"archived": 1' "plain archive sends archived:1"
lacks "$TMP/c1" 'archive_outcome' "plain archive sends no archive_outcome"
lacks "$TMP/c1" 'authorized_by' "plain archive sends no authorized_by"

# 2. --archive-outcome alone.
export CAPTURE="$TMP/c2"; : > "$CAPTURE"
"$AMUX_BIN" board archive TEST-1 --archive-outcome "answered informally" >/dev/null 2>&1 || true
has "$TMP/c2" '"archive_outcome": "answered informally"' "--archive-outcome sends the field"

# 3. Both flags, in this order.
export CAPTURE="$TMP/c3"; : > "$CAPTURE"
"$AMUX_BIN" board archive TEST-1 --authorized-by ethan --archive-outcome "no longer needed" >/dev/null 2>&1 || true
has "$TMP/c3" '"authorized_by": "ethan"' "authorized-by then archive-outcome: auth sent"
has "$TMP/c3" '"archive_outcome": "no longer needed"' "authorized-by then archive-outcome: outcome sent"

# 4. Both flags, REVERSED order -- the composition this entry exists to pin.
export CAPTURE="$TMP/c4"; : > "$CAPTURE"
"$AMUX_BIN" board archive TEST-1 --archive-outcome "no longer needed" --authorized-by ethan >/dev/null 2>&1 || true
has "$TMP/c4" '"authorized_by": "ethan"' "archive-outcome then authorized-by: auth sent"
has "$TMP/c4" '"archive_outcome": "no longer needed"' "archive-outcome then authorized-by: outcome sent"

rm -rf "$TMP"
echo "board archive --archive-outcome: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
