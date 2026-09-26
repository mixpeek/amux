#!/bin/bash
# The idle cargo-target reaper in scripts/mac-cleanup-tick.sh (DESKT-51).
#
# On 2026-09-26 the box held 233 GB in 127 cache-signed directories, and nothing
# automated was looking: the debris reaper (SCHED-452) is off and its header
# excludes /private/tmp/claude-501 outright. One hand pass over the ones idle for
# 24h+ freed 66.8 GiB. This arm deletes tens of GB in OTHER lanes' trees, so each
# guard has a cell that fails if the guard is removed (ethos rule 7), and every
# "kept" outcome has a positive control: the same fixture must reap when the
# guard's condition is lifted, or the negative proves nothing.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
trap 'rm -rf -- "${FIX:?}"' EXIT
mkdir -p "$FIX/tmp" "$FIX/bin"
export TMPDIR="$FIX/tmp"
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
yn() { if "$@"; then echo yes; else echo no; fi; }
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"
DEFAULT_KEEP=$TARGET_KEEP; DEFAULT_ROOTS=$TARGET_ROOTS      # what the scheduler actually runs with, before this file overrides the knobs

# Knobs the arm reads. Small budgets: nothing here should ever wait on them.
TARGET_DEPTH=8; TARGET_SCAN_S=30; TARGET_WALK_S=30; TARGET_BUDGET_S=120
TARGET_KEEP=""
# One row that names nothing under test: the snapshot must be NON-EMPTY or the arm
# (correctly) reads it as "lsof saw nothing" and refuses to delete anything.
printf 'launchd 1 root cwd DIR 1,18 64 2 /\n' > "$FIX/lsof.base"
LSOF_CMD="cat $FIX/lsof.base"

# A cargo target root with REAL allocated bytes (dd, not sparse): 2 MB of build
# output, the signature tag, and .rustc_info.json. `old` backdates every file.
mk_target() { # <dir> <old|new>
  mkdir -p "$1/debug/deps"
  printf 'Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n' > "$1/CACHEDIR.TAG"
  echo '{}' > "$1/.rustc_info.json"
  dd if=/dev/zero of="$1/debug/deps/lib.rlib" bs=1024 count=2048 2>/dev/null
  if [ "$2" = old ]; then find "$1" -exec touch -t 202001010000 {} + ; fi
}
fresh_root() { R="$FIX/root.$1"; rm -rf -- "${R:?}"; mkdir -p "$R"; }
run() { reap_idle_cargo_targets "$R" 24 "${1:-0}" > "$FIX/out.txt"; }
summary() { grep 'cargo targets:' "$FIX/out.txt" | tail -1; }

echo "1. discovery: only a tagged target root with .rustc_info.json qualifies"
fresh_root disc
mk_target "$R/proj/target" old
mkdir -p "$R/py/uvcache"; printf 'Signature: 8a477f597d28d172789f06886806bc55\n' > "$R/py/uvcache/CACHEDIR.TAG"    # tag, no .rustc_info.json (a name find does NOT prune)
mkdir -p "$R/plain/target/debug"; echo '{}' > "$R/plain/target/.rustc_info.json"                                # .rustc_info.json, no tag
mk_target "$R/web/node_modules/pkg/target" old                                                                   # under a pruned name
find_cargo_targets "$R" 8 30 > "$FIX/found.txt"
check "the tagged cargo root is found"                 "yes" "$(yn grep -qx "$R/proj/target" "$FIX/found.txt")"
check "a tag without .rustc_info.json is NOT found"    "no"  "$(yn grep -q uvcache "$FIX/found.txt")"
check "a .rustc_info.json without the tag is NOT found" "no" "$(yn grep -q 'plain' "$FIX/found.txt")"
check "a target under node_modules is pruned, not found" "no" "$(yn grep -q node_modules "$FIX/found.txt")"
check "exactly one target was found"                   "1"   "$(grep -c . "$FIX/found.txt" | tr -d ' ')"
check "a finished scan says complete"                  "yes" "$TARGET_SCAN_COMPLETE"

