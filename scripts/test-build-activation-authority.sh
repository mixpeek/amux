#!/usr/bin/env bash
# ATE-93 — a builder stamp is not activation authority. This runs the shipped
# builder against two committed checkouts sharing one install/stamp/lock: an
# elected origin/main and a stale local branch. It proves both halves of the
# takeover incident:
#
#   1. a foreign checkout cannot install over the elected image; and
#   2. if an old process did replace it, a matching local stamp does not make
#      the elected builder exit early — /api/health forces re-adoption.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BUILDER="$ROOT/scripts/rust-auto-build.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
AUTH="$TMP/authority"
FOREIGN="$TMP/foreign"
FAKE_HOME="$TMP/home"
INSTALL="$TMP/bin/amux-server-rs"
STAMP="$TMP/stamp"
LOCK="$TMP/lock"
LOG="$TMP/build.log"
HEALTH="$TMP/health.json"
TRACE="$TMP/build.trace"
PASS=0; FAIL=0
export TRACE

ok() { PASS=$((PASS + 1)); }
bad() { FAIL=$((FAIL + 1)); echo "FAIL: $1"; }
expect() { if "$@"; then ok; else bad "$*"; fi; }

git init -q -b main "$AUTH"
mkdir -p "$AUTH/crates" "$AUTH/scripts" "$FAKE_HOME/.amux/rust-build-target"
mkdir -p "$AUTH/scripts/claude-hooks" "$FAKE_HOME/.amux/hooks"
for half in pre post; do
  printf '# elected hook\n' > "$AUTH/scripts/claude-hooks/observed-edits-${half}.py"
  printf '# stale hook\n' > "$FAKE_HOME/.amux/hooks/observed-edits-${half}.py"
done
printf '[workspace]\n' > "$AUTH/Cargo.toml"
printf 'elected source\n' > "$AUTH/crates/input.rs"
cat > "$AUTH/scripts/safe-cargo.sh" <<'EOF'
#!/bin/sh
set -eu
sha=$(git rev-parse HEAD)
printf 'build %s\n' "$sha" >> "$TRACE"
# Output dir the way cargo picks it: --profile NAME, else --release.
out=release; prev=
for a in "$@"; do [ "$prev" = --profile ] && out="$a"; prev="$a"; done
mkdir -p "$CARGO_TARGET_DIR/$out"
printf '#!/bin/sh\necho %s\n' "$sha" > "$CARGO_TARGET_DIR/$out/amux-server"
chmod 0755 "$CARGO_TARGET_DIR/$out/amux-server"
EOF
chmod +x "$AUTH/scripts/safe-cargo.sh"
(
  cd "$AUTH"
  git add -A
  git -c user.name=test -c user.email=test@example.com commit -qm elected
)
ELECTED=$(git -C "$AUTH" rev-parse HEAD)
# The production default is origin/main. A synthetic repository has no remote,
# so give it the exact remote-tracking ref rather than weakening the real gate.
git -C "$AUTH" update-ref refs/remotes/origin/main "$ELECTED"
git clone -q "$AUTH" "$FOREIGN"
(
  cd "$FOREIGN"
  git checkout -qb stale/local
  printf 'foreign source\n' > crates/stale.rs
  git add -A
  git -c user.name=test -c user.email=test@example.com commit -qm stale
)
STALE=$(git -C "$FOREIGN" rev-parse HEAD)
printf '{"commit":"%s"}\n' "$STALE" > "$HEALTH"

# The adoption cooldown (default 120s) would turn every build after the first
# into a COOLDOWN skip, since these cases run seconds apart. It is off here so
# each case reaches the path it names.
run_builder() {
  HOME="$FAKE_HOME" \
  AMUX_REPO="$1" \
  AMUX_RS_INSTALL="$INSTALL" \
  AMUX_RS_BUILD_STAMP="$STAMP" \
  AMUX_RS_BUILD_LOCK="$LOCK" \
  AMUX_RS_BUILD_LOG="$LOG" \
  AMUX_RS_BUILD_PROVENANCE="$TMP/provenance.json" \
  AMUX_RS_HEALTH_URL="file://$HEALTH" \
  AMUX_BUILD_MIN_FREE_GB=0 \
  AMUX_BUILD_DEBUG_CLEAR_ABOVE_GB=999999 \
  AMUX_RS_BUILD_COOLDOWN=0 \
    bash "$BUILDER" >/dev/null
}

