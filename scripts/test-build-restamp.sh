#!/usr/bin/env bash
# A commit with no compile-input change deploys without a compile (2026-11-08).
#
# Runs the shipped builder against a committed checkout with a fake cargo and a
# fake server that adopts whatever commit the receipt beside the binary names.
# A docs commit must re-stamp the receipt and compile nothing; a change to a
# file the server compiles in (an include_str! target outside crates/) must
# compile; so must a server that does not read receipts, and a restamp that is
# never adopted, which also has to put the old receipt back.
set -euo pipefail
export AMUX_RS_RUNS_URL=http://127.0.0.1:9/none
export AMUX_ACTIVATION_ROLLBACK=0

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BUILDER="$ROOT/scripts/rust-auto-build.sh"
TMP=$(mktemp -d)
SERVER_PID=""
trap '[ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null; cp "$LOG" "${KEEP_LOG:-/dev/null}" 2>/dev/null; rm -rf "$TMP"' EXIT
REPO="$TMP/repo"
FAKE_HOME="$TMP/home"
INSTALL="$TMP/bin/amux-server-rs"
STAMP="$TMP/stamp"
LOG="$TMP/build.log"
HEALTH="$TMP/health.json"
TRACE="$TMP/build.trace"
MODE="$TMP/server.mode"
PASS=0; FAIL=0
export TRACE

ok() { PASS=$((PASS + 1)); }
bad() { FAIL=$((FAIL + 1)); echo "FAIL: $1"; }
expect() { if "$@"; then ok; else bad "$*"; fi; }
builds() { grep -c '^build ' "$TRACE" 2>/dev/null || echo 0; }

git init -q -b main "$REPO"
mkdir -p "$REPO/crates/srv/src" "$REPO/scripts" "$REPO/docs" "$FAKE_HOME/.amux/rust-build-target" "$(dirname "$INSTALL")"
printf '[workspace]\n' > "$REPO/Cargo.toml"
# A compiled-in file outside crates/, the shape of the bash CLI and the hooks.
printf 'const BAKED: &str = include_str!("../../../scripts/baked.sh");\n' > "$REPO/crates/srv/src/lib.rs"
printf 'echo baked v1\n' > "$REPO/scripts/baked.sh"
printf 'notes\n' > "$REPO/docs/notes.md"
cp "$ROOT/scripts/ci-rust-key.mjs" "$REPO/scripts/ci-rust-key.mjs"
cat > "$REPO/scripts/safe-cargo.sh" <<'EOF'
#!/bin/sh
set -eu
sha=$(git rev-parse HEAD)
printf 'build %s\n' "$sha" >> "$TRACE"
out=release; prev=
for a in "$@"; do [ "$prev" = --profile ] && out="$a"; prev="$a"; done
mkdir -p "$CARGO_TARGET_DIR/$out"
printf '#!/bin/sh\necho %s\n' "$sha" > "$CARGO_TARGET_DIR/$out/amux-server"
chmod 0755 "$CARGO_TARGET_DIR/$out/amux-server"
EOF
chmod +x "$REPO/scripts/safe-cargo.sh"

commit() {
  (cd "$REPO" && git add -A && git -c user.name=test -c user.email=test@example.com commit -qm "$1")
  git -C "$REPO" update-ref refs/remotes/origin/main "$(git -C "$REPO" rev-parse HEAD)"
  git -C "$REPO" rev-parse HEAD
}

# The fake server: in mode "adopt" it serves the receipt's commit, as the real
# one does after it re-executes; in "ignore" it never adopts; in "old" it
# adopts but, like a server from before receipts, publishes no build_commit.
printf 'adopt\n' > "$MODE"
(
  while :; do
    python3 - "${INSTALL}.identity.json" "$HEALTH" "$(cat "$MODE")" <<'PY' 2>/dev/null || true
import json,os,sys
receipt,health,mode=sys.argv[1:4]
try: d=json.load(open(receipt))
except (OSError,ValueError): raise SystemExit
try: h=json.load(open(health))
except (OSError,ValueError): h={}
if mode != 'ignore' or not h:
    h={'commit_full':d['sha'],'commit':d['sha'][:12],'build':d['build']}
    if mode != 'old': h['build_commit']=d.get('compiled_at', d['sha'])
tmp=health+'.tmp'; json.dump(h,open(tmp,'w')); os.replace(tmp,health)
PY
    sleep 0.2
  done
) &
SERVER_PID=$!

