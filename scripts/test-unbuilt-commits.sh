#!/usr/bin/env bash
# AMUX-3797 — scripts/unbuilt-commits.sh must tell "never compiled" from
# "compiled", and must REFUSE rather than guess when it cannot tell.
#
# CELL 3 IS THE LOAD-BEARING ONE. With no builder log, the naive implementation
# reports every commit as unbuilt — absence read as emptiness, the failure this
# repo has paid for repeatedly. It must exit 2 and report no count at all.
#
# CELL 2 encodes the subtlety that produced this card's first draft: a SKIP line
# names a sha and does NOT mean it was built. 962c15d7 was logged SKIP four
# seconds AFTER the provenance file recorded it as built, and reading that SKIP
# as "never built" is what put a wrong mechanism on the card. A sha that appears
# ONLY on a SKIP line was seen, deferred, and never compiled — unbuilt.
#
# Runs against a fixture repo and a fake log; touches no real builder state.
# Exit 0 = pass, 1 = failure.
set -uo pipefail
cd "$(dirname "$0")/.."
SCRIPT="$(pwd)/scripts/unbuilt-commits.sh"
[ -x "$SCRIPT" ] || { echo "FAIL: $SCRIPT missing"; exit 1; }

D=$(mktemp -d) || exit 1
trap 'rm -rf "$D"' EXIT
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   — $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL — $1"; }

git init -q "$D/r"; cd "$D/r"
git config user.email t@t; git config user.name t
# FOUR commits, and `base` is the ZEROTH. `base..HEAD` EXCLUDES base, so a
# fixture that branches base at the first commit under test puts only two in
# range — the first draft did that and cell 1 failed against a correct script.
# COMMITS MUST TOUCH crates/, or the script correctly excludes them: the
# builder triggers on `git log -1 -- crates/ Cargo.toml Cargo.lock`, so a
# fixture writing to a bare `f` produces an empty range and every cell reads
# "no commits" — which is what happened when the Rust-path filter landed.
mkdir -p crates
for n in 0 1 2 3; do echo "$n" > crates/f.rs; git add crates/f.rs; git commit -qm "c$n"; done
C0=$(git rev-parse HEAD~3)
C1=$(git rev-parse HEAD~2); C2=$(git rev-parse HEAD~1); C3=$(git rev-parse HEAD)
git branch -f base "$C0"

echo "unbuilt-commits — never-compiled must be distinguishable, or refused"

# C3 was built. C2 appears ONLY on a SKIP line. C1 appears nowhere.
cat > "$D/log" <<EOF
== 2026-08-27 07:00:00 building $C3 (trigger: $C3, previous stamp: deadbeef)
== 2026-08-27 06:00:00 SKIP $C2 — build already running (pid 1)
EOF

out=$(AMUX_RS_BUILD_LOG="$D/log" "$SCRIPT" base..HEAD 2>&1); rc=$?

# 1 — the built one is not listed, and the count is right
if [ "$rc" = 1 ] && echo "$out" | grep -q "NEVER built:        2"; then
  ok "1: counts 2 of 3 as never built (the built sha is excluded)"
else
  bad "1: rc=$rc out=[$(echo "$out" | tr '\n' ' ')]"
fi

# 2 — a SKIP-only sha is UNBUILT, not built
if echo "$out" | grep -q "$(git log -1 --format=%h "$C2")"; then
  ok "2: a sha seen only on a SKIP line counts as never built"
else
  bad "2: SKIP-only sha was treated as built — the first-draft mistake"
fi

# 3 — CONTROL: no log means REFUSE, never "everything is unbuilt"
out3=$(AMUX_RS_BUILD_LOG="$D/nope" "$SCRIPT" base..HEAD 2>&1); rc3=$?
if [ "$rc3" = 2 ] && ! echo "$out3" | grep -q "NEVER built"; then
  ok "3: control — an absent log refuses (exit 2) instead of reporting a count"
else
  bad "3: absent log produced a count: rc=$rc3 out=[$(echo "$out3" | tr '\n' ' ')]"
fi

# 4 — a range where everything was built exits 0
cat > "$D/log2" <<EOF
== building $C2 (trigger: $C2, previous stamp: x)
== building $C3 (trigger: $C3, previous stamp: x)
EOF
out4=$(AMUX_RS_BUILD_LOG="$D/log2" "$SCRIPT" "$C1..HEAD" 2>&1); rc4=$?  # C2,C3 only
if [ "$rc4" = 0 ] && echo "$out4" | grep -q "every commit in this range"; then
  ok "4: an all-built range exits 0 and says so"
