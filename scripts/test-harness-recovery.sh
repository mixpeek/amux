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
# AF-1056: the tmux control-mode client against a PRIVATE tmux server, under the
# shape that crashed the fleet's server on 2026-10-29 (sessions closing under the
# client, the client SIGKILLed mid-command). The server must survive every round,
# or the test names the crash. No TMUX: it must never see the fleet's server.
env -u TMUX scripts/test-contended.sh -p amux-server --test tmux_control_chaos -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/tmux-control-chaos.log"
# Each scenario owns its HOME, port and tmux socket (e2e/chaos/harness.mjs), so
# they run side by side. Serially they took about 7 of this job's 13 minutes.
# AMUX_CHAOS_JOBS caps concurrency (default: CPU count, at most 4).
CHAOS_SCENARIOS=(
  e2e/chaos/orchestration-recovery.mjs orchestration
  e2e/chaos/orchestration-pool-recovery.mjs pool
  e2e/chaos/review-capacity-recovery.mjs review-capacity
  e2e/chaos/api-error-background-recovery.mjs api-error
  e2e/chaos/auto-resume-handoff-recovery.mjs auto-resume-handoff
  e2e/chaos/conversation-recycle-recovery.mjs conversation-recycle
  e2e/chaos/maintenance-recovery.mjs maintenance
  e2e/chaos/native-launch.mjs native-launch
  e2e/chaos/observed-edits-identity-recovery.mjs observed-edits
  e2e/chaos/scheduler-dispatch-recovery.mjs scheduler-dispatch
  e2e/chaos/peer-receive-recovery.mjs peer-receive
  e2e/chaos/worker-group-boundary-recovery.mjs worker-group-boundary
  e2e/chaos/land-startup-recovery.mjs land-startup
  e2e/chaos/sessions-snapshot-restart.mjs sessions-snapshot
)
CHAOS_JOBS="${AMUX_CHAOS_JOBS:-$(n=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2); echo $(( n > 4 ? 4 : n )))}"
chaos_pids=(); chaos_names=(); chaos_failed=()
chaos_reap() {  # wait for the oldest running scenario and record its status
  local pid="${chaos_pids[0]}" name="${chaos_names[0]}"
  chaos_pids=("${chaos_pids[@]:1}"); chaos_names=("${chaos_names[@]:1}")
  if wait "$pid"; then echo "chaos PASS $name"; else echo "chaos FAIL $name (see $ARTIFACT_DIR/$name.log)"; chaos_failed+=("$name"); fi
}
for ((i = 0; i < ${#CHAOS_SCENARIOS[@]}; i += 2)); do
  script="${CHAOS_SCENARIOS[i]}" name="${CHAOS_SCENARIOS[i+1]}"
  (( ${#chaos_pids[@]} >= CHAOS_JOBS )) && chaos_reap
  AMUX_CHAOS_BINARY="$BINARY" node "$script" > "$ARTIFACT_DIR/$name.log" 2>&1 &
  chaos_pids+=("$!"); chaos_names+=("$name")
done
while (( ${#chaos_pids[@]} )); do chaos_reap; done
for ((i = 1; i < ${#CHAOS_SCENARIOS[@]}; i += 2)); do
  printf '===== %s =====\n' "${CHAOS_SCENARIOS[i]}"; cat "$ARTIFACT_DIR/${CHAOS_SCENARIOS[i]}.log"
done
if (( ${#chaos_failed[@]} )); then echo "chaos scenarios failed: ${chaos_failed[*]}" >&2; exit 1; fi
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
