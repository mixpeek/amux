#!/bin/bash
# The container writers line of scripts/mac-cleanup-tick.sh (Mac disk RCA
# 20261022-200009). The host sees a VM's datadisk as the writer and names no
# lane; the bundle now lists the VM's containers by block writes over their
# uptime, so ts-dropin-mx (1102G in 28h) is named in the escalation itself.
# Each cell fails if its guard is removed (ethos rule 7).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
trap 'rm -rf -- "${FIX:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
export AMUX_CLEANUP_SCOPE_FILE=/dev/null AMUX_CLEANUP_SESSIONS_CMD=false AMUX_CLEANUP_PRESSURE_CMD="echo 1" AMUX_CLEANUP_HEALTH_PROBES=off
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"

# What `docker stats` and `docker inspect` printed on 2026-10-22, plus a
# container that wrote more in total but over a far longer life.
NOW=$(python3 -c 'import datetime;print(int(datetime.datetime(2026,10,23,0,20,tzinfo=datetime.timezone.utc).timestamp()))')
cat > "$FIX/raw" <<'R'
S	ts-dropin-mx	12.3GB / 1.18TB
S	supabase_db_ffe-supabase	1.2GB / 14.2GB
S	old-archiver	3GB / 1.5TB
S	idle	0B / 0B
I	/ts-dropin-mx	2026-10-21T20:20:00.123456789Z
I	/supabase_db_ffe-supabase	2026-10-21T15:00:00Z
I	/old-archiver	2026-09-01T00:00:00Z
I	/idle	2026-10-22T00:00:00Z
R
VM_WRITERS_CMD="cat $FIX/raw"
out=$(vm_container_writers goal-shared "$NOW")
echo "1. containers are ranked by write rate since they started, not by total"
check "the fastest writer comes first"          "ts-dropin-mx" "$(printf '%s\n' "$out" | head -1 | sed 's/.*colima-goal-shared: \([^ ]*\).*/\1/')"
check "with its total, uptime and rate"         "yes" "$(printf '%s\n' "$out" | head -1 | grep -q 'wrote 1180G in 28.0h (42.1G/h)' && echo yes || echo no)"
check "only the first carries the verdict"      "1" "$(printf '%s\n' "$out" | grep -c 'verdict=vm_container_writer')"
check "a larger total over a longer life ranks below it" "yes" "$(printf '%s\n' "$out" | sed -n 2p | grep -q 'old-archiver' && echo yes || echo no)"
check "a container that wrote nothing is not listed" "0" "$(printf '%s\n' "$out" | grep -c ' idle ')"
echo "2. a VM docker cannot reach lists nothing and does not fail the tick"
VM_WRITERS_CMD="false"
check "no lines"                                "" "$(vm_container_writers goal-shared "$NOW")"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-writers — all checks passed"; exit 0; fi
echo "mac-cleanup-writers: $fails check(s) FAILED"; exit 1
