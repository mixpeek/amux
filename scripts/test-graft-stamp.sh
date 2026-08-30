#!/usr/bin/env bash
# AMUX-3782 — a GRAFTED commit must be as attributable as a committed one.
#
# `git commit-tree` is plumbing: it fires no hooks, so `prepare-commit-msg`
# never runs and a graft-pushed commit carries no `Amux-Session` trailer. On a
# shared checkout where every lane commits as the same git author, that trailer
# is the only discriminator, so the workflow used to land work past a peer's
# dirty worktree is the workflow that erases who did it.
#
# MEASURED 2026-08-26, last 400 commits on each repo's origin/main:
#   amux     380/400 present (95%) — lanes here commit with `git commit`
#   mixpeek  202/400 present (50%) — the repo where grafting is routine
# The absence is not spread evenly; it concentrates in the graft path.
#
# THE FIX IS NOT A NEW SNIPPET. mixpeek-homepage-claude proposed adding a
# `git interpret-trailers --trailer "Amux-Session: $AMUX_SESSION"` line to the
# graft recipe, tested and working. It works, and it is a SECOND PRODUCER of
# the trailer block — which had already drifted before it was written: AMUX-3780
# added `Amux-Conversation` to the hook, and a hand-rolled one-liner stamps only
# the field its author knew about. Cell D is exactly that discriminator; it is
# green for "call the shipped hook" and red for the snippet.
#
# The hook already takes a message file and stamps it in place, so the recipe is
# to run the same code git would have run:
#
#     .git/hooks/prepare-commit-msg "$MSG"
#     SHA=$(git commit-tree -p origin/main -F "$MSG" "$TREE")
#
# This test installs the TRACKED hook into a throwaway fixture and then invokes
# that literal recipe path, so it pins the string a reader will copy rather than
# a paraphrase of it.
#
# CELL B IS THE LOAD-BEARING CONTROL. Every other cell asserts a trailer is
# present; without B, a version of this file that stamped nothing and read the
# trailer off the wrong object would pass. B omits the hook step and requires
# the trailer to be ABSENT.
#
# Exit 0 = pass, 1 = failure.
set -uo pipefail
cd "$(dirname "$0")/.."
REPO=$(pwd)
# GRAFT_STAMP_HOOK exists so these cells can be shown to FAIL: point it at a
# mutated copy of the hook and the cell that covers the mutated line goes red.
# Verified 2026-08-26 — drop the Amux-Conversation block and D fails ALONE (that
# mutant IS the hand-rolled snippet); drop the final-newline guard and C fails
# alone, reproducing the 2026-08-22 glue bug verbatim as
# `subject='subject only, no newline Amux-Session: testlane'`; make the hook a
# no-op and A, C, D, E all fail while the B control stays green.
HOOK_SRC="${GRAFT_STAMP_HOOK:-$REPO/scripts/git-hooks/prepare-commit-msg}"
[ -r "$HOOK_SRC" ] || { echo "FAIL: no hook at $HOOK_SRC"; exit 1; }

D=$(mktemp -d) || exit 1
trap 'rm -rf "$D"' EXIT

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   — $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL — $1"; }

# An isolated AMUX_HOME: the hook reads the lane's conversation id from session
# meta, and this must never depend on (or touch) the real ~/.amux.
export AMUX_HOME="$D/amuxhome"
mkdir -p "$AMUX_HOME/sessions"
printf '{"cc_conversation_id":"conv-abc-123"}' > "$AMUX_HOME/sessions/testlane.meta.json"
export AMUX_SESSION=testlane

git init -q "$D/r" || exit 1
cd "$D/r" || exit 1
git config user.email t@example.com
git config user.name  t
echo base > f
git add f
git commit -qm base            # before the hook is installed, so it is unstamped
BASE=$(git rev-parse HEAD)
TREE=$(git rev-parse 'HEAD^{tree}')

# Install exactly as install-hooks.sh does, so the recipe path below is real.
cp "$HOOK_SRC" .git/hooks/prepare-commit-msg
chmod +x .git/hooks/prepare-commit-msg

