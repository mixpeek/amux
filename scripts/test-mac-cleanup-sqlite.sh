#!/bin/bash
# The orphaned sqlite .backup arm of scripts/mac-cleanup-tick.sh (DESKT-94). A
# .backup of the live amux.db never finishes (AMUX-3491), and when the caller's
# shell times out the sqlite3 child spins on under launchd: 2h52m at 96% CPU on
# 2026-10-10. Only an orphan is stopped. The ps listing is a fixture that names
# REAL child pids, so the kill path runs for real without depending on how this
# platform reparents orphans. Each cell fails if its guard is removed.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
kids=()
trap 'for k in ${kids[@]+"${kids[@]}"}; do kill "$k" 2>/dev/null || true; done; rm -rf -- "${FIX:?}"' EXIT
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { if printf '%s' "$1" | grep -q -- "$2"; then echo yes; else echo no; fi; }
alive() { kill -0 "$1" 2>/dev/null && echo alive || echo gone; }
export AMUX_CLEANUP_SCOPE_FILE=/dev/null AMUX_CLEANUP_FSEVENTSD_CMD=true AMUX_CLEANUP_CLAUDE_TMP_ROOT="$FIX/none"
export AMUX_CLEANUP_SQLITE_PS_CMD=true   # the live process table must not decide a test (DESKT-94)
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"
DB="$FIX/amux"; mkdir -p "$DB"; STATE_DIR="$FIX/state"; SQLITE_DB_ROOT="$DB"
# Never $(spawn): a command substitution waits for the child's stdout to close.
spawn() { sleep 300 >/dev/null 2>&1 & kids+=("$!"); eval "$1=$!"; }

setup() {
  spawn O; spawn L; spawn X; spawn T; spawn Y
  cat > "$FIX/ps.txt" <<PS
$O 1 03:00:00 sqlite3 $DB/amux.db .backup /tmp/x/board-copy.db
$L 4242 03:00:00 sqlite3 $DB/amux.db .backup /tmp/x/copy.db
$X 1 03:00:00 sqlite3 /other/place/amux.db .backup /tmp/x/copy.db
$T 1 03:00:00 sqlite3 $DB/amux.db .tables
$Y 1 05:00 sqlite3 $DB/amux.db .backup /tmp/x/young.db
PS
  SQLITE_PS_CMD="cat $FIX/ps.txt"; SQLITE_ORPHAN_MIN=20
}

echo "1. an orphaned .backup of an amux db past the floor is stopped and ledgered"
setup
out=$(reap_orphan_sqlite_backups 0); echo "$out" > "$FIX/out1"
check "the orphan is stopped"                          gone  "$(alive "$O")"
check "it is in the ledger"                            yes   "$(grep -q "stopped	$O	" "$STATE_DIR/sqlite-orphans.log" 2>/dev/null && echo yes || echo no)"
check "the line says why"                              yes   "$(has "$out" "stopped orphaned sqlite .backup pid=$O")"
echo "2. everything else is left running"
check "one with a live caller is left alone"           alive "$(alive "$L")"
check "and reported"                                   yes   "$(has "$out" "pid=$L age=03:00:00 has a live caller (ppid 4242)")"
check "a db outside the amux root is ignored"          alive "$(alive "$X")"
check "a sqlite3 that is not a .backup is ignored"     alive "$(alive "$T")"
check "one younger than the floor is ignored"          alive "$(alive "$Y")"
check "the summary counts each population"             yes   "$(has "$out" 'sqlite .backup: found=2 stopped=1 live_caller=1 (floor 20m)')"

echo "3. dry run stops nothing"
for k in "${kids[@]}"; do kill "$k" 2>/dev/null || true; done; kids=()
setup
out=$(reap_orphan_sqlite_backups 1)
check "dry run leaves the orphan running"              alive "$(alive "$O")"
check "and says what it would do"                      yes   "$(has "$out" "pid=$O age=03:00:00 would be stopped (dry run)")"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-sqlite — all checks passed"; exit 0; fi
echo "mac-cleanup-sqlite: $fails check(s) FAILED"; exit 1
