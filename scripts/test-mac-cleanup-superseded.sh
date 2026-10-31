#!/bin/bash
# The superseded-artifact arm in scripts/mac-cleanup-tick.sh (disk RCA
# 20261018-075226): inside a cargo target that is still being built, prune test
# executables past the newest N per stem and orphaned *.rcgu.o, never a file
# younger than the age floor or one a process holds open. Each guard has a cell
# that fails if the guard is removed, beside a positive control that prunes.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
export AMUX_CLEANUP_FSEVENTSD_CMD=true AMUX_CLEANUP_SQLITE_PS_CMD=true AMUX_CLEANUP_HEALTH_PROBES=off
export AMUX_CLEANUP_CLAUDE_TMP_ROOT="$FIX/no-claude-tmp" AMUX_CLEANUP_SCOPE_FILE=/dev/null AMUX_CLEANUP_SESSIONS_CMD=false
# The pruner now always adds TARGET_KEEP, whose default is the live shared target:
# a test must never prune the real ~/.amux/rust-build-target.
export AMUX_CLEANUP_TARGET_KEEP="$FIX/no-keep"
trap 'rm -rf -- "${FIX:?}"' EXIT
mkdir -p "$FIX/tmp"; export TMPDIR="$FIX/tmp"
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
ex() { if [ -e "$1" ]; then echo yes; else echo no; fi; }
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"
export AMUX_CLEANUP_STATE_DIR="$FIX/assess-state" AMUX_CLEANUP_RECLAIM_TRIGGER_CMD="${RECLAIM_TRIGGER_STUB:-echo 204}" AMUX_CLEANUP_DEMOTE_PS_CMD=true AMUX_CLEANUP_ESCALATE_CMD="true" AMUX_CLEANUP_HISTORY_CMD="true" AMUX_CLEANUP_CARD_CMD="true" AMUX_CLEANUP_VM_LIST_CMD="true"
TARGET_DEPTH=8; TARGET_SCAN_S=30
printf 'launchd 1 root cwd DIR 1,18 64 2 /\n' > "$FIX/lsof.base"
LSOF_CMD="cat $FIX/lsof.base"

# A target with five hashed copies of one test binary, aged 5h..1h apart, plus
# their .d/.dSYM, an rlib that is never an executable, and codegen units.
# Portable mtime "<hours> ago": CI runs this on Linux, where date -v does not exist.
aged() { python3 -c 'import os,sys,time; t=time.time()-float(sys.argv[2])*3600; os.utime(sys.argv[1],(t,t))' "$1" "$2"; }
mk() {
  R="$FIX/root"; rm -rf -- "${R:?}"; T="$R/proj/target"; D="$T/debug/deps"; mkdir -p "$D"
  printf 'Signature: 8a477f597d28d172789f06886806bc55\n' > "$T/CACHEDIR.TAG"; echo '{}' > "$T/.rustc_info.json"
  local i=0
  for h in 1111111111111111 2222222222222222 3333333333333333 4444444444444444 5555555555555555; do
    i=$((i+1)); f="$D/mvs_shard-$h"
    dd if=/dev/zero of="$f" bs=1024 count=64 2>/dev/null; chmod +x "$f"; : > "$f.d"; mkdir -p "$f.dSYM"
    aged "$f" $((6-i))     # 1111 oldest (5h), 5555 newest (1h)
  done
  dd if=/dev/zero of="$D/libmvs_shard-aaaaaaaaaaaaaaaa.rlib" bs=1024 count=64 2>/dev/null
  touch -t 202001010000 "$D/libmvs_shard-aaaaaaaaaaaaaaaa.rlib"
  : > "$D/mvs_shard-9999999999999999.mvs_shard.x-cgu.01.rcgu.o"; touch -t 202001010000 "$D/mvs_shard-9999999999999999.mvs_shard.x-cgu.01.rcgu.o"
  : > "$D/mvs_shard-8888888888888888.mvs_shard.x-cgu.02.rcgu.o"   # fresh: a compile in flight
}
run() { prune_superseded_artifacts "$R" "${1:-3}" "${2:-60}" "${3:-0}" > "$FIX/out.txt"; }

echo "0. a kept target over the cargo budget keeps one fewer per stem; under it, keep is unchanged"
mk
superseded_keep 3 "$T" 1 > "$FIX/keep.txt"
check "over the cap: keep lowered to 2" 2 "$SUPERSEDED_KEEP_EFFECTIVE"
check "and it says so" yes "$(grep -q 'verdict=superseded_keep_lowered_over_cap' "$FIX/keep.txt" && echo yes || echo no)"
superseded_keep 3 "$T" 999999999 > "$FIX/keep.txt"
check "under the cap: keep stays 3" 3 "$SUPERSEDED_KEEP_EFFECTIVE"
superseded_keep 1 "$T" 1 > "$FIX/keep.txt"
check "never below 1" 1 "$SUPERSEDED_KEEP_EFFECTIVE"
superseded_keep 3 "$T" 1 > /dev/null; run "$SUPERSEDED_KEEP_EFFECTIVE" 60
check "over the cap, the third newest goes too" no "$(ex "$D/mvs_shard-3333333333333333")"
check "over the cap, the two newest stay" yes "$(ex "$D/mvs_shard-4444444444444444")"