echo "2. an idle target is reaped, and only the target"
fresh_root idle
mk_target "$R/proj/target" old; mkdir -p "$R/proj/src"; echo 'fn main(){}' > "$R/proj/src/main.rs"
run 0
check "the idle target is gone"                        "no"  "$(yn test -e "$R/proj/target")"
check "its sibling source file is untouched"           "yes" "$(yn test -f "$R/proj/src/main.rs")"
check "the counter says one reaped"                    "1"   "$TARGETS_REAPED"
check "the reaped size is real, not zero"              "yes" "$(yn test "$TARGETS_REAPED_KB" -ge 2048)"
check "the line names the path and its size"           "yes" "$(yn grep -Eq "reaped [0-9]+M $R/proj/target" "$FIX/out.txt")"
check "the summary says reaped 1"                      "yes" "$(summary | grep -q 'reaped 1 (' && echo yes || echo no)"

echo "3. the idle window is 24h: a write inside it keeps the target, a write outside it does not"
fresh_root active
mk_target "$R/proj/target" old
ago() { perl -e 'my $t=time-$ARGV[0]*3600; utime($t,$t,$ARGV[1])' "$1" "$2"; }   # set a file's mtime <hours> ago, portably
touch "$R/proj/target/debug/deps/fresh.o"
run 0
check "a write just now keeps the target"              "yes" "$(yn test -d "$R/proj/target")"
check "it is counted active"                           "yes" "$(summary | grep -q 'active 1' && echo yes || echo no)"
check "nothing reaped"                                 "0"   "$TARGETS_REAPED"
ago 2 "$R/proj/target/debug/deps/fresh.o"; run 0
check "a write 2 hours ago is still inside the window"  "yes" "$(yn test -d "$R/proj/target")"
ago 30 "$R/proj/target/debug/deps/fresh.o"; run 0
check "control: the same write 30 hours ago is outside it, and the target is reaped" "no" "$(yn test -e "$R/proj/target")"

echo "4. an open handle protects the target it is inside, and NOT a target whose name is a prefix of it"
fresh_root open
mk_target "$R/a/target" old; mk_target "$R/a/target-x" old
# The handle is inside target-x. The bare prefix ".../a/target" matches that row too,
# which is the bug a naive match has: the SHORTER name is wrongly protected.
{ cat "$FIX/lsof.base"; printf 'rustc 99 ethan txt REG 1,18 100 5 %s/a/target-x/debug/deps/lib.rlib\n' "$R"; } > "$FIX/lsof.open"
LSOF_CMD="cat $FIX/lsof.open"; run 0
check "the target with a handle inside survives"       "yes" "$(yn test -d "$R/a/target-x")"
check "its shorter-named sibling is NOT protected by that handle" "no" "$(yn test -e "$R/a/target")"
check "the summary counts one open"                    "yes" "$(summary | grep -q 'open 1' && echo yes || echo no)"
LSOF_CMD="cat $FIX/lsof.base"; run 0
check "control: with the handle gone target-x is reaped" "no" "$(yn test -e "$R/a/target-x")"
fresh_root cwd
mk_target "$R/b/target" old
{ cat "$FIX/lsof.base"; printf 'zsh 7 ethan cwd DIR 1,18 64 9 %s/b/target\n' "$R"; } > "$FIX/lsof.cwd"
LSOF_CMD="cat $FIX/lsof.cwd"; run 0
check "a process whose cwd IS the target protects it"  "yes" "$(yn test -d "$R/b/target")"
LSOF_CMD="cat $FIX/lsof.base"