# Establish a real elected install and its stamp while /health reports a
# different image. No shortcut is possible on this first run because no stamp
# exists yet.
run_builder "$AUTH"
expect test -x "$INSTALL"
expect grep -q "$ELECTED" "$INSTALL"
expect test "$(cat "$STAMP")" = "$ELECTED"
expect test "$(grep -c '^build ' "$TRACE")" = 1
for half in pre post; do
  expect cmp -s "$AUTH/scripts/claude-hooks/observed-edits-${half}.py" "$FAKE_HOME/.amux/hooks/observed-edits-${half}.py"
done
expect grep -q 'verdict=observed_hook_synced' "$LOG"

# Chaos: a separately-running stale checkout uses the same normal activation
# path and shared install locations. It must be refused before cargo/install;
# keeping the return status zero makes the timer quiet but the builder log is
# the durable sweep signal.
printf '# foreign hook\n' > "$FOREIGN/scripts/claude-hooks/observed-edits-post.py"
run_builder "$FOREIGN"
expect cmp -s "$AUTH/scripts/claude-hooks/observed-edits-post.py" "$FAKE_HOME/.amux/hooks/observed-edits-post.py"
expect grep -q "$ELECTED" "$INSTALL"
expect test "$(grep -c '^build ' "$TRACE")" = 1
expect grep -q "ACTIVATION AUTHORITY REFUSED $STALE" "$LOG"

# A delayed adoption needs no second build while the elected bytes remain installed.
run_builder "$AUTH"
expect test "$(grep -c '^build ' "$TRACE")" = 1
expect grep -q 'ACTIVATION AWAITING ADOPTION' "$LOG"
# Simulate the actual foreign overwrite; the old receipt must not hide it.
printf '#!/bin/sh\necho foreign\n' > "$INSTALL"

# The exact live incident: the elected stamp still names ELECTED, but the
# process answering health is STALE. The elected builder must rebuild instead
# of considering its local stamp proof that its binary is still live.
run_builder "$AUTH"
expect test "$(grep -c '^build ' "$TRACE")" = 2
expect grep -q "ACTIVATION STAMP DRIFT $ELECTED" "$LOG"
expect grep -q "$ELECTED" "$INSTALL"

# AMUX-4225: full and abbreviated identities mean the same image. A failed
# measurement is NOT a measured mismatch. In the incident the first two curls
# timed out and a third matched, but only that third answer reached STAMP DRIFT.
for identity in "$ELECTED" "${ELECTED:0:12}"; do
  printf '{"commit":"%s","build":"fixture","pid":123}\n' "$identity" > "$HEALTH"
  run_builder "$AUTH"
  expect test "$(grep -c '^build ' "$TRACE")" = 2
done
mkdir -p "$FAKE_HOME/.cargo/bin"
export CURL_TRACE="$TMP/curl.trace" HEALTH
cat > "$FAKE_HOME/.cargo/bin/curl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$CURL_TRACE"
n=$(wc -l < "$CURL_TRACE" | tr -d ' ')
if [ "$n" -le 2 ]; then exit 28; fi
cat "$HEALTH"
EOF
chmod +x "$FAKE_HOME/.cargo/bin/curl"
run_builder "$AUTH"
expect test "$(grep -c '^build ' "$TRACE")" = 2
expect test "$(wc -l < "$CURL_TRACE" | tr -d ' ')" = 1
expect grep -q 'ACTIVATION IDENTITY UNMEASURED.*curl_exit=28.*action=defer' "$LOG"
rm -f "$FAKE_HOME/.cargo/bin/curl" "$CURL_TRACE"
# Missing/invalid identities must not become prefix matches or rebuild orders,
# and a dirty build of the ELECTED commit is left alone.
for identity in '' a unknown "${ELECTED:0:12}-dirty"; do
  printf '{"commit":"%s"}\n' "$identity" > "$HEALTH"
  run_builder "$AUTH"
  expect test "$(grep -c '^build ' "$TRACE")" = 2
done
expect grep -q "ACTIVATION IDENTITY DIRTY expected=$ELECTED live=${ELECTED:0:12}-dirty same_commit=true" "$LOG"
# 2026-10-02: a dirty build of a DIFFERENT commit is a measured foreign image.
# It used to read as unmeasured and pinned live 176 commits behind for an hour.
# Here the elected bytes are already installed, so the measured answer is
# "awaiting adoption" with no rebuild. It used to be "unmeasured, defer".
printf '{"commit":"%s-dirty"}\n' "${STALE:0:12}" > "$HEALTH"
run_builder "$AUTH"
expect test "$(grep -c '^build ' "$TRACE")" = 2
expect grep -q "ACTIVATION AWAITING ADOPTION expected=$ELECTED live=${STALE:0:12}-dirty" "$LOG"

