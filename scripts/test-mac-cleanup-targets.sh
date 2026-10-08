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
export AMUX_CLEANUP_FSEVENTSD_CMD=true   # the live fseventsd must not decide a test (DESKT-81)
export AMUX_CLEANUP_CLAUDE_TMP_ROOT="$FIX/no-claude-tmp"   # never the live session temp tree (DESKT-77)
trap 'rm -rf -- "${FIX:?}"' EXIT
mkdir -p "$FIX/tmp" "$FIX/bin"
export TMPDIR="$FIX/tmp"
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
yn() { if "$@"; then echo yes; else echo no; fi; }
export AMUX_CLEANUP_SCOPE_FILE=/dev/null   # the live global scope must not configure a test (DESKT-72)
export AMUX_CLEANUP_SESSIONS_CMD=false        # the VM-reference report must not read the live fleet (AMUX-5491)
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"
export AMUX_CLEANUP_STATE_DIR="$FIX/assess-state" AMUX_CLEANUP_ESCALATE_CMD="true" AMUX_CLEANUP_HISTORY_CMD="true" AMUX_CLEANUP_CARD_CMD="true" AMUX_CLEANUP_VM_LIST_CMD="true"   # never page a real lane from a test (DESKT-57)
DEFAULT_KEEP=$TARGET_KEEP; DEFAULT_ROOTS=$TARGET_ROOTS; DEFAULT_VM_PRUNE=$VM_PRUNE_CMD; DEFAULT_VM_CAP=$VM_PRUNE_CAP_CMD; DEFAULT_VM_IMAGE=$VM_IMAGE_PRUNE_CMD; DEFAULT_VM_MAX_USED=$VM_PRUNE_MAX_USED; DEFAULT_VM_STOP=$VM_STOP_CMD      # what the scheduler actually runs with, before this file overrides the knobs

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
orig_dir_idle=$(declare -f dir_idle); orig_find_w=$(declare -f find_cargo_targets)
# The scan is stubbed so only the WALK phase spends the budget: on a loaded Mac
# (load 100+) the real fixture scan alone could use up a 3s budget, and the cell
# then measured the scan instead of the walk guard.
find_cargo_targets() { printf '%s\n' "$R/one/target" "$R/two/target"; TARGET_SCAN_COMPLETE=yes; TARGET_ROOTS_SCANNED=1; }
dir_idle() { echo x >> "$FIX/walks"; sleep 6; return 0; }
rm -f "$FIX/walks"; : > "$FIX/walks"; TARGET_BUDGET_S=3; run 0
check "the second candidate is not even walked once the budget is spent" "1" "$(grep -c x "$FIX/walks" | tr -d ' ')"
eval "$orig_dir_idle"; eval "$orig_find_w"
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

echo "12b. under disk pressure the idle floor drops (DESKT-62)"
check "plenty of disk keeps the normal floor"          "24" "$(effective_target_idle_h 600 24 250 6)"
check "under the threshold the tight floor applies"    "6"  "$(effective_target_idle_h 155 24 250 6)"
check "exactly at the threshold keeps the normal floor" "24" "$(effective_target_idle_h 250 24 250 6)"
check "an unmeasured disk keeps the normal floor"      "24" "$(effective_target_idle_h -1 24 250 6)"
check "a tight floor above the normal one is ignored"  "24" "$(effective_target_idle_h 100 24 250 48)"
fresh_root tight
mk_target "$R/proj/target" old
ago 10 "$R/proj/target/debug/deps/lib.rlib" 2>/dev/null || perl -e 'my $t=time-10*3600; utime($t,$t,$ARGV[0])' "$R/proj/target/debug/deps/lib.rlib"
reap_idle_cargo_targets "$R" 24 0 > "$FIX/out.txt"
check "a target last written 10h ago survives the normal 24h floor" "yes" "$(yn test -d "$R/proj/target")"
reap_idle_cargo_targets "$R" "$(effective_target_idle_h 155 24 250 6)" 0 > "$FIX/out.txt"
check "and is reaped at the tight 6h floor"            "no"  "$(yn test -e "$R/proj/target")"