echo "1. keeps the newest N per stem and prunes the older ones with their .d/.dSYM"
mk; run 3 60
check "oldest copy pruned"          no  "$(ex "$D/mvs_shard-1111111111111111")"
check "its .d pruned"               no  "$(ex "$D/mvs_shard-1111111111111111.d")"
check "its .dSYM pruned"            no  "$(ex "$D/mvs_shard-1111111111111111.dSYM")"
check "second oldest pruned"        no  "$(ex "$D/mvs_shard-2222222222222222")"
check "third newest kept"           yes "$(ex "$D/mvs_shard-3333333333333333")"
check "second newest kept"          yes "$(ex "$D/mvs_shard-4444444444444444")"
check "newest kept"                 yes "$(ex "$D/mvs_shard-5555555555555555")"
check "an rlib is never an executable target" yes "$(ex "$D/libmvs_shard-aaaaaaaaaaaaaaaa.rlib")"
check "an old orphaned .rcgu.o pruned"        no  "$(ex "$D/mvs_shard-9999999999999999.mvs_shard.x-cgu.01.rcgu.o")"
check "a fresh .rcgu.o (compile in flight) kept" yes "$(ex "$D/mvs_shard-8888888888888888.mvs_shard.x-cgu.02.rcgu.o")"
check "the summary counts them" yes "$(grep -q 'pruned 2 superseded executable(s) + 1 orphaned .rcgu.o' "$FIX/out.txt" && echo yes || echo no)"

echo "1b. a shared target with no CACHEDIR.TAG is still pruned (disk RCA 20261025-190143)"
mk; rm -f "$T/CACHEDIR.TAG"
TARGET_KEEP_SAVED=$TARGET_KEEP; TARGET_KEEP="$FIX/elsewhere:$T"
run 3 60
TARGET_KEEP=$TARGET_KEEP_SAVED
check "its old orphaned .rcgu.o pruned"        no  "$(ex "$D/mvs_shard-9999999999999999.mvs_shard.x-cgu.01.rcgu.o")"
check "its oldest superseded copy pruned"      no  "$(ex "$D/mvs_shard-1111111111111111")"
check "its newest copy kept"                   yes "$(ex "$D/mvs_shard-5555555555555555")"
check "the summary says it came from the keep list" yes "$(grep -q 'targets 1 (1 shared without CACHEDIR.TAG)' "$FIX/out.txt" && echo yes || echo no)"

echo "2. the age floor: a superseded copy younger than the floor stays"
mk; run 3 360      # 6h floor: every copy is younger
check "superseded but young: kept" yes "$(ex "$D/mvs_shard-1111111111111111")"
mk; run 3 180      # 3h floor: 1111 (5h) and 2222 (4h) old enough
check "positive control: old enough is pruned" no "$(ex "$D/mvs_shard-1111111111111111")"

echo "3. a file a process holds open stays"
mk; printf 'mvs_shard 42 ethan txt REG 1,18 65536 9 %s\n' "$D/mvs_shard-1111111111111111" >> "$FIX/lsof.base"; run 3 60
check "open superseded copy kept" yes "$(ex "$D/mvs_shard-1111111111111111")"
check "the unopened one beside it pruned" no "$(ex "$D/mvs_shard-2222222222222222")"
printf 'launchd 1 root cwd DIR 1,18 64 2 /\n' > "$FIX/lsof.base"

echo "4. no lsof snapshot means nothing is deleted"
mk; LSOF_CMD=true run 3 60
check "lsof empty: oldest kept" yes "$(ex "$D/mvs_shard-1111111111111111")"
check "lsof empty: says UNMEASURED" yes "$(grep -q UNMEASURED "$FIX/out.txt" && echo yes || echo no)"

echo "5. dry run and the off switch delete nothing"
mk; run 3 60 1
check "dry run: oldest kept" yes "$(ex "$D/mvs_shard-1111111111111111")"
check "dry run: reports would prune" yes "$(grep -q 'would prune 2' "$FIX/out.txt" && echo yes || echo no)"
mk; run 0 60
check "keep=0: off" yes "$(ex "$D/mvs_shard-1111111111111111")"

echo "6. scale and budget (self RCA 20261018-103045: the first live run took ~30 min on 5830 candidates)"
mk; for i in $(seq 1 3000); do : > "$D/big-$i.x-cgu.$i.rcgu.o"; done
find "$D" -name 'big-*.rcgu.o' -exec touch -t 202001010000 {} +
awk 'BEGIN{for(i=0;i<40000;i++) printf "proc %d ethan txt REG 1,18 4096 %d /Applications/Some.app/Contents/lib%d.dylib\n", i, i, i}' >> "$FIX/lsof.base"
s0=$(date +%s); run 3 60; el=$(( $(date +%s) - s0 ))
check "3000 codegen units against a 40k-line lsof snapshot finish in under 30s (took ${el}s)" yes "$( [ "$el" -lt 30 ] && echo yes || echo no)"
check "and all of them were pruned" yes "$(grep -q '+ 3001 orphaned .rcgu.o' "$FIX/out.txt" && echo yes || echo no)"
printf 'launchd 1 root cwd DIR 1,18 64 2 /\n' > "$FIX/lsof.base"
mk; prune_superseded_artifacts "$R" 3 60 0 0 > "$FIX/out.txt"
check "a spent budget deletes nothing"   yes "$(ex "$D/mvs_shard-1111111111111111")"
check "and says complete=no"             yes "$(grep -q 'complete=no' "$FIX/out.txt" && echo yes || echo no)"

echo "7. the tick runs the arm"
case "$TARGET_ROOTS" in "$HOME/.amux/tmp@3:$HOME/.amux/tmp@9:"*) r=yes ;; *) r=no ;; esac
check "per-worker targets (~/.amux/tmp@3) are scanned before the deep walk" yes "$r"
check "wired into the tick" yes "$(grep -q 'prune_superseded_artifacts "$TARGET_ROOTS"' "$TICK" && echo yes || echo no)"

[ "$fails" -eq 0 ] && echo "PASS: superseded-artifact arm" || { echo "FAIL: $fails cell(s)"; exit 1; }
