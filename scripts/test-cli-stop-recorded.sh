#!/usr/bin/env bash
# `amux stop` on a tmux worker must go through the server, so the stop is
# recorded as session.stopped/api-stop. board-drive's owner_stopped() reads
# exactly that row; a bare `tmux kill-session` left none, and board-drive
# restarted mixpeek-frustrations 13s after SCHED-776 stopped it (MF-4369).
#
# Isolated CC_HOME, stub tmux and stub curl on PATH: nothing reaches a real
# worker, the real tmux socket or the live server.
#   1. server up   -> POST /api/sessions/<n>/stop is sent, tmux is NOT killed
#   2. server down -> falls back to tmux kill-session and says the stop is unrecorded
set -euo pipefail
cd "$(dirname "$0")/.." || exit 1
AMUX_BIN="${AMUX_BIN:-./amux}"
PASS=0; FAIL=0
ok()  { echo "  ok   $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL $1"; [ -n "${2:-}" ] && echo "       $2"; FAIL=$((FAIL+1)); }

WORK=$(mktemp -d)
trap 'rm -rf "${WORK:?}"' EXIT
CC="$WORK/home/.amux"; BIN="$WORK/bin"
mkdir -p "$CC/sessions" "$WORK/workdir" "$BIN"
printf 'CC_DIR="%s"\nCC_PROVIDER="claude"\n' "$WORK/workdir" > "$CC/sessions/stopsmoke.env"

# tmux: the pane is alive until something stops it.
cat > "$BIN/tmux" <<STUB
#!/usr/bin/env bash
echo "tmux \$*" >> "$WORK/calls"
case "\$1" in
  has-session) [ -e "$WORK/stopped" ] && exit 1 || exit 0 ;;
  kill-session) touch "$WORK/stopped" "$WORK/tmux-killed"; exit 0 ;;
  *) exit 0 ;;
esac
STUB
# curl: answers like the server's stop verb only when "server-up" exists.
cat > "$BIN/curl" <<STUB
#!/usr/bin/env bash
echo "curl \$*" >> "$WORK/calls"
[ -e "$WORK/server-up" ] || exit 7
case "\$*" in
  */api/sessions/stopsmoke/stop*) touch "$WORK/stopped"; echo '{"ok":true}' ;;
  *) echo '{}' ;;
esac
STUB
chmod +x "$BIN/tmux" "$BIN/curl"

run_stop() {
  rm -f "${WORK:?}/calls" "${WORK:?}/stopped" "${WORK:?}/tmux-killed"
  env -i HOME="$WORK/home" PATH="$BIN:/usr/bin:/bin" CC_HOME="$CC" \
    AMUX_API="https://127.0.0.1:1" AMUX_BACKEND=tmux AMUX_CURL_CONNECT_TIMEOUT=1 \
    bash "$AMUX_BIN" stop stopsmoke > "$WORK/out" 2>&1 || true
}

echo "== server reachable: stop is recorded through the API"
touch "$WORK/server-up"; run_stop
if grep -q '/api/sessions/stopsmoke/stop' "$WORK/calls" 2>/dev/null; then ok "POSTed /api/sessions/stopsmoke/stop"
else bad "stop never reached the server" "$(cat "$WORK/out")"; fi
if [ ! -e "$WORK/tmux-killed" ]; then ok "tmux not killed behind the server's back"
else bad "tmux kill-session ran although the server accepted the stop" "$(cat "$WORK/calls")"; fi

echo "== server unreachable: falls back to tmux and says so"
rm -f "${WORK:?}/server-up"; run_stop
if [ -e "$WORK/tmux-killed" ]; then ok "fallback killed the tmux session"
else bad "worker left running when the server was down" "$(cat "$WORK/out")"; fi
if grep -q 'not recorded' "$WORK/out"; then ok "fallback warns the stop is unrecorded"
else bad "fallback was silent about the unrecorded stop" "$(cat "$WORK/out")"; fi

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