echo "12c. under disk pressure, build cache in running VMs is pruned and nothing else (DESKT-69)"
VMREC="$FIX/vm-calls"; : > "$VMREC"
printf '%s\n' '{"name":"gs12-a","status":"Running"}' '{"name":"gs12-b","status":"Stopped"}' '{"name":"gs12-c","status":"Running"}' > "$FIX/vms.json"
printf '#!/bin/bash\necho "$@" >> %s\necho "Total:\t13.17GB"\n' "$VMREC" > "$FIX/vmrec.sh"; chmod +x "$FIX/vmrec.sh"
VM_LIST_CMD="cat $FIX/vms.json"; VM_PRUNE_CMD="$FIX/vmrec.sh prune PROFILE"; VM_TRIM_CMD="$FIX/vmrec.sh trim PROFILE"; VM_STEP_S=20; VM_PRUNE_MAX_USED=0
check "only RUNNING profiles are listed" "gs12-a gs12-c" "$(running_vm_profiles | tr '\n' ' ' | sed 's/ $//')"
prune_vm_build_caches 0 > "$FIX/out.txt"
check "each running VM is pruned and trimmed once" "4" "$(grep -c . "$VMREC" | tr -d ' ')"
check "the stopped VM is never touched" "0" "$(grep -c gs12-b "$VMREC" | tr -d ' ')"
check "the prune command is build-cache only" "yes" "$(grep -q '^prune gs12-a' "$VMREC" && ! grep -q -E 'image|volume|system' "$VMREC" && echo yes || echo no)"
check "the summary counts two pruned" "yes" "$(grep -q 'pruned 2 running VM' "$FIX/out.txt" && echo yes || echo no)"
: > "$VMREC"; prune_vm_build_caches 1 > "$FIX/out.txt"
check "dry run runs nothing" "0" "$(grep -c . "$VMREC" | tr -d ' ')"
check "and says what it would do" "2" "$(grep -c 'would prune build cache' "$FIX/out.txt" | tr -d ' ')"
VM_PRUNE_CMD="false"; : > "$VMREC"; prune_vm_build_caches 0 > "$FIX/out.txt"
check "a failed prune is reported and does not trim" "yes" "$(grep -q 'build-cache prune FAILED' "$FIX/out.txt" && ! grep -q trim "$VMREC" && echo yes || echo no)"
check "and is counted failed" "yes" "$(grep -q 'failed 2' "$FIX/out.txt" && echo yes || echo no)"
VM_LIST_CMD="true"; prune_vm_build_caches 0 > "$FIX/out.txt"
check "no running VM says so" "yes" "$(grep -q 'no running colima VM' "$FIX/out.txt" && echo yes || echo no)"
check "the default prune is build cache only" "yes" "$(printf '%s' "$DEFAULT_VM_PRUNE" | grep -q 'builder prune -af' && ! printf '%s' "$DEFAULT_VM_PRUNE" | grep -q -E 'system|image|volume' && echo yes || echo no)"
check "and it releases finished builds' cache older than 6 h, not only dangling cache" "yes" "$(printf '%s' "$DEFAULT_VM_PRUNE" | grep -q -- '-af --filter until=AGE' && [ "$VM_PRUNE_AGE" = 6h ] && echo yes || echo no)"
# 2026-10-06: the gs12 VM wrote build cache at ~78G/h, so 6h-old cache was
# almost none of it. Under the urgent floor the window is 2h.
echo "12e. an image's age is its last TAG, not its creation; in-use images are kept (disk RCA 20261007-081932)"
# Python, not date: BSD date -r takes an epoch, GNU date -r a file (CI is Linux).
NOW=$(date -u +%s); iso(){ python3 -c 'import sys,time;print(time.strftime("%Y-%m-%dT%H:%M:%S.123456789Z",time.gmtime(int(sys.argv[1]))))' "$1"; }
cat > "$FIX/images.json" <<JSON
[{"Id":"sha256:fresh","Created":"$(iso $((NOW-864000)))","Metadata":{"LastTagTime":"$(iso $((NOW-60)))"},"Size":9000000000},
 {"Id":"sha256:stale","Created":"$(iso $((NOW-864000)))","Metadata":{"LastTagTime":"$(iso $((NOW-86400)))"},"Size":2000000000},
 {"Id":"sha256:pinned","Created":"$(iso $((NOW-864000)))","Metadata":{"LastTagTime":"$(iso $((NOW-86400)))"},"Size":5000000000},
 {"Id":"sha256:pulled","Created":"$(iso $((NOW-864000)))","Metadata":{"LastTagTime":"0001-01-01T00:00:00Z"},"Size":1000000000}]
