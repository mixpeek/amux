#!/bin/bash
# Safety properties of scripts/reap-amux-debris.sh, against a FIXTURE root.
#
# This reaper deletes directories without asking, so the properties that keep it
# safe are the ones worth pinning: it takes only amux's own prefixes, only past
# the age floor, never a live session's scratchpad, and nothing at all without
# --apply. Each cell below fails if its guard is removed — the point of the test
# is that it can go red, not that it is green today (ethos rule 7).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
REAPER="$HERE/reap-amux-debris.sh"
FIX=$(mktemp -d)                     # never a fixed name: /tmp is shared by every lane
trap 'rm -rf -- "$FIX"' EXIT
fails=0
# Hermetic git: the cells assert what `git status` reports, and a developer's
# global excludes (node_modules/ is a common one) would hide the untracked
# entries cell 8 depends on, so it passed vacuously on one Mac and not in CI.
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
# Setup and helper failures abort before a success verdict. Assertion failures
# still accumulate because check() handles them explicitly rather than returning
# a failing shell status. Keep the missing-helper diagnostic as well.
[ -x "$REAPER" ] || { echo "FAIL: $REAPER is missing or not executable — no cell below ran"; exit 1; }
check() { # check <label> <expected> <actual>
  if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi
}

seed() {
  rm -rf -- "${FIX:?}"/* 2>/dev/null
  mkdir -p "$FIX/amux-lc-stale" "$FIX/amux-lc-fresh" "$FIX/amux-e2e-stale" \
           "$FIX/claude-501" "$FIX/not-ours" "$FIX/amux-lc-stale-but-claude-501"
  # Backdate everything that should be eligible well past the 6h floor.
  for d in amux-lc-stale amux-e2e-stale not-ours; do
    touch -t 202501010000 "$FIX/$d" 2>/dev/null
  done
  # A live session scratchpad, old enough to qualify on age alone.
  touch -t 202501010000 "$FIX/claude-501" 2>/dev/null
  mkdir -p "$FIX/claude-501/amux-lc-inside"
  touch -t 202501010000 "$FIX/claude-501/amux-lc-inside" 2>/dev/null
}

echo "1. dry run deletes nothing"
seed
AMUX_DEBRIS_ROOTS="$FIX" "$REAPER" --repo /nonexistent >/dev/null 2>&1
check "stale dir survives a dry run" "yes" "$([ -d "$FIX/amux-lc-stale" ] && echo yes || echo no)"

echo "2. --apply takes the stale amux dirs"
seed
AMUX_DEBRIS_ROOTS="$FIX" "$REAPER" --apply --repo /nonexistent >/dev/null 2>&1
check "stale amux-lc- removed"  "no"  "$([ -d "$FIX/amux-lc-stale" ] && echo yes || echo no)"
check "stale amux-e2e- removed" "no"  "$([ -d "$FIX/amux-e2e-stale" ] && echo yes || echo no)"

echo "3. the guards hold"
check "fresh dir kept (age floor)"        "yes" "$([ -d "$FIX/amux-lc-fresh" ] && echo yes || echo no)"
check "non-amux prefix untouched"         "yes" "$([ -d "$FIX/not-ours" ] && echo yes || echo no)"
# The claude-501 guard is only reachable when the ROOT PATH itself is a live
# session scratchpad — the reaper walks `find -maxdepth 1 -name '<prefix>*'`, so
# a sibling directory merely NAMED claude-501 never becomes a candidate and
# asserting on one is a check that cannot fail. Point a root at a claude-501
# path holding a stale amux dir, which is the shape the guard exists for.
SCRATCH="$FIX/claude-501/-Users-ethan-Dev-amux/scratchpad"
mkdir -p "$SCRATCH/amux-lc-inside-live-session"
touch -t 202501010000 "$SCRATCH/amux-lc-inside-live-session" 2>/dev/null
AMUX_DEBRIS_ROOTS="$SCRATCH" "$REAPER" --apply --repo /nonexistent >/dev/null 2>&1
check "stale amux dir inside a live claude-501 scratchpad kept" "yes" \
  "$([ -d "$SCRATCH/amux-lc-inside-live-session" ] && echo yes || echo no)"

echo "4. a dirty worktree is never removed"
# Real repo, real worktree, one uncommitted byte: `git worktree remove` must
# refuse it and the reaper must report it as dirty rather than forcing.
WTREPO="$FIX/repo"; mkdir -p "$WTREPO"
git -C "$WTREPO" init -q 2>/dev/null
git -C "$WTREPO" -c user.email=t@t -c user.name=t commit -q --allow-empty -m seed 2>/dev/null
WT="$FIX/wt-dirty"
git -C "$WTREPO" worktree add --detach "$WT" -q 2>/dev/null
echo dirty > "$WT/uncommitted.txt"
touch -t 202501010000 "$WT" 2>/dev/null
# The reaper only considers worktrees under a scratch root; $FIX is under one
# on macOS (/var/folders) and Linux (/tmp), which is why mktemp -d is used here.
out=$(AMUX_DEBRIS_ROOTS="$FIX" "$REAPER" --apply --repo "$WTREPO" 2>&1)
check "dirty worktree still present" "yes" "$([ -d "$WT" ] && echo yes || echo no)"
# Assert the COUNT. Matching only the label passed while the arm considered no
# worktree at all, because the label prints with a zero too.
case "$out" in *"considered, 1 left alone as dirty"*) echo "  ok   report counts exactly one dirty skip" ;;
  *) echo "  FAIL report does not count exactly one dirty skip: $out"; fails=$((fails+1)) ;; esac

echo "5. a clean worktree whose HEAD no ref holds is kept"
# A detached worktree can hold the only ref to its commits. Removing it because
# `git status` is clean strands them for gc (DESKT-39: 9 such worktrees were
# live on 2026-09-14). Asserts the COUNT, so the label alone cannot pass.
ORIGIN="$FIX/origin.git"
git init -q --bare "$ORIGIN" 2>/dev/null
git -C "$WTREPO" remote add origin "$ORIGIN" 2>/dev/null
git -C "$WTREPO" push -q origin HEAD:refs/heads/main 2>/dev/null
git -C "$WTREPO" fetch -q origin 2>/dev/null
WTL="$FIX/wt-local-only"
git -C "$WTREPO" worktree add --detach "$WTL" -q 2>/dev/null
git -C "$WTL" -c user.email=t@t -c user.name=t commit -q --allow-empty -m local-only 2>/dev/null
touch -t 202501010000 "$WTL" 2>/dev/null
out=$(AMUX_DEBRIS_ROOTS="$FIX" "$REAPER" --apply --repo "$WTREPO" 2>&1)
check "local-only worktree still present" "yes" "$([ -d "$WTL" ] && echo yes || echo no)"
case "$out" in *", 1 kept because no branch, tag or remote holds HEAD"*) echo "  ok   report counts the local-only skip" ;;
  *) echo "  FAIL report does not count exactly one local-only skip: $out"; fails=$((fails+1)) ;; esac

echo "5b. a clean worktree whose HEAD only a LOCAL branch holds is reclaimed, and the branch survives"
# The branch outlives the worktree, so nothing is stranded. Positive control for
# cell 5: the same shape of commit, now named by a branch, must be taken.
WTB="$FIX/wt-local-branch"
git -C "$WTREPO" worktree add --detach "$WTB" -q 2>/dev/null
git -C "$WTB" -c user.email=t@t -c user.name=t commit -q --allow-empty -m on-a-local-branch 2>/dev/null
tip=$(git -C "$WTB" rev-parse HEAD)
git -C "$WTREPO" branch keep-me "$tip" 2>/dev/null
touch -t 202501010000 "$WTB" 2>/dev/null
AMUX_DEBRIS_ROOTS="$FIX" "$REAPER" --apply --repo "$WTREPO" >/dev/null 2>&1
check "worktree held only by a local branch is removed" "no"  "$([ -d "$WTB" ] && echo yes || echo no)"
check "and its branch still points at the commit"     "$tip" "$(git -C "$WTREPO" rev-parse -q --verify refs/heads/keep-me 2>/dev/null || echo gone)"
check "the unheld worktree from cell 5 is still kept" "yes" "$([ -d "$WTL" ] && echo yes || echo no)"

echo "6. a clean on-origin worktree under \$REPO/scratch is reclaimed"
# Temp roots disabled, so only the scratch-root rule can select it.
mkdir -p "$WTREPO/scratch"
WTS="$WTREPO/scratch/wt-pushed"
git -C "$WTREPO" worktree add --detach "$WTS" origin/main -q 2>/dev/null
touch -t 202501010000 "$WTS" 2>/dev/null
AMUX_DEBRIS_WT_TEMP_ROOTS="" AMUX_DEBRIS_ROOTS="$FIX/none" "$REAPER" --apply --repo "$WTREPO" >/dev/null 2>&1
check "scratch-root worktree removed" "no" "$([ -d "$WTS" ] && echo yes || echo no)"

echo "7. a worktree a live process is standing in is kept"
WTU="$FIX/wt-in-use"
git -C "$WTREPO" worktree add --detach "$WTU" origin/main -q 2>/dev/null
touch -t 202501010000 "$WTU" 2>/dev/null
( cd "$WTU" && exec sleep 60 ) &
HOLDER=$!
sleep 1
out=$(AMUX_DEBRIS_ROOTS="$FIX" "$REAPER" --apply --repo "$WTREPO" 2>&1)
kill "$HOLDER" 2>/dev/null || true; wait "$HOLDER" 2>/dev/null || true
check "in-use worktree still present" "yes" "$([ -d "$WTU" ] && echo yes || echo no)"
case "$out" in *", 1 kept in use"*) echo "  ok   report counts the in-use skip" ;;
  *) echo "  FAIL report does not count exactly one in-use skip: $out"; fails=$((fails+1)) ;; esac
case "$out" in *"cwd probe unavailable"*) echo "  FAIL the cwd probe did not run, so this cell proved nothing: $out"; fails=$((fails+1)) ;;
  *) echo "  ok   cwd probe ran" ;; esac

echo "8. untracked node_modules alone does not pin a worktree; any other untracked file does"
WTN="$FIX/wt-node-modules"; WTX="$FIX/wt-untracked-file"
git -C "$WTREPO" worktree add --detach "$WTN" origin/main -q 2>/dev/null
git -C "$WTREPO" worktree add --detach "$WTX" origin/main -q 2>/dev/null
mkdir -p "$WTN/node_modules/x" "$WTX/node_modules/x"
# A FILE inside, or git lists no untracked entry at all (it ignores empty dirs)
# and neither the allowlist nor the --force path is ever exercised.
echo "module.exports = 1" > "$WTN/node_modules/x/index.js"
echo "module.exports = 1" > "$WTX/node_modules/x/index.js"
# Positive control: the cell is about an UNTRACKED node_modules. If git does not
# report one, nothing below can fail, so fail here instead.
case "$(git -C "$WTN" status --porcelain 2>/dev/null)" in *"?? node_modules"*) echo "  ok   fixture shows untracked node_modules" ;;
  *) echo "  FAIL fixture has no untracked node_modules, so cell 8 would prove nothing"; fails=$((fails+1)) ;; esac
echo keep > "$WTX/notes.local.ts"
touch -t 202501010000 "$WTN" "$WTX" 2>/dev/null
AMUX_DEBRIS_ROOTS="$FIX" "$REAPER" --apply --repo "$WTREPO" >/dev/null 2>&1
check "node_modules-only worktree removed" "no" "$([ -d "$WTN" ] && echo yes || echo no)"
check "worktree with another untracked file kept" "yes" "$([ -d "$WTX" ] && echo yes || echo no)"

echo "9. --force removes a symlinked node_modules as a link and never follows it"
# The real push-check worktrees link node_modules to the main checkout's copy.
# --force must take the link and leave the shared tree whole, or one reap
# empties node_modules for every lane on the machine.
SHARED="$FIX/shared-node-modules"
mkdir -p "$SHARED/pkg"; echo canary > "$SHARED/pkg/canary.js"
WTY="$FIX/wt-symlinked-node-modules"
git -C "$WTREPO" worktree add --detach "$WTY" origin/main -q 2>/dev/null
ln -s "$SHARED" "$WTY/node_modules"
touch -t 202501010000 "$WTY" 2>/dev/null
case "$(git -C "$WTY" status --porcelain 2>/dev/null)" in *"?? node_modules"*) echo "  ok   fixture shows the node_modules symlink as untracked" ;;
  *) echo "  FAIL fixture shows no untracked node_modules link, so cell 9 would prove nothing"; fails=$((fails+1)) ;; esac
AMUX_DEBRIS_ROOTS="$FIX" "$REAPER" --apply --repo "$WTREPO" >/dev/null 2>&1
check "worktree with symlinked node_modules removed" "no" "$([ -d "$WTY" ] && echo yes || echo no)"
check "shared node_modules canary survives" "yes" "$([ -f "$SHARED/pkg/canary.js" ] && echo yes || echo no)"

echo "10. stale side cargo targets go; the shared target and a fresh one stay"
# 76.6 GB sat in four side targets on 2026-09-15, and one came back hours after
# a manual delete. Reaping the SHARED target instead would make every lane in
# the fleet rebuild at once, so that one is never a candidate.
TR="$FIX/amuxhome"
# The shared target is named as a SIBLING here on purpose: the default
# ~/.amux/rust-build-target is already outside the rust-build-target-* glob, so
# a fixture using that name cannot fail when the guard is deleted. The guard is
# what protects a shared target someone CONFIGURED to a matching name.
mkdir -p "$TR/rust-build-target-shared/debug" "$TR/rust-build-target-stale/debug" "$TR/rust-build-target-fresh/debug"
echo x > "$TR/rust-build-target-shared/debug/a"; echo x > "$TR/rust-build-target-stale/debug/a"; echo x > "$TR/rust-build-target-fresh/debug/a"
touch -t 202501010000 "$TR/rust-build-target-shared/debug/a" "$TR/rust-build-target-shared/debug" "$TR/rust-build-target-shared" 2>/dev/null
touch -t 202501010000 "$TR/rust-build-target-stale/debug/a" "$TR/rust-build-target-stale/debug" "$TR/rust-build-target-stale" 2>/dev/null
out=$(AMUX_DEBRIS_TARGET_ROOT="$TR" AMUX_SHARED_TARGET="$TR/rust-build-target-shared" \
      AMUX_DEBRIS_ROOTS="$FIX/none" "$REAPER" --apply --repo /nonexistent 2>&1)
check "stale side target removed"        "no"  "$([ -d "$TR/rust-build-target-stale" ] && echo yes || echo no)"
check "configured shared target untouched" "yes" "$([ -d "$TR/rust-build-target-shared" ] && echo yes || echo no)"
check "freshly written side target kept" "yes" "$([ -d "$TR/rust-build-target-fresh" ] && echo yes || echo no)"
case "$out" in *"side cargo targets 1 ("*) echo "  ok   report counts exactly one reclaimed target" ;;
  *) echo "  FAIL report does not count exactly one reclaimed target: $out"; fails=$((fails+1)) ;; esac

echo "11. a side target a live process is standing in is kept"
TR2="$FIX/amuxhome2"
mkdir -p "$TR2/rust-build-target-busy/debug"
echo x > "$TR2/rust-build-target-busy/debug/a"
touch -t 202501010000 "$TR2/rust-build-target-busy/debug/a" "$TR2/rust-build-target-busy/debug" "$TR2/rust-build-target-busy" 2>/dev/null
( cd "$TR2/rust-build-target-busy" && exec sleep 60 ) &
THOLDER=$!
sleep 1
out=$(AMUX_DEBRIS_TARGET_ROOT="$TR2" AMUX_SHARED_TARGET="$TR2/rust-build-target" \
      AMUX_DEBRIS_ROOTS="$FIX/none" "$REAPER" --apply --repo /nonexistent 2>&1)
kill "$THOLDER" 2>/dev/null || true; wait "$THOLDER" 2>/dev/null || true
check "busy side target still present" "yes" "$([ -d "$TR2/rust-build-target-busy" ] && echo yes || echo no)"
case "$out" in *"side cargo targets 0 ("*) echo "  ok   report counts nothing reclaimed while it is busy" ;;
  *) echo "  FAIL report claims a reclaim while the target was busy: $out"; fails=$((fails+1)) ;; esac

echo "12. a side-target root that does not exist says so instead of reading as clean"
# The first scheduled run printed "0 (0 MB), 0 kept" because $HOME resolved
# elsewhere under the scheduler. A wrong root must not render like a machine
# with nothing to reclaim.
out=$(AMUX_DEBRIS_TARGET_ROOT="$FIX/no-such-root" AMUX_DEBRIS_ROOTS="$FIX/none" \
      "$REAPER" --apply --repo /nonexistent 2>&1)
case "$out" in *"side-target root $FIX/no-such-root MISSING"*) echo "  ok   the missing root is named" ;;
  *) echo "  FAIL a missing side-target root is not reported: $out"; fails=$((fails+1)) ;; esac
out=$(AMUX_DEBRIS_TARGET_ROOT="$TR" AMUX_SHARED_TARGET="$TR/rust-build-target-shared" \
      AMUX_DEBRIS_ROOTS="$FIX/none" "$REAPER" --repo /nonexistent 2>&1)
case "$out" in *"side-target root $TR present"*) echo "  ok   control: a real root reports present" ;;
  *) echo "  FAIL a real root did not report present: $out"; fails=$((fails+1)) ;; esac

echo
if [ "$fails" -eq 0 ]; then echo "PASS: reap-amux-debris — all checks passed"; exit 0; fi
echo "reap-amux-debris: $fails check(s) FAILED"; exit 1
