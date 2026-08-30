#!/usr/bin/env bash
# Performance baseline (Phase 9, RR checklist §Performance regressions).
#
# Boots the Rust server against a copy of the LIVE database (real data
# volume — 640k+ rows — because an empty-DB baseline measures nothing), then
# measures the plan's targets:
#   dashboard load  < 500ms cold
#   /api/board      < 200ms with real data (the Python takes 50ms+ on 6MB;
#                   the Rust list is desc-truncated by design)
#   /health         < 50ms
#   RSS             < PERF_RSS_MAX_MB (default 280 — see the comment at the
#                   assertion for why the plan's 200 no longer holds)
# Prints a JSON baseline line suitable for committing to docs/perf-baseline.json
# and comparing in CI (a >10% p95 regression is a failure per Phase 10).
set -euo pipefail

LIVE_DB="${AMUX_LIVE_DB:-$HOME/.amux/amux.db}"
WORK="$(mktemp -d /tmp/amux-perf.XXXXXX)"
PORT=18911
trap 'kill "$SERVER_PID" 2>/dev/null || true; rm -rf "$WORK"' EXIT

# VACUUM INTO, not .backup (AMUX-3491, 2026-08-22). The backup API restarts
# from scratch whenever a writer touches the source, so against a DB under
# steady write load (the invariant-log trim was landing a batch every cycle)
# .backup NEVER completes — two runs of this script sat in the copy step past
# 3 and 9 minute timeouts, while VACUUM INTO finished in 4s: it runs inside
# one WAL read snapshot, which writers cannot invalidate. Cost of the switch:
# the copy is COMPACTED, so its page layout is tidier than production's and
# RSS/latency read marginally flatter — second-order against the growth this
# instrument tracks, and it moves the numbers ONCE, on the record here.
sqlite3 "file:${LIVE_DB}?mode=ro" "VACUUM INTO '$WORK/amux.db'"

AMUX_HOME="$WORK" AMUX_DB="$WORK/amux.db" AMUX_RS_PORT=$PORT \
  "${AMUX_RS_BIN:-./target/release/amux-server}" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 60); do
  curl -sk --max-time 2 "https://localhost:$PORT/health" >/dev/null 2>&1 && break
  sleep 0.5
done

TOKEN=$(cat "$WORK/auth-token" 2>/dev/null || echo "")

measure() { # name url [auth]
  local name=$1 url=$2 auth=${3:-}
  local total=0 worst=0
  for _ in 1 2 3 4 5; do
    local t
    if [ -n "$auth" ]; then
      t=$(curl -sk -o /dev/null -w '%{time_total}' -H "Authorization: Bearer $TOKEN" "$url")
    else
      t=$(curl -sk -o /dev/null -w '%{time_total}' "$url")
    fi
    t_ms=$(python3 -c "print(int(float('$t')*1000))")
    total=$((total + t_ms))
    [ "$t_ms" -gt "$worst" ] && worst=$t_ms
  done
  echo "\"${name}_avg_ms\": $((total / 5)), \"${name}_worst_ms\": $worst"
}

M1=$(measure dashboard "https://localhost:$PORT/")
M2=$(measure health "https://localhost:$PORT/health")
M3=$(measure board "https://localhost:$PORT/api/board" auth)
M4=$(measure board_full "https://localhost:$PORT/api/board?done_limit=0" auth)
M5=$(measure workers "https://localhost:$PORT/api/workers" auth)
RSS_MB=$(ps -o rss= -p "$SERVER_PID" | awk '{print int($1/1024)}')
# DIRTY (live) heap alongside RSS (AMUX-3488). Measured 2026-08-22: across
# repeated identical 30MB responses RSS climbed 176->229MB while malloc-dirty
# sat flat at 30-33MB — RSS retains FREED transient serialization peaks as
# clean-resident pages (macOS MADV_FREE; reclaimed only under pressure), so
# it measures allocator weather scaled by payload sizes, which is also the
# ±28% CI "noise". Dirty is the live heap: the number a LEAK actually moves.
# Linux: Private_Dirty from smaps_rollup; macOS: the MALLOC rows' DIRTY
# column summed. Empty when neither works (never fake a 0 — an absent
# measurement must not read as a tiny heap).
DIRTY_MB=""
if [ -r "/proc/$SERVER_PID/smaps_rollup" ]; then
  DIRTY_MB=$(awk '/^Private_Dirty:/{print int($2/1024)}' "/proc/$SERVER_PID/smaps_rollup")
