#!/usr/bin/env bash
# tmux-watchdog: detect tmux server death AND dead worker processes, then recover.
#
# Two failure modes:
#
# 1. tmux SERVER death: macOS can kill tmux under memory pressure because it is
#    "not RunningBoard jetsam managed" (confirmed in system logs 2026-09-25).
#    All workers die instantly. Recovery: restart tmux server, then start-all.
#
# 2. Individual worker death: a Claude process inside a tmux session exits (e.g.
#    account usage limit reached, crash, auth error) while the tmux session
#    stays alive as a bare shell. Nothing restarts it because start-all sees
#    the tmux session and says "already running". Recovery: detect sessions
#    whose Claude process has exited and restart them.
#    (2026-10-17 RCA: trymixpeek.com hit 96% 5h usage, workers rate-limited
#    and exited one by one over ~1 hour. tmux server stayed alive. No recovery
#    fired. 30+ workers stayed stopped until manual intervention.)
#
# Mode 2 only judges Claude workers: a codex or gemini pane never has a claude
# process, so it read as dead and was killed and restarted on every scan.
# Isolated workers are left alone (an owner who exits Claude in that pane meant
# it), and a worker restarted WORKER_RESTART_MAX times inside
# WORKER_RESTART_WINDOW is held for a human instead of being killed every minute.
#
# This script is called by com.amux.tmux-watchdog.plist on a 30s interval.
# It is idempotent. Test: scripts/test-tmux-watchdog.sh.

set -uo pipefail

AMUX_DIR="${AMUX_WATCHDOG_HOME:-${HOME}/.amux}"
LOG="${AMUX_DIR}/logs/tmux-watchdog.log"
AMUX_BIN="${AMUX_BIN:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
STAGGER="${AMUX_START_ALL_STAGGER:-2}"
COOLDOWN_FILE="${AMUX_DIR}/tmux-watchdog-last-recovery"
COOLDOWN_SECONDS=120
WORKER_CHECK_COOLDOWN_FILE="${AMUX_DIR}/tmux-watchdog-last-worker-check"
WORKER_CHECK_COOLDOWN_SECONDS="${AMUX_WATCHDOG_WORKER_COOLDOWN:-60}"
CC_SESSIONS="${AMUX_DIR}/sessions"
RESTARTS_DIR="${AMUX_DIR}/tmux-watchdog-restarts"
WORKER_RESTART_MAX="${AMUX_WATCHDOG_RESTART_MAX:-3}"
WORKER_RESTART_WINDOW="${AMUX_WATCHDOG_RESTART_WINDOW:-1800}"

mkdir -p "$(dirname "$LOG")"