echo "5. lsof that yields nothing means UNMEASURED, and nothing is deleted"
fresh_root unm
mk_target "$R/proj/target" old
LSOF_CMD="false"; run 0
check "lsof failing keeps the target"                  "yes" "$(yn test -d "$R/proj/target")"
check "the line says UNMEASURED"                       "yes" "$(grep -q 'UNMEASURED' "$FIX/out.txt" && echo yes || echo no)"
LSOF_CMD="true"; run 0
check "an EMPTY snapshot is also refused, not read as 'nothing open'" "yes" "$(yn test -d "$R/proj/target")"
check "an EMPTY snapshot says UNMEASURED too (the first probe, not the second look, is what refused it)" "yes" "$(grep -q 'UNMEASURED' "$FIX/out.txt" && echo yes || echo no)"
check "counter stays zero"                             "0"   "$TARGETS_REAPED"
LSOF_CMD="cat $FIX/lsof.base"; run 0
check "control: a working lsof reaps the same target"  "no"  "$(yn test -e "$R/proj/target")"

echo "6. a protected (shared) target is never a candidate"
fresh_root shared
mk_target "$R/shared/target" old; mk_target "$R/shared-2/target" old
TARGET_KEEP="$R/shared"; run 0
check "the protected target survives, though idle"     "yes" "$(yn test -d "$R/shared/target")"
check "a path that only shares its NAME PREFIX is not protected" "no" "$(yn test -e "$R/shared-2/target")"
check "the summary counts one shared"                  "yes" "$(summary | grep -q 'shared 1' && echo yes || echo no)"
TARGET_KEEP=""

echo "7. dry run reports and deletes nothing"
fresh_root dry
mk_target "$R/proj/target" old
run 1
check "the target survives a dry run"                  "yes" "$(yn test -d "$R/proj/target")"
check "the line says would reap"                       "yes" "$(grep -q "would reap .* $R/proj/target" "$FIX/out.txt" && echo yes || echo no)"
check "the counter is zero"                            "0"   "$TARGETS_REAPED"
run 0
check "control: the same tree is reaped when not dry"  "no"  "$(yn test -e "$R/proj/target")"

echo "8. the time budget guards BOTH phases, independently"
fresh_root bud
mk_target "$R/proj/target" old
TARGET_BUDGET_S=0; run 0
check "a spent budget deletes nothing"                 "yes" "$(yn test -d "$R/proj/target")"
check "the summary counts it over budget"              "yes" "$(summary | grep -q 'over budget 1' && echo yes || echo no)"
TARGET_BUDGET_S=120; run 0
check "control: within budget it is reaped"            "no"  "$(yn test -e "$R/proj/target")"
# Walk phase: two candidates, each walk takes 3s, budget 2s (second-granularity clocks, so the
# margin is a full second either side). Only the first may be walked.
fresh_root budw
mk_target "$R/one/target" old; mk_target "$R/two/target" old
orig_dir_idle=$(declare -f dir_idle)
dir_idle() { echo x >> "$FIX/walks"; sleep 3; return 0; }
rm -f "$FIX/walks"; : > "$FIX/walks"; TARGET_BUDGET_S=2; run 0
check "the second candidate is not even walked once the budget is spent" "1" "$(grep -c x "$FIX/walks" | tr -d ' ')"
eval "$orig_dir_idle"
# Delete phase: the walk is instant but sizing (du) takes 3s, budget 2s. The delete loop must stop by itself.
fresh_root budd
mk_target "$R/proj/target" old
printf '#!/bin/bash\nsleep 3\nexec /usr/bin/du "$@"\n' > "$FIX/bin/du"; chmod +x "$FIX/bin/du"
OLDPATH=$PATH; PATH="$FIX/bin:$PATH"; TARGET_BUDGET_S=2; run 0; PATH=$OLDPATH; rm "$FIX/bin/du"
check "a budget spent between selection and delete keeps the target" "yes" "$(yn test -d "$R/proj/target")"
check "and the summary counts it over budget"          "yes" "$(summary | grep -q 'over budget 1' && echo yes || echo no)"
TARGET_BUDGET_S=120

