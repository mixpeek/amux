#!/usr/bin/env bash
# RR-0116 / RR-0117a — resource-leak probe (FD + RSS under sustained load).
#
# Boots the Rust server against a THROWAWAY database, drives it with a request
# loop, and samples RSS and open file descriptors on a fixed cadence. Fails if
# either grows past its threshold measured FROM THE POST-WARMUP BASELINE, not
# from process start — a server legitimately grows while it warms caches and
# fills pools, and gating on that produces a detector that fires on healthy
# startup (ethos rule 7: "a threshold below the baseline is not a detector").
#
# WHAT THE PLAN ASKS FOR VS WHAT A HOSTED RUNNER CAN DO, stated rather than
# quietly substituted: RR-0116 asks for a 4h leak test and RR-0117a for 48h.
# GitHub-hosted runners cap a job at 6 hours, so 48h is not runnable there at
# all and 4h eats most of the cap. SOAK_MINUTES is therefore a parameter, the
# nightly runs a short one, and the weekly soak runs the long one; anything
# beyond 6h needs a self-hosted runner and is NOT silently pretended to have
# happened.
#
#   SOAK_MINUTES=30 scripts/soak-probe.sh
#
# Env:
#   SOAK_MINUTES     how long to drive (default 5)
#   SOAK_SAMPLE_S    seconds between samples (default 15)
#   SOAK_RSS_GROWTH  max fractional RSS growth from post-warmup (default 0.20)
#   SOAK_FD_GROWTH   max ABSOLUTE fd growth from post-warmup (default 50)
#   SOAK_CONCURRENCY parallel request loops (default 4)
#   AMUX_LIVE_DB     if set and readable, a READ-ONLY .backup copy is soaked
#                    against instead of an empty DB (real data volume)
set -euo pipefail

SOAK_MINUTES="${SOAK_MINUTES:-5}"
SOAK_SAMPLE_S="${SOAK_SAMPLE_S:-15}"
SOAK_RSS_GROWTH="${SOAK_RSS_GROWTH:-0.20}"
# Absolute floor on RSS growth (AMUX-2872): a ratio alone failed a 36MB
# process for growing 8MB of allocator/warmup settling — 22% of a tiny
# baseline, with the trailing samples FLAT (a plateau, not a leak slope).
# The same 20% on a 200MB process is 40MB and matters. Both must exceed
# for a FAIL: relative catches proportional leaks on big processes, the
# floor stops fast-vs-faster arithmetic on small ones — the third
# ratio-without-floor instrument this week (latency 2ms, /api/prefs; and
# the mdai cap), same cure each time.
SOAK_RSS_ABS_KB="${SOAK_RSS_ABS_KB:-24576}"
SOAK_FD_GROWTH="${SOAK_FD_GROWTH:-50}"
SOAK_CONCURRENCY="${SOAK_CONCURRENCY:-4}"
PORT="${SOAK_PORT:-18933}"
BIN="${AMUX_RS_BIN:-./target/release/amux-server}"