JSON
RMI="$FIX/rmi.txt"; : > "$RMI"
cat > "$FIX/fakedocker.sh" <<SH
#!/bin/bash
shift 2
case "\$1 \$2" in
  "images -q") printf 'sha256:fresh\nsha256:stale\nsha256:pinned\nsha256:pulled\n' ;;
  "ps -aq") echo c1 ;;
  "inspect -f") echo sha256:pinned ;;
  "image inspect") cat "$FIX/images.json" ;;
  "rmi "*) echo "\$2" >> "$RMI" ;;
esac
SH
chmod +x "$FIX/fakedocker.sh"
out=$(VM_DOCKER="$FIX/fakedocker.sh" vm_image_prune_by_tag colima-x 6h)
check "a fresh tag on an old image is kept" "no" "$(yn grep -q fresh "$RMI")"
check "a stale tag nobody uses is removed" "yes" "$(yn grep -q stale "$RMI")"
check "an image a container uses is kept" "no" "$(yn grep -q pinned "$RMI")"
check "a never-tagged image falls back to its creation time" "yes" "$(yn grep -q pulled "$RMI")"
check "the summary reports bytes and its verdict" "yes" "$(yn sh -c 'printf "%s" "$1" | grep -q "Total reclaimed space: 3.00GB (2 image(s), verdict=vm_images_pruned_by_tag_age)"' _ "$out")"
check "the default image prune is the tag-age function" "yes" "$(yn sh -c 'printf "%s" "$1" | grep -q "^vm_image_prune_by_tag colima-PROFILE AGE$"' _ "$DEFAULT_VM_IMAGE")"

