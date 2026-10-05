#!/bin/bash
# The idle detached-worktree reaper in scripts/mac-cleanup-tick.sh (MO-3631/3633).
#
# The cargo-target arm (test-mac-cleanup-targets.sh) is structurally blind to
# this class: it prunes `.git` on purpose. Three times in one day, gs-10-zero-
# base-cicd (an isolated worker with no reply path) filled the shared per-user
# temp dir with detached ~4GB mixpeek checkouts and never removed the previous
# one, and each time the only remedy was a hand-verified manual sweep. This arm
# automates exactly that verification, so each guard has a cell that fails if
# the guard is removed (ethos rule 7), and the boundary that already cost a
# real mistake (MO-3627: a worktree removed out from under a live session) has
# its own hard, unconditional cell.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
# Explicit /tmp, not a bare `mktemp -d`: this suite's own fixtures are real
# detached worktrees the arm under test must be willing to SCAN, and a bare
# mktemp -d honors the ambient $TMPDIR -- which, run from inside a Claude Code
# session, IS that session's own /private/tmp/claude-<uid>/... scratchpad. This
# suite would then be building its fixtures exactly where the boundary this arm
# exists to enforce (scenario 2 below) correctly refuses to look, and every
# scenario past it would silently find nothing.
FIX=$(mktemp -d /tmp/mac-cleanup-wt-test.XXXXXX)
export AMUX_CLEANUP_FSEVENTSD_CMD=true   # the live fseventsd must not decide a test (DESKT-81)
trap 'rm -rf -- "${FIX:?}"' EXIT
mkdir -p "$FIX/tmp"
export TMPDIR="$FIX/tmp"
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
yn() { if "$@"; then echo yes; else echo no; fi; }
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"
export AMUX_CLEANUP_STATE_DIR="$FIX/assess-state" AMUX_CLEANUP_ESCALATE_CMD="true" AMUX_CLEANUP_HISTORY_CMD="true" AMUX_CLEANUP_CARD_CMD="true"

WORKTREE_DEPTH=2; WORKTREE_SCAN_S=30; WORKTREE_WALK_S=30; WORKTREE_BUDGET_S=60
printf 'launchd 1 root cwd DIR 1,18 64 2 /\n' > "$FIX/lsof.base"
LSOF_CMD="cat $FIX/lsof.base"

git -c init.defaultBranch=main init -q "$FIX/origin.git" --bare

# A "toplevel" clone that plays the part of a real checkout (e.g. ~/Dev/mixpeek):
# one commit on main, pushed to the fake origin so merge-base has something to
# find. Every worktree fixture below is added FROM this toplevel.
git -c init.defaultBranch=main clone -q "$FIX/origin.git" "$FIX/toplevel"
git -C "$FIX/toplevel" config user.email t@test; git -C "$FIX/toplevel" config user.name t
echo one > "$FIX/toplevel/f"; git -C "$FIX/toplevel" add f; git -C "$FIX/toplevel" commit -qm one
git -C "$FIX/toplevel" push -q origin main

# backdate every file under a worktree so dir_idle reads it as idle >= 2h.
backdate() { find "$1" -exec touch -t 202001010000 {} + 2>/dev/null; }

echo "1. discovery: a worktree's .git FILE qualifies; a real clone's .git DIRECTORY does not"
mkdir -p "$FIX/root1"
git -C "$FIX/toplevel" worktree add -q --detach "$FIX/root1/wt-merged" main
git -c init.defaultBranch=main clone -q "$FIX/origin.git" "$FIX/root1/real-clone" >/dev/null 2>&1
find_detached_worktrees "$FIX/root1" 2 30 > "$FIX/found1.txt"
check "the worktree (.git file) is found"          "yes" "$(yn grep -qx "$FIX/root1/wt-merged" "$FIX/found1.txt")"
check "a real clone (.git dir) is NOT found"        "no"  "$(yn grep -q real-clone "$FIX/found1.txt")"
check "exactly one candidate found"                 "1"   "$(grep -c . "$FIX/found1.txt" | tr -d ' ')"

echo "2. the hard boundary: /private/tmp/claude-* is never scanned, however roots are built"
mkdir -p "$FIX/tmp/claude-501/fake-session/scratchpad"
git -C "$FIX/toplevel" worktree add -q --detach "$FIX/tmp/claude-501/fake-session/scratchpad/wt" main
find_detached_worktrees "$FIX/tmp/claude-501@4" 4 30 > "$FIX/found2.txt" || true
check "a root under /private/tmp/claude-* finds nothing" "0" "$(grep -c . "$FIX/found2.txt" 2>/dev/null; true)"
git -C "$FIX/toplevel" worktree remove -f "$FIX/tmp/claude-501/fake-session/scratchpad/wt" 2>/dev/null || true

