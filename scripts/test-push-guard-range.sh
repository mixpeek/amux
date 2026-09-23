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
set -euo pipefail
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

# ── Q/R. AF-479: a foreign stamp on a commit THIS lane actually made ────────
# prepare-commit-msg adds `Amux-Committer` only when the message already
# declared a different `Amux-Session`. Two very different things land here as
# foreign — a cherry-pick keeping its real author, and a hand-typed stamp naming
# the wrong lane — and every exit this refusal offers assumes the first. Measured
# 2026-09-04: ac550324 was stamped `amux` from a typed trailer while the lane was
# `amux-frustrations`, so the guard would have prescribed asking a peer for
# consent to ship work they never touched.
#
# The cell asserts the ROUTING NOTE, and R asserts the commit is still BLOCKED.
# Splitting them is the point: a "fix" that cleared these commits would pass Q
# and hollow the guard out for exactly the cherry-picked-peer-WIP case it exists
# to stop.
Q="$TMP/q"; mkrepo "$Q"
echo committed-here > "$Q/work/qfile"; git -C "$Q/work" add qfile
git -C "$Q/work" commit -qm "typed the wrong lane

Amux-Session: other-lane
Amux-Committer: mine"
QTIP=$(git -C "$Q/work" rev-parse HEAD)
rc=$(run_hook "$Q" "$QTIP" "$ZERO" main)
if [ "$rc" -ne 0 ] && grep -q "COMMITTED BY YOU, STAMPED TO ANOTHER LANE" "$TMP/out"; then
  ok "Q: a foreign stamp with Amux-Committer=mine is named as committed here"
else
  bad "Q: the refusal sent me to ask a peer about a commit this lane made (rc=$rc)"
  sed 's/^/       /' "$TMP/out"
fi
if [ "$rc" -ne 0 ] && grep -q "PUSH BLOCKED" "$TMP/out"; then
  ok "R: and it is STILL BLOCKED — a cherry-picked peer WIP has this exact shape"
else
  bad "R: the committer note cleared the commit; the guard is hollowed out (rc=$rc)"
  sed 's/^/       /' "$TMP/out"
fi

# ── S. CONTROL for Q. Same foreign stamp, no Amux-Committer trailer. ────────
# Without this, a note printed unconditionally on every foreign commit would
# pass Q and tell every reader their peer's commit was really their own.
S="$TMP/s"; mkrepo "$S"
commit_as "$S" "other-lane" "genuinely-theirs"
STIP=$(git -C "$S/work" rev-parse HEAD)
rc=$(run_hook "$S" "$STIP" "$ZERO" main)
if [ "$rc" -ne 0 ] && ! grep -q "COMMITTED BY YOU" "$TMP/out"; then
  ok "S: a plain foreign commit gets no committed-here note (presence is the signal)"
else
  bad "S: the note fired on a commit with no Amux-Committer trailer (rc=$rc)"
  sed 's/^/       /' "$TMP/out"
fi

# ── T-W. CITED author consent for an ISOLATED author (AMUX-4972) ───────────
# Cells O and P above pin that the bare two-field form is REFUSED for an
# isolated author, on the reasoning that its consent cannot have been obtained.
# That reasoning is half right: isolation refuses INBOUND sends, so such an
# author cannot be ASKED, but nothing stops it INITIATING. On 2026-09-23 one
# did exactly that, and the guard had no field able to carry the yes, leaving
# only forms that assert something nobody said (ethos rule 3).
#
# These four are chosen so no ONE of them passes a broken implementation, the
# same discipline as E-H. T proves a citation WORKS; U, V and W each prove it
# is stricter than a second way around the guard. An implementation that
# accepted any 4-field token would pass T and fail all three others, and one
# that refused everything would fail T alone.
cat > "$TMP/msgstore.py" <<'PYEOF'
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
fixture = json.load(open(sys.argv[1]))
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        rec = fixture.get(self.path.rsplit("/", 1)[-1])
        if rec is None:
            self.send_response(404); self.end_headers(); self.wfile.write(b"{}"); return
        body = json.dumps(rec).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass
srv = HTTPServer(("127.0.0.1", 0), H)
open(sys.argv[2], "w").write(str(srv.server_port))
srv.serve_forever()
PYEOF

# A throwaway store on loopback, so these cells drive the REAL resolution path
# rather than a stub of it. Hermetic: no amux server, and the fixture is built
# from the actual commit time so the ordering rule is checked against a real
# value instead of a constant that would still pass if the comparison inverted.
MSGPID=""
start_msgstore() {
  rm -f "${TMP:?}/msgport"
  # stdout/stderr to /dev/null, or the `MPORT=$(start_msgstore ...)` command
  # substitution below blocks until this background server closes stdout,
  # which it never does. The port travels via the file, not the pipe.
  python3 "$TMP/msgstore.py" "$1" "$TMP/msgport" >/dev/null 2>&1 & MSGPID=$!
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    [ -s "$TMP/msgport" ] && break
    sleep 0.2
  done
  cat "$TMP/msgport" 2>/dev/null
}
stop_msgstore() { [ -n "$MSGPID" ] && kill "$MSGPID" 2>/dev/null || true; MSGPID=""; }