echo "12d. the build cache is also capped by size, after the age prune and before the trim (disk RCA 20261007-081932)"
VM_LIST_CMD="cat $FIX/vms.json"; VM_PRUNE_CMD="$FIX/vmrec.sh prune PROFILE"; VM_PRUNE_CAP_CMD="$FIX/vmrec.sh cap PROFILE CAP"; VM_PRUNE_MAX_USED=40gb; : > "$VMREC"
prune_vm_build_caches 0 > "$FIX/out.txt"
check "each running VM is capped at the configured size" "2" "$(grep -c '^cap [^ ]* 40gb$' "$VMREC" | tr -d ' ')"
check "the cap runs after the age prune and before the trim" "prune cap trim" "$(grep 'gs12-a' "$VMREC" | cut -d' ' -f1 | tr '\n' ' ' | sed 's/ $//')"
check "the cap logs its verdict" "2" "$(grep -c 'verdict=vm_build_cache_capped' "$FIX/out.txt" | tr -d ' ')"
VM_PRUNE_MAX_USED=0; : > "$VMREC"; prune_vm_build_caches 0 > "$FIX/out.txt"
check "0 disables the cap" "0" "$(grep -c '^cap ' "$VMREC" | tr -d ' ')"
check "the default cap is the measuring function" "yes" "$(printf '%s' "$DEFAULT_VM_CAP" | grep -q '^vm_build_cache_cap colima-PROFILE CAP$' && [ "$DEFAULT_VM_MAX_USED" = 40gb ] && echo yes || echo no)"
DUREC="$FIX/du.rec"; : > "$DUREC"
cat > "$FIX/dudocker.sh" <<SH
#!/bin/bash
shift 2
if [ "\$1 \$2" = "builder du" ]; then printf 'Private:\t1GB\nTotal:\t%s\n' "\$DU_TOTAL"; exit 0; fi
echo "\$@" >> "$DUREC"; printf 'Total:\t21.81GB\n'
SH
chmod +x "$FIX/dudocker.sh"
out=$(DU_TOTAL=87.84GB VM_DOCKER="$FIX/dudocker.sh" VM_PRUNE_CAP_AGE=1h vm_build_cache_cap colima-x 40gb)
check "over the cap, the cache is pruned by the short window" "yes" "$(yn grep -q 'prune -af --filter until=1h' "$DUREC")"
check "and the reclaimed total is reported" "yes" "$(yn sh -c 'printf "%s" "$1" | grep -q "Total:.21.81GB"' _ "$out")"
: > "$DUREC"; out=$(DU_TOTAL=12.5GB VM_DOCKER="$FIX/dudocker.sh" vm_build_cache_cap colima-x 40gb)
check "within the cap nothing is pruned" "0" "$(grep -c . "$DUREC" | tr -d ' ')"
: > "$DUREC"; out=$(DU_TOTAL=512MB VM_DOCKER="$FIX/dudocker.sh" vm_build_cache_cap colima-x 1gb)
check "units compare across MB and GB" "0" "$(grep -c . "$DUREC" | tr -d ' ')"
VM_LIST_CMD="cat $FIX/vms.json"; VM_PRUNE_CMD="$FIX/vmrec.sh prune PROFILE AGE"; : > "$VMREC"
prune_vm_build_caches 0 60 > "$FIX/out.txt"
check "under the urgent floor the prune window is 2h" "yes" "$(grep -q '^prune gs12-a 2h' "$VMREC" && grep -q 'pruning cache unused for 2h' "$FIX/out.txt" && echo yes || echo no)"
: > "$VMREC"; prune_vm_build_caches 0 180 > "$FIX/out.txt"
check "above it the window stays 6h" "yes" "$(grep -q '^prune gs12-a 6h' "$VMREC" && echo yes || echo no)"

