#!/usr/bin/env bash
# AMUX-6530: a deploy build logs a heartbeat while cargo runs, so a long build
# does not read as a dead builder (deploy.builder_is_ticking), and the heartbeat
# stops with the build: none after the build's own "elapsed_s" line, no
# leftover process.
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
sleep 3
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

HOME="$FAKE_HOME" \
AMUX_REPO="$REPO" \
AMUX_RS_INSTALL="$INSTALL" \
AMUX_RS_BUILD_STAMP="$TMP/stamp" \
AMUX_RS_BUILD_LOG="$TMP/build.log" \
AMUX_RS_BUILD_LOCK="$TMP/build.lock" \
AMUX_RS_BUILD_PROVENANCE="$TMP/provenance.json" \
AMUX_RS_ACTIVATION_REF=HEAD \
AMUX_BUILD_MIN_FREE_GB=0 \
AMUX_BUILD_DEBUG_MAX_GB=999999 \
AMUX_BUILD_HEARTBEAT_S=1 \
AMUX_RS_SHELL_DEFER_MAX_S=0 \
AMUX_RS_RUNS_URL=http://127.0.0.1:9/none \
  bash "$BUILDER" > "$TMP/stdout" 2>&1 || { echo "FAIL: the builder exited non-zero"; cat "$TMP/stdout"; exit 1; }
cat "$TMP/stdout" "$TMP/build.log" 2>/dev/null > "$TMP/out"

pass=0
beats=$(grep -c 'verdict=deploy_build_running' "$TMP/out" || true)
if [ "$beats" -ge 2 ]; then pass=$((pass+1)); else echo "FAIL: expected >= 2 heartbeats during a 3 s build, got $beats"; cat "$TMP/out"; exit 1; fi
after=$(sed -n '/deploy build elapsed_s=/,$p' "$TMP/out" | grep -c 'verdict=deploy_build_running' || true)
if [ "$after" -eq 0 ]; then pass=$((pass+1)); else echo "FAIL: $after heartbeat(s) after the build finished"; exit 1; fi
sleep 2
late=$(grep -c 'verdict=deploy_build_running' "$TMP/out" || true)
if [ "$late" -eq "$beats" ]; then pass=$((pass+1)); else echo "FAIL: the heartbeat outlived the builder ($beats -> $late)"; exit 1; fi
echo "test-build-heartbeat: $pass passed, 0 failed"
