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
case "$out" in *"left alone as dirty"*) echo "  ok   report names the dirty skip" ;;
  *) echo "  FAIL report never mentions the dirty skip: $out"; fails=$((fails+1)) ;; esac

echo
if [ "$fails" -eq 0 ]; then echo "PASS: reap-amux-debris — all checks passed"; exit 0; fi
echo "reap-amux-debris: $fails check(s) FAILED"; exit 1
