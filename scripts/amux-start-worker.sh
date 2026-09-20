#!/bin/bash
# Start every registered amux worker lane after the server is ready.
#
# AMUX-49 (2026-08-31): this used to hardcode the `amux` lane only, so a
# container reboot silently dropped every other lane (confirmed live: of 8
# registered lanes, only `amux` + `frontstage` had a running tmux session —
# `frontstage` only because someone happened to restart it by hand). This
# now iterates every lane amux itself knows about (~/.amux/sessions/*.env,
# the same source `amux start-all` reads) instead of one hardcoded name.
#
# Per-lane failures are isolated and logged, not fatal to the loop: a lane
# with its own pre-existing problem (e.g. a stale platform-specific config)
# must not take the rest of the fleet down with it — that exact shape bit
# `amux start-all` itself before INIT-2 fixed a `set -e` abort-on-first-
# failure bug in the CLI. This script doesn't depend on that CLI's own
# internal error handling; it drives the HTTP API directly per lane so a
# single failure can never truncate the loop.
# Derived, never a hardcoded user: a copy of this script on a host whose user
# was not `syseng` pointed at that user's nonexistent home, so the lane glob was
# empty and boot recovery exited 1 without being able to write this log.
AMUX_HOME="${AMUX_HOME:-${CC_HOME:-$HOME/.amux}}"
LOG_FILE="$AMUX_HOME/worker-start.log"
# This install's API base: the server's own endpoint file, else AMUX_RS_PORT,
# else the default. Hardcoding 8824 meant an install on any other port waited
# out the boot timeout and silently started nobody.
AMUX_API="${AMUX_URL:-}"
if [[ -z "$AMUX_API" && -f "$AMUX_HOME/endpoint.json" ]]; then
  AMUX_API="$(sed -n 's/.*"canonical_url":"\([^"]*\)".*/\1/p' "$AMUX_HOME/endpoint.json" | head -1)"
fi
AMUX_API="${AMUX_API:-https://localhost:${AMUX_RS_PORT:-8824}}"
AMUX_API="${AMUX_API%/}"
SESSIONS_DIR="$AMUX_HOME/sessions"

# Lanes deliberately NOT auto-started at boot. The default stays "start every
# lane amux knows about" (AMUX-49) — a lane is never silently dropped because
# nobody listed it — with two narrow exceptions:
#   - SKIP_LANES: on-demand lanes, space-separated (AMUX_BOOT_SKIP_LANES)
#   - archived or paused lanes: start_session refuses them, so trying would
#     only count a deliberate park as a failure.
SKIP_LANES="${AMUX_BOOT_SKIP_LANES-}"

echo "$(date): Starting worker startup script" >> "$LOG_FILE"

# Maximum retries and exponential backoff parameters
MAX_RETRIES=5
INITIAL_WAIT=3
MAX_WAIT=15

# Wait for server to be ready with exponential backoff
retry_count=0
wait_time=$INITIAL_WAIT

while true; do
    echo "$(date): Waiting ${wait_time}s before attempting to start workers (attempt $((retry_count + 1))/$MAX_RETRIES)" >> "$LOG_FILE"
    sleep "$wait_time"

    # Try to reach the health endpoint
    if curl -sk --connect-timeout 2 --max-time 5 "$AMUX_API/health" > /dev/null 2>&1; then
        echo "$(date): Server is responding, attempting to start workers..." >> "$LOG_FILE"
        break
    fi

    retry_count=$((retry_count + 1))
    if [ $retry_count -ge $MAX_RETRIES ]; then
        echo "$(date): ERROR: Server not responding after $MAX_RETRIES attempts. Worker startup failed." >> "$LOG_FILE"
        exit 1
    fi

    # Exponential backoff: increase wait time, capped at MAX_WAIT
    wait_time=$((wait_time * 2))
    if [ $wait_time -gt $MAX_WAIT ]; then
        wait_time=$MAX_WAIT
    fi
done

# Clean up the initialization session (no longer needed)
echo "$(date): Cleaning up amux-init session" >> "$LOG_FILE"
tmux kill-session -t amux-init 2>/dev/null || true

# Every lane amux knows about, in the same directory `amux start-all` reads.
# Glob a var so an empty dir doesn't loop once over a literal "*.env".
shopt -s nullglob
lane_files=("$SESSIONS_DIR"/*.env)
shopt -u nullglob

if [ ${#lane_files[@]} -eq 0 ]; then
    echo "$(date): ERROR: no lane files found in $SESSIONS_DIR — nothing to start" >> "$LOG_FILE"
    exit 1
fi

started=0
failed=0
failed_names=()

# AMUX-120: "ok":true here only means the API accepted the launch request
# and sent the keystrokes — it was being logged and counted as
# "successful" even when the pane's shell never actually got a live claude
# child (confirmed live 2026-09-04: a transient PATH/.bashrc problem at
# boot left 5 of 7 lanes with a dead "claude: command not found" shell,
# while this script's own log read "7 started, 0 failed" the whole time).
# A status code has no operand (ethos rule 4) — verify the thing the log
# claims, don't just trust the accept response. Poll the session's real
# `running` state (the same is_running() probe the invariant trusts) with
# a short retry window before counting a lane as started.
verify_running() {
    local name="$1" tries=0
    while [ $tries -lt 5 ]; do
        sleep 2
        if curl -sk --connect-timeout 3 --max-time 5 "$AMUX_API/api/sessions/$name" 2>/dev/null \
          | grep -q '"running":true'; then
            return 0
        fi
        tries=$((tries + 1))
    done
    return 1
}

skipped=0
skipped_names=()

for f in "${lane_files[@]}"; do
    name=$(basename "$f" .env)
    # Deliberate skips are neither starts nor failures.
    if grep -q '^CC_ARCHIVED="\?1' "$f"; then
        echo "$(date): [$name] SKIPPED — archived; wake it by hand with: amux wake $name" >> "$LOG_FILE"
    elif grep -q '^CC_PAUSED="\?1' "$f"; then
        echo "$(date): [$name] SKIPPED — paused; resume it by hand before starting" >> "$LOG_FILE"
    elif [[ " $SKIP_LANES " == *" $name "* ]]; then
        echo "$(date): [$name] SKIPPED — on-demand; start it by hand with: amux start $name" >> "$LOG_FILE"
    else
        echo "$(date): Calling amux API to start worker: $name..." >> "$LOG_FILE"
        RESPONSE=$(curl -sk --connect-timeout 5 --max-time 10 -X POST "$AMUX_API/api/sessions/$name/start" \
          -H "Content-Type: application/json" \
          -d '{"backend": "tmux"}' 2>&1)
        CURL_RC=$?
        echo "$(date): [$name] curl exit=$CURL_RC response=$RESPONSE" >> "$LOG_FILE"

        if [ $CURL_RC -eq 0 ] && echo "$RESPONSE" | grep -q '"ok":true'; then
            if verify_running "$name"; then
                echo "$(date): [$name] worker startup successful (verified running)" >> "$LOG_FILE"
                started=$((started + 1))
            else
                echo "$(date): [$name] WARN worker startup ACCEPTED but pane never came up running (checked for ~10s) — likely a dead shell (bad PATH, missing binary, .bashrc error); treating as failed" >> "$LOG_FILE"
                failed=$((failed + 1))
                failed_names+=("$name")
            fi
        else
            echo "$(date): [$name] worker startup FAILED" >> "$LOG_FILE"
            failed=$((failed + 1))
            failed_names+=("$name")
        fi
        continue
    fi
        skipped=$((skipped + 1))
        skipped_names+=("$name")
done

echo "$(date): worker startup summary: $started started, $failed failed (${failed_names[*]:-none}), $skipped skipped (${skipped_names[*]:-none})" >> "$LOG_FILE"

# Partial success is still success for this unit: a lane with its own
# pre-existing, unrelated problem (the historical shape) must not read as
# "the whole boot-recovery mechanism is broken" the way a hard exit 1 would
# under systemd's oneshot status. Only a total loss (every lane failed, or
# the loop never ran) is a real failure of THIS script's own job — judged on
# the lanes actually ATTEMPTED, so all-skipped is not a failure.
if [ $started -eq 0 ] && [ $failed -gt 0 ]; then
    echo "$(date): ERROR: every lane that was attempted failed to start" >> "$LOG_FILE"
    exit 1
fi
exit 0