run_builder() {
  env HOME="$FAKE_HOME" AMUX_REPO="$REPO" AMUX_RS_INSTALL="$INSTALL" AMUX_RS_BUILD_STAMP="$STAMP" \
  AMUX_RS_BUILD_LOCK="$TMP/lock" AMUX_RS_BUILD_LOG="$LOG" AMUX_RS_BUILD_PROVENANCE="$TMP/provenance.json" \
  AMUX_RS_HEALTH_URL="file://$HEALTH" AMUX_BUILD_MIN_FREE_GB=0 AMUX_BUILD_DEBUG_CLEAR_ABOVE_GB=999999 \
  AMUX_RS_BUILD_COOLDOWN=0 AMUX_ACTIVATION_HEALTH_S=3 "$@" bash "$BUILDER" >/dev/null
}
receipt() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2],""))' "${INSTALL}.identity.json" "$1"; }
live() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["commit_full"])' "$HEALTH"; }
wait_live() { for _ in $(seq 1 30); do [ "$(live 2>/dev/null)" = "$1" ] && return 0; sleep 0.2; done; return 1; }
reads_receipts() { for _ in $(seq 1 30); do grep -q build_commit "$HEALTH" 2>/dev/null && return 0; sleep 0.2; done; return 1; }

# 1. The first install compiles, and the receipt says where.
FIRST=$(commit first)
run_builder
expect test "$(builds)" = 1
expect test "$(receipt compiled_at)" = "$FIRST"
expect wait_live "$FIRST"

# 2. A docs commit: no compile, the receipt names it, the server serves it.
DOCS=$(printf 'more notes\n' >> "$REPO/docs/notes.md"; commit docs)
run_builder
expect test "$(builds)" = 1
expect grep -q "ACTIVATION RESTAMPED.*verdict=activation_restamped" "$LOG"
expect test "$(receipt sha)" = "$DOCS"
expect test "$(receipt compiled_at)" = "$FIRST"
expect test "$(live)" = "$DOCS"
expect grep -q "sha=$DOCS .*restamped=1 verdict=activation_healthy" "$LOG"

# 3. A compiled-in script changed: crates/ did not, and it must still compile.
BAKED=$(printf 'echo baked v2\n' > "$REPO/scripts/baked.sh"; commit baked)
run_builder
expect grep -q "sha=$BAKED: compile inputs changed since $FIRST (scripts/baked.sh) verdict=restamp_inputs_changed" "$LOG"
expect test "$(builds)" = 2
expect test "$(receipt compiled_at)" = "$BAKED"
expect wait_live "$BAKED"

# 4. A server that does not read receipts cannot adopt one: compile.
printf 'old\n' > "$MODE"; sleep 0.5
OLD=$(printf 'old server\n' >> "$REPO/docs/notes.md"; commit old-server)
run_builder
expect grep -q "sha=$OLD: the running server does not read receipts" "$LOG"
expect test "$(builds)" = 3
printf 'adopt\n' > "$MODE"; expect wait_live "$OLD"; expect reads_receipts

# 5. A restamp nobody adopts is undone, and the same run compiles it.
printf 'ignore\n' > "$MODE"
IGN=$(printf 'ignored\n' >> "$REPO/docs/notes.md"; commit ignored)
run_builder
expect grep -q "ACTIVATION RESTAMP UNADOPTED sha=$IGN: not live after 3s; the receipt is restored; compiling now" "$LOG"
expect test "$(builds)" = 4
expect test "$(tail -1 "$TRACE")" = "build $IGN"
expect test "$(cat "${STAMP}.restamp_failed")" = "$IGN"
printf 'adopt\n' > "$MODE"; expect wait_live "$IGN"; expect reads_receipts
# ...and a commit whose restamp already failed goes straight to a compile.
AGAIN=$(printf 'again\n' >> "$REPO/docs/notes.md"; commit again)
printf '%s\n' "$AGAIN" > "${STAMP}.restamp_failed"
run_builder
expect grep -q "sha=$AGAIN: an earlier restamp of it was not adopted; compiling verdict=restamp_retry_compiles" "$LOG"
expect test "$(builds)" = 5
expect wait_live "$AGAIN"

# 6. The switch.
OFF=$(printf 'off\n' >> "$REPO/docs/notes.md"; commit off)
run_builder AMUX_RS_RESTAMP=0
expect grep -q "RESTAMP OFF sha=$OFF" "$LOG"
expect test "$(builds)" = 6

echo "test-build-restamp: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
