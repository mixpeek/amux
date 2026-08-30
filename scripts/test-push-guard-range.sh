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
# PUSH_GUARD_HOOK lets a caller point these cells at a DIFFERENT copy of the
# guard — specifically an older one, to confirm a new cell actually fails
# against the code it was written for. Without the seam that check needs an
# edit to this file, which means it is done once and never again;
# scripts/test-frustration-scan.sh has had FRUSTRATION_SCAN for the same
# reason and it is what made AF-207's control reproducible by its reviewer.
#   git show <sha>^:scripts/git-hooks/pre-push > /tmp/prefix-guard
#   PUSH_GUARD_HOOK=/tmp/prefix-guard bash scripts/test-push-guard-range.sh
HOOK="${PUSH_GUARD_HOOK:-$(pwd)/scripts/git-hooks/pre-push}"
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
# HERMETIC ON BOTH AXES, and it was not (found while adding cell P).
#
#   HOME     -> the isolation probe reads ~/.amux/sessions/<name>.env. Only
#               run_hook_consent redirected it, so any cell using run_hook read
#               the DEVELOPER'S real fleet. Cell P passed here because `desktop`
#               happens to be isolated on this machine and would have failed on
#               every CI run — which is precisely how test J went red at 21:14
#               once already. A cell whose verdict depends on the host's state
#               is not a cell.
#   AMUX_URL -> the blocking-run footer probes GET /api/sessions for the
#               blocker's live status. Pointed at a closed port so it fails
#               FAST and DETERMINISTICALLY, instead of reaching whatever server
#               happens to be up and taking the full timeout on CI where none is.
run_hook() {
  local d="$1" local_sha="$2" remote_sha="$3" ref="$4"
  ( cd "$d/work" && \
    echo "refs/heads/$ref $local_sha refs/heads/$ref $remote_sha" | \
    AMUX_SESSION=mine AMUX_ALLOW_FOREIGN= \
    HOME="$TMP/fakehome" \
    AMUX_URL="https://127.0.0.1:9" AMUX_PUSH_GUARD_API_TIMEOUT_S=1 \
    python3 "$HOOK" origin "$d/origin.git" ) > "$TMP/out" 2>&1
  echo $?
}

# Same, with AMUX_FOREIGN_CONSENT set (AMUX-3533).
run_hook_consent() {
  local d="$1" local_sha="$2" remote_sha="$3" ref="$4" consent="$5"
  ( cd "$d/work" && \
    echo "refs/heads/$ref $local_sha refs/heads/$ref $remote_sha" | \
    AMUX_SESSION=mine AMUX_ALLOW_FOREIGN= AMUX_FOREIGN_CONSENT="$consent" \
    HOME="$TMP/fakehome" \
    AMUX_URL="https://127.0.0.1:9" AMUX_PUSH_GUARD_API_TIMEOUT_S=1 \
    python3 "$HOOK" origin "$d/origin.git" ) > "$TMP/out" 2>&1
  echo $?
}
ZERO=0000000000000000000000000000000000000000

# The fake fleet both runners point HOME at. Created BEFORE the first cell,
# not beside the cells that read it: every run_hook call now redirects HOME,
# so a fixture built two hundred lines down would leave the early cells
# pointing at a directory that does not exist yet.
mkdir -p "$TMP/fakehome/.amux/sessions"
printf 'CC_ISOLATED="1"\n' > "$TMP/fakehome/.amux/sessions/desktop.env"
printf 'CC_TAGS="amux"\n'   > "$TMP/fakehome/.amux/sessions/other-lane.env"

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

# ── E-H. AMUX_FOREIGN_CONSENT (AMUX-3533) ──────────────────────────────────
# The guard modelled ONE consenting party: the human. On a shared checkout the
# consenting party is routinely the AUTHOR, and none of the three offered exits
# fit that — so two sessions reached for the blanket override on the same day,
# one of them without noticing the wording did not cover them.
#
# These four cases are chosen so that no ONE of them can pass a broken
# implementation: E proves consent WORKS, F/G/H prove it is STRICTER than
# AMUX_ALLOW_FOREIGN rather than a second way around the guard. A version that
# simply returned 0 whenever the variable was set would pass E and fail all
# three others.
E="$TMP/e"; mkrepo "$E"
git -C "$E/work" checkout -q -b feat
commit_as "$E" "other-lane" "theirs-unpushed"
commit_as "$E" "mine" "mine-one"
ETIP=$(git -C "$E/work" rev-parse feat)
ESHA=$(git -C "$E/work" log --format=%h --all --grep="theirs-unpushed" | head -1)

