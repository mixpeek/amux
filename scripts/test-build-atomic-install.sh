#!/usr/bin/env bash
# The live server watches its installed executable and self-execs when it moves.
# Therefore the builder must prepare the entire replacement beside that path and
# reveal it with one rename; writing the watched path directly creates a window
# where every client, including the Tailscale URL, can lose the server.
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
mkdir -p "$CARGO_TARGET_DIR/release"
printf '%s\n' '#!/bin/sh' 'printf "%s\\n" new-build' > "$CARGO_TARGET_DIR/release/amux-server"
chmod 0755 "$CARGO_TARGET_DIR/release/amux-server"
EOF
chmod +x "$REPO/scripts/safe-cargo.sh"
(
  cd "$REPO"
  git add -A
  git -c user.name=test -c user.email=test@example.com commit -qm fixture
)

# Observe the actual filesystem verbs without replacing the builder's logic.
cat > "$FAKE_HOME/.cargo/bin/install" <<'EOF'
#!/bin/sh
printf 'install\t%s\n' "$*" >> "$TRACE"
exec /usr/bin/install "$@"
EOF
cat > "$FAKE_HOME/.cargo/bin/mv" <<'EOF'
#!/bin/sh
printf 'mv\t%s\n' "$*" >> "$TRACE"
exec /bin/mv "$@"
EOF
chmod +x "$FAKE_HOME/.cargo/bin/install" "$FAKE_HOME/.cargo/bin/mv"
export TRACE

printf '#!/bin/sh\nprintf old-build\\n\n' > "$INSTALL"
chmod 0755 "$INSTALL"

HOME="$FAKE_HOME" \
AMUX_REPO="$REPO" \
AMUX_RS_INSTALL="$INSTALL" \
AMUX_RS_BUILD_STAMP="$TMP/stamp" \
AMUX_RS_BUILD_LOG="$TMP/build.log" \
AMUX_RS_BUILD_LOCK="$TMP/build.lock" \
AMUX_RS_BUILD_PROVENANCE="$TMP/provenance.json" \
AMUX_BUILD_MIN_FREE_GB=0 \
AMUX_BUILD_DEBUG_MAX_GB=999999 \
  bash "$BUILDER"

install_line=$(sed -n '/^install/p' "$TRACE")
mv_line=$(sed -n '/^mv/p' "$TRACE")
case "$install_line" in
  *"$INSTALL.new."*) ;;
  *) echo "FAIL: build was not prepared at a sibling temp path: $install_line"; exit 1 ;;
esac
case "$mv_line" in
  *"$INSTALL.new."*" $INSTALL") ;;
  *) echo "FAIL: completed build was not atomically renamed into place: $mv_line"; exit 1 ;;
esac
case "$install_line" in
  *" $INSTALL") echo "FAIL: builder wrote directly to the watched executable"; exit 1 ;;
esac
if find "$(dirname "$INSTALL")" -name 'amux-server-rs.new.*' | grep -q .; then
  echo "FAIL: temporary install was not cleaned up"
  exit 1
fi
if ! "$INSTALL" | grep -q '^new-build$'; then
  echo "FAIL: final executable is not the completed replacement"
  exit 1
fi

echo "test-build-atomic-install: 5 passed, 0 failed"