echo "8b. roots may carry a depth, may overlap, and a target that vanished is not 'reaped'"
fresh_root dep
mk_target "$R/a/b/c/target" old                         # tag at depth 5 below the root
find_cargo_targets "$R@2" 8 30 > "$FIX/found.txt"
check "path@2 does NOT reach a target at depth 5"       "0"   "$(grep -c . "$FIX/found.txt" | tr -d ' ')"
find_cargo_targets "$R@6" 8 30 > "$FIX/found.txt"
check "control: path@6 finds it"                        "1"   "$(grep -c . "$FIX/found.txt" | tr -d ' ')"
find_cargo_targets "$R" 2 30 > "$FIX/found.txt"
check "a bare root uses the default depth argument"     "0"   "$(grep -c . "$FIX/found.txt" | tr -d ' ')"
reap_idle_cargo_targets "$R@8:$R@6:$R" 24 0 > "$FIX/out.txt"
check "overlapping roots list the target once"          "1"   "$TARGETS_FOUND"
check "and it is reaped exactly once"                   "1"   "$TARGETS_REAPED"
fresh_root gone
mk_target "$R/real/target" old
orig_find=$(declare -f find_cargo_targets)
find_cargo_targets() { printf '%s\n' "$R/vanished/target" "$R/real/target"; TARGET_SCAN_COMPLETE=yes; TARGET_ROOTS_SCANNED=1; }
reap_idle_cargo_targets "$R" 24 0 > "$FIX/out.txt"
check "a candidate that no longer exists is not counted eligible" "1" "$TARGETS_ELIGIBLE"
check "nor reaped a second time at size zero"          "1"   "$TARGETS_REAPED"
eval "$orig_find"

echo "9. a walk that cannot finish is UNKNOWN, and the target is kept"
fresh_root walk
mk_target "$R/proj/target" old
printf '#!/bin/bash\nexec sleep 5\n' > "$FIX/bin/find"; chmod +x "$FIX/bin/find"
OLDPATH=$PATH; PATH="$FIX/bin:$PATH"
rc=0; dir_idle "$R/proj/target" 24 1 || rc=$?
PATH=$OLDPATH
check "dir_idle returns 2 (unknown) when the walk is cut off" "2" "$rc"
rc=0; dir_idle "$R/proj/target" 24 30 || rc=$?
check "control: with the real find the same dir is idle (0)"   "0" "$rc"
orig_dir_idle=$(declare -f dir_idle)
dir_idle() { return 2; }                 # the reap loop must honour rc 2, whatever produced it
run 0
check "an unknown walk keeps the target"               "yes" "$(yn test -d "$R/proj/target")"
check "the summary counts one unmeasured"              "yes" "$(summary | grep -q 'unmeasured 1' && echo yes || echo no)"
eval "$orig_dir_idle"                    # restore the real dir_idle (re-sourcing would reset every knob above)

echo "10. a scan cut off by its budget says complete=no"
fresh_root scan
mk_target "$R/proj/target" old
OLDPATH=$PATH; PATH="$FIX/bin:$PATH"
find_cargo_targets "$R" 8 1 > "$FIX/found.txt"
PATH=$OLDPATH
check "an interrupted scan reports complete=no"        "no"  "$TARGET_SCAN_COMPLETE"
find_cargo_targets "$R" 8 30 > "$FIX/found.txt"
check "control: an uninterrupted scan reports complete=yes" "yes" "$TARGET_SCAN_COMPLETE"
check "control: and it finds the target"               "1"   "$(grep -c . "$FIX/found.txt" | tr -d ' ')"

echo "11. a delete that leaves the directory behind is reported FAILED, not counted"
fresh_root fail
mk_target "$R/proj/target" old
rm "$FIX/bin/find"
printf '#!/bin/bash\nexit 0\n' > "$FIX/bin/rm"; chmod +x "$FIX/bin/rm"   # a rm that claims success and removes nothing
OLDPATH=$PATH; PATH="$FIX/bin:$PATH"; run 0; PATH=$OLDPATH
check "the target is still there"                      "yes" "$(yn test -d "$R/proj/target")"
check "it is NOT counted as reaped"                    "0"   "$TARGETS_REAPED"
check "the line says FAILED"                           "yes" "$(grep -q 'FAILED to remove' "$FIX/out.txt" && echo yes || echo no)"
check "the summary counts one failed"                  "yes" "$(summary | grep -q 'failed 1' && echo yes || echo no)"
rm "$FIX/bin/rm"

