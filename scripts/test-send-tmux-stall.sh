#!/usr/bin/env bash
# A send while tmux is stalled must not stop the worker it is sending to.
#
# 2026-11-02: under host pressure tmux stopped answering, the send read a live
# worker as stopped, and the start it triggered killed the worker. This runs a
# private server with a real claude on the cheapest model, freezes that server's own tmux with SIGSTOP
# (a real stall, nothing mocked), sends, and checks the same process survives.
#
# usage: scripts/test-send-tmux-stall.sh <amux-server binary> [port]
# Copy the binary out of the shared target dir first: a peer's build replaces it.
set -u
BIN="${1:?server binary}"
PORT="${2:-18993}"
ROOT="${AMUX_TEST_ROOT:-${TMPDIR:-/tmp}}"
T="$(mktemp -d "$ROOT/stall.XXXXXX")"
T="$(cd "$T" && pwd -P)"
# Short on purpose: a tmux socket path has a length limit.
TD="$(mktemp -d /tmp/stall.XXXXXX)"
mkdir -p "$T/home" "$T/repo"
git -C "$T/repo" init -q

TMUX_PID=""
cleanup() {
  [ -n "$TMUX_PID" ] && kill -CONT "$TMUX_PID" 2>/dev/null
  [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null
  [ -n "$TMUX_PID" ] && kill "$TMUX_PID" 2>/dev/null
  [ "${KEEP_LOG:-}" ] && cp "$T/server.log" "$KEEP_LOG"
  rm -rf "$T" "$TD"
}
trap cleanup EXIT

# Scrub the fleet's env, as a private server must, so it cannot reach the real one.
(
  for k in ${!AMUX_@} ${!CC_@} ${!CLAUDE_CODE_@} CLAUDECODE; do unset "$k"; done
  unset TMUX TMUX_PANE
  export AMUX_HOME="$T/home" AMUX_RS_PORT="$PORT" AMUX_NO_SELF_ADOPT=1 AMUX_EXIT_WITH_PARENT=0
  export AMUX_ALLOW_TMUX_SPAWN_FROM_TEST_HOME=1 TMUX_TMPDIR="$TD"
  exec "$BIN" > "$T/server.log" 2>&1
) &
SRV=$!

U=""
for _ in $(seq 1 60); do
  for scheme in https http; do
    if curl -sk --max-time 2 "$scheme://127.0.0.1:$PORT/health" | grep -q '"status"'; then
      U="$scheme://127.0.0.1:$PORT"
      break 2
    fi
  done
  sleep 1
done
[ -n "$U" ] || { echo "FAIL server did not start"; tail -5 "$T/server.log"; exit 2; }

curl -sk --max-time 60 -X POST -H 'Content-Type: application/json' \
  -d "{\"name\":\"w\",\"dir\":\"$T/repo\",\"provider\":\"claude\",\"flags\":\"--model haiku\"}" \
  "$U/api/sessions" >/dev/null

# The worker's process: the child of its pane's shell.
worker_pid() {
  local shell_pid
  shell_pid="$(env -u TMUX TMUX_TMPDIR="$TD" tmux list-panes -t amux-w -F '#{pane_pid}' 2>/dev/null | head -1)"
  [ -n "$shell_pid" ] && pgrep -P "$shell_pid" | head -1
}

# Creating a worker starts it. A first message clears boot readiness.
BEFORE=""
for _ in $(seq 1 90); do
  BEFORE="$(worker_pid)"
  [ -n "$BEFORE" ] && curl -sk --max-time 30 "$U/api/sessions/w/peek?lines=20" | grep -q 'bypass\|shortcuts\|❯' && break
  sleep 2
done
[ -n "$BEFORE" ] || { echo "FAIL the worker never launched"; tail -5 "$T/server.log"; exit 2; }
curl -sk --max-time 120 -X POST -H 'Content-Type: application/json' \
  -d '{"text":"Reply with just: ok"}' "$U/api/sessions/w/send" >/dev/null
for _ in $(seq 1 30); do
  grep -q '"boot_readiness_pending": *false' "$T/home/sessions/w.meta.json" && break
  sleep 2
done
grep -q '"boot_readiness_pending": *false' "$T/home/sessions/w.meta.json" \
  || { echo "FAIL boot readiness never cleared, so this run proves nothing"; exit 2; }
# The first turn must be over, or the stalled send only waits behind it.
for _ in $(seq 1 90); do
  grep -aq 'event=String("Stop")' "$T/server.log" && break
  sleep 2
done
grep -aq 'event=String("Stop")' "$T/server.log" \
  || { echo "FAIL the first turn never finished, so this run proves nothing"; exit 2; }
sleep 5
BEFORE="$(worker_pid)"
[ -n "$BEFORE" ] || { echo "FAIL the worker exited before the test"; exit 2; }

TMUX_PID="$(env -u TMUX TMUX_TMPDIR="$TD" tmux display-message -p '#{pid}')"
[ -n "$TMUX_PID" ] || { echo "FAIL no private tmux server"; exit 2; }

kill -STOP "$TMUX_PID"
curl -sk --max-time 120 -X POST -H 'Content-Type: application/json' \
  -d '{"text":"hello during a stall"}' "$U/api/sessions/w/send" > "$T/reply" &
SEND=$!
# Long enough for every probe the send makes to time out, under load.
sleep "${STALL_S:-40}"
kill -CONT "$TMUX_PID"
wait "$SEND"
REPLY="$(cat "$T/reply")"
sleep 45
echo "reply: $REPLY"
AFTER="$(worker_pid)"
fail=0
if kill -0 "$BEFORE" 2>/dev/null && [ "$AFTER" = "$BEFORE" ]; then
  echo "ok   the worker's process survived a send during the stall (pid $BEFORE)"
else
  echo "FAIL the send replaced the worker: pid $BEFORE became $AFTER"
  fail=1
fi
if grep -aqE 'caller="send-boot"|verdict="boot_delivery_start_failed"' "$T/server.log"; then
  echo "FAIL the send tried to start the worker"
  fail=1
else
  echo "ok   the send did not try to start the worker"
fi
if grep -aq 'verdict="send_held_tmux_unanswered"' "$T/server.log"; then
  echo "ok   the held send is in the log"
else
  echo "FAIL no send_held_tmux_unanswered line in the log"
  fail=1
fi
exit "$fail"
