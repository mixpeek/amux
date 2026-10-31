#!/usr/bin/env bash
# The build lock with an EMPTY pid file (implicit state machines inventory
# 2026-10-31, #22). The prebuild writes its pid one step after mkdir, and the
# builder used to break any lock without a pid, so two builds ran and the disk
# guard's rm could race a live one. A young empty lock is now respected; one
# over a minute old is still broken as stale.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BUILDER="$ROOT/scripts/rust-auto-build.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
REPO="$TMP/repo"
FAKE_HOME="$TMP/home"
TRACE="$TMP/fs.trace"
INSTALL="$TMP/bin/amux-server-rs"

git init -q -b main "$REPO"
mkdir -p "$REPO/crates" "$REPO/scripts" "$FAKE_HOME/.cargo/bin" "$(dirname "$INSTALL")"
printf '[workspace]\n' > "$REPO/Cargo.toml"
printf 'fixture\n' > "$REPO/crates/input.rs"
cat > "$REPO/scripts/safe-cargo.sh" <<'EOF'
#!/bin/sh
# Output dir the way cargo picks it: --profile NAME, else --release.
out=release; prev=
for a in "$@"; do [ "$prev" = --profile ] && out="$a"; prev="$a"; done
mkdir -p "$CARGO_TARGET_DIR/$out"
printf '%s\n' '#!/bin/sh' 'printf "%s\\n" new-build' > "$CARGO_TARGET_DIR/$out/amux-server"
chmod 0755 "$CARGO_TARGET_DIR/$out/amux-server"
EOF
chmod +x "$REPO/scripts/safe-cargo.sh"
(
  cd "$REPO"
  git add -A
  git -c user.name=test -c user.email=test@example.com commit -qm fixture
)

printf '#!/bin/sh\nprintf old-build\\n\n' > "$INSTALL"
chmod 0755 "$INSTALL"

run() {
  HOME="$FAKE_HOME" AMUX_REPO="$REPO" AMUX_RS_INSTALL="$INSTALL" AMUX_RS_BUILD_STAMP="$TMP/stamp" \
  AMUX_RS_BUILD_LOG="$TMP/build.log" AMUX_RS_BUILD_LOCK="$TMP/build.lock" AMUX_RS_BUILD_PROVENANCE="$TMP/provenance.json" \
  AMUX_RS_ACTIVATION_REF=HEAD AMUX_BUILD_MIN_FREE_GB=0 AMUX_BUILD_DEBUG_MAX_GB=999999 AMUX_RS_SHELL_DEFER_MAX_S=0 \
  AMUX_ACTIVATION_ROLLBACK=0 AMUX_RS_RUNS_URL=http://127.0.0.1:9/none bash "$BUILDER" > "$TMP/stdout" 2>&1
}
pass=0
mkdir "$TMP/build.lock"   # a holder between mkdir and its pid write
run || { echo "FAIL: the builder exited non-zero"; cat "$TMP/stdout"; exit 1; }
if grep -q 'verdict=build_lock_young' "$TMP/build.log" && ! grep -q 'deploy build elapsed_s' "$TMP/build.log"; then
  pass=$((pass+1)); else echo "FAIL: a young empty lock was broken"; cat "$TMP/build.log"; exit 1; fi
[ -d "$TMP/build.lock" ] && pass=$((pass+1)) || { echo "FAIL: the young lock was removed"; exit 1; }
touch -t 202001010000 "$TMP/build.lock"   # the same lock, long abandoned
run || { echo "FAIL: the builder exited non-zero"; cat "$TMP/stdout"; exit 1; }
if grep -q 'breaking stale lock' "$TMP/build.log" && grep -q 'deploy build elapsed_s' "$TMP/build.log"; then
  pass=$((pass+1)); else echo "FAIL: an old empty lock was not broken"; cat "$TMP/build.log"; exit 1; fi
echo "test-build-lock-young: $pass passed, 0 failed"
