#!/usr/bin/env bash
# Cells for the observed-edits walk root (AMUX-3920, handed over from MHC-527).
#
# THE DEFECT. The harness reports the SESSION cwd and the hook walked exactly
# that. When cwd is a SUBDIRECTORY of a shared checkout, every edit above it
# produced no record at all. MHC-527's control, one Bash call writing two files
# in one repo: `homepage/scripts/.probe` recorded, `scripts/.probe` not, hook
# logged n=1. In that session 13 of 16 committed paths were above cwd — 81%
# structurally unobservable — and the staged guard, correctly reading "NO
# session has an edit record for this", asked for VERIFIED_SOLO on three commits
# and ALLOW_FOREIGN on two. Overriding a guard that is right most of the time is
# how a fleet learns to wave it through.
#
# THE CONSTRAINT THAT SHAPES THE FIX. amux is 2,222 files and walks in 0.03s;
# ~/Dev/mixpeek is ~640,000 and sits right on the 1.5s budget — 2.5-2.9s COLD,
# ~1.0s once the filesystem cache is warm. (My first figure was 2.54s and I
# reported it without noting it was a cold walk; alternating the arms three times
# is what separated the two.) On a box that compiles continuously the cold case
# is not rare, and mixpeek-homepage-claude confirms every recent run on their
# lane carries TRUNCATED=budget.
#
# So a walk that starts at the repo root can exhaust the budget before ever
# reaching the session's own directory — trading a known blind spot for an
# unpredictable one.
#
# Hence: cwd first, then the rest of the repo with the remaining budget, and a
# distinct marker when the budget or cap cuts. Never less coverage than before,
# more when it fits, and the shortfall is named rather than silent.
set -uo pipefail
cd "$(dirname "$0")/.."
HOOK="${OBSERVED_EDITS_HOOK:-$(pwd)/scripts/claude-hooks/observed-edits-post.py}"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
no(){ FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

# Run the hook against a scratch repo whose session cwd is a SUBDIRECTORY.
# Echoes the last log line. $1 = python file to run (allows a patched copy).
run_probe() {
  local hookfile="$1" tmp
  tmp="$(mktemp -d)"
  local R="$tmp/repo"
  mkdir -p "$R/sub/deep" "$R/top"
  git init -q "$R"
  export AMUX_HOME="$tmp/home" AMUX_SESSION="probe-3920"
  mkdir -p "$AMUX_HOME/hooks/state"
  # The PRE half writes this marker; its mtime is t0 and must precede the writes.
  touch "$AMUX_HOME/hooks/state/observed-$AMUX_SESSION.t0"
  sleep 1
  echo a > "$R/top/above.txt"
  echo b > "$R/sub/deep/below.txt"
  # A second in-scope file so the cap has something to cut. With one file under
  # cwd there is nothing to truncate and the cell could not fail.
  echo c > "$R/sub/deep/below2.txt"
  # `cp` rather than `echo`: a pure-read command claims nothing (AF-124) and the
  # hook correctly returns before walking.
  printf '{"cwd":"%s","tool_input":{"command":"cp src dst"}}' "$R/sub" \
    | AMUX_URL="http://127.0.0.1:9" python3 "$hookfile" >/dev/null 2>&1
  tail -1 "$AMUX_HOME/hooks/state/observed-edits.log" 2>/dev/null || echo ""
  rm -rf "$tmp"
}

echo "observed-edits walk cells (AMUX-3920)"

line="$(run_probe "$HOOK")"
# WITHDRAWN AND INVERTED (AMUX-3933). This cell used to assert that a file ABOVE
# cwd is recorded. Widening the root to get that is unsound: mtime says a file
# was written in this window, never BY WHOM, and cwd was the only thing bounding
# the smear. Live specimen: byo-ray recorded another lane's uncommitted file,
# written 22s earlier and never touched by byo-ray, and the staged guard reads
# exactly these records to decide ownership.
#
# So the assertion is now that a file above cwd is NOT claimed. That is a real
# blind spot and AMUX-3933 owns it; claiming a peer's work is worse, which is the
# module's own doctrine (cross-linking is strictly worse than staying blind).
case "$line" in
  *top/above.txt*) no "a file above cwd must NOT be claimed — that is a peer's work" "$line" ;;
  *) ok "a file above the session cwd is not claimed (bounded smear, AMUX-3933)" ;;
esac
# CONTROL: widening must not lose what already worked.
case "$line" in
  *deep/below.txt*) ok "the file below cwd is still recorded" ;;
  *) no "coverage that worked before must not regress" "$line" ;;