trailer() { git log -1 --format="%(trailers:key=$2,valueonly,separator=|)" "$1" | tr -d '[:space:]'; }

echo "graft stamp — the shipped hook, invoked by hand, must survive commit-tree"

# ── A: the recipe, verbatim ─────────────────────────────────────────────────
printf 'subject line\n\nbody paragraph\n' > "$D/m1"
.git/hooks/prepare-commit-msg "$D/m1"
C1=$(git commit-tree "$TREE" -p "$BASE" -F "$D/m1")
[ "$(trailer "$C1" Amux-Session)" = "testlane" ] \
  && ok "A: Amux-Session parses on a commit-tree commit" \
  || bad "A: expected 'testlane', got '$(trailer "$C1" Amux-Session)'"

# ── D: the field a hand-rolled one-liner drops (AMUX-3780) ──────────────────
[ "$(trailer "$C1" Amux-Conversation)" = "conv-abc-123" ] \
  && ok "D: Amux-Conversation stamped too — one producer, not two" \
  || bad "D: expected 'conv-abc-123', got '$(trailer "$C1" Amux-Conversation)'"

# ── B: NEGATIVE CONTROL — omit the step, the trailer must be absent ─────────
printf 'subject line\n\nbody paragraph\n' > "$D/m2"
C2=$(git commit-tree "$TREE" -p "$BASE" -F "$D/m2")
[ -z "$(trailer "$C2" Amux-Session)" ] \
  && ok "B: control — skipping the step leaves it UNstamped, so this test discriminates" \
  || bad "B: control was stamped anyway ('$(trailer "$C2" Amux-Session)') — the test proves nothing"

# ── C: subject-only with no trailing newline (the 2026-08-22 glue bug) ──────
# git 2.39 welds a trailer onto an unterminated last line: subject and trailer
# become one paragraph, the trailer is unparseable, and the push guard then
# reads the lane's own commit as foreign. The hook's final-newline guard is what
# prevents it, and grafts are the path most likely to hand over a bare subject.
printf 'subject only, no newline' > "$D/m3"
.git/hooks/prepare-commit-msg "$D/m3"
C3=$(git commit-tree "$TREE" -p "$BASE" -F "$D/m3")
S3=$(git log -1 --format=%s "$C3")
if [ "$(trailer "$C3" Amux-Session)" = "testlane" ] && [ "$S3" = "subject only, no newline" ]; then
  ok "C: subject-only message parses AND the subject is not glued"
else
  bad "C: trailer='$(trailer "$C3" Amux-Session)' subject='$S3'"
fi

# ── E: idempotent — a retried graft must not stack duplicate trailers ───────
printf 'subject\n\nbody\n' > "$D/m4"
.git/hooks/prepare-commit-msg "$D/m4"
.git/hooks/prepare-commit-msg "$D/m4"
C4=$(git commit-tree "$TREE" -p "$BASE" -F "$D/m4")
[ "$(trailer "$C4" Amux-Session)" = "testlane" ] \
  && ok "E: idempotent — a second run adds no duplicate" \
  || bad "E: expected 'testlane', got '$(trailer "$C4" Amux-Session)'"

# ── F: a peer's trailer survives ────────────────────────────────────────────
# Grafting someone else's commit is legitimate (mixpeek's graft-push.sh calls it
# --i-own-this). Overwriting their trailer with the pusher's lane would credit
# their work to you, which is the misattribution this whole field exists to stop.
printf 'subject\n\nbody\n\nAmux-Session: peer-lane\n' > "$D/m5"
.git/hooks/prepare-commit-msg "$D/m5"
C5=$(git commit-tree "$TREE" -p "$BASE" -F "$D/m5")
[ "$(trailer "$C5" Amux-Session)" = "peer-lane" ] \
  && ok "F: an existing peer trailer is preserved, not overwritten" \
  || bad "F: expected 'peer-lane', got '$(trailer "$C5" Amux-Session)'"

echo
echo "pass=$PASS fail=$FAIL"
[ "$FAIL" = 0 ]