# E. correct consent clears the push.
rc=$(run_hook_consent "$E" "$ETIP" "$ZERO" feat "$ESHA:other-lane")
if [ "$rc" -eq 0 ]; then
  ok "E: consent naming the REAL author clears the push"
else
  bad "E: correct author consent was still blocked"; sed 's/^/       /' "$TMP/out"
fi

# F. THE CONTROL THAT MATTERS. Consent naming the WRONG session must REFUSE —
# a blanket override would have shipped this. If F passes only because the
# guard refuses everything, E would have failed.
rc=$(run_hook_consent "$E" "$ETIP" "$ZERO" feat "$ESHA:somebody-else")
if [ "$rc" -ne 0 ] && grep -qi "does not match" "$TMP/out"; then
  ok "F: consent naming the WRONG author is REFUSED, not waved through"
else
  bad "F: wrong-author consent was accepted — this is weaker than ALLOW_FOREIGN"; sed 's/^/       /' "$TMP/out"
fi

# G. PARTIAL consent still blocks, and names what is uncovered. Without this a
# caller who granted 3 of 4 reads the refusal as the mechanism not working.
G="$TMP/g"; mkrepo "$G"
git -C "$G/work" checkout -q -b feat
commit_as "$G" "lane-a" "a-commit"
commit_as "$G" "lane-b" "b-commit"
GTIP=$(git -C "$G/work" rev-parse feat)
GSHA=$(git -C "$G/work" log --format=%h --all --grep="a-commit" | head -1)
rc=$(run_hook_consent "$G" "$GTIP" "$ZERO" feat "$GSHA:lane-a")
if [ "$rc" -ne 0 ] && grep -q "b-commit" "$TMP/out"; then
  ok "G: partial consent still BLOCKS and names the uncovered commit"
else
  bad "G: partial consent was treated as full consent"; sed 's/^/       /' "$TMP/out"
fi

# H. A malformed entry is REFUSED, never silently skipped — skipping would let
# a caller believe they granted a consent they did not.
rc=$(run_hook_consent "$E" "$ETIP" "$ZERO" feat "just-a-sha-no-session")
if [ "$rc" -ne 0 ] && grep -qi "malformed" "$TMP/out"; then
  ok "H: a malformed consent entry is REFUSED, not skipped"
else
  bad "H: malformed consent did not refuse"; sed 's/^/       /' "$TMP/out"
fi

# I. And the escape must be DISCOVERABLE: an ordinary refusal has to name it,
# or it is decoration (ethos rule 1). The refusal that sent two sessions to the
# blanket override listed only exits that did not fit.
rc=$(run_hook "$E" "$ETIP" "$ZERO" feat)
if [ "$rc" -ne 0 ] && grep -q "AMUX_FOREIGN_CONSENT=" "$TMP/out"; then
  ok "I: the refusal NAMES the author-consent exit, with the pairs filled in"
else
  bad "I: the refusal does not offer AMUX_FOREIGN_CONSENT — an escape nobody is handed"
  sed 's/^/       /' "$TMP/out"
fi


# ── J/K. OWNER CONSENT for an ISOLATED worker (Ethan, 2026-08-23) ───────────
# An isolated raw-agent worker has the harness stripped and refuses peer sends,
# so its consent CANNOT be obtained: "ask that session to push" is unaskable and
# the two-field consent form needs a yes nobody can give. `:owner` is the exit,
# and the property that keeps it from being a second blanket override is that it
# is REFUSED for a worker you could simply have asked. K is that control, and it
# is the case that fails if the isolation check is dropped.
#
# These two consult the LIVE server for isolation (fail-closed on any doubt), so
# they use real lane names: `desktop` is isolated on this machine, `other-lane`
# is not a lane at all and must therefore be refused.
# The fixture the probe reads. HOME is already redirected to $TMP/fakehome by
# run_hook_consent, so this is hermetic: no server, no real ~/.amux, and the
# same file layout the server itself reads (~/.amux/sessions/<name>.env).
# Test J used to depend on the LIVE server saying `desktop` is isolated, which
# passed on a developer laptop and failed on every CI run — main went red at
# 21:14 for exactly that reason.

