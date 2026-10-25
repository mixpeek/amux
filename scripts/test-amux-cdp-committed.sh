#!/bin/bash
# `amux cdp` runs the chrome-cdp helper's COMMITTED bytes (origin/main), never a
# stale working-tree copy. The shared checkout's copy lacked the never-copy-
# Google guard (3baba214f) and signed Ethan's Chrome out of Google, 2026-10-18.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
AMUX="$HERE/amux"
T=$(mktemp -d); trap 'rm -rf -- "${T:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
command -v node >/dev/null || { echo "FAIL: node missing, no cell ran"; exit 1; }

R="$T/repo"; mkdir -p "$R/skills/chrome-cdp/scripts" "$T/home"
helper() { printf "import './profile-sync.mjs'; console.log('%s ' + process.argv.slice(2).join(' '));\n" "$1" > "$R/skills/chrome-cdp/scripts/cdp.mjs"; }
helper committed; echo "export const x = 1;" > "$R/skills/chrome-cdp/scripts/profile-sync.mjs"
git -C "$R" init -q && git -C "$R" add -A && git -C "$R" -c user.email=t@t -c user.name=t commit -qm base
git -C "$R" update-ref refs/remotes/origin/main HEAD
run() { HOME="$T/home" AMUX_REPO_DIR="$R" AMUX_CDP_SCRIPT= "$AMUX" cdp "$@" 2>"$T/err"; }

echo "1. a clean checkout runs the committed helper, from the cache"
check "output"             "committed list" "$(run list)"
check "no differs line"    "0" "$(cat "$T/home/.amux/logs/cdp-helper.log" 2>/dev/null | wc -l | tr -d ' ')"
check "sibling module came along" yes "$( [ -f "$T/home/.amux/cache/chrome-cdp/$(git -C "$R" rev-parse origin/main:skills/chrome-cdp/scripts)/profile-sync.mjs" ] && echo yes || echo no)"

echo "2. a stale or edited working tree is NOT what runs, and says so in the log"
helper worktree
check "output"             "committed list" "$(run list)"
check "differs logged"     "1" "$(grep -c cdp_worktree_helper_differs_ran_committed "$T/home/.amux/logs/cdp-helper.log")"

echo "3. a new origin/main is picked up on the next call"
helper newer; git -C "$R" -c user.email=t@t -c user.name=t commit -qam newer; git -C "$R" update-ref refs/remotes/origin/main HEAD
check "output"             "newer list" "$(run list)"

echo "4. AMUX_CDP_SCRIPT still wins"
printf "console.log('explicit');\n" > "$T/x.mjs"
check "explicit"           "explicit" "$(HOME="$T/home" AMUX_REPO_DIR="$R" AMUX_CDP_SCRIPT="$T/x.mjs" "$AMUX" cdp list 2>/dev/null)"

echo "5. no origin/main helper: falls back to the working tree and warns"
git -C "$R" update-ref -d refs/remotes/origin/main
check "fallback output"    "newer list" "$(run list)"
check "fallback warns"     yes "$(grep -q 'WARN running .* from a working tree' "$T/err" && echo yes || echo no)"

[ "$fails" -eq 0 ] && echo "PASS: amux cdp runs committed helper bytes" || { echo "FAIL: $fails cell(s)"; exit 1; }
