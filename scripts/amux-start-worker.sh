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
LOG_FILE="/home/syseng/.amux/worker-start.log"
SESSIONS_DIR="/home/syseng/.amux/sessions"

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
    if /usr/bin/curl -sk --connect-timeout 2 --max-time 5 "https://localhost:8824/health" > /dev/null 2>&1; then
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
/usr/bin/tmux kill-session -t amux-init 2>/dev/null || true

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

for f in "${lane_files[@]}"; do
    name=$(basename "$f" .env)
    echo "$(date): Calling amux API to start worker: $name..." >> "$LOG_FILE"
    RESPONSE=$(/usr/bin/curl -sk --connect-timeout 5 --max-time 10 -X POST "https://localhost:8824/api/sessions/$name/start" \
      -H "Content-Type: application/json" \
      -d '{"backend": "tmux"}' 2>&1)
    CURL_RC=$?
    echo "$(date): [$name] curl exit=$CURL_RC response=$RESPONSE" >> "$LOG_FILE"

    if [ $CURL_RC -eq 0 ] && echo "$RESPONSE" | grep -q '"ok":true'; then
        echo "$(date): [$name] worker startup successful" >> "$LOG_FILE"
        started=$((started + 1))
    else
        echo "$(date): [$name] worker startup FAILED" >> "$LOG_FILE"
        failed=$((failed + 1))
        failed_names+=("$name")
    fi
done

echo "$(date): worker startup summary: $started started, $failed failed (${failed_names[*]:-none})" >> "$LOG_FILE"

# Partial success is still success for this unit: a lane with its own
# pre-existing, unrelated problem (the historical shape) must not read as
# "the whole boot-recovery mechanism is broken" the way a hard exit 1 would
# under systemd's oneshot status. Only a total loss (every lane failed, or
# the loop never ran) is a real failure of THIS script's own job.
if [ $started -eq 0 ]; then
    echo "$(date): ERROR: every lane failed to start" >> "$LOG_FILE"
    exit 1
fi
exit 0