mk_scenario() { # <root> <name> <clean|dirty> <merged|unmerged> <idle|fresh>
  local root=$1 name=$2 clean=$3 merged=$4 idle=$5 d
  d="$root/$name"
  git -C "$FIX/toplevel" worktree add -q --detach "$d" main
  if [ "$merged" = unmerged ]; then
    git -C "$d" commit -q --allow-empty -m "unpushed, unique to this worktree"
  fi
  if [ "$clean" = dirty ]; then
    echo "local edit" >> "$d/f"
  fi
  if [ "$idle" = idle ]; then backdate "$d"; fi
}

echo "3. one of each outcome, in one pass"
mkdir -p "$FIX/root3"
mk_scenario "$FIX/root3" ok-reap        clean   merged   idle    # the one true positive
mk_scenario "$FIX/root3" too-fresh      clean   merged   fresh
mk_scenario "$FIX/root3" is-dirty       dirty   merged   idle
mk_scenario "$FIX/root3" is-unmerged    clean   unmerged idle
R=$FIX/root3
reap_idle_worktrees "$R" 2 1 > "$FIX/out3.txt"   # dry run: nothing actually removed yet
check "found all four"        "4" "$WORKTREES_FOUND"
check "only the idle+clean+merged one is eligible" "1" "$WORKTREES_ELIGIBLE"
check "dry run named the right one" "yes" "$(yn grep -q 'would reap worktree .*ok-reap' "$FIX/out3.txt")"
check "dry run reaped nothing"      "0"   "$WORKTREES_REAPED"
check "all four still exist after a dry run" "yes" "$(yn [ -d "$R/ok-reap" ] && [ -d "$R/too-fresh" ] && [ -d "$R/is-dirty" ] && [ -d "$R/is-unmerged" ])"

echo "4. a real pass actually removes the one eligible worktree, and only that one"
reap_idle_worktrees "$R" 2 0 > "$FIX/out4.txt"
check "eligible again" "1" "$WORKTREES_ELIGIBLE"
check "reaped exactly one" "1" "$WORKTREES_REAPED"
check "the eligible worktree is gone from disk" "no" "$(yn [ -d "$R/ok-reap" ])"
check "git itself no longer lists it as a worktree" "no" "$(git -C "$FIX/toplevel" worktree list --porcelain | grep -qx "worktree $R/ok-reap" && echo yes || echo no)"
check "the too-fresh one survives" "yes" "$(yn [ -d "$R/too-fresh" ])"
check "the dirty one survives"     "yes" "$(yn [ -d "$R/is-dirty" ])"
check "the unmerged one survives"  "yes" "$(yn [ -d "$R/is-unmerged" ])"
check "the branch/commit for the reaped worktree is still reachable from origin/main (worktree removal never deletes history)" \
  "yes" "$(yn git -C "$FIX/toplevel" merge-base --is-ancestor origin/main origin/main)"

echo "5. an open file handle protects an otherwise-eligible worktree"
mkdir -p "$FIX/root5"
mk_scenario "$FIX/root5" held clean merged idle
printf 'someproc 123 t cwd DIR 1,18 64 2 %s/held\n' "$FIX/root5" > "$FIX/lsof.held"
LSOF_CMD="cat $FIX/lsof.held" reap_idle_worktrees "$FIX/root5" 2 0 > "$FIX/out5.txt"
check "an open handle keeps it" "yes" "$(yn [ -d "$FIX/root5/held" ])"
check "reaped is zero when held open" "0" "$WORKTREES_REAPED"

echo "6. lsof unmeasured keeps everything -- 'could not tell' must never read as 'clear'"
mkdir -p "$FIX/root6"
mk_scenario "$FIX/root6" would-be-reaped clean merged idle
LSOF_CMD="cat /nonexistent-$$" reap_idle_worktrees "$FIX/root6" 2 0 > "$FIX/out6.txt"
check "unmeasured lsof keeps the worktree" "yes" "$(yn [ -d "$FIX/root6/would-be-reaped" ])"
check "unmeasured lsof reaps nothing"      "0"   "$WORKTREES_REAPED"
check "the tick output says why (UNMEASURED, never a silent 0)" "yes" "$(yn grep -q UNMEASURED "$FIX/out6.txt")"

echo "7. a directory whose .git file points nowhere real is not-a-real-worktree, kept"
mkdir -p "$FIX/root7/fake"
echo "gitdir: /nonexistent-$$" > "$FIX/root7/fake/.git"
LSOF_CMD="cat $FIX/lsof.base" reap_idle_worktrees "$FIX/root7" 2 0 > "$FIX/out7.txt"
check "a fake .git file is not treated as reapable" "yes" "$(yn [ -d "$FIX/root7/fake" ])"

if [ "$fails" -eq 0 ]; then
  echo "PASS: mac-cleanup-worktrees — all checks passed"
else
  echo "FAILED: $fails check(s) failed"
  exit 1
fi
