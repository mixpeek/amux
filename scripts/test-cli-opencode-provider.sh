#!/usr/bin/env bash
# Smoke: the bash CLI routes an OpenCode worker's START through the server API
# (server_owns_start), so the server owns the provider->binary map and the
# provider-specific command setup — the same path codex/ollama/gemini/muse take.
#
# This REPLACES the M0 assertion that the CLI builds an `opencode ... --auto`
# command itself. `amux start` no longer builds an opencode command locally:
# commit bd17ff83 added opencode to server_owns_start(), so `start --dry-run`
# reports the delegation instead. The --yolo->--auto normalization and the
# model handling now live server-side and are pinned by the Rust unit tests
# `opencode_yolo_is_auto_and_has_no_invented_model` and
# `opencode_launch_is_standalone_and_normalizes_yolo` (session_verbs.rs), plus
# `opencode_is_a_session_provider_and_launches_opencode`. Asserting the command
# text here again would test a path the CLI no longer walks.
#
# Dry-run only: `amux start NAME --dry-run` prints the plan and returns BEFORE
# any tmux session or server call, so this touches no fleet and needs no stub.
# Isolated CC_HOME + throwaway dir, like scripts/test-cli-quoting.sh.
#
# Discrimination: if opencode were NOT in server_owns_start(), the Claude
# fallback would build a local `claude ...` command and the "no claude fallback"
# cell fails. The Claude case is a positive control that must keep passing.
set -euo pipefail
cd "$(dirname "$0")/.." || exit 1

AMUX_BIN="${AMUX_BIN:-./amux}"
PASS=0; FAIL=0
ok()  { echo "  ok   $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL $1"; [ -n "${2:-}" ] && printf '       %s\n' "$2"; FAIL=$((FAIL+1)); }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
WORK="$TMP/work"; mkdir -p "$TMP/sessions" "$WORK"

write_env() { # <name> <provider> <flags>
  cat > "$TMP/sessions/$1.env" <<EOF
CC_NAME="$1"
CC_DIR="$WORK"
CC_PROVIDER="$2"
CC_FLAGS="$3"
EOF
}

dry() { timeout 20 env CC_HOME="$TMP" bash "$AMUX_BIN" start "$1" --dry-run 2>&1 || true; }

# delegated <out> <name>: the dry-run says the server runs the start, for <name>.
delegated() { printf '%s' "$1" | grep -q "started via server API: POST /api/sessions/$2/start"; }

# ── 1. opencode + yolo + model: START delegates to the server ───────────────
write_env oc1 opencode "--model opencode-go/deepseek-v4.1-flash --dangerously-skip-permissions"
OUT="$(dry oc1)"
if delegated "$OUT" oc1; then ok "opencode start is delegated to the server API"; else bad "opencode start not delegated" "$OUT"; fi
if printf '%s' "$OUT" | grep -q 'claude'; then bad "claude fallback leaked for opencode" "$OUT"; else ok "no claude fallback for opencode"; fi
if printf '%s' "$OUT" | grep -q -- '--dangerously-skip-permissions'; then bad "claude yolo flag leaked into the opencode plan" "$OUT"; else ok "claude yolo flag not leaked"; fi

# ── 2. opencode plain ───────────────────────────────────────────────────────
write_env oc2 opencode ""
OUT="$(dry oc2)"
if delegated "$OUT" oc2; then ok "plain opencode start is delegated"; else bad "plain opencode start not delegated" "$OUT"; fi

# ── 3. positive control: claude still builds a local command ────────────────
write_env cl claude ""
OUT="$(dry cl)"
if printf '%s' "$OUT" | grep -q 'claude --model sonnet'; then ok "claude regression intact (claude --model sonnet)"; else bad "claude path changed" "$OUT"; fi

# ── 4. register --provider writes CC_PROVIDER and start honours it ──────────
timeout 20 env CC_HOME="$TMP" bash "$AMUX_BIN" register oc4 --provider opencode --dir "$WORK" >/dev/null 2>&1 || true
if grep -q '^CC_PROVIDER="opencode"$' "$TMP/sessions/oc4.env" 2>/dev/null; then
  ok "register --provider opencode -> CC_PROVIDER=opencode"
else
  bad "register --provider did not write CC_PROVIDER=opencode" "$(cat "$TMP/sessions/oc4.env" 2>/dev/null)"
fi
OUT="$(dry oc4)"
if delegated "$OUT" oc4; then ok "registered opencode start is delegated"; else bad "registered opencode not delegated" "$OUT"; fi

echo
echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