J="$TMP/j"; mkrepo "$J"
git -C "$J/work" checkout -q -b feat
commit_as "$J" "desktop" "isolated-lane-commit"
commit_as "$J" "mine" "mine-one"
JTIP=$(git -C "$J/work" rev-parse feat)
JSHA=$(git -C "$J/work" log --format=%h --all --grep="isolated-lane-commit" | head -1)

rc=$(run_hook_consent "$J" "$JTIP" "$ZERO" feat "$JSHA:desktop:owner")
if [ "$rc" -eq 0 ]; then
  ok "J: owner consent clears a commit by an ISOLATED worker that cannot be asked"
else
  bad "J: owner consent was refused for an isolated worker — the exit is unwalkable again"
  sed 's/^/       /' "$TMP/out"
fi

# K. THE CONTROL. `:owner` for a REACHABLE worker must REFUSE — otherwise it is
# just AMUX_ALLOW_FOREIGN with extra typing, and the whole point is that you
# must ask a peer you can reach.
rc=$(run_hook_consent "$E" "$ETIP" "$ZERO" feat "$ESHA:other-lane:owner")
if [ "$rc" -ne 0 ] && grep -qi "only for ISOLATED" "$TMP/out"; then
  ok "K: ':owner' is REFUSED for a worker that is not isolated — ask them instead"
else
  bad "K: ':owner' cleared a reachable worker — that is a blanket override wearing a suffix"
  sed 's/^/       /' "$TMP/out"
fi

# ── O/P. AF-206: the ISOLATED author must not be spellable as author consent, ─
#         and the refusal must NAME the exit that does exist.
#
# `:owner` shipped 2026-08-23 and appeared in NO exit the refusal message
# offered. On 2026-08-24 a lane hit that wall, correctly rejected
# AMUX_ALLOW_FOREIGN as recording consent from nobody, and escalated to the
# owner to DESIGN a remedy that was already implemented eleven commits below it.
# An escape nobody is handed is decoration (ethos rule 1).
#
# O is the sharper half. K already refused `:owner` for a REACHABLE author; the
# inverse was accepted — a TWO-field grant naming an ISOLATED author cleared on
# a trailer match alone and was logged as author consent. So the audit recorded
# "the author said yes" about a worker whose `amux send` is refused by
# construction, and nobody had to be careless to produce it: the refusal message
# computed that exact two-field string and invited you to paste it. Without O,
# the `:owner` distinction is decorative, because you can spell an owner grant
# as an author grant and the log cannot tell.

rc=$(run_hook_consent "$J" "$JTIP" "$ZERO" feat "$JSHA:desktop")
if [ "$rc" -ne 0 ] && grep -qi "isolated" "$TMP/out" && grep -q "desktop:owner" "$TMP/out"; then
  ok "O: author consent for an ISOLATED worker is REFUSED, and points at ':owner'"
else
  bad "O: a two-field grant cleared an unaskable author — the audit now says they consented"
  sed 's/^/       /' "$TMP/out"
fi

# P. THE DISCOVERY PATH. Blocked with an isolated author among the foreign
# commits, the refusal must say the author is isolated AND print the `:owner`
# string, rather than telling you to ask someone who cannot be asked. It must
# ALSO not offer that sha in the two-field string — handing over a ready-made
# false claim is what O exists to refuse one step later.
rc=$(run_hook "$J" "$JTIP" "$ZERO" feat)
if [ "$rc" -ne 0 ] \
   && grep -qi "ISOLATED AUTHOR" "$TMP/out" \
   && grep -q "$JSHA:desktop:owner" "$TMP/out" \
   && ! grep -qE "FOREIGN_CONSENT=\"[^\"]*$JSHA:desktop[^:]" "$TMP/out"; then
  ok "P: the refusal names the isolated author and the ':owner' exit, not a false author grant"
else
  bad "P: the refusal did not route an isolated author to the owner exit"
  sed 's/^/       /' "$TMP/out"
fi