run_hook_consent_at() {
  local d="$1" local_sha="$2" remote_sha="$3" ref="$4" consent="$5" url="$6"
  ( cd "$d/work" && \
    echo "refs/heads/$ref $local_sha refs/heads/$ref $remote_sha" | \
    AMUX_SESSION=mine AMUX_ALLOW_FOREIGN= AMUX_FOREIGN_CONSENT="$consent" \
    HOME="$TMP/fakehome" \
    AMUX_URL="$url" AMUX_PUSH_GUARD_API_TIMEOUT_S=5 \
    python3 "$HOOK" origin "$d/origin.git" ) > "$TMP/out" 2>&1
  echo $?
}

T="$TMP/t"; mkrepo "$T"
git -C "$T/work" checkout -q -b feat
commit_as "$T" "desktop" "isolated-theirs"
commit_as "$T" "mine" "mine-one"
TTIP=$(git -C "$T/work" rev-parse feat)
TSHA=$(git -C "$T/work" log --format=%h --all --grep="isolated-theirs" | head -1)
TCT=$(git -C "$T/work" log -1 --format=%ct "$TSHA")
python3 - "$TMP/fx.json" "$TCT" <<'PYEOF'
import json, sys
ct = int(sys.argv[2])
json.dump({
    # sender matches the commit trailer, and postdates it
    "100": {"id": 100, "origin": "desktop", "session": "mine", "ts": (ct + 60) * 1000},
    # right shape, WRONG sender
    "101": {"id": 101, "origin": "somebody-else", "session": "mine", "ts": (ct + 60) * 1000},
    # right sender, but recorded BEFORE the commit existed
    "102": {"id": 102, "origin": "desktop", "session": "mine", "ts": (ct - 60) * 1000},
}, open(sys.argv[1], "w"))
PYEOF
MPORT=$(start_msgstore "$TMP/fx.json")

# T. A citation resolving to the author's own stamped message clears the push,
# and SAYS which message it relied on.
rc=$(run_hook_consent_at "$T" "$TTIP" "$ZERO" feat "$TSHA:desktop:msg:MSG-100" "http://127.0.0.1:$MPORT")
if [ "$rc" -eq 0 ] && grep -q "MSG-100" "$TMP/out"; then
  ok "T: a cited consent message from the ISOLATED author clears the push and names MSG-100"
else
  bad "T: a valid consent citation was refused (rc=$rc)"; sed 's/^/       /' "$TMP/out"
fi

# T2. The audit must record this as its OWN fact. "the author volunteered, here
# is the id" is not "the owner granted it on their behalf", and a log that
# spells one as the other cannot answer who authorized the push later.
# SCOPED TO THE LINE THIS PUSH WROTE, not to the file. The log accumulates
# across every cell in this run, so cell J's legitimate CONSENT-OWNER entry is
# already in it; a file-wide `! grep CONSENT-OWNER` fails on a correct
# implementation. The question is which KIND the line carrying MSG-100 has.
_audlog="$TMP/fakehome/.amux/logs/push-guard.log"
if grep -q "CONSENT-MSG .*MSG-100" "$_audlog" 2>/dev/null \
   && ! grep -q "CONSENT-OWNER.*MSG-100" "$_audlog" 2>/dev/null; then
  ok "T2: the cleared push is audited as CONSENT-MSG, distinct from CONSENT-OWNER"
else
  bad "T2: the audit did not record a cited consent distinctly"
  sed 's/^/       /' "$_audlog" 2>/dev/null || true
fi

# U. THE CONTROL FOR T. A message that exists and resolves, but was sent by
# someone other than the commit's author, must REFUSE. Without this, citing any
# message id at all would clear any commit.
rc=$(run_hook_consent_at "$T" "$TTIP" "$ZERO" feat "$TSHA:desktop:msg:101" "http://127.0.0.1:$MPORT")
if [ "$rc" -ne 0 ] && grep -q "was sent by" "$TMP/out"; then
  ok "U: a citation whose sender is NOT the commit author is REFUSED"
else
  bad "U: a message from the wrong sender cleared the push (rc=$rc)"; sed 's/^/       /' "$TMP/out"
fi

# V. A yes recorded BEFORE the commit existed cannot be about it, and
# back-dating is the cheapest way to manufacture one.
rc=$(run_hook_consent_at "$T" "$TTIP" "$ZERO" feat "$TSHA:desktop:msg:102" "http://127.0.0.1:$MPORT")
if [ "$rc" -ne 0 ] && grep -q "predates" "$TMP/out"; then
  ok "V: a citation PREDATING the commit it consents to is REFUSED"
else
  bad "V: a back-dated message cleared the push (rc=$rc)"; sed 's/^/       /' "$TMP/out"
fi
stop_msgstore

# W. THE ONE THAT DECIDES WHETHER THIS IS SAFE AT ALL. With the store
# unreachable, the SAME citation that passed T must refuse. If an unresolvable
# citation were accepted, an offline server would become a blanket override,
# which is the exact shape the consent mechanism exists to avoid. This also
# pins CI behaviour: CI has no amux server, so every citation refuses there,
# deliberately. run_hook_consent already points AMUX_URL at 127.0.0.1:9.
rc=$(run_hook_consent "$T" "$TTIP" "$ZERO" feat "$TSHA:desktop:msg:MSG-100")
if [ "$rc" -ne 0 ] && grep -q "could not be read" "$TMP/out"; then
  ok "W: with the message store unreachable the citation REFUSES, never assumes"
else
  bad "W: an unresolvable citation was accepted — offline is now a blanket override (rc=$rc)"
  sed 's/^/       /' "$TMP/out"
fi

echo "push-guard range: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
