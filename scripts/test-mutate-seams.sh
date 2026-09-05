#!/usr/bin/env bash
# test-mutate-seams.sh — AF-439. `mutate.sh seams` swaps two same-typed
# arguments at a call site and reports what objects: the type system
# (held-by-types), a test (killed), or nothing at all (SURVIVED).
#
# The class it exists for is seven instances across two repos in one night, and
# mvs-pitr's diagnosis is what made it buildable: every one was a missing
# DIRECTION rather than a missing assertion, and none was visible from either
# side alone. A test per component passes exactly as well when the seam between
# them is broken.
#
# THE CELLS THAT MATTER ARE 1 AND 2 TOGETHER. A tool reporting SURVIVED for
# everything is useless and passes cell 1 alone; one reporting killed for
# everything is worse, because it is reassuring, and passes cell 2 alone. Both
# fixtures are the same shape and differ only in whether the checker observes
# the argument order.
#
# The fixture is Rust-SHAPED text rather than a crate: the tool's enumerate /
# mutate / classify / restore mechanics are what is under test here, and
# standing up a cargo project to exercise them would make this suite too slow to
# run and would test rustc rather than this script.
set -u
SRC="$(cd "$(dirname "$0")/.." && pwd)"
MUT="$SRC/scripts/mutate.sh"
T=$(mktemp -d) || exit 2
trap 'rm -rf "$T"' EXIT
pass=0; fail=0
ok() { if [ "$2" = "$3" ]; then pass=$((pass+1)); echo "  ok   $1"; else
       fail=$((fail+1)); echo "  FAIL $1: want [$3] got [$2]"; fi; }

cat > "$T/lib.rs" <<'LIB'
fn observed() -> String {
    join_dir_and_name(dir, name)
}
fn unobserved() -> String {
    join_dir_and_name(other, thing)
}
LIB
# The "suite": passes only while the OBSERVED call site keeps its order. The
# unobserved one is invisible to it, exactly like a real per-component test.
cat > "$T/suite.sh" <<'SUITE'
grep -q 'join_dir_and_name(dir, name)' "$1/lib.rs"
SUITE

BEFORE=$(shasum "$T/lib.rs" | cut -d' ' -f1)
out=$("$MUT" seams "$T/lib.rs" -- bash "$T/suite.sh" "$T" 2>&1)

echo "cell 1: a swap NO checker observes is reported SURVIVED"
ok "the unobserved call site survives" \
   "$(echo "$out" | grep -c 'SURVIVED .*join_dir_and_name(other,thing)')" "1"

echo "cell 2: THE CONTROL — a swap the checker DOES observe is killed"
ok "the observed call site is killed" \
   "$(echo "$out" | grep -c 'killed .*join_dir_and_name(dir,name)')" "1"
ok "the tally names both" "$(echo "$out" | grep -c '0 held-by-types, 1 killed, 1 SURVIVED')" "1"

echo "cell 3: the file returns to its starting bytes"
ok "restored" "$(shasum "$T/lib.rs" | cut -d' ' -f1)" "$BEFORE"

echo "cell 4: every exclusion is counted and the buckets sum"
ok "exclusions reported" "$(echo "$out" | grep -c 'no qualifying call')" "1"
sums=$(printf '%s\n' "$out" | python3 -c '
import sys, re
t = sys.stdin.read()
m = re.search(r"of (\d+) lines: (\d+) swappable, (\d+) non-unique, (\d+) symmetric or\s*\n.*?(\d+) no qualifying call, (\d+) comment or blank", t, re.S)
if not m: print("PARSE-FAIL"); raise SystemExit
tot = int(m.group(1)); parts = sum(int(m.group(i)) for i in range(2, 7))
print("OK" if tot == parts else f"MISMATCH lines={tot} buckets={parts}")')
ok "buckets account for every line" "$sums" "OK"

echo "cell 5: identical args and symmetric callees are excluded"
cat > "$T/sym.rs" <<'SYM'
fn a() -> u32 { max(a, a) }
fn b() -> u32 { min(x, y) }
fn c() -> u32 { real_call(left, right) }
SYM
out2=$("$MUT" seams "$T/sym.rs" -- true 2>&1)
ok "only the non-symmetric call is swappable" \
   "$(echo "$out2" | grep -c 'of 4 lines: 1 swappable, 0 non-unique, 2 symmetric or')" "1"

echo "cell 6: --build separates held-by-types from killed"
# Stands in for a type error: this "build" rejects the swapped form, so the
# outcome is attributed to the compiler rather than to the suite.
cat > "$T/build.sh" <<'BUILD'
if grep -q 'join_dir_and_name(name, dir)' "$1/lib.rs"; then exit 1; fi
exit 0
BUILD
out3=$("$MUT" seams "$T/lib.rs" --build "bash $T/build.sh $T" -- bash "$T/suite.sh" "$T" 2>&1)
ok "the observed swap is attributed to the build" \
   "$(echo "$out3" | grep -c 'held-by-types .*join_dir_and_name(dir,name)')" "1"
ok "the unobserved one still survives it" \
   "$(echo "$out3" | grep -c 'SURVIVED .*join_dir_and_name(other,thing)')" "1"
ok "and without --build the report says the axis is missing" \
   "$(echo "$out" | grep -c 'no --build given')" "1"

echo "cell 7: the script REFUSES to mutate itself (AF-440)"
# Measured twice on 2026-09-03: mutating mutate.sh with mutate.sh left the
# mutation applied, because bash reads a script by byte offset as it runs and
# rewriting it underneath the interpreter loses the revert. `bash -n` passed
# both times; the only symptom was the NEXT run refusing with an argument error.
BEFORE_SELF=$(shasum "$MUT" | cut -d' ' -f1)
selfout=$("$MUT" run "$MUT" 'usage() {' 'usage_x() {' -- true 2>&1); rc=$?
ok "refuses" "$rc" "2"
ok "names the reason" "$(echo "$selfout" | grep -c 'REFUSING to mutate')" "1"
ok "offers the copy-and-mutate path" "$(echo "$selfout" | grep -c 'mutate-under-test')" "1"
ok "and left itself untouched" "$(shasum "$MUT" | cut -d' ' -f1)" "$BEFORE_SELF"

echo "cell 8: THE CONTROL — it still mutates any OTHER file"
printf 'v=true\n' > "$T/other.sh"
B4=$(shasum "$T/other.sh" | cut -d' ' -f1)
o8=$("$MUT" run "$T/other.sh" 'v=true' 'v=false' -- true 2>&1)
ok "applies to another file" "$(echo "$o8" | grep -c 'apply: LANDED')" "1"
ok "and reverts it" "$(shasum "$T/other.sh" | cut -d' ' -f1)" "$B4"

echo ""
echo "test-mutate-seams: $pass passed, $fail failed"
[ "$fail" = 0 ]
