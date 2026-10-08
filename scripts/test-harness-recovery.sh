#!/usr/bin/env bash
# Link the tested executable privately: another checkout can rewrite the shared
# Cargo binary while a suite is running. Dependency artifacts still share the
# sanctioned target. All server state and tmux sockets are disposable fixtures.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ARTIFACT_DIR="${AMUX_RECOVERY_ARTIFACT_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/amux-recovery.XXXXXX")}"
mkdir -p "$ARTIFACT_DIR"
ARTIFACT_DIR="$(cd "$ARTIFACT_DIR" && pwd)"
BINARY="$ARTIFACT_DIR/amux-server"
scripts/safe-cargo.sh rustc -p amux-server --bin amux-server -- "--emit=link=$BINARY" 2>&1 | tee "$ARTIFACT_DIR/build.log"
python3 - "$BINARY" > "$ARTIFACT_DIR/binary.sha256" <<'PY'
import hashlib, pathlib, sys
p=pathlib.Path(sys.argv[1]); print(hashlib.sha256(p.read_bytes()).hexdigest(), p)
PY
AMUX_RESTART_BIN="$BINARY" scripts/test-contended.sh -p amux-server --test restart_persistence -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/restart.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/orchestration-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/orchestration.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/orchestration-pool-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/pool.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/review-capacity-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/review-capacity.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/api-error-background-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/api-error.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/auto-resume-handoff-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/auto-resume-handoff.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/conversation-recycle-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/conversation-recycle.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/maintenance-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/maintenance.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/native-launch.mjs 2>&1 | tee "$ARTIFACT_DIR/native-launch.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/observed-edits-identity-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/observed-edits.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/scheduler-dispatch-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/scheduler-dispatch.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/peer-receive-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/peer-receive.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/worker-group-boundary-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/worker-group-boundary.log"
AMUX_CHAOS_BINARY="$BINARY" node e2e/chaos/land-startup-recovery.mjs 2>&1 | tee "$ARTIFACT_DIR/land-startup.log"
python3 scripts/test-orch-pace-scope.py 2>&1 | tee "$ARTIFACT_DIR/scope.log"
bash scripts/test-orch-pace-proof.sh 2>&1 | tee "$ARTIFACT_DIR/pace.log"
python3 - "$BINARY" "$ARTIFACT_DIR/binary.sha256" <<'PY'
import hashlib, pathlib, sys
actual=hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest()
expected=pathlib.Path(sys.argv[2]).read_text().split()[0]
assert actual == expected, 'tested binary changed while the recovery suite ran'
print('recovery binary identity: unchanged', actual)
PY
printf 'Recovery verification passed; retained artifacts: %s\n' "$ARTIFACT_DIR"