echo "12. a build that starts AFTER selection is caught by the second look before the delete"
fresh_root late
mk_target "$R/proj/target" old
printf '%s\n' '#!/bin/bash' 'n=$(cat "$1" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$1"' \
  "cat \"$FIX/lsof.base\"" \
  "if [ \"\$n\" -ge 2 ]; then printf 'rustc 99 ethan cwd DIR 1,18 64 9 %s/proj/target\\n' \"$R\"; fi" > "$FIX/lsof_seq.sh"
rm -f "$FIX/lsof.count"; LSOF_CMD="bash $FIX/lsof_seq.sh $FIX/lsof.count"; run 0
check "the target survives: the second snapshot shows a handle" "yes" "$(yn test -d "$R/proj/target")"
check "it is counted open, not reaped"                 "0"   "$TARGETS_REAPED"
LSOF_CMD="cat $FIX/lsof.base"; run 0
check "control: with a quiet lsof throughout it is reaped"   "no"  "$(yn test -e "$R/proj/target")"

echo "13. the defaults the scheduler runs with"
check "the amux shared target is protected by default"  "yes" "$(printf '%s' "$DEFAULT_KEEP" | grep -Eq "\.amux/rust-build-target(:|\$)" && echo yes || echo no)"
check "the ao shared target is protected by default"    "yes" "$(printf '%s' "$DEFAULT_KEEP" | grep -Eq "\.ao/data/cargo-target-shared(:|\$)" && echo yes || echo no)"
check "the per-session scratchpads are scanned by default" "yes" "$(printf '%s' "$DEFAULT_ROOTS" | grep -q "/private/tmp/claude-$(id -u)@" && echo yes || echo no)"
if [ "$(uname)" = Darwin ]; then
  # The path goes in the environment, never as $1: a sourced tick parses its own positional args.
  unset_roots=$(TICK_PATH="$TICK" env -u TMPDIR bash -c 'AMUX_CLEANUP_LIB_ONLY=1 . "$TICK_PATH"; printf "%s" "$TARGET_ROOTS"' 2>/dev/null) || unset_roots=""
  check "with TMPDIR unset the temp root is the real per-user dir, not the /tmp symlink" "yes" "$(printf '%s' "$unset_roots" | grep -q '/var/folders/' && echo yes || echo no)"
fi

echo "14. the whole tick reports the arm and deletes nothing in --dry-run (macOS only)"
if [ "$(uname)" = Darwin ]; then
  fresh_root e2e
  mk_target "$R/proj/target" old
  tick_out=$(AMUX_CLEANUP_TARGET_ROOTS="$R" AMUX_CLEANUP_TARGET_KEEP="" AMUX_CLEANUP_LSOF_CMD="cat $FIX/lsof.base" bash "$TICK" --dry-run 2>&1)
  check "the tick prints the cargo-targets line"       "yes" "$(printf '%s' "$tick_out" | grep -q 'mac-cleanup: cargo targets: found 1' && echo yes || echo no)"
  check "the tick says it WOULD reap the fixture"      "yes" "$(printf '%s' "$tick_out" | grep -q "would reap .* $R/proj/target" && echo yes || echo no)"
  check "the done line carries targets_reaped=0"       "yes" "$(printf '%s' "$tick_out" | grep -q 'targets_reaped=0' && echo yes || echo no)"
  check "the fixture target survived the dry run"      "yes" "$(yn test -d "$R/proj/target")"
else
  echo "  skip macOS-only cell (the tick's probes are macOS commands): not run on $(uname)"
fi

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-targets — all checks passed"; exit 0; fi
echo "mac-cleanup-targets: $fails check(s) FAILED"; exit 1
