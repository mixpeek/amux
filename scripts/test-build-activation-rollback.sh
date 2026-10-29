#!/usr/bin/env bash
# amux Cloud deploys main's tip without waiting for CI (AMUX_DEPLOY_CI_GATE=0),
# so a build that installs but does not come up healthy must not stay live:
# with AMUX_ACTIVATION_ROLLBACK on (the default since 2026-10-29, for the Mac too) the builder waits for /api/health to report
# the new commit, else restores the previous binary and its identity and holds
# that commit until a newer one (Ethan, 2026-10-27: every cloud instance gets
# main as it comes out).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BUILDER="$ROOT/scripts/rust-auto-build.sh"
TMP=$(mktemp -d)
HEALTH_PID=""
cleanup() {
  [ -n "$HEALTH_PID" ] && kill "$HEALTH_PID" 2>/dev/null || true
  if [ "$fails" != 0 ]; then echo "builder log: $TMP/build.log"; tail -15 "$TMP/build.log" 2>/dev/null; else rm -rf "$TMP"; fi
}
fails=0
trap cleanup EXIT
REPO="$TMP/repo"
FAKE_HOME="$TMP/home"
INSTALL="$TMP/bin/amux-server-rs"
fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }

git init -q -b main "$REPO"
mkdir -p "$REPO/crates" "$REPO/scripts" "$FAKE_HOME" "$(dirname "$INSTALL")"
printf '[workspace]\n' > "$REPO/Cargo.toml"
printf 'fixture\n' > "$REPO/crates/input.rs"
cat > "$REPO/scripts/safe-cargo.sh" <<'EOF'
#!/bin/sh
out=release; prev=
for a in "$@"; do [ "$prev" = --profile ] && out="$a"; prev="$a"; done
mkdir -p "$CARGO_TARGET_DIR/$out"
printf '%s\n' '#!/bin/sh' 'printf "%s\\n" new-build' > "$CARGO_TARGET_DIR/$out/amux-server"
chmod 0755 "$CARGO_TARGET_DIR/$out/amux-server"
EOF
chmod +x "$REPO/scripts/safe-cargo.sh"
(cd "$REPO" && git add -A && git -c user.name=test -c user.email=test@example.com commit -qm fixture)
SHA=$(git -C "$REPO" rev-parse HEAD)

# /api/health reports whatever commit is in $TMP/live.
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
python3 - "$PORT" "$TMP/live" <<'PY' &
import http.server, json, sys
port, live = int(sys.argv[1]), sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        c = open(live).read().strip()
        b = json.dumps({"status": "ok", "commit": c[:12], "commit_full": c}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json"); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
PY
HEALTH_PID=$!
for _ in $(seq 1 50); do curl -s "http://127.0.0.1:$PORT/" >/dev/null 2>&1 && break; sleep 0.1; done

# Never the real amux on this machine: its in-flight schedule runs would defer
# the activation (a test run on 2026-10-27 was held by a live SCHED-751).
# Rollback is on by default since 2026-10-29, so `run` leaves the switch unset
# and tests the default; case 4 passes AMUX_ACTIVATION_ROLLBACK=0.
run() {
  env -u AMUX_URL -u AMUX_ACTIVATION_ROLLBACK "$@" AMUX_RS_RUNS_URL="http://127.0.0.1:$PORT/runs" \
  HOME="$FAKE_HOME" AMUX_REPO="$REPO" AMUX_RS_INSTALL="$INSTALL" AMUX_RS_BUILD_STAMP="$TMP/stamp" \
  AMUX_RS_BUILD_LOG="$TMP/build.log" AMUX_RS_BUILD_LOCK="$TMP/build.lock" AMUX_RS_BUILD_PROVENANCE="$TMP/provenance.json" \
  AMUX_RS_ACTIVATION_REF=HEAD AMUX_BUILD_MIN_FREE_GB=0 AMUX_BUILD_DEBUG_MAX_GB=999999 \
  AMUX_RS_HEALTH_URL="http://127.0.0.1:$PORT/api/health" AMUX_ACTIVATION_HEALTH_S=6 AMUX_RS_BUILD_COOLDOWN=0 \
    bash "$BUILDER" >/dev/null 2>&1 || true
}

# 1. The new build never reports healthy: the old binary and identity come back.
printf '#!/bin/sh\nprintf old-build\\n\n' > "$INSTALL"; chmod 0755 "$INSTALL"
printf '{"build":"old"}\n' > "$INSTALL.identity.json"
echo "0000000000000000000000000000000000000000" > "$TMP/live"
run
grep -q old-build "$INSTALL" && ok "an unhealthy build is rolled back to the previous binary" || bad "the unhealthy build stayed live: $(cat "$INSTALL")"
grep -q '"old"' "$INSTALL.identity.json" && ok "its identity is restored with it" || bad "identity: $(cat "$INSTALL.identity.json")"
grep -q "verdict=activation_rolled_back" "$TMP/build.log" && ok "the rollback is logged" || bad "no rollback verdict"
[ ! -e "$INSTALL.rollback" ] && ok "no rollback copy is left behind" || bad "rollback copy left"
# 2. The same commit is held, not retried on the failure cooldown.
run
grep -q "verdict=activation_rollback_hold" "$TMP/build.log" && grep -q old-build "$INSTALL" \
  && ok "the rolled-back commit is held until a newer one" || bad "the rolled-back commit was retried"

# 3. A newer commit that does come up healthy stays live.
printf 'fixture 2\n' > "$REPO/crates/input.rs"
(cd "$REPO" && git -c user.name=test -c user.email=test@example.com commit -qam next)
SHA=$(git -C "$REPO" rev-parse HEAD)
echo "$SHA" > "$TMP/live"
run
grep -q new-build "$INSTALL" && ok "a healthy build stays live" || bad "a healthy build was rolled back"
grep -q "verdict=activation_healthy" "$TMP/build.log" && ok "its health is logged" || bad "no healthy verdict"
[ ! -e "$TMP/stamp.rolled_back" ] && ok "the hold clears once a build is healthy" || bad "hold left in place"

# 4. Opted out: an unhealthy build stays (what every Mac had before 2026-10-29).
printf 'fixture 3\n' > "$REPO/crates/input.rs"
(cd "$REPO" && git -c user.name=test -c user.email=test@example.com commit -qam third)
printf '#!/bin/sh\nprintf old-build\\n\n' > "$INSTALL"; chmod 0755 "$INSTALL"
echo "0000000000000000000000000000000000000000" > "$TMP/live"
run AMUX_ACTIVATION_ROLLBACK=0
grep -q new-build "$INSTALL" && ok "AMUX_ACTIVATION_ROLLBACK=0 keeps the new build without a health wait" || bad "opt-out still rolled back: $(cat "$INSTALL")"

echo "test-build-activation-rollback: $fails failed"
exit "$fails"
