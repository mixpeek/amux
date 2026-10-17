#!/usr/bin/env bash
# test-main-ci-watch.sh: the red-main watcher says WHY the board refused its
# card, and a failed fallback send is retried once with its error kept.
#
# 2026-10-16 13:23 (SCHED-713): two HTTP 400s and "could not message the amux
# lane either", and nothing said why. The 400s were the lane's WIP gate; the
# send went through from a bare environment minutes later. Every input here is
# a stub on PATH: no network, no board, no real lane.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
FIX=$(mktemp -d "${TMPDIR:-/tmp}/mciw.XXXXXX")
trap 'rm -rf -- "${FIX:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { printf '%s' "$1" | grep -qF -- "$2" && echo yes || echo no; }

BIN="$FIX/bin"; mkdir -p "$BIN" "$FIX/home/.amux/github-app"
printf '#!/bin/sh\necho "export GH_TOKEN=stub"\n' > "$FIX/home/.amux/github-app/get-token.sh"
cat > "$BIN/gh" <<'EOF'
#!/bin/sh
case "$*" in
  *workflows/rust.yml/runs*) printf '101\tfailure\tabc123def456\thttps://example/run/101\n' ;;
  *workflows/checks.yml/runs*) printf '' ;;
  *runs/101/jobs*) printf '7\n' ;;
  # "1) [ios-safari] " is 16 bytes and the filler 72, so the 3-byte "›" sits at
  # bytes 89-91: a byte-counting cut at 90 splits it (the 13:31 failure).
  *jobs/7/logs*) printf '2026-10-16T17:00:00.0000000Z   1) [ios-safari] e2e/draw-tab.spec.ts:460:5 Draw live mode is zen mode inside the amux sh\342\200\272 tail\n' ;;
  *) printf '' ;;
esac
EOF
cat > "$BIN/curl" <<'EOF'
#!/bin/sh
out=""; body=""; while [ $# -gt 0 ]; do case "$1" in -o) out=$2; shift ;; -d) body=$2; shift ;; esac; shift; done
[ "${CURL_TIMEOUT:-0}" = 1 ] && { printf 000; exit 28; }
# Parse the body strictly, as the board does: a lone surrogate is a 400.
if ! printf '%s' "$body" | python3 -c 'import sys,json; json.loads(sys.stdin.buffer.read().decode("utf-8")); s=sys.stdin' 2>/dev/null \
   || printf '%s' "$body" | grep -q '\\ud[89a-f]'; then
  printf 'Failed to parse the request body as JSON: lone leading surrogate' > "$out"; printf 400; exit 0
fi
printf '%s' "$body" >> "$CARD_BODIES"
printf '{"blocked":true,"error":"WIP limit: holding 348, limit 20"}' > "$out"; printf 400
EOF
cat > "$BIN/amux" <<'EOF'
#!/bin/sh
[ "$1" = url ] && { echo https://stub; exit 0; }
n=$(cat "$AMUX_SEND_COUNT" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$AMUX_SEND_COUNT"
t=$(cat); printf '%s' "$t" | grep -q "$(printf '\357\277\275')" || true
[ "$n" -ge "${AMUX_SEND_OK_AT:-2}" ] && { echo "sent"; exit 0; }
echo "send refused: lane busy (stub)" >&2; exit 1
EOF
cat > "$BIN/sleep" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$BIN"/* "$FIX/home/.amux/github-app/get-token.sh"
run() { HOME="$FIX/home" PATH="$BIN:$PATH" MAIN_CI_WATCH_STATE="$FIX/state.json" AMUX_SEND_COUNT="$FIX/sends" CARD_BODIES="$FIX/bodies" \
  AMUX_SEND_OK_AT="$1" bash "$HERE/main-ci-watch.sh" 2>&1; }

echo "1. a refused card names its reason; a failed send is retried and lands"
rm -f "$FIX/state.json" "$FIX/sends"
out=$(run 2 || true)
check "the refusal body is printed"            yes "$(has "$out" 'HTTP 400: {"blocked":true,"error":"WIP limit: holding 348')"
check "the first send's own error is kept"     yes "$(has "$out" 'could not message the amux lane (try 1): send refused: lane busy')"
check "the retry lands, so nothing is lost"    yes "$(has "$out" '"alerts_not_delivered":0')"
check "two sends were made"                    2   "$(cat "$FIX/sends")"

check "the card body is valid JSON with the test name" yes "$(has "$(cat "$FIX/bodies" 2>/dev/null)" 'draw-tab.spec.ts:460:5 Draw live')"
check "no lone surrogate reached the board"   no  "$(has "$out" 'lone leading surrogate')"

echo "1b. a board that times out (curl exit 28) still falls through to the message"
rm -f "$FIX/state.json" "$FIX/sends"
out=$(CURL_TIMEOUT=1 run 1 || true)
check "the timeout is reported as a refusal"   yes "$(has "$out" 'board refused a todo card: HTTP 000')"
check "the alert still lands by message"       yes "$(has "$out" '"alerts_not_delivered":0')"

echo "2. two failed sends report the alert as not delivered"
rm -f "$FIX/state.json" "$FIX/sends"
out=$(run 9 || true)
check "both tries are named"                   yes "$(has "$out" '(try 2): send refused')"
check "it is counted undelivered"              yes "$(has "$out" '"alerts_not_delivered":1')"

[ "$fails" = 0 ] || { echo "--- last script output ---"; printf '%s\n' "$out" | tail -20; }
if [ "$fails" = 0 ]; then echo "PASS: main-ci-watch — all checks passed"; else echo "main-ci-watch: $fails check(s) FAILED"; exit 1; fi
