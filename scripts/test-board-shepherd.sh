#!/usr/bin/env bash
# AF-697: the server's own cross-board-reassignment refusal, and this CLI's own
# help text, both name `shepherd` as a peer hand-off escape hatch on par with
# `reviewer` -- but no CLI verb existed to set it, even though `shepherd` is a
# fully PATCH-able server field. A how_to_fix naming a verb that does not exist
# sends a caller straight at a raw, unattributed PATCH (ethos rule 6).
#
# This is a CLI-side write (the PATCH body is built in the shell), so no server
# log could catch a regression -- runs the REAL shipped verb against a MOCK
# curl, same pattern as test-needsyou-signin-link.sh. No live server needed.
set -euo pipefail
cd "$(dirname "$0")/.."
AMUX_BIN="${AMUX_BIN:-./amux}"
PASS=0; FAIL=0
has()  { if grep -qF -- "$2" "$1"; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); echo "FAIL: $3 (missing '$2')"; fi; }

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
  echo '{"ok":true,"id":"TEST-1","shepherd":"peer-lane"}'
else
  echo '{"item":{"id":"TEST-1","status":"doing","type":"code"}}'
fi
MOCK
chmod +x "$TMP/bin/curl"
export PATH="$TMP/bin:$PATH"
export AMUX_API="https://localhost:9999"   # never contacted -- curl is mocked
export AMUX_SESSION="wtest" AMUX_WORKER="wtest"

# 1. Setting a shepherd sends the field, attributed via X-Amux-Worker (the CLI
#    adds that header on every board write; not re-checked here since every
#    other board verb already pins it).
export CAPTURE="$TMP/c1"; : > "$CAPTURE"
"$AMUX_BIN" board shepherd TEST-1 gtm-engine >/dev/null 2>&1 || true
has "$TMP/c1" '"shepherd": "gtm-engine"' "shepherd set sends the field"

# 2. 'none' clears it to an empty string, not a literal absence -- the same
#    convention `reviewer` uses, and the one thing a caller cannot express by
#    omitting the argument (omitting dies on usage, cell 3 below).
export CAPTURE="$TMP/c2"; : > "$CAPTURE"
"$AMUX_BIN" board shepherd TEST-1 none >/dev/null 2>&1 || true
has "$TMP/c2" '"shepherd": ""' "'none' clears the field"

# 3. Missing the peer argument must die on usage, not silently PATCH an empty
#    shepherd -- the failure mode a hand-typed `shepherd <id>` with no second
#    arg would otherwise hit.
export CAPTURE="$TMP/c3"; : > "$CAPTURE"
out3=$("$AMUX_BIN" board shepherd TEST-1 2>&1) && rc3=0 || rc3=$?
if [ "$rc3" -ne 0 ] && [ ! -s "$TMP/c3" ]; then
  PASS=$((PASS+1))
else
  FAIL=$((FAIL+1)); echo "FAIL: missing peer must die on usage without PATCHing anything (rc=$rc3): $out3"
fi

rm -rf "$TMP"
echo "board shepherd verb: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
