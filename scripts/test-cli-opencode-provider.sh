#!/usr/bin/env bash
# M0 smoke: the bash CLI builds an OpenCode command for CC_PROVIDER=opencode,
# and translates amux's --yolo into opencode's `--auto` (NOT claude's
# `--dangerously-skip-permissions`), while the Claude path is unchanged.
#
# Dry-run only: `amux start NAME --dry-run` prints the command and returns
# BEFORE any tmux session or server call, so this touches no fleet and needs no
# stub. Isolated CC_HOME + throwaway dir, like scripts/test-cli-quoting.sh.
#
# Discrimination: the opencode branch is what rewrites --yolo; without it the
# command reads `claude ... --dangerously-skip-permissions` and the first
# assertion fails. The Claude case is a positive control that must keep passing.
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

# ── 1. opencode + yolo + model ──────────────────────────────────────────────
write_env oc1 opencode "--model opencode-go/deepseek-v4.1-flash --dangerously-skip-permissions"
OUT="$(dry oc1)"
if printf '%s' "$OUT" | grep -q 'opencode'; then ok "opencode binary is used"; else bad "opencode binary missing" "$OUT"; fi
if printf '%s' "$OUT" | grep -q -- '--auto'; then ok "amux --yolo becomes opencode --auto"; else bad "no --auto (yolo not translated)" "$OUT"; fi
if printf '%s' "$OUT" | grep -q -- '--dangerously-skip-permissions'; then bad "claude yolo flag leaked into opencode argv" "$OUT"; else ok "claude yolo flag not leaked"; fi
if printf '%s' "$OUT" | grep -q -- '--model opencode-go/deepseek-v4.1-flash'; then ok "configured model preserved"; else bad "model dropped" "$OUT"; fi

# ── 2. opencode plain ───────────────────────────────────────────────────────
write_env oc2 opencode ""
OUT="$(dry oc2)"
if printf '%s' "$OUT" | grep -Eq '^[[:space:]]*opencode[[:space:]]*$'; then ok "plain opencode command is exactly 'opencode'"; else bad "plain opencode command is not 'opencode'" "$OUT"; fi

# ── 3. positive control: claude unchanged ───────────────────────────────────
write_env cl claude ""
OUT="$(dry cl)"
if printf '%s' "$OUT" | grep -q 'claude --model sonnet'; then ok "claude regression intact (claude --model sonnet)"; else bad "claude path changed" "$OUT"; fi

# ── 4. register --provider writes CC_PROVIDER and start honours it ──────────
timeout 20 env CC_HOME="$TMP" bash "$AMUX_BIN" register oc4 --provider opencode --dir "$WORK" >/dev/null 2>&1 || true
OUT="$(dry oc4)"
if printf '%s' "$OUT" | grep -Eq '^[[:space:]]*opencode[[:space:]]*$'; then ok "register --provider opencode -> opencode"; else bad "register --provider not honoured" "$OUT"; fi

echo
echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