echo "12d. idle VMs are stopped under pressure, busy or unmeasured ones never (DESKT-70)"
printf '%s\n' '{"name":"idle","status":"Running"}' '{"name":"busy","status":"Running"}' '{"name":"quiet-but-working","status":"Running"}' '{"name":"blind","status":"Running"}' '{"name":"off","status":"Stopped"}' > "$FIX/vms2.json"
cat > "$FIX/vmload.sh" <<'VL'
#!/bin/bash
case "$1" in idle|quiet-but-working|healthy-stack) printf '0.05 0.10 0.12 1/300 9\n86400.0 1000.0\n';; young) printf '0.01 0.01 0.01 1/300 9\n600.0 50.0\n';; busy) printf '6.1 6.0 5.9 9/300 9\n86400.0 10.0\n';; *) exit 1;; esac
VL
cat > "$FIX/vmev.sh" <<'VE'
#!/bin/bash
case "$1" in idle|healthy-stack) printf 'exec_create\nexec_start\nexec_die\n';; quiet-but-working) printf 'exec_start\ncreate\nstart\n';; *) :;; esac
VE
printf '#!/bin/bash\necho "$1" >> %s\n' "$FIX/stops" > "$FIX/vmstop.sh"; chmod +x "$FIX"/vmload.sh "$FIX"/vmev.sh "$FIX"/vmstop.sh
cat > "$FIX/vmps.sh" <<'VP'
#!/bin/bash
case "$1" in idle) printf 'gs12-pypi-cache\n';; healthy-stack) printf 'gs12-pypi-cache\ngs12-restore-proof-gr98\n';; noprobe) exit 1;; *) :;; esac
VP
chmod +x "$FIX/vmps.sh"
VM_LIST_CMD="cat $FIX/vms2.json"; VM_LOAD_CMD="$FIX/vmload.sh PROFILE"; VM_EVENTS_CMD="$FIX/vmev.sh PROFILE"; VM_STOP_CMD="$FIX/vmstop.sh PROFILE"; VM_PS_CMD="$FIX/vmps.sh PROFILE"; VM_IDLE_IGNORE="gs12-pypi-cache"
STATE_DIR="$FIX/vmstate"; : > "$FIX/stops"
rc=0; vm_is_idle idle || rc=$?;              check "healthcheck-only activity at low load is idle" "0" "$rc"
rc=0; vm_is_idle busy || rc=$?;              check "a loaded VM is busy" "1" "$rc"
rc=0; vm_is_idle quiet-but-working || rc=$?; check "a container created in the window is busy, even at low load" "1" "$rc"
rc=0; vm_is_idle blind || rc=$?;             check "an unreadable guest is unmeasured, not idle" "2" "$rc"
rc=0; vm_is_idle young || rc=$?;             check "a VM up less than the window is busy, however quiet" "1" "$rc"
rc=0; vm_is_idle healthy-stack || rc=$?;     check "a running proof stack with only healthchecks is busy (goal-shared 2026-10-04 18:06Z)" "1" "$rc"
stop_idle_vms 0 "memory pressure 2" > "$FIX/out.txt"
check "only the idle VM is stopped" "idle" "$(tr '\n' ' ' < "$FIX/stops" | sed 's/ $//')"
check "the stop is recorded with its restore command" "yes" "$(grep -q 'stopped colima VM idle .*Restore: colima start -p idle' "$FIX/vmstate/vm-stops.log" && echo yes || echo no)"
check "the summary counts kept busy and unmeasured" "yes" "$(grep -q 'stopped 1, kept busy 2, kept unmeasured 1' "$FIX/out.txt" && echo yes || echo no)"
: > "$FIX/stops"; stop_idle_vms 1 "memory pressure 2" > "$FIX/out.txt"
check "dry run stops nothing" "0" "$(grep -c . "$FIX/stops" | tr -d ' ')"
check "and names what it would stop" "yes" "$(grep -q 'would stop idle colima VM idle' "$FIX/out.txt" && echo yes || echo no)"
check "the default stop is a stop, never a delete" "yes" "$(printf '%s' "$DEFAULT_VM_STOP" | grep -q '^colima stop -p PROFILE$' && echo yes || echo no)"