else
  bad "4: rc=$rc4 out=[$(echo "$out4" | tr '\n' ' ')]"
fi

# 5 — a NON-RUST commit is not a miss. The builder never targets it, so
#     counting it inflates the answer — measured on the live repo as 83-of-235
#     against a true 0-of-152 once the filter landed.
echo doc > README.md; git add README.md; git commit -qm "docs only"
out5=$(AMUX_RS_BUILD_LOG="$D/log2" "$SCRIPT" "$C2..HEAD" 2>&1); rc5=$?
if [ "$rc5" = 0 ] && ! echo "$out5" | grep -q "docs only"; then
  ok "5: a docs-only commit is excluded, not reported as never built"
else
  bad "5: docs-only commit counted as a miss: rc=$rc5 out=[$(echo "$out5" | tr '\n' ' ')]"
fi

# 6 — A TRUNCATED/ROTATED LOG MUST ANNOUNCE ITSELF (amux-frustrations, reviewing
#     12627007). The absent-log refusal at cell 3 does not cover a log that
#     exists but starts AFTER the range: every commit before it has no
#     `building` line and reports as never built, so a measurement gap reads as
#     a large regression. This is the tool's own class one layer up.
cat > "$D/log3" <<EOF
== 2099-01-01 00:00:00 building $C3 (trigger: $C3, previous stamp: x)
EOF
out6=$(AMUX_RS_BUILD_LOG="$D/log3" "$SCRIPT" base..HEAD 2>&1)
if echo "$out6" | grep -q "COVERAGE GAP" && echo "$out6" | grep -q "UPPER BOUND"; then
  ok "6: a log starting after the range prints a coverage gap and calls the count a bound"
else
  bad "6: silent on a log that cannot see the range: [$(echo "$out6" | tr '\n' ' ')]"
fi

# 7 — CONTROL for cell 6: a log that DOES cover the range must stay quiet. A
#     warning that always fires is one people stop reading.
if ! echo "$out" | grep -q "COVERAGE GAP"; then
  ok "7: control — a log covering the range prints no coverage gap"
else
  bad "7: coverage gap fired on a log that covers the range"
fi

# 8 — the percentage must ROUND, not truncate. 125/839 is 14.9 and printed as
#     "14%", understating — the wrong direction for a how-bad-is-this number.
if echo "$out" | grep -qE "NEVER built: +[0-9]+ \([0-9]+\.[0-9]%\)"; then
  ok "8: the percentage carries a decimal instead of truncating"
else
  bad "8: percentage is truncated: [$(echo "$out" | grep NEVER)]"
fi

# 9 — THE SECOND REFUSAL, which was written and never written down
#     (amux-frustrations, reviewing AMUX-3797). Cell 3 covers an ABSENT log.
#     unbuilt-commits.sh:76 refuses a second way: a log that EXISTS and is
#     readable but carries no `building <sha>` line at all — a truncated log, a
#     rotated one, a builder that has only ever logged SKIPs. The built set is
#     then empty and every commit in range reads as never built, which is cell
#     3's failure reached through a file that is present rather than missing.
#
#     It is the harder of the two to notice precisely because the log is THERE:
#     `[ -r "$LOG" ]` passes, nothing looks wrong, and the answer is 100%.
#
#     A SKIP-only log is the realistic shape, not a contrived empty file: it is
#     what a fresh rotation looks like when the first thing after it is a
#     duplicate wakeup. So the fixture is a SKIP line, which also proves the
#     refusal keys on `building` rather than on the file being non-empty.
cat > "$D/log4" <<EOF
== 2026-08-27 07:00:00 SKIP $C3 — build already running (pid 1)
EOF
out9=$(AMUX_RS_BUILD_LOG="$D/log4" "$SCRIPT" base..HEAD 2>&1); rc9=$?
if [ "$rc9" = 2 ] && ! echo "$out9" | grep -q "NEVER built"; then
  ok "9: a log with no 'building' line refuses (exit 2) rather than reporting 100%"
else
  bad "9: log with no build lines produced a count: rc=$rc9 out=[$(echo "$out9" | tr '\n' ' ')]"
fi

echo
echo "pass=$PASS fail=$FAIL"
[ "$FAIL" = 0 ]
