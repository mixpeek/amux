#!/usr/bin/env bash
# test-mutate-survey.sh — AF-422. `mutate.sh survey` is the tool that makes
# ethos rule 7 cheap, so it is the last place a check that cannot fail belongs.
#
# The cells that matter are the CONTROLS, not the specimen. A survey that
# reported everything killed would be a comforting no-op, and a survey that
# reported everything survived would be noise; cells 1 and 2 pin both directions
# on the same fixture. Cell 3 pins the property the shared checkout depends on:
# the file comes back byte-identical. Cells 4 and 5 pin that it says what it did
# NOT examine, which is the difference between a measurement and a reassurance.
set -u
SRC="$(cd "$(dirname "$0")/.." && pwd)"
MUT="$SRC/scripts/mutate.sh"
T=$(mktemp -d) || exit 2
trap 'rm -rf "$T"' EXIT
FAILS=0
fail() { echo "FAIL: $1" >&2; FAILS=$((FAILS + 1)); }

# A tiny "library" with two decisions, and a "suite" that only tests one of
# them. The survey must tell them apart.
cat > "$T/lib.sh" <<'LIB'
covered() {
  if [ "$1" -eq 7 ]; then echo yes; else echo no; fi
}
uncovered() {
  if [ "$1" -eq 99 ]; then echo yes; else echo no; fi
}
LIB
cat > "$T/suite.sh" <<'SUITE'
. "$1/lib.sh"
[ "$(covered 7)" = yes ] || exit 1
[ "$(covered 3)" = no ]  || exit 1
exit 0
SUITE

BEFORE=$(shasum "$T/lib.sh" | cut -d' ' -f1)
out=$("$MUT" survey "$T/lib.sh" --stop-at '' -- bash "$T/suite.sh" "$T" 2>&1)

# 1) The line the suite depends on is KILLED.
case "$out" in *"killed"*'-eq -> -ne'*) ;; *) fail "the covered branch was not killed: $out" ;; esac

# 2) THE CONTROL. The line the suite ignores SURVIVES. Without this the tool
#    could report everything killed and look healthy.
# "  SURVIVED L", the ROW, not "*SURVIVED*" — the summary always prints
# "N SURVIVED.", so the loose glob matched a survey with ZERO survivors and this
# assertion could not have failed. Caught by cell 6 failing on the same glob.
case "$out" in *"  SURVIVED L"*) ;; *) fail "the uncovered branch did not survive — the survey cannot discriminate" ;; esac
case "$out" in *"1 killed, 1 SURVIVED"*) ;; *) fail "the tally does not match one covered and one uncovered branch" ;; esac

# 3) The file is byte-identical afterwards. This is the property the whole
#    apply/revert design exists for on a shared checkout.
AFTER=$(shasum "$T/lib.sh" | cut -d' ' -f1)
[ "$BEFORE" = "$AFTER" ] || fail "the surveyed file did not return to its starting bytes"

# 4) --limit truncation is REPORTED. A survey that examined 2 of 40 lines and
#    printed only "all killed" is the shape this repo keeps filing.
out2=$("$MUT" survey "$T/lib.sh" --limit 1 --stop-at '' -- bash "$T/suite.sh" "$T" 2>&1)
case "$out2" in *"were NOT run"*) ;; *) fail "a truncated survey did not say what it skipped" ;; esac
case "$out2" in *"says nothing about the rest"*) ;; *) fail "the truncation note did not bound its own claim" ;; esac

# 5) Non-unique lines are skipped AND counted, not silently dropped.
# GENUINELY identical lines. The first version of this fixture differed by the
# function name, so the lines were unique and the cell reported 0 skipped while
# asserting 2 — it failed for the right reason on the first run.
cat > "$T/dup.sh" <<'DUP'
a() {
  if [ "$1" -eq 7 ]; then echo y; fi
}
b() {
  if [ "$1" -eq 7 ]; then echo y; fi
}
DUP
out3=$("$MUT" survey "$T/dup.sh" --stop-at '' -- true 2>&1)
case "$out3" in *"non-unique"*) ;; *) fail "duplicate lines were not reported as skipped" ;; esac
case "$out3" in *"2 non-unique"*) ;; *) fail "the skip COUNT is wrong or absent: $out3" ;; esac

# 6) --stop-at bounds the scope and the bound is stated, so nobody reads a
#    partial survey as a whole-file verdict.
out4=$("$MUT" survey "$T/lib.sh" --stop-at 'uncovered' -- bash "$T/suite.sh" "$T" 2>&1)
case "$out4" in *"scope: lines 1-"*) ;; *) fail "the survey did not state its scope" ;; esac
case "$out4" in *"  SURVIVED L"*) fail "--stop-at did not exclude the region after the marker" ;; esac
case "$out4" in *"scope: lines 1-3 of 7"*) ;; *) fail "the stated scope does not match the marker's line" ;; esac

# 7) EVERY EXCLUSION IS COUNTED, not just the one that was already reported.
#    ts-gke's reciprocal point on 2026-09-03: a positive control belongs on a
#    filter's EXCLUSIONS as much as on its matches. `skipped_nomut` was computed
#    and never printed, and comment/blank lines were dropped with no counter at
#    all, so "N mutable lines found" could not be told from "N found out of 1391
#    scanned, most of which I silently ignored" — the exact property this tool's
#    docstring claims.
cat > "$T/mix.sh" <<'MIX'
a() {
  # if [ "$1" -eq 7 ]; a COMMENT that carries a rule and must not be mutated
  echo plain
  if [ "$1" -eq 7 ]; then echo y; fi
}
MIX
out5=$("$MUT" survey "$T/mix.sh" --stop-at '' -- true 2>&1)
case "$out5" in *"comment or blank"*) ;; *) fail "the comment/blank exclusion is not reported" ;; esac
case "$out5" in *"with no"*"applicable rule"*) ;; *) fail "the no-rule exclusion is not reported" ;; esac
# The COMMENT carrying a rule is the control: it looks mutable and must be
# excluded, or the tool would mutate a comment and call the result coverage.
case "$out5" in *"1 mutable"*) ;; *) fail "a rule inside a comment was counted as mutable: $out5" ;; esac

# 8) The buckets SUM to the scanned line count. This is the assertion that makes
#    a future silent exclusion impossible: a new `continue` without a counter
#    breaks it loudly rather than quietly shrinking the survey.
sums=$(printf '%s\n' "$out5" | python3 -c '
import sys, re
t = sys.stdin.read()
scope = re.search(r"scope: lines 1-(\d+) of", t)
buckets = re.search(r"of those \d+: (\d+) mutable, (\d+) non-unique, (\d+) with no\b.*?(\d+) comment or blank", t, re.S)
if not scope or not buckets:
    print("PARSE-FAIL"); raise SystemExit
n = int(scope.group(1)); tot = sum(int(g) for g in buckets.groups())
print("OK" if n == tot else f"MISMATCH scanned={n} buckets={tot}")')
[ "$sums" = "OK" ] || fail "the exclusion buckets do not account for every scanned line: $sums"

if [ "$FAILS" -eq 0 ]; then
  echo "ok: mutate survey — all 8 cases pass"
  exit 0
fi
echo "$FAILS case(s) failed" >&2
exit 1
