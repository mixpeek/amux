#!/usr/bin/env bash
# The builder replaces the installed authority launcher with the elected
# commit's bytes (rename, so a running launcher keeps its inode), leaves an
# absent or symlinked copy alone, and refuses bytes that do not parse. Runs the
# shipped function lifted from rust-auto-build.sh, not a paraphrase.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0
expect() { if "$@"; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); echo "FAIL: $*"; fi; }

awk '/^sync_authority_launcher\(\) \{/,/^}$/' "$ROOT/scripts/rust-auto-build.sh" > "$TMP/fn.sh"
expect test -s "$TMP/fn.sh"
REPO="$TMP/repo"; LOG="$TMP/log"; DEST="$TMP/bin/amux-build-authority"
git init -q "$REPO"; mkdir -p "$REPO/scripts" "$TMP/bin"
commit() { printf '%s\n' "$1" > "$REPO/scripts/rust-auto-build-authority.sh"
  git -C "$REPO" add -A; git -C "$REPO" -c user.name=t -c user.email=t@e commit -qm x; git -C "$REPO" rev-parse HEAD; }
run() { REPO="$REPO" LOG="$LOG" AMUX_AUTHORITY_INSTALL="$DEST" bash -c "source '$TMP/fn.sh'; sync_authority_launcher $1"; }

NEW=$(commit 'echo new launcher')
run "$NEW"
expect test ! -e "$DEST"                       # never installs a copy that was not there
echo 'echo old launcher' > "$DEST"
INODE=$(stat -f %i "$DEST" 2>/dev/null || stat -c %i "$DEST")
run "$NEW"
expect grep -qx 'echo new launcher' "$DEST"
expect test "$(stat -f %i "$DEST" 2>/dev/null || stat -c %i "$DEST")" != "$INODE"
expect grep -q "verdict=authority_launcher_synced" "$LOG"
BAD=$(commit 'if then fi (')
run "$BAD"
expect grep -qx 'echo new launcher' "$DEST"
expect grep -q "verdict=authority_launcher_invalid" "$LOG"

echo "test-authority-launcher-sync: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