echo "12c2. every running VM is trimmed every tick, not only when the disk is tight (escalation 20261005-013827)"
printf '#!/bin/bash\necho "$1" >> %s\n' "$FIX/trims" > "$FIX/vmtrim.sh"; chmod +x "$FIX/vmtrim.sh"
printf '%s\n' '{"name":"a","status":"Running"}' '{"name":"b","status":"Running"}' '{"name":"off","status":"Stopped"}' > "$FIX/vmst.json"
_lc=$VM_LIST_CMD; _tc=$VM_TRIM_CMD; VM_LIST_CMD="cat $FIX/vmst.json"; VM_TRIM_CMD="$FIX/vmtrim.sh PROFILE"; : > "$FIX/trims"
trim_vms 0 > "$FIX/out.txt"
check "both running VMs are trimmed, the stopped one is not" "a b" "$(tr '\n' ' ' < "$FIX/trims" | sed 's/ $//')"
check "and the line counts them" "yes" "$(grep -q 'vm trim: trimmed 2 running VM(s), failed 0' "$FIX/out.txt" && echo yes || echo no)"
: > "$FIX/trims"; trim_vms 1 > "$FIX/out.txt"
check "a dry run trims nothing" "0" "$(grep -c . "$FIX/trims" | tr -d ' ')"
echo "12c3. the build-cache cap runs on a normal tick too, before the trim (disk RCA 20261008-124643)"
printf '#!/bin/bash\necho "cap $1 $2" >> %s\n' "$FIX/trims" > "$FIX/vmcap.sh"; chmod +x "$FIX/vmcap.sh"
_cc=$VM_PRUNE_CAP_CMD; _mu=$VM_PRUNE_MAX_USED; VM_PRUNE_CAP_CMD="$FIX/vmcap.sh PROFILE CAP"; VM_PRUNE_MAX_USED=40gb; : > "$FIX/trims"
trim_vms 0 > "$FIX/out.txt"
check "each running VM is capped, then trimmed" "cap a 40gb|a|cap b 40gb|b" "$(tr '\n' '|' < "$FIX/trims" | sed 's/|$//')"
check "the cap logs its verdict on a normal tick" "2" "$(grep -c 'verdict=vm_build_cache_capped' "$FIX/out.txt" | tr -d ' ')"
VM_PRUNE_MAX_USED=0; : > "$FIX/trims"; trim_vms 0 > "$FIX/out.txt"
check "0 disables the cap on a normal tick" "a b" "$(tr '\n' ' ' < "$FIX/trims" | sed 's/ $//')"
VM_PRUNE_CAP_CMD=$_cc; VM_PRUNE_MAX_USED=$_mu
check "the tick calls trim_vms when the disk is not tight" "yes" "$(grep -q 'then prune_vm_build_caches "$DRY" "$tgt_free"; else trim_vms "$DRY"; fi' "$TICK" && echo yes || echo no)"
VM_LIST_CMD=$_lc; VM_TRIM_CMD=$_tc

echo "12d2. Docker Desktop is quit under pressure only when idle the whole window (escalation 20261004-213936)"
printf '#!/bin/bash\necho dd >> %s\n' "$FIX/ddstops" > "$FIX/ddstop.sh"; chmod +x "$FIX/ddstop.sh"
DD_STOP_CMD="$FIX/ddstop.sh"; DD_PID_CMD="echo 4242"; DD_UP_CMD="echo 3-00:00:00"; DD_PS_CMD="true"; DD_EVENTS_CMD="printf exec_start\\n"
: > "$FIX/ddstops"; stop_idle_docker_desktop 0 "memory pressure 2" > "$FIX/out.txt"
check "an idle Docker Desktop is quit" "1" "$(grep -c . "$FIX/ddstops" | tr -d ' ')"
check "and the quit is recorded with its restore command" "yes" "$(grep -q "quit Docker Desktop .*Restore: open -a 'Docker Desktop'" "$FIX/vmstate/vm-stops.log" && echo yes || echo no)"
: > "$FIX/ddstops"; DD_PS_CMD="echo some-stack"; stop_idle_docker_desktop 0 "x" > "$FIX/out.txt"
check "a running container keeps it" "0" "$(grep -c . "$FIX/ddstops" | tr -d ' ')"
DD_PS_CMD="true"; DD_EVENTS_CMD="printf build\\n"; stop_idle_docker_desktop 0 "x" > "$FIX/out.txt"
check "a docker event in the window keeps it" "0" "$(grep -c . "$FIX/ddstops" | tr -d ' ')"
DD_EVENTS_CMD="false"; stop_idle_docker_desktop 0 "x" > "$FIX/out.txt"
check "unreadable events keep it" "0" "$(grep -c . "$FIX/ddstops" | tr -d ' ')"
DD_EVENTS_CMD="true"; DD_UP_CMD="echo 05:00"; stop_idle_docker_desktop 0 "x" > "$FIX/out.txt"
check "up less than the window keeps it" "0" "$(grep -c . "$FIX/ddstops" | tr -d ' ')"
DD_UP_CMD="echo 3-00:00:00"; DD_PID_CMD="true"; stop_idle_docker_desktop 0 "x" > "$FIX/out.txt"
check "not running is a no-op that says so" "yes" "$(grep -q 'docker desktop: not running' "$FIX/out.txt" && [ ! -s "$FIX/ddstops" ] && echo yes || echo no)"
DD_PID_CMD="echo 4242"; stop_idle_docker_desktop 1 "x" > "$FIX/out.txt"
check "a dry run quits nothing and names it" "yes" "$(grep -q 'would quit idle Docker Desktop' "$FIX/out.txt" && [ ! -s "$FIX/ddstops" ] && echo yes || echo no)"