# ── L. AC-227: a commit ALREADY UPSTREAM under a different sha ──────────────
# The reported bug. A cherry-pick / rebase / replay produces a new object with
# the SAME diff, so it sits in the foreign range permanently and no sha
# comparison can see it. amux-cloud's push was blocked and they asked a peer for
# consent the peer did not need to give. Only the diff answers this, which is
# what patch-id is for; `acdbfdf` and `9ebc42c` in this repo still share
# patch-id dff284cf093aecaa today.
L="$TMP/l"; mkrepo "$L"
commit_as "$L" "other-lane" "theirs-upstream"
git -C "$L/work" push -q origin main
THEIRS=$(git -C "$L/work" rev-parse HEAD)
git -C "$L/work" checkout -q -b feat HEAD~1
# -x, not a bare cherry-pick: replaying a commit onto its OWN parent reproduces
# the identical object (same tree, same parent, same message, same second), so
# the fixture had no distinct sha to detect and "passed" against a branch with
# no foreign commit on it at all. -x appends a provenance line to the MESSAGE,
# which changes the sha and leaves the diff untouched. Confirmed by hand before
# trusting it: the first version printed a foreign range of one commit, mine.
git -C "$L/work" cherry-pick -x "$THEIRS" >/dev/null
commit_as "$L" "mine" "mine-one"
LTIP=$(git -C "$L/work" rev-parse feat)
rc=$(run_hook "$L" "$LTIP" "$ZERO" feat)
if [ "$rc" -eq 0 ] && grep -q "ALREADY UPSTREAM" "$TMP/out"; then
  ok "L: a replayed commit already on origin is not foreign (AC-227)"
else
  bad "L: a cherry-pick of an upstream commit still blocks the push (AC-227 live)"
  sed 's/^/       /' "$TMP/out"
fi

# ── M. THE CONTROL for L. Same shape, but the patch is NOT upstream. ─────────
# Without this, a "fix" that cleared every foreign commit would pass L
# perfectly. Case B covers the plain range; this one proves the DUPLICATE path
# specifically does not fire on a diff origin has never seen.
M="$TMP/m"; mkrepo "$M"
git -C "$M/work" checkout -q -b feat
commit_as "$M" "other-lane" "theirs-never-pushed"
commit_as "$M" "mine" "mine-one"
MTIP=$(git -C "$M/work" rev-parse feat)
rc=$(run_hook "$M" "$MTIP" "$ZERO" feat)
if [ "$rc" -ne 0 ] && ! grep -q "ALREADY UPSTREAM" "$TMP/out"; then
  ok "M: a foreign commit origin has never seen is still REFUSED"
else
  bad "M: the duplicate check cleared a commit that is not upstream — hollowed out"
  sed 's/^/       /' "$TMP/out"
fi

# ── N. REVERT SAFETY, the inverse hazard AC-227's own entry names ───────────
# "the dangerous direction is the inverse — a session assuming a familiar-looking
# commit is last week's duplicate and shipping something genuinely unreviewed."
# If the patch was applied upstream and then REVERTED there, re-applying it
# ships something real. A revert's diff is the reverse of the original, so its
# patch-id equals the candidate's REVERSE patch-id: both directions present
# means the net state is ambiguous and the commit must stay foreign.
N="$TMP/n"; mkrepo "$N"
commit_as "$N" "other-lane" "theirs-upstream"
NTHEIRS=$(git -C "$N/work" rev-parse HEAD)
git -C "$N/work" revert --no-edit "$NTHEIRS" >/dev/null
git -C "$N/work" push -q origin main
git -C "$N/work" checkout -q -b feat "$NTHEIRS~1"
git -C "$N/work" cherry-pick -x "$NTHEIRS" >/dev/null
commit_as "$N" "mine" "mine-one"
NTIP=$(git -C "$N/work" rev-parse feat)
rc=$(run_hook "$N" "$NTIP" "$ZERO" feat)
if [ "$rc" -ne 0 ] && ! grep -q "ALREADY UPSTREAM" "$TMP/out"; then
  ok "N: a patch applied AND reverted upstream is NOT cleared (re-applying ships work)"
else
  bad "N: a reverted-upstream patch was waved through as a duplicate"
  sed 's/^/       /' "$TMP/out"
fi

echo "push-guard range: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
