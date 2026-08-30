#!/usr/bin/env bash
# Which commits has the auto-builder never actually built? (AMUX-3797)
#
# The builder builds `rev-parse HEAD` at each wakeup. On this checkout lanes
# land work a minute apart, so several commits accumulate between wakeups and
# only the LAST is ever compiled. The ones in between are never built as
# committed — they are built only as part of a later tree that already contains
# their successor's fixes.
#
# That is invisible today. `~/.amux/rust-build-provenance.json` holds the LAST
# successful build and nothing else, and the log reads like throughput. So
# "this sha was never compiled" is a fact the box already has and nobody asks.
#
# ONLY RUST-TOUCHING COMMITS COUNT, and getting this wrong inflates the answer
# badly. rust-auto-build.sh:46 triggers on
# `git log -1 --format=%H -- crates/ Cargo.toml Cargo.lock`, so a docs or
# markdown commit is CORRECTLY never a build target and must not be reported as
# a miss. Counting every commit in the range gave 83 of 235 here; over the
# builder's whole life the same mistake reads 706 of 1574 against a true 161 of
# 1028 (amux-frustrations measured both). The range filter below is what keeps
# this honest.
#
# MEASURED 2026-08-27 across the builder's life (7253465c onward): 161 of 1028
# Rust-touching commits on main — 16% — were never a `building` target. Those
# are the set that would go red on whoever's PR happens to include them, and the
# set a bisect breaks on.
#
# A `SKIP` LINE IS NOT A MISS. It is the dedupe declining a second trigger for a
# sha a running build already has: 287 shas were SKIPped at least once and only
# 7 were never subsequently built. Reading SKIP as "never built" counts the
# dedupe as a loss — the mistake this card was first filed on.
#
# NOT A GATE. It reports; it does not refuse. Building every commit was measured
# at ~15-22s against a fleet that lands them far faster, so serialising a build
# per commit would make the builder the bottleneck it currently avoids. The cost
# objection is real; the SILENCE is what was not defensible.
#
#   scripts/unbuilt-commits.sh                 # unpushed range (origin/main..HEAD)
#   scripts/unbuilt-commits.sh <range>         # any git range
#   scripts/unbuilt-commits.sh --build         # ...and compile each one, in order
#
# Exit 0 = every commit in the range was built · 1 = some were not · 2 = broke.
set -uo pipefail
# NO `cd` TO THIS SCRIPT'S OWN REPO. It needs no repo-relative paths (the log
# path is absolute), and cd-ing there would make the report always describe the
# amux checkout no matter which repo you ran it in — including when a test
# drives it against a fixture, which is how that was caught.
LOG="${AMUX_RS_BUILD_LOG:-$HOME/.amux/logs/rust-auto-build.log}"

DO_BUILD=0
RANGE=""
for a in "$@"; do
  case "$a" in
    --build) DO_BUILD=1 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) RANGE="$a" ;;
  esac
done
[ -n "$RANGE" ] || RANGE="origin/main..HEAD"

[ -r "$LOG" ] || {
  # An absent log is NOT an empty answer: it would report every commit as
  # unbuilt, which is the "absence read as emptiness" failure this repo keeps
  # paying for. Refuse instead.
  echo "no builder log at $LOG — cannot tell built from unbuilt. Not reporting a count." >&2
  exit 2
}

# The definitive built set is the `building <sha>` lines. SKIP lines name a sha
# too, and they do NOT mean unbuilt — a skip is a redundant wakeup whose sha a
# concurrent build is already handling. Reading a SKIP as "never built" is the
# mistake that produced this card's first draft: 962c15d7 was logged SKIP four
# seconds AFTER the provenance file recorded it as built.
BUILT=$(mktemp) || exit 2
trap 'rm -f "$BUILT"' EXIT
grep -o "building [0-9a-f]\{40\}" "$LOG" 2>/dev/null | awk '{print $2}' | sort -u > "$BUILT"
[ -s "$BUILT" ] || { echo "builder log has no 'building <sha>' lines — refusing to guess." >&2; exit 2; }