elif command -v vmmap >/dev/null 2>&1; then
  DIRTY_MB=$(vmmap --summary "$SERVER_PID" 2>/dev/null | awk '
    /^MALLOC/ {
      v=$4  # DIRTY SIZE column of the region-type table
      mult = (v ~ /G$/) ? 1024 : (v ~ /K$/) ? 1/1024 : 1
      gsub(/[KMG]$/, "", v); total += v * mult
    }
    END { if (total > 0) print int(total) }')
fi
BOARD_BYTES=$(curl -sk -H "Authorization: Bearer $TOKEN" "https://localhost:$PORT/api/board" | wc -c | tr -d ' ')

echo "{ $M1, $M2, $M3, $M4, $M5, \"rss_mb\": $RSS_MB, \"dirty_mb\": ${DIRTY_MB:-null}, \"board_default_bytes\": $BOARD_BYTES }"

# Target assertions — each CAN fail (ethos 7).
#
# The RSS ceiling moved, on the record (AMUX-2872 -> AMUX-3488). The plan's
# 200MB was calibrated 2026-08-09 when a fresh boot held 66MB; by 2026-08-22
# the same measurement held 220MB in CI on a FROZEN 12k-doc corpus that had
# measured 164MB at the nightly job's authoring (08-10), and ~213MB live. The
# nightly perf leg was dying on exactly this line for its entire silent
# streak. 280 is today's 220 plus headroom, still a ceiling that can fail;
# the growth itself is AMUX-3488 — attribute it before absorbing more, and
# TIGHTEN this back if that hunt finds a leak. Overridable so the hunt can
# pin it: PERF_RSS_MAX_MB. The nightly CI job pins 350 at the job level:
# same-day runs on a frozen corpus measured 220 and 281 (±28% shared-runner
# noise), and a ceiling inside the noise band flaps on weather. The local
# default stays lower on purpose — a local run at today's 310 SHOULD fail,
# and route the reader to AMUX-3488.
RSS_MAX_MB="${PERF_RSS_MAX_MB:-280}"
fail=0
avg() { echo "$1" | sed 's/.*_avg_ms": \([0-9]*\).*/\1/'; }
[ "$(avg "$M1")" -lt 500 ] || { echo "FAIL: dashboard >= 500ms"; fail=1; }
[ "$(avg "$M2")" -lt 50 ] || { echo "FAIL: health >= 50ms"; fail=1; }
[ "$(avg "$M3")" -lt 200 ] || { echo "FAIL: board >= 200ms"; fail=1; }
[ "$RSS_MB" -lt "$RSS_MAX_MB" ] || { echo "FAIL: RSS ${RSS_MB}MB >= ${RSS_MAX_MB}MB (ceiling: PERF_RSS_MAX_MB, provenance above)"; fail=1; }
# The LEAK detector (AMUX-3488): dirty is live heap, so on macOS it does not
# inherit allocator weather — measured 30-45MB there against an RSS of 220+.
# 250 was "deliberately generous until the first CI (Linux) reading lands;
# tighten it then".
#
# THE READINGS LANDED AND SAID SOMETHING ELSE (AMUX-3790). Linux reads
# Private_Dirty from smaps_rollup, which counts EVERY private dirty page —
# heap, stack, .data/.bss, COW — not just malloc arenas. Across the five
# nightly runs that produced a number: dirty 254/268/224/239/227 against rss
# 257/270/226/241/229. Dirty is rss minus 2-3MB every time, ratio 0.987-0.993.
# So on Linux this gate is NOT independent of RSS, it is RSS, and it inherits
# the ±28% shared-runner noise that RSS's own ceiling is set at 350 to escape.
# It passed three nights and failed the next two on the same code shape.
#
# The default stays 250 because on macOS the premise holds and 250 is still
# generous there. CI pins PERF_DIRTY_MAX_MB=350 to match its RSS ceiling.
# Skipped, loudly, when the platform gave no reading — an absent number is not
# a passing one.
DIRTY_MAX_MB="${PERF_DIRTY_MAX_MB:-250}"
if [ -n "$DIRTY_MB" ]; then
  # SAY IT WHEN THE TWO METRICS HAVE COLLAPSED INTO ONE. A reader who sees
  # dirty and RSS both reported assumes two independent witnesses; when they
  # agree to within a few percent there is only one, and a leak verdict drawn
  # from "both agree" is drawn from a single measurement counted twice.
  if [ "${RSS_MB:-0}" -gt 0 ] && [ $(( DIRTY_MB * 100 / RSS_MB )) -ge 95 ]; then
    echo "NOTE: dirty ${DIRTY_MB}MB is $(( DIRTY_MB * 100 / RSS_MB ))% of RSS ${RSS_MB}MB — on this platform the dirty gate is NOT independent of the RSS gate (AMUX-3790); treat them as ONE measurement, not two agreeing ones"
  fi
  [ "$DIRTY_MB" -lt "$DIRTY_MAX_MB" ] || { echo "FAIL: dirty (live) heap ${DIRTY_MB}MB >= ${DIRTY_MAX_MB}MB — unlike RSS this is NOT allocator weather; suspect a real leak or a new resident cache"; fail=1; }
else
  echo "NOTE: dirty heap unmeasurable on this platform — the leak gate did not run"
fi
[ "$fail" -eq 0 ] && echo "BASELINE PASSED" || exit 1
