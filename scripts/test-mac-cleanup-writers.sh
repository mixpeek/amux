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

echo "3. a file open for write is named by its growth, wherever it lives (RCA 20261025-100211)"
# Two samples, 1s apart: the dump grows 1G, the VM disk grows 64K (0.2G/h,
# under the 1G/h floor), a big file shrinks, and a file that appears only in the second sample
# has no rate.
printf '0' > "$FIX/n"
fake_open() {
  local n; n=$(cat "$FIX/n"); printf '%s' $((n + 1)) > "$FIX/n"
  printf '68285\tpg_dump\t%s\t/Users/x/.config/ts-parity/item7-dump/ss_s1.dir/5040.dat.gz\n' $(( (34 + n) * 1073741824 ))
  printf '41602\tcom.apple\t%s\t/Users/x/.colima/_lima/_disks/colima-goal-shared/datadisk\n' $(( 160000000000 + n * 65536 ))
  printf '11205\tamux-serv\t%s\t/Users/x/.amux/amux.db\n' $(( 16000000000 - n * 1000 ))
  [ "$n" -eq 0 ] || printf '777\tnew\t%s\t/Users/x/new.bin\n' 9000000000
}
OPEN_WRITERS_CMD=fake_open
out=$(open_file_writers 1)
check "the growing file is named first, with its rate" "yes" "$(printf '%s\n' "$out" | head -1 | grep -q '5040.dat.gz growing 3600.0G/h (35.0G now) by pid=68285 pg_dump' && echo yes || echo no)"
check "and carries the verdict"                   "1" "$(printf '%s\n' "$out" | grep -c 'verdict=open_file_writer')"
check "a VM disk under 1G/h, a shrinking file and a new one are not writers" "1" "$(printf '%s\n' "$out" | grep -c .)"
OPEN_WRITERS_CMD=false
check "no lsof answer lists nothing"               "" "$(open_file_writers 0)"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-writers — all checks passed"; exit 0; fi
echo "mac-cleanup-writers: $fails check(s) FAILED"; exit 1