# CAN THE LOG EVEN SEE THIS RANGE? (amux-frustrations, reviewing 12627007.)
# The refusal above covers an ABSENT log. A TRUNCATED or ROTATED one is the same
# defect wearing a different hat: every commit predating the surviving log has no
# `building` line, so it reports NEVER BUILT with no signal, and a measurement
# gap reads as a large regression. That is this tool's own class one layer up —
# it refuses to let absence read as emptiness for the log, then lets it read as
# emptiness for the log's WINDOW.
#
# Today the log opens at `2026-08-09 14:23:16 building 7253465c`, the builder's
# own first commit, and nothing in rust-auto-build.sh rotates it. Which is
# exactly why it would not announce itself the day something does.
LOG_START=$(grep -m1 -o '^== [0-9][0-9-]* [0-9:]*' "$LOG" 2>/dev/null | sed 's/^== //')
OLDEST=$(git log --reverse --format='%cd' --date=format:'%Y-%m-%d %H:%M:%S' \
           "$RANGE" -- crates/ Cargo.toml Cargo.lock 2>/dev/null | head -1)
COVERAGE_WARN=""
if [ -n "$LOG_START" ] && [ -n "$OLDEST" ]; then
  # Lexical compare is correct for zero-padded `YYYY-MM-DD HH:MM:SS`.
  if [ "$LOG_START" \> "$OLDEST" ]; then
    COVERAGE_WARN="the builder log begins $LOG_START but this range begins $OLDEST — commits before the log opens CANNOT be seen and are counted as never built"
  fi
fi

total=0; unbuilt=0
UNBUILT_LIST=$(mktemp) || exit 2
trap 'rm -f "$BUILT" "$UNBUILT_LIST"' EXIT
while read -r sha; do
  [ -n "$sha" ] || continue
  total=$((total+1))
  grep -qxF "$sha" "$BUILT" || { echo "$sha" >> "$UNBUILT_LIST"; unbuilt=$((unbuilt+1)); }
done < <(git rev-list "$RANGE" -- crates/ Cargo.toml Cargo.lock 2>/dev/null)

[ "$total" -gt 0 ] || { echo "no commits in range $RANGE"; exit 0; }

echo "range $RANGE — $total commit(s)"
echo "  built as committed: $((total-unbuilt))"
# ONE DECIMAL, ROUNDED. Integer truncation printed 125/839 as "14%" when it is
# 14.9 — understating, which is the wrong direction for a how-bad-is-this
# number (amux-frustrations).
_tenths=$(( (unbuilt * 1000 + total / 2) / total ))
echo "  NEVER built:        $unbuilt ($(( _tenths / 10 )).$(( _tenths % 10 ))%)"
if [ -n "$COVERAGE_WARN" ]; then
  echo
  echo "  ** COVERAGE GAP: $COVERAGE_WARN"
  echo "  ** The count above is an UPPER BOUND, not a measurement."
fi

if [ "$unbuilt" -eq 0 ]; then
  echo "every commit in this range has been compiled as committed."
  exit 0
fi

echo
echo "never built, oldest first (lane in brackets):"
tac "$UNBUILT_LIST" | while read -r sha; do
  git log -1 --format='  %h [%(trailers:key=Amux-Session,valueonly,separator=)] %s' "$sha" 2>/dev/null
done

if [ "$DO_BUILD" = "1" ]; then
  echo
  echo "building each, oldest first. Ctrl-C is safe — each build is an isolated worktree."
  rc=0
  tac "$UNBUILT_LIST" | while read -r sha; do
    sw=$(mktemp -d "${TMPDIR:-/tmp}/amux-unbuilt-XXXXXX") || exit 2
    rm -rf "$sw"
    if git worktree add --detach -q "$sw" "$sha" >/dev/null 2>&1; then
      if ( cd "$sw" && CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.amux/rust-build-target}" \
             cargo check --workspace >/dev/null 2>&1 ); then
        echo "  OK   $(git log -1 --format='%h %s' "$sha" | cut -c1-70)"
      else
        echo "  FAIL $(git log -1 --format='%h %s' "$sha" | cut -c1-70)"
        rc=1
      fi
      git worktree remove --force "$sw" >/dev/null 2>&1
    else
      echo "  ??   $sha — could not materialise a worktree"
      rc=1
    fi
    rm -rf "$sw"
  done
  git worktree prune >/dev/null 2>&1
  exit "$rc"
fi

echo
echo "to compile them: scripts/unbuilt-commits.sh $RANGE --build"
exit 1
