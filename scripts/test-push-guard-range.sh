#!/usr/bin/env bash
# AEAB-48 — the push-guard must count what a push ACTUALLY ships, in both
# directions.
#
# The bug this pins: `remote_sha..local_sha` reads as "what this push adds" and
# is that ONLY while the push fast-forwards. Rebase a branch onto current
# origin/main — the hygiene everyone is told to do — and the old remote tip
# stops being an ancestor, so the range widens to every commit rebased ONTO.
# Measured 2026-08-22 on a branch adding 2 commits: 28 in the old range, 2 in
# the correct one, all 26 of the difference provably already on origin. The
# guard refused with "would ship 22 commit(s) authored by another session".
#
# The reason a HALF test is not enough, and it is the whole point of this file:
# a "fix" that made the guard return nothing at all would pass the rebase case
# perfectly. So case B pins the guard still REFUSING a genuinely foreign commit
# that origin does not have. One case proves the fix, the other proves the fix
# did not hollow the guard out; either alone is theatre.
#
# Runs the SHIPPED hook against real throwaway repos rather than restating its
# logic — the hook is what ships, and simulating what you believe it does
# cannot catch it doing something else.
#
# Exit 0 = all pass, 1 = a failure. Wired into .github/workflows/checks.yml.
set -uo pipefail
cd "$(dirname "$0")/.."
HOOK="$(pwd)/scripts/git-hooks/pre-push"
PASS=0; FAIL=0
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

ok()   { PASS=$((PASS+1)); echo "  ok   — $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  FAIL — $1"; }

# A repo with a bare origin, one commit on main, pushed.
mkrepo() {
  local d="$1"
  git init -q --bare "$d/origin.git"
  git init -q "$d/work"
  git -C "$d/work" config user.email t@t; git -C "$d/work" config user.name t
  git -C "$d/work" config commit.gpgsign false
  git -C "$d/work" remote add origin "$d/origin.git"
  echo base > "$d/work/f"; git -C "$d/work" add f
  git -C "$d/work" commit -qm "base

Amux-Session: mine"
  git -C "$d/work" branch -M main
  git -C "$d/work" push -q origin main
}

# Commit with an explicit Amux-Session trailer.
# Each commit touches its OWN file. The first draft appended to one shared
# file, so case A's rebase hit a content conflict, silently did not happen, and
# the case "passed" against a branch that was never rebased — a fixture that
# could not have failed, which is the exact trap this file exists to avoid.
# After fixing it, case A was confirmed to FAIL against the pre-fix range logic.
commit_as() {
  local d="$1" who="$2" msg="$3"
  echo "$msg" > "$d/work/$msg"; git -C "$d/work" add "$msg"
  git -C "$d/work" commit -qm "$msg

Amux-Session: $who"
}

# Run the shipped hook the way git does: refspec lines on stdin.
# Echoes the exit code; output goes to $TMP/out.
run_hook() {
  local d="$1" local_sha="$2" remote_sha="$3" ref="$4"
  ( cd "$d/work" && \
    echo "refs/heads/$ref $local_sha refs/heads/$ref $remote_sha" | \
    AMUX_SESSION=mine AMUX_ALLOW_FOREIGN= \
    python3 "$HOOK" origin "$d/origin.git" ) > "$TMP/out" 2>&1
  echo $?
}
ZERO=0000000000000000000000000000000000000000

# ── A. the reported bug: rebase onto an advanced origin/main ────────────────
# main gains a commit from ANOTHER session and it is pushed to origin. My
# branch, which adds one commit of my own, is rebased onto it. Nothing of
# theirs is being shipped — origin already has it.
A="$TMP/a"; mkrepo "$A"
commit_as "$A" "other-lane" "theirs-upstream"
git -C "$A/work" push -q origin main
git -C "$A/work" checkout -q -b feat HEAD~1
commit_as "$A" "mine" "mine-one"
git -C "$A/work" push -q origin feat 2>/dev/null
OLD_TIP=$(git -C "$A/work" rev-parse feat)
git -C "$A/work" rebase -q main
NEW_TIP=$(git -C "$A/work" rev-parse feat)
rc=$(run_hook "$A" "$NEW_TIP" "$OLD_TIP" feat)
if [ "$rc" -eq 0 ]; then
  ok "A: rebase-then-push is ALLOWED (nothing shipped that origin lacks)"
else
  bad "A: rebase-then-push was BLOCKED — this is the reported bug"; sed 's/^/       /' "$TMP/out"
fi

# ── B. the guard must still bite ────────────────────────────────────────────
# THE CONTROL, and the one that fails against a hollowed-out guard: a foreign
# commit that origin does NOT have must still refuse.
B="$TMP/b"; mkrepo "$B"
git -C "$B/work" checkout -q -b feat
commit_as "$B" "other-lane" "theirs-unpushed"
commit_as "$B" "mine" "mine-one"
TIP=$(git -C "$B/work" rev-parse feat)
rc=$(run_hook "$B" "$TIP" "$ZERO" feat)
if [ "$rc" -ne 0 ] && grep -q "other-lane" "$TMP/out"; then
  ok "B: a foreign commit origin does NOT have is still BLOCKED, and named"
else
  bad "B: foreign, unpushed work was allowed through — the guard is hollow (rc=$rc)"
  sed 's/^/       /' "$TMP/out"
fi

# ── C. the plain case must keep working ─────────────────────────────────────
# A fast-forward push of only my own commits: allowed, and it must not depend
# on the rebase path.
C="$TMP/c"; mkrepo "$C"
git -C "$C/work" checkout -q -b feat
commit_as "$C" "mine" "mine-one"
TIP=$(git -C "$C/work" rev-parse feat)
rc=$(run_hook "$C" "$TIP" "$ZERO" feat)
if [ "$rc" -eq 0 ]; then
  ok "C: a branch of only my own commits is ALLOWED"
else
  bad "C: my own work was blocked (rc=$rc)"; sed 's/^/       /' "$TMP/out"
fi

# ── D. an untrailered commit is not silently trusted ────────────────────────
# A commit with NO Amux-Session trailer is attributable to nobody, and the
# guard's existing behaviour is to flag it rather than wave it through.
D="$TMP/d"; mkrepo "$D"
git -C "$D/work" checkout -q -b feat
echo x > "$D/work/untrailered"; git -C "$D/work" add untrailered
git -C "$D/work" commit -qm "no trailer at all"
TIP=$(git -C "$D/work" rev-parse feat)
rc=$(run_hook "$D" "$TIP" "$ZERO" feat)
if [ "$rc" -ne 0 ]; then
  ok "D: a commit with no Amux-Session trailer is BLOCKED, not assumed mine"
else
  bad "D: an unattributed commit sailed through (rc=$rc)"; sed 's/^/       /' "$TMP/out"
fi

echo
echo "push-guard range: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