log() { printf '%s %s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" "$*" >> "$LOG"; }

# ---- Mode 1: tmux server is dead ----
if ! tmux list-sessions &>/dev/null; then
    if [[ -f "$COOLDOWN_FILE" ]]; then
        last_recovery=$(cat "$COOLDOWN_FILE" 2>/dev/null || echo 0)
        now=$(date +%s)
        elapsed=$(( now - last_recovery ))
        if (( elapsed < COOLDOWN_SECONDS )); then
            log "tmux still down, but last recovery was ${elapsed}s ago (cooldown ${COOLDOWN_SECONDS}s) -- skipping"
            exit 1
        fi
    fi

    log "=== tmux server is DOWN -- starting recovery ==="
    log "memory_pressure: $(memory_pressure 2>/dev/null | grep 'free percentage' || echo 'unavailable')"
    log "swap: $(sysctl -n vm.swapusage 2>/dev/null || echo 'unavailable')"

    tmux new-session -d -s keepalive 'exec bash' 2>/dev/null
    sleep 1

    if ! tmux list-sessions &>/dev/null; then
        log "FAILED to restart tmux server -- cannot recover"
        exit 1
    fi

    session_count=$(tmux list-sessions 2>/dev/null | wc -l | tr -d ' ')
    log "tmux server restored (${session_count} sessions). Starting fleet..."

    date +%s > "$COOLDOWN_FILE"

    base="$("$AMUX_BIN" url 2>/dev/null || echo "https://localhost:8824")"
    curl -sk --max-time 5 -X POST -H 'Content-Type: application/json' \
        -d '{"level":"warn","message":"tmux-watchdog: tmux server was dead, recovered and restarting fleet","source":"tmux-watchdog"}' \
        "$base/api/log" >/dev/null 2>&1 || true

    export AMUX_START_ALL_STAGGER="$STAGGER"
    summary=$("$AMUX_BIN" start-all 2>&1 | sed 's/\x1b\[[0-9;]*m//g')
    rc=$?

    printf '%s\n' "$summary" >> "$LOG"
    log "start-all exited rc=$rc"

    if curl -sk --max-time 5 "$base/health" 2>/dev/null | grep -q '"status":"ok"'; then
        running=$(curl -sk --max-time 10 "$base/api/sessions" 2>/dev/null \
            | python3 -c "import json,sys; ss=json.load(sys.stdin); print(len([s for s in ss if not s.get('archived') and s.get('running')]))" 2>/dev/null || echo '?')
        total=$(curl -sk --max-time 10 "$base/api/sessions" 2>/dev/null \
            | python3 -c "import json,sys; ss=json.load(sys.stdin); print(len([s for s in ss if not s.get('archived')]))" 2>/dev/null || echo '?')
        log "verdict: ${running}/${total} non-archived workers running after recovery"
    fi

    log "=== tmux recovery complete ==="
    exit "$rc"
fi

# ---- Mode 2: tmux is alive, check for dead worker processes ----

# Cooldown: don't scan every 30s, once per minute is enough.
if [[ -f "$WORKER_CHECK_COOLDOWN_FILE" ]]; then
    last_check=$(cat "$WORKER_CHECK_COOLDOWN_FILE" 2>/dev/null || echo 0)
    now=$(date +%s)
    elapsed=$(( now - last_check ))
    if (( elapsed < WORKER_CHECK_COOLDOWN_SECONDS )); then
        exit 0
    fi
fi
date +%s > "$WORKER_CHECK_COOLDOWN_FILE"

# For each registered, non-archived, non-paused worker: check if its tmux
# session exists AND has a live Claude process. If the session exists but
# Claude is dead (pane is just a shell), kill the session and restart the worker.
dead_workers=()
skipped_provider=0
skipped_isolated=0
shopt -s nullglob
env_files=("$CC_SESSIONS"/*.env)
shopt -u nullglob

for f in ${env_files[@]+"${env_files[@]}"}; do
    [[ -f "$f" ]] || continue
    name=$(basename "$f" .env)

    # Skip archived/paused workers
    if grep -q '^CC_ARCHIVED="\?1' "$f" 2>/dev/null; then continue; fi
    if grep -q '^CC_PAUSED="\?1' "$f" 2>/dev/null; then continue; fi
    # Only a Claude pane has a claude process to look for.
    provider=$(sed -n 's/^CC_PROVIDER="\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' "$f" | tail -1)
    if [[ -n "$provider" && "$provider" != "claude" ]]; then
        skipped_provider=$((skipped_provider + 1)); continue
    fi
    if grep -q '^CC_ISOLATED="\?1' "$f" 2>/dev/null; then
        skipped_isolated=$((skipped_isolated + 1)); continue
    fi

    tmux_name="amux-${name}"

    # If tmux session doesn't exist, start-all will handle it. We only care
    # about sessions that exist but have a dead Claude process.
    if ! tmux has-session -t "=${tmux_name}" 2>/dev/null; then
        continue
    fi

    # Get the pane's shell PID
    pane_pid=$(tmux list-panes -t "=${tmux_name}" -F '#{pane_pid}' 2>/dev/null | head -1)
    [[ -n "$pane_pid" ]] || continue

    # Check if any child of the pane shell is a claude process.
    # pgrep -P gets children of the shell. We look for 'claude' in the
    # process tree. If there are no children at all, the Claude process
    # has exited and the pane is just sitting at a bash prompt.
    has_claude=0
    while IFS= read -r child_pid; do
        [[ -n "$child_pid" ]] || continue
        child_cmd=$(ps -p "$child_pid" -o command= 2>/dev/null || true)
        if [[ "$child_cmd" == *claude* ]]; then
            has_claude=1
            break
        fi
        # Also check grandchildren (claude may be wrapped in /bin/sh -c)
        while IFS= read -r grandchild_pid; do
            [[ -n "$grandchild_pid" ]] || continue
            gc_cmd=$(ps -p "$grandchild_pid" -o command= 2>/dev/null || true)
            if [[ "$gc_cmd" == *claude* ]]; then
                has_claude=1
                break 2
            fi
        done < <(pgrep -P "$child_pid" 2>/dev/null || true)
    done < <(pgrep -P "$pane_pid" 2>/dev/null || true)

    if (( has_claude == 0 )); then
        dead_workers+=("$name")
    fi
done

if (( ${#dead_workers[@]} == 0 )); then
    exit 0
fi

log "=== dead worker processes detected: ${dead_workers[*]} (not judged: ${skipped_provider} non-claude, ${skipped_isolated} isolated) ==="
log "memory_pressure: $(memory_pressure 2>/dev/null | grep 'free percentage' || echo 'unavailable')"

base="$("$AMUX_BIN" url 2>/dev/null || echo "https://localhost:8824")"
curl -sk --max-time 5 -X POST -H 'Content-Type: application/json' \
    -d "{\"level\":\"warn\",\"message\":\"tmux-watchdog: ${#dead_workers[@]} worker(s) have dead Claude processes, restarting: ${dead_workers[*]}\",\"source\":\"tmux-watchdog\"}" \
    "$base/api/log" >/dev/null 2>&1 || true

restarted=0
failed=0
held=0
mkdir -p "$RESTARTS_DIR"
now=$(date +%s)
for name in "${dead_workers[@]}"; do
    tmux_name="amux-${name}"
    # Backoff: a worker that dies again right after each restart (usage limit,
    # bad auth) is held for a human rather than restarted every minute.
    rfile="${RESTARTS_DIR}/${name}"
    recent=$(awk -v now="$now" -v w="$WORKER_RESTART_WINDOW" 'now - $1 < w' "$rfile" 2>/dev/null | wc -l | tr -d ' ')
    if (( recent >= WORKER_RESTART_MAX )); then
        held=$((held + 1))
        if [[ ! -f "${rfile}.held" ]]; then
            touch "${rfile}.held"
            log "HOLD $name: ${recent} restarts in ${WORKER_RESTART_WINDOW}s, not restarting again (verdict=worker_restart_held)"
            curl -sk --max-time 5 -X POST -H 'Content-Type: application/json' \
                -d "{\"level\":\"warn\",\"message\":\"tmux-watchdog: $name died after each of ${recent} restarts; held for a human\",\"source\":\"tmux-watchdog\"}" \
                "$("$AMUX_BIN" url 2>/dev/null || echo "https://localhost:8824")/api/log" >/dev/null 2>&1 || true
        fi
        continue
    fi
    rm -f "${rfile}.held"
    awk -v now="$now" -v w="$WORKER_RESTART_WINDOW" 'now - $1 < w' "$rfile" 2>/dev/null > "${rfile}.tmp"
    echo "$now" >> "${rfile}.tmp"
    mv "${rfile}.tmp" "$rfile"
    # Kill the dead tmux session so start can create a fresh one
    tmux kill-session -t "=${tmux_name}" 2>/dev/null || true
    sleep 1

    if "$AMUX_BIN" start "$name" --detach 2>>"$LOG"; then
        log "restarted $name"
        restarted=$((restarted + 1))
    else
        log "FAILED to restart $name"
        failed=$((failed + 1))
    fi

    # Stagger restarts
    local_stagger="${AMUX_START_ALL_STAGGER:-2}"
    [[ "$local_stagger" != "0" ]] && sleep "$local_stagger"
done

log "=== worker recovery complete: ${restarted} restarted, ${failed} failed, ${held} held ==="
exit 0