esac
# The paths sent to the server are absolute; the LOG is repo-relative. Once the
# walk root moved above cwd this line rendered every hit as ../../../.., which
# for an observability hook is most of the defect.
case "$line" in
  *../../*) no "log paths must be repo-relative, not ../../.." "$line" ;;
  *) ok "log paths are repo-relative and readable" ;;
esac
# n= must not double-count. The second root CONTAINS the first, and on macOS
# /var vs /private/var makes the same file two strings — found by an n=3 in a
# two-file probe.
case "$line" in
  *"n=2 "*) ok "n= counts the in-scope files, not the one above cwd" ;;
  *) no "n= should be 2: both files under cwd, neither above it" "$line" ;;
esac

# TRUNCATION IS NAMED, and cwd coverage survives it. On the monorepo above this
# is the EXPECTED path, so "found 3" and "found 3 so far" must not read alike.
CAP="$(mktemp -d)/cap.py"
sed 's/^MAX_PATHS = 80/MAX_PATHS = 1/' "$HOOK" > "$CAP"
line="$(run_probe "$CAP")"
case "$line" in
  *TRUNCATED=cap*) ok "a capped walk says so" ;;
  *) no "a truncated walk must be distinguishable from a clean one" "$line" ;;
esac
# WITHDRAWN WITH THE WIDENING. This asserted the ordering guarantee that made a
# two-root walk safe; with one root there is no ordering to guarantee. What
# replaces it is the two-lane cell below, which pins the property the ordering
# was only approximating: a session records its own work and not a peer's.
case "$line" in
  *deep/*) ok "under truncation the surviving path is still under cwd" ;;
  *) no "a truncated walk must still record something in scope" "$line" ;;
esac

# A CACHE WRITE IS NOT A LANE'S EDIT (mixpeek-cicd's specimen: n=3 in which two
# of the three recorded paths were .pytest_cache and .ruff_cache entries). Those
# become edit records, and an edit record is what the staged guard reads to
# decide who touched a file — so a test run minted attribution for files no
# guard should care about. `.cache`-prefixed names were already excluded;
# `.pytest_cache` is not `.cache`-prefixed, which is how it slipped through.
cache_probe() {
  local tmp; tmp="$(mktemp -d)"
  local R="$tmp/repo"
  mkdir -p "$R/sub" "$R/.pytest_cache/v" "$R/.ruff_cache/x"
  git init -q "$R"
  export AMUX_HOME="$tmp/home" AMUX_SESSION="probe-cache"
  mkdir -p "$AMUX_HOME/hooks/state"
  touch "$AMUX_HOME/hooks/state/observed-$AMUX_SESSION.t0"
  sleep 1
  echo real > "$R/sub/real.txt"
  echo junk > "$R/.pytest_cache/v/cache.json"
  echo junk > "$R/.ruff_cache/x/entry"
  printf '{"cwd":"%s","tool_input":{"command":"cp src dst"}}' "$R/sub" \
    | AMUX_URL="http://127.0.0.1:9" python3 "$HOOK" >/dev/null 2>&1
  tail -1 "$AMUX_HOME/hooks/state/observed-edits.log" 2>/dev/null || echo ""
  rm -rf "$tmp"
}
line="$(cache_probe)"
case "$line" in
  *pytest_cache*|*ruff_cache*) no "a tool cache write must not become an edit record" "$line" ;;
  *) ok "tool-cache writes are pruned, not attributed" ;;
esac
# CONTROL: pruning caches must not lose the real file beside them.
case "$line" in
  *real.txt*) ok "the real edit beside the caches is still recorded" ;;
  *) no "pruning must not drop genuine edits" "$line" ;;
esac

# THE CELL THAT WOULD HAVE CAUGHT THE REGRESSION, and did not exist because
# every probe here used ONE lane. Two lanes writing in the same window: the
# session must claim its own file and NOT its neighbour's. mtime cannot tell them
# apart, so the only bound is scope — which is why the root must stay at cwd
# until something bounds it by IDENTITY instead.
two_lane_probe() {
  local tmp; tmp="$(mktemp -d)"
  local R="$tmp/repo"
  mkdir -p "$R/laneA" "$R/laneB"
  git init -q "$R"
  export AMUX_HOME="$tmp/home" AMUX_SESSION="laneA"
  mkdir -p "$AMUX_HOME/hooks/state"
  touch "$AMUX_HOME/hooks/state/observed-$AMUX_SESSION.t0"
  sleep 1
  echo mine  > "$R/laneA/mine.txt"
  echo peers > "$R/laneB/peers.txt"
  printf '{"cwd":"%s","tool_input":{"command":"cp src dst"}}' "$R/laneA" \
    | AMUX_URL="http://127.0.0.1:9" python3 "$HOOK" >/dev/null 2>&1
  tail -1 "$AMUX_HOME/hooks/state/observed-edits.log" 2>/dev/null || echo ""
  rm -rf "$tmp"
}
line="$(two_lane_probe)"
case "$line" in
  *peers.txt*) no "a session must NEVER record a peer's file — the staged guard reads this" "$line" ;;
  *) ok "a peer's file written in the same window is not claimed" ;;
esac
case "$line" in
  *mine.txt*) ok "and the session's own file still is (bound, not blindness)" ;;
  *) no "bounding must not stop the hook recording the session's own work" "$line" ;;
esac

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