# An elected image may already be running while its hook is stale. Refresh the
# hook without rebuilding that image or taking an uncommitted edit's bytes.
printf '# new elected hook\n' > "$AUTH/scripts/claude-hooks/observed-edits-post.py"
git -C "$AUTH" add scripts/claude-hooks/observed-edits-post.py
git -C "$AUTH" -c user.name=test -c user.email=test@example.com commit -qm hook-only
HOOK_SHA=$(git -C "$AUTH" rev-parse HEAD)
git -C "$AUTH" update-ref refs/remotes/origin/main "$HOOK_SHA"
printf '{"commit":"%s"}\n' "$HOOK_SHA" > "$HEALTH"
printf '# uncommitted hook\n' > "$AUTH/scripts/claude-hooks/observed-edits-post.py"
run_builder "$AUTH"
expect grep -q '^# new elected hook$' "$FAKE_HOME/.amux/hooks/observed-edits-post.py"
expect test "$(grep -c '^build ' "$TRACE")" = 2
expect grep -q "OBSERVED HOOK SYNCED sha=$HOOK_SHA half=post" "$LOG"
git -C "$AUTH" restore scripts/claude-hooks/observed-edits-post.py

# Worker-attributed diagnostic runs must remain offline, while a real install
# of that same commit must still fail closed without a measured overlap permit.
printf 'worker source\n' >> "$AUTH/crates/input.rs"
git -C "$AUTH" add crates/input.rs
git -C "$AUTH" -c user.name=test -c user.email=test@example.com commit \
  -qm $'worker revision\n\nAmux-Session: fixture-worker'
WORKER=$(git -C "$AUTH" rev-parse HEAD)
git -C "$AUTH" update-ref refs/remotes/origin/main "$WORKER"
mkdir -p "$FAKE_HOME/.cargo/bin"
export CURL_TRACE="$TMP/curl.trace"
cat > "$FAKE_HOME/.cargo/bin/curl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$CURL_TRACE"
exit 7
EOF
chmod +x "$FAKE_HOME/.cargo/bin/curl"
for mode in AMUX_RS_DISK_CLEAR_ONLY AMUX_RS_BUILD_PROVENANCE_ONLY; do
  rm -f "$CURL_TRACE" "$TMP/provenance.json"
  export "$mode=1"
  run_builder "$AUTH"
  unset "$mode"
  expect test ! -e "$CURL_TRACE"
  if [ "$mode" = AMUX_RS_BUILD_PROVENANCE_ONLY ]; then
    expect test -s "$TMP/provenance.json"
  else
    expect test ! -e "$TMP/provenance.json"
    expect grep -q "building $WORKER" "$LOG"
  fi
  expect grep -q "OVERLAP GUARD NOT APPLICABLE $WORKER" "$LOG"
  expect test "$(grep -c '^build ' "$TRACE")" = 2
  expect grep -q "$ELECTED" "$INSTALL"
  expect test "$(cat "$STAMP")" = "$ELECTED"
done
run_builder "$AUTH"
expect grep -q 'overlap/deployment-permit?session=fixture-worker' "$CURL_TRACE"
expect grep -q "OVERLAP GUARD UNMEASURED $WORKER" "$LOG"
expect test "$(grep -c '^build ' "$TRACE")" = 2
expect grep -q "$ELECTED" "$INSTALL"
expect test "$(cat "$STAMP")" = "$ELECTED"

# A retired duplicate may leave endpoint.json naming its dead process. The
# builder must resolve the configured server without editing that receipt.
printf '{"canonical_url":"https://localhost:8823","pid":2147483647}\n' > "$FAKE_HOME/.amux/endpoint.json"
run_builder "$AUTH"
expect grep -q 'https://localhost:8824/api/board/overlap/deployment-permit' "$CURL_TRACE"
expect grep -q 'ENDPOINT OWNER EXITED.*action=use_configured_server' "$LOG"
# A live owner remains authoritative, including a nondefault port.
printf '{"canonical_url":"https://localhost:18824","pid":%s}\n' "$$" > "$FAKE_HOME/.amux/endpoint.json"
run_builder "$AUTH"
expect grep -q 'https://localhost:18824/api/board/overlap/deployment-permit' "$CURL_TRACE"

echo "test-build-activation-authority: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
