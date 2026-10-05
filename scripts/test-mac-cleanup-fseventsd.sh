#!/bin/bash
# The fseventsd arm of scripts/mac-cleanup-tick.sh (DESKT-81). fseventsd grew from
# under 10G to 76G over 2026-10-03..05 while the tick printed its size and never
# its cause. When it is hot the tick now names the busiest directories and
# escalates the class. Each cell fails if its guard is removed (ethos rule 7).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
export AMUX_CLEANUP_FSEVENTSD_CMD=true
export AMUX_CLEANUP_CLAUDE_TMP_ROOT="$FIX/no-claude-tmp"
trap 'rm -rf -- "${FIX:?}"' EXIT
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { if printf '%s' "$1" | grep -q -- "$2"; then echo yes; else echo no; fi; }
export AMUX_CLEANUP_SCOPE_FILE=/dev/null AMUX_CLEANUP_SESSIONS_CMD=false AMUX_CLEANUP_VM_LIST_CMD=true AMUX_CLEANUP_PRESSURE_CMD="echo 1"
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"

R="$FIX/roots"
mkfiles() { mkdir -p "$1"; local i; for i in $(seq 1 "$2"); do : > "$1/f$i"; done; }
mkfiles "$R/a/proj/sess/scratchpad/tip/server" 50     # busy, and deeper than the grouping
mkfiles "$R/a/proj/quiet" 5                            # under the floor
mkfiles "$R/a/proj/old/x" 50; find "$R/a/proj/old" -exec touch -t 202001010000 {} +
mkfiles "$R/b/lane/tmp" 30
mkfiles "$R/a/proj/sess/.git/objects" 40               # git internals are pruned

echo "1. the census names the busiest directories, grouped three levels down"
churn_census "$R/a:$R/b" 5 30 10 > "$FIX/c.out"; out=$(cat "$FIX/c.out")
check "the busy session is named with its count"      "50 $R/a/proj/sess/scratchpad" "$(printf '%s\n' "$out" | head -1)"
check "a second root is counted too"                   "yes" "$(has "$out" "30 $R/b/lane/tmp")"
check "a directory under the floor is not named"       "no"  "$(has "$out" "$R/a/proj/quiet")"
check "old files are not churn"                        "no"  "$(has "$out" "$R/a/proj/old")"
check "git internals are not counted"                  "no"  "$(has "$out" "/.git")"
check "a full scan says complete"                      "yes" "$CHURN_COMPLETE"
churn_census "$R/a" 5 0 10 >/dev/null
check "a spent budget says the scan is incomplete"     "no"  "$CHURN_COMPLETE"

echo "2. end to end: a hot fseventsd names its feed and escalates; a quiet one does neither (macOS only)"
if [ "$(uname)" = Darwin ]; then
  REC="$FIX/rec.sh"; printf '#!/bin/bash\ncp "$2" %s/sent.msg; echo "to=$1" >> %s/sent.log\n' "$FIX" "$FIX" > "$REC"; chmod +x "$REC"
  tick() { AMUX_CLEANUP_FSEVENTSD_CMD="$1" AMUX_CLEANUP_CHURN_ROOTS="$R/a" AMUX_CLEANUP_CHURN_FILES=10 \
           AMUX_CLEANUP_STATE_DIR="$FIX/tick" AMUX_CLEANUP_TARGET_ROOTS="$FIX/none" AMUX_CLEANUP_PURGE_CMD=true \
           AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 AMUX_CLEANUP_SNAPSHOT_FLOOR_GB=0 \
           AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 AMUX_CLEANUP_CPU_SHARE=99 AMUX_CLEANUP_SWAP_FREE_FLOOR_MB=0 \
           AMUX_CLEANUP_DISK_FLOOR_GB=0 AMUX_CLEANUP_HISTORY_CMD=true AMUX_CLEANUP_CARD_CMD=true \
           AMUX_CLEANUP_ESCALATE_TO=mac-ops-test AMUX_CLEANUP_ESCALATE_CMD="$REC TARGET FILE" bash "$TICK" 2>&1; }
  out=$(tick "echo 0.01 3")
  check "control: a quiet fseventsd is under its thresholds" "yes" "$(has "$out" 'fseventsd 0.01G 3% CPU, under')"
  check "and escalates nothing"                         "no"  "$([ -f "$FIX/sent.log" ] && echo yes || echo no)"
  out=$(tick "echo 30 120")
  check "a hot fseventsd names its top feed"            "yes" "$(has "$out" "fseventsd feed: 50 files modified in 5m under $R/a/proj/sess/scratchpad")"
  check "it becomes a constraint"                       "yes" "$(has "$out" 'constraints 1\|constraint fseventsd')"
  check "and is escalated"                              "yes" "$([ -f "$FIX/sent.log" ] && echo yes || echo no)"
  check "the escalation names the feed"                 "yes" "$(grep -q "Top feed: 50 files in 5m under $R/a/proj/sess/scratchpad" "$FIX/sent.msg" 2>/dev/null && echo yes || echo no)"
else
  echo "  skip (the end-to-end tick needs macOS)"
fi

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-fseventsd — all checks passed"; exit 0; fi
echo "mac-cleanup-fseventsd: $fails check(s) FAILED"; exit 1