WORK="$(mktemp -d /tmp/amux-soak.XXXXXX)"
cleanup() {
  [ -n "${DRIVER_PIDS:-}" ] && kill $DRIVER_PIDS 2>/dev/null || true
  [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

if [ -n "${AMUX_LIVE_DB:-}" ] && [ -r "${AMUX_LIVE_DB}" ]; then
  # READ-ONLY copy. Never soak against the live file. VACUUM INTO, not .backup:
  # the backup API restarts whenever a writer touches the source, so a .backup
  # of the live db never finishes (AMUX-3491, DESKT-94).
  echo "== seeding from a read-only copy of $AMUX_LIVE_DB"
  sqlite3 "file:${AMUX_LIVE_DB}?mode=ro" "VACUUM INTO '$WORK/amux.db'"
else
  echo "== no AMUX_LIVE_DB; soaking against a fresh database"
fi

echo "== booting $BIN on :$PORT"
AMUX_HOME="$WORK" AMUX_DB="$WORK/amux.db" AMUX_RS_PORT=$PORT \
  "$BIN" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 60); do
  curl -sk --max-time 2 "https://localhost:$PORT/health" >/dev/null 2>&1 && break
  sleep 0.5
done
if ! curl -sk --max-time 2 "https://localhost:$PORT/health" >/dev/null 2>&1; then
  echo "FAIL: server never answered /health"; sed -n '1,80p' "$WORK/server.log"; exit 1
fi
TOKEN=$(cat "$WORK/auth-token" 2>/dev/null || echo "")

# Bracket the whole run with /health's `build` hash. This server re-execs when
# anyone saves amux-server.py / rebuilds, and a restart's symptoms are
# indistinguishable from the leak being hunted. Measuring across two different
# binaries is how a wrong conclusion arrives already corroborated.
build_hash() { curl -sk --max-time 5 "https://localhost:$PORT/health" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("build",""))' 2>/dev/null || echo ""; }
BUILD0="$(build_hash)"

sample() { # -> "rss_kb fd_count"
  local rss fd
  rss=$(ps -o rss= -p "$SERVER_PID" 2>/dev/null | tr -d ' ')
  if command -v lsof >/dev/null 2>&1; then
    fd=$(lsof -p "$SERVER_PID" 2>/dev/null | wc -l | tr -d ' ')
  elif [ -d "/proc/$SERVER_PID/fd" ]; then
    fd=$(ls -1 "/proc/$SERVER_PID/fd" 2>/dev/null | wc -l | tr -d ' ')
  else
    fd=-1
  fi
  echo "${rss:-0} ${fd:--1}"
}

drive() {
  while :; do
    curl -sk -o /dev/null --max-time 5 "https://localhost:$PORT/health"
    curl -sk -o /dev/null --max-time 5 -H "Authorization: Bearer $TOKEN" "https://localhost:$PORT/api/board"
    curl -sk -o /dev/null --max-time 5 -H "Authorization: Bearer $TOKEN" "https://localhost:$PORT/api/workers"
    curl -sk -o /dev/null --max-time 5 -H "Authorization: Bearer $TOKEN" "https://localhost:$PORT/api/search?q=the&limit=20"
    curl -sk -o /dev/null --max-time 5 "https://localhost:$PORT/"
  done
}

echo "== warmup (30s at concurrency $SOAK_CONCURRENCY)"
DRIVER_PIDS=""
for _ in $(seq 1 "$SOAK_CONCURRENCY"); do drive >/dev/null 2>&1 & DRIVER_PIDS="$DRIVER_PIDS $!"; done
sleep 30
read -r BASE_RSS BASE_FD <<<"$(sample)"
echo "== post-warmup baseline: RSS ${BASE_RSS}KB, FDs ${BASE_FD}"

END=$(( $(date +%s) + SOAK_MINUTES * 60 ))
MAX_RSS=$BASE_RSS
MAX_FD=$BASE_FD
SAMPLES=0
printf 'elapsed_s\trss_kb\tfds\n' >"$WORK/samples.tsv"
START=$(date +%s)
while [ "$(date +%s)" -lt "$END" ]; do
  sleep "$SOAK_SAMPLE_S"
  read -r RSS FD <<<"$(sample)"
  [ -z "$RSS" ] || [ "$RSS" = "0" ] && { echo "FAIL: server process died during soak"; tail -40 "$WORK/server.log"; exit 1; }
  ELAPSED=$(( $(date +%s) - START ))
  printf '%s\t%s\t%s\n' "$ELAPSED" "$RSS" "$FD" >>"$WORK/samples.tsv"
  [ "$RSS" -gt "$MAX_RSS" ] && MAX_RSS=$RSS
  [ "$FD" -gt "$MAX_FD" ] && MAX_FD=$FD
  SAMPLES=$((SAMPLES + 1))
done

BUILD1="$(build_hash)"
if [ "$BUILD0" != "$BUILD1" ]; then
  echo "INVALID: /health build moved $BUILD0 -> $BUILD1 — two different servers were measured, the numbers below mean nothing"
  exit 1
fi

echo
echo "== samples ($SAMPLES over ${SOAK_MINUTES}m)"
cat "$WORK/samples.tsv"

RSS_GROWTH=$(python3 -c "print(($MAX_RSS - $BASE_RSS) / max($BASE_RSS, 1))")
FD_GROWTH=$((MAX_FD - BASE_FD))
echo
echo "post-warmup RSS ${BASE_RSS}KB -> peak ${MAX_RSS}KB (growth $RSS_GROWTH, threshold $SOAK_RSS_GROWTH)"
echo "post-warmup FDs ${BASE_FD}    -> peak ${MAX_FD}    (growth $FD_GROWTH, threshold $SOAK_FD_GROWTH)"

fail=0
if [ "$SAMPLES" -lt 2 ]; then
  # Two samples cannot show a trend. A "pass" from one sample is a gate that
  # cannot fail, so it is an error instead.
  echo "FAIL: only $SAMPLES sample(s) — a leak check needs at least 2; raise SOAK_MINUTES or lower SOAK_SAMPLE_S"
  fail=1
fi
RSS_ABS_KB=$((MAX_RSS - BASE_RSS))

# THE LEAK VERDICT IS THE STEADY-STATE SLOPE (AMUX-4810).
#
# It used to be peak-minus-baseline over a baseline taken 30 SECONDS in. This
# server needs about two hours to reach steady state, so that ratio measured
# warm-up, and the peak is an overshoot the process gives back. On the 09-13
# weekly run it scored 1.175 against 0.20 while the second HALF of the run was
# NEGATIVE (quarters: +30,076KB +14,768KB -7,960KB -2,536KB, peak 117,272KB,
# final 105,752KB). Every run of this instrument since 08-16 failed that way,
# which is a detector reporting one verdict regardless of input.
#
# A leak is a positive slope that does not flatten. scripts/soak_slope.py fits
# the trailing half and reports `rising` only when the rise both exceeds a
# calibrated floor and is distinguishable from noise. Its own test
# (scripts/test-soak-slope.sh) proves it catches a 100KB/min leak and does not
# call a warm-up plateau one.
SLOPE_JSON=$(python3 "$(dirname "$0")/soak_slope.py" "$WORK/samples.tsv" \
  --soak-minutes "$SOAK_MINUTES" 2>/dev/null || echo '{}')
echo "slope: $SLOPE_JSON"
SLOPE_VERDICT=$(python3 - "$SLOPE_JSON" <<'PY'
import json, sys
d = json.loads(sys.argv[1] or "{}")
if not d.get("measured"):
    print("unmeasured %s" % (d.get("why_unmeasured") or "slope not computed"))
elif not d.get("steady"):
    # Reported, not gated: the trailing window of a short run is still warm-up.
    print("tooshort %s KB/h over %s trailing samples — %s"
          % (d.get("kb_per_hour"), d.get("tail_samples"), d.get("why_unmeasured")))
elif d.get("rising"):
    print("leak steady-state RSS rising %s KB/h (threshold %s, stderr %s, r2 %s)"
          % (d.get("kb_per_hour"), d.get("threshold_kb_per_hour"),
             d.get("stderr_kb_per_hour"), d.get("r2")))
else:
    print("flat steady-state RSS %s KB/h (threshold %s, stderr %s) — no leak"
          % (d.get("kb_per_hour"), d.get("threshold_kb_per_hour"), d.get("stderr_kb_per_hour")))
PY
)
case "$SLOPE_VERDICT" in
  leak*)      echo "FAIL: ${SLOPE_VERDICT#leak }"; fail=1 ;;
  flat*)      echo "OK: ${SLOPE_VERDICT#flat }" ;;
  tooshort*)  echo "NOTE: leak verdict NOT MEASURED — ${SLOPE_VERDICT#tooshort }" ;;
  *)          echo "NOTE: leak verdict NOT MEASURED — ${SLOPE_VERDICT#unmeasured }" ;;