echo "12e. thresholds come from amux's global scope, and an explicit env value wins (DESKT-72)"
printf 'OTHER=1\nAMUX_CLEANUP_TARGET_TIGHT_FREE_GB="400"\nAMUX_CLEANUP_VM_IDLE_MIN=90\n' > "$FIX/scope.env"
got=$(env -u AMUX_CLEANUP_TARGET_TIGHT_FREE_GB -u AMUX_CLEANUP_VM_IDLE_MIN AMUX_CLEANUP_SCOPE_FILE="$FIX/scope.env" TICK_PATH="$TICK" \
      bash -c 'AMUX_CLEANUP_LIB_ONLY=1 . "$TICK_PATH"; printf "%s %s" "$TARGET_TIGHT_FREE_GB" "$VM_IDLE_MIN"')
check "a scope value configures the tick (quotes stripped)" "400 90" "$got"
got=$(AMUX_CLEANUP_TARGET_TIGHT_FREE_GB=123 AMUX_CLEANUP_SCOPE_FILE="$FIX/scope.env" TICK_PATH="$TICK" \
      bash -c 'AMUX_CLEANUP_LIB_ONLY=1 . "$TICK_PATH"; printf "%s" "$TARGET_TIGHT_FREE_GB"')
check "an explicit environment value beats the scope" "123" "$got"
got=$(env -u AMUX_CLEANUP_TARGET_TIGHT_FREE_GB AMUX_CLEANUP_SCOPE_FILE="$FIX/scope.env" TICK_PATH="$TICK" bash -c 'AMUX_CLEANUP_LIB_ONLY=1 . "$TICK_PATH"; printf "%s" "${OTHER:-unset}"')
check "keys outside AMUX_CLEANUP_ are not imported" "unset" "$got"

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
  # HERMETIC (cpu RCA 20261007-101721): every scan root points into the
  # fixture. With only the target roots overridden this one dry run walked the
  # real ~/Dev, /private/tmp and ~/.amux for 15+ minutes at a core or more,
  # and the suite was itself a load source in two cpu escalations
  # (AMUX-5635 (b), this one).
  mkdir -p "$FIX/h/ctmp" "$FIX/h/ltmp" "$FIX/h/utmp" "$FIX/h/lima" "$FIX/h/wt"
  t0=$(date +%s)
  tick_out=$(AMUX_CLEANUP_TARGET_ROOTS="$R" AMUX_CLEANUP_TARGET_KEEP="" AMUX_CLEANUP_LSOF_CMD="cat $FIX/lsof.base" \
    AMUX_CLEANUP_CHURN_ROOTS="$R" AMUX_CLEANUP_WORKTREE_ROOTS="$FIX/h/wt@1" AMUX_CLEANUP_CLAUDE_TMP_ROOT="$FIX/h/ctmp" \
    AMUX_CLEANUP_LANE_TMP_ROOT="$FIX/h/ltmp" AMUX_CLEANUP_USER_TMP_ROOT="$FIX/h/utmp" AMUX_CLEANUP_LIMA_ROOT="$FIX/h/lima" \
    bash "$TICK" --dry-run 2>&1)
  check "the dry run stays inside the fixture (under 120 s)" "yes" "$(yn test $(( $(date +%s) - t0 )) -lt 120)"
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
