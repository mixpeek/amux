#!/usr/bin/env bash
# AMUX-3726 / AF-182 — the pre-commit gate must answer "is what I am COMMITTING
# sound", not "is the tree sound".
#
# The defect: the gate builds the WORKING TREE. On a shared checkout that cannot
# distinguish "your staged change is broken" from "a peer is mid-edit two files
# away", so a peer's uncommitted work refuses YOUR commit. Three AF-182
# instances ended in `--no-verify`, which also drops the security scan, the
# staged-guard, the append-only guard and the JS checks.
#
# `_amux_staged_recheck` materialises the INDEX in a worktree and re-runs the
# build there, but only when the tree is already red AND lint-blame proved
# (exit 10) that none of the offenders are staged.
#
# CELL 4 IS THE ONE THAT GOES WRONG QUIETLY. A version that always built the
# index would pass cells 1-3 perfectly and cost the fleet ~22s on EVERY commit,
# forever. So it asserts the BUILD DID NOT RUN when the re-check is not licensed.
#
# Its first draft counted WORKTREES after the call and was worthless: the trap
# removes the worktree on the way out, so the count is 1 either way. It detected
# a LEAK, not a build, while its comment claimed the opposite — and the mutation
# that removes the exit-10 gate (precisely the always-builds version) sailed
# past it. Caught by running that mutation rather than by re-reading the cell.
# The stub build now drops a marker file, so "did it build" is observable.
#
# Cargo is not invoked here: these cells drive the DECISION logic with a stub
# build command, because what is under test is when the re-check runs and what
# it concludes — not whether cargo works. The cost of a real build (~22s, and it
# does not amortise) is measured on the card, not re-measured per cell.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT_REPO="$(pwd)"
PASS=0; FAIL=0
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
ok()  { PASS=$((PASS+1)); echo "  ok   — $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL — $1"; }

# Lift the helper out of the shipped hook rather than restating it: the hook is
# what runs, and simulating what it is believed to do cannot catch it doing
# something else.
HOOK="${STAGED_RECHECK_HOOK:-$ROOT_REPO/scripts/git-hooks/pre-commit}"
python3 - "$HOOK" "$TMP/helper.sh" <<'PY'
import sys
src = open(sys.argv[1]).read()
start = src.index("_amux_staged_recheck() {")
# Balance to the closing brace at column 0.
end = src.index("\n}\n", start) + 3
open(sys.argv[2], "w").write(src[start:end])
PY
grep -q "AMUX_STAGED_RECHECK" "$TMP/helper.sh" || { echo "  SETUP FAILED — helper not extracted"; exit 1; }

echo "AMUX-3726 — staged-content re-check"

# A throwaway repo with one committed file and one staged change.
REPO="$TMP/repo"; mkdir -p "$REPO"; cd "$REPO"
git init -q -b main; git config user.email t@t; git config user.name t
echo "mine v1" > mine.txt; echo "peer v1" > peer.txt
git add -A >/dev/null; git -c core.hooksPath=/dev/null commit -qm seed
# The helper resolves lint-blame under $ROOT, so the fixture repo must carry it
# — in production $ROOT is the amux checkout, which does. Copying the SHIPPED
# script rather than a stub: its exit-10 contract is half of what is under test.
mkdir -p scripts/git-hooks
cp "$ROOT_REPO/scripts/git-hooks/lint-blame.py" scripts/git-hooks/
git add -A >/dev/null; git -c core.hooksPath=/dev/null commit -qm hooks
grep -q "return 10" scripts/git-hooks/lint-blame.py \
  || { echo "  SETUP FAILED — the copied lint-blame has no exit-10 contract"; exit 1; }

echo "mine v2" > mine.txt; git add mine.txt        # STAGED
echo "peer DIRTY" > peer.txt                        # peer's uncommitted edit

run_cell() {   # $1 = clippy output, $2 = stub build cmd ; echoes rc
  ( set +e
    ROOT="$REPO"
    STAGED="mine.txt"
    export ROOT STAGED
    # shellcheck disable=SC1090
    . "$TMP/helper.sh"
    _amux_staged_recheck "$1" "$2" >/dev/null 2>&1
    echo $? )
}

# --- CELL 1: tree red from a FOREIGN file, staged content clean -> ALLOW.
rc=$(run_cell "error: x
  --> peer.txt:1:1" "true")
[ "$rc" = "0" ] && ok "1: foreign offender + clean staged content -> allowed" \
                || bad "1: should have allowed (rc=$rc)"

# --- CELL 2: the staged file IS an offender -> REFUSE, without building at all.
# This is what stops the fix from being "make the gate lenient".
rc=$(run_cell "error: x
  --> mine.txt:1:1" "true")
[ "$rc" != "0" ] && ok "2: staged file among the offenders -> refused" \
                 || bad "2: allowed a commit whose OWN staged file is red"

# --- CELL 3: foreign offender, but the staged content is ALSO broken -> REFUSE.
rc=$(run_cell "error: x
  --> peer.txt:1:1" "false")
[ "$rc" != "0" ] && ok "3: staged content fails its own build -> refused" \
                 || bad "3: allowed although the index build failed"

# --- CELL 4: THE QUIET ONE. The build must NOT run when the re-check is not
# licensed. Asserted on a MARKER the stub build writes, because "it refused" is
# equally true of a version that built first and refused after — and that
# version costs the fleet 22s on every red commit.
MARK="$TMP/built.marker"; rm -f "$MARK"
rc=$(run_cell "error: x
  --> mine.txt:1:1" "touch '$MARK'")
[ ! -f "$MARK" ] && ok "4: an unlicensed re-check does NOT build the index" \
                 || bad "4: the index was built for a commit whose own file is red (22s/commit)"
# CONTROL for cell 4: the marker mechanism must be able to fire at all, or the
# assertion above passes against a stub that never writes it.
rm -f "$MARK"
rc=$(run_cell "error: x
  --> peer.txt:1:1" "touch '$MARK'")
[ -f "$MARK" ] && ok "4b: control — a LICENSED re-check does build the index" \
               || bad "4b: the marker never fires, so cell 4 proves nothing"

# --- CELL 5: the opt-out is honoured, and it REFUSES rather than allowing.
rc=$( AMUX_STAGED_RECHECK=0 run_cell "error: x
  --> peer.txt:1:1" "true" )
[ "$rc" != "0" ] && ok "5: AMUX_STAGED_RECHECK=0 falls back to refusing" \
                 || bad "5: the opt-out allowed the commit (rc=$rc)"

# --- CELL 6: no worktree is left behind after a cell that DID build.
git -C "$REPO" worktree prune >/dev/null 2>&1
n=$(git -C "$REPO" worktree list | wc -l | tr -d ' ')
[ "$n" = "1" ] && ok "6: no worktree left behind (only the main one)" \
               || bad "6: $n worktrees remain — a leak is a 15GB-class hazard here"

echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