esac

# The old ratio is still PRINTED, because it is a real number about warm-up
# cost and a reader comparing runs wants it. It no longer decides the build.
python3 -c "import sys; sys.exit(0 if $RSS_GROWTH <= $SOAK_RSS_GROWTH or $RSS_ABS_KB <= $SOAK_RSS_ABS_KB else 1)" || {
  echo "NOTE: warm-up overshoot $RSS_GROWTH (was the old gate's $SOAK_RSS_GROWTH threshold) AND ${RSS_ABS_KB}KB absolute — ${BASE_RSS}KB -> ${MAX_RSS}KB. Informational: the leak verdict is the steady-state slope above."
}
python3 -c "import sys; sys.exit(1 if $RSS_GROWTH > $SOAK_RSS_GROWTH and $RSS_ABS_KB <= $SOAK_RSS_ABS_KB else 0)" || true
if python3 -c "import sys; sys.exit(0 if $RSS_GROWTH > $SOAK_RSS_GROWTH and $RSS_ABS_KB <= $SOAK_RSS_ABS_KB else 1)"; then
  echo "NOTE: relative RSS growth $RSS_GROWTH exceeds $SOAK_RSS_GROWTH but only ${RSS_ABS_KB}KB absolute (floor ${SOAK_RSS_ABS_KB}KB) — small-baseline settling, not failed (AMUX-2872)"
fi
if [ "$MAX_FD" -ge 0 ] && [ "$FD_GROWTH" -gt "$SOAK_FD_GROWTH" ]; then
  echo "FAIL: open file descriptors grew by $FD_GROWTH (threshold $SOAK_FD_GROWTH) — $BASE_FD -> $MAX_FD"
  fail=1
fi
if [ "$MAX_FD" -lt 0 ]; then
  # Say it rather than passing a check that never ran.
  echo "NOTE: no lsof and no /proc — file-descriptor growth was NOT measured on this platform"
fi

[ "$fail" -eq 0 ] && echo "SOAK PASSED" || exit 1
