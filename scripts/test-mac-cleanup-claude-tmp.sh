#!/bin/bash
# The Claude Code session temp-dir arm in scripts/mac-cleanup-tick.sh (DESKT-77).
#
# On 2026-10-02 /private/tmp/claude-501 held 214G in 258 session dirs with the disk
# at 35G free. This arm removes a session dir only when the session is dead by
# every measure, and holds live sessions to a quota by removing their idle
# scratchpad entries. Each cell fails if its guard is removed (ethos rule 7).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
export AMUX_CLEANUP_FSEVENTSD_CMD=true   # the live fseventsd must not decide a test (DESKT-81)
export AMUX_CLEANUP_SQLITE_PS_CMD=true   # the live process table must not decide a test (DESKT-94)
export AMUX_CLEANUP_HEALTH_PROBES=off    # the health probes must not stop or read live processes in a test
trap 'rm -rf -- "$FIX"' EXIT
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { [ -e "$1" ] && echo present || echo gone; }
AMUX_CLEANUP_LIB_ONLY=1 AMUX_CLEANUP_SCOPE_FILE=/dev/null . "$TICK"

OLD=202601010000                      # far outside every idle window
age() { find "$@" -exec touch -h -t "$OLD" {} + ; }
mb() { mkdir -p "$(dirname "$1")"; dd if=/dev/zero of="$1" bs=1024 count=$(( $2 * 1024 )) 2>/dev/null; }

build() {
  rm -rf -- "${FIX:?}/root" "${FIX:?}/projects" "${FIX:?}/state" "${FIX:?}/repo"
  R="$FIX/root"; P="$FIX/projects"
  STATE_DIR="$FIX/state"; CLAUDE_PROJECTS="$P"
  mkdir -p "$P/proj"
  # dead: old transcript, old files
  mb "$R/proj/dead/scratchpad/big.tar" 3; touch "$P/proj/dead.jsonl"
  # live by transcript: files old, transcript written now
  mb "$R/proj/livet/scratchpad/x" 1; touch "$P/proj/livet.jsonl"
  # live by files: no transcript at all, a file written now
  mb "$R/proj/livef/scratchpad/x" 1
  # open: dead by age, but lsof shows a handle inside
  mb "$R/proj/open/scratchpad/x" 1; touch "$P/proj/open.jsonl"
  # dirty: dead by age, holds a git worktree with an untracked file
  git init -q "$FIX/repo"
  git -C "$FIX/repo" -c user.email=t@t -c user.name=t commit -q --allow-empty -m i
  mkdir -p "$R/proj/dirty/scratchpad"
  git -C "$FIX/repo" worktree add -q --detach "$R/proj/dirty/scratchpad/wt" 2>/dev/null
  echo draft > "$R/proj/dirty/scratchpad/wt/only-copy.txt"
  touch "$P/proj/dirty.jsonl"
  age "$R/proj/dead" "$R/proj/livet" "$R/proj/open" "$R/proj/dirty" "$P/proj/dead.jsonl" "$P/proj/open.jsonl" "$P/proj/dirty.jsonl"
  touch "$P/proj/livet.jsonl"
  touch "$R/proj/livef/scratchpad/x"
  printf 'cat 1 ethan cwd DIR %s\n' "$R/proj/open/scratchpad/x" > "$FIX/lsof"
  LSOF_CMD="cat $FIX/lsof"
  CLAUDE_TMP_QUOTA_KB=0; CLAUDE_TMP_ENTRY_MIN_MB=0   # cells 4-6 use megabyte fixtures; cell 7 tests the floor
}

echo "1. a dead session dir is removed and ledgered; every live signal keeps one"
build
out=$(reap_claude_tmp "$R" 48 0)
check "dead session removed"                         gone    "$(has "$R/proj/dead")"
check "recent transcript keeps the session"          present "$(has "$R/proj/livet")"
check "a recent file keeps a session with no transcript" present "$(has "$R/proj/livef")"
check "an open handle keeps the session"             present "$(has "$R/proj/open")"
check "a dirty worktree keeps the session"           present "$(has "$R/proj/dirty/scratchpad/wt/only-copy.txt")"
check "the removal is in the ledger"                 yes     "$(grep -q "session idle >= 48h.*$R/proj/dead" "$STATE_DIR/claude-tmp-reaps.log" && echo yes || echo no)"
check "the summary counts removed and kept"          yes     "$(printf '%s' "$out" | grep -q 'removed 1 .*kept: live 2, open 1, dirty worktree 1' && echo yes || echo no)"

echo "2. no lsof snapshot means nothing is removed"
build; LSOF_CMD="true"
out=$(reap_claude_tmp "$R" 48 0)
check "dead session kept without lsof"               present "$(has "$R/proj/dead")"
check "the line says UNMEASURED"                     yes     "$(printf '%s' "$out" | grep -q 'open handles UNMEASURED' && echo yes || echo no)"

echo "3. dry run and the off switch remove nothing"
build
reap_claude_tmp "$R" 48 1 >/dev/null
check "dry run keeps the dead session"               present "$(has "$R/proj/dead")"
reap_claude_tmp "$R" 0 0 >/dev/null
check "idle 0 turns session removal off"             present "$(has "$R/proj/dead")"

echo "4. a live session over quota loses its biggest idle entries, nothing else"
build
S="$R/proj/livet/scratchpad"
mb "$S/old-big/a.tar" 4; mb "$S/old-small/a.tar" 1; mb "$S/fresh-big/a.tar" 4
# a CLEAN worktree holding a committed 2M file, so only the worktree guard can keep it
mb "$FIX/repo/blob" 2; git -C "$FIX/repo" add blob; git -C "$FIX/repo" -c user.email=t@t -c user.name=t commit -q -m blob
git -C "$FIX/repo" worktree add -q --detach "$S/old-wt" 2>/dev/null
age "$S/old-big" "$S/old-small" "$S/old-wt"
touch "$P/proj/livet.jsonl"
CLAUDE_TMP_QUOTA_KB=$(( 8 * 1024 )); CLAUDE_TMP_ENTRY_IDLE_H=6
out=$(reap_claude_tmp "$R" 48 0)
check "biggest idle entry removed"                   gone    "$(has "$S/old-big")"
check "removal stops once under quota"               present "$(has "$S/old-small")"
check "a recent entry is never removed"              present "$(has "$S/fresh-big")"
check "a worktree entry is never removed"            present "$(has "$S/old-wt")"
check "the quota removal is ledgered"                yes     "$(grep -q "quota, entry idle >= 6h.*old-big" "$STATE_DIR/claude-tmp-reaps.log" && echo yes || echo no)"
check "the live session itself stays"                present "$(has "$R/proj/livet/scratchpad")"

echo "5. over quota with nothing removable says so"
build
S="$R/proj/livet/scratchpad"; mb "$S/fresh-big/a.tar" 6
CLAUDE_TMP_QUOTA_KB=$(( 2 * 1024 ))
out=$(reap_claude_tmp "$R" 48 0)
check "fresh entry kept"                             present "$(has "$S/fresh-big")"
check "the session is named OVER QUOTA"              yes     "$(printf '%s' "$out" | grep -q "OVER QUOTA .*$R/proj/livet" && echo yes || echo no)"

echo "6. a session too big to sum in time is measured by its entries, not skipped"
build
S="$R/proj/livet/scratchpad"; mb "$S/old-big/a.tar" 4; mb "$S/fresh/a.tar" 1; age "$S/old-big"; touch "$P/proj/livet.jsonl"
CLAUDE_TMP_QUOTA_KB=$(( 2 * 1024 ))
eval "real_$(declare -f du_kb)"
du_kb() { case "$1" in */scratchpad/*) real_du_kb "$@" ;; *) return 0 ;; esac; }   # the whole-session walk "times out"
out=$(reap_claude_tmp "$R" 48 0)
eval "$(declare -f real_du_kb | sed 's/^real_du_kb/du_kb/')"
check "the idle entry of an unsummable session is removed" gone "$(has "$S/old-big")"
check "its recent entry stays"                       present "$(has "$S/fresh")"

echo "7. the quota rule never removes a small entry, however far over quota"
build
S="$R/proj/livet/scratchpad"; mb "$S/fresh-big/a.tar" 6; mb "$S/old-note.py" 1; mb "$S/old-big/a.tar" 3
age "$S/old-note.py" "$S/old-big"; touch "$P/proj/livet.jsonl"
CLAUDE_TMP_QUOTA_KB=$(( 1 * 1024 )); CLAUDE_TMP_ENTRY_MIN_MB=2
out=$(reap_claude_tmp "$R" 48 0)
check "an idle entry over the size floor is removed" gone    "$(has "$S/old-big")"
check "an idle entry under the size floor stays"     present "$(has "$S/old-note.py")"
check "the session is still named OVER QUOTA"        yes     "$(printf '%s' "$out" | grep -q "OVER QUOTA .*$R/proj/livet" && echo yes || echo no)"

echo "8. a container folder over quota is opened: its idle children are candidates, a clone with local work is kept"
build
S="$R/proj/livet/scratchpad/work"
mb "$S/old-copy/a.tar" 4; mb "$S/fresh-copy/a.tar" 1
# a full clone (.git DIR) holding a commit no remote has: somebody's only copy
git clone -q "$FIX/repo" "$S/ahead-clone" 2>/dev/null; mb "$S/ahead-clone/big" 3
git -C "$S/ahead-clone" add big; git -C "$S/ahead-clone" -c user.email=t@t -c user.name=t commit -q -m local
# a full clone with nothing local: safe to drop when idle
git clone -q "$FIX/repo" "$S/clean-clone" 2>/dev/null; mb "$S/clean-clone/scratch.out" 3; printf 'scratch.out\n' >> "$S/clean-clone/.git/info/exclude"
age "$S/old-copy" "$S/ahead-clone" "$S/clean-clone"; touch "$P/proj/livet.jsonl"
CLAUDE_TMP_QUOTA_KB=$(( 2 * 1024 )); CLAUDE_TMP_ENTRY_IDLE_H=6
out=$(reap_claude_tmp "$R" 48 0)
check "an idle child of the container is removed"   gone    "$(has "$S/old-copy")"
check "a recent child stays"                         present "$(has "$S/fresh-copy")"
check "a clone with an unpushed commit stays"        present "$(has "$S/ahead-clone/big")"
check "an idle clone with nothing local is removed"  gone    "$(has "$S/clean-clone")"
check "the container folder itself stays"            present "$(has "$S")"

echo "9. a child too big to size in time is reported as uncounted, never claimed recent"
build
S="$R/proj/livet/scratchpad/work"; mb "$S/huge/a.tar" 3; age "$S/huge"; touch "$P/proj/livet.jsonl"
CLAUDE_TMP_QUOTA_KB=$(( 1 * 1024 ))
eval "real_$(declare -f du_kb)"
du_kb() { case "$1" in */huge|*/work|*/livet) return 0 ;; *) real_du_kb "$@" ;; esac; }
out=$(reap_claude_tmp "$R" 48 0)
eval "$(declare -f real_du_kb | sed 's/^real_du_kb/du_kb/')"
check "an unsized child is not removed"              present "$(has "$S/huge")"
check "the line says it was not counted"            yes     "$(printf '%s' "$out" | grep -q "NOT counted and NOT considered" && echo yes || echo no)"

echo "10. the quota pass keeps its own budget after the session pass spends its share"
build
rm -rf -- "${R:?}/proj/dead" "${R:?}/proj/dirty" "${R:?}/proj/livef" "${R:?}/proj/open"
S="$R/proj/livet/scratchpad"; mb "$S/old-big/a.tar" 4; age "$S/old-big"; touch "$P/proj/livet.jsonl"
CLAUDE_TMP_QUOTA_KB=$(( 2 * 1024 )); CLAUDE_TMP_ENTRY_IDLE_H=6
saved_b=$CLAUDE_TMP_BUDGET_S saved_q=$CLAUDE_TMP_QUOTA_BUDGET_S
# Budgets are compared with whole-second `date +%s`, so a 1s budget expires at the
# next second boundary, after anywhere from 0 to 1s (flaky in CI, 2 of 5 local runs).
# 3s still flaked on a loaded CI runner (077b3ef1f, 2026-10-16: the quota pass's
# walk outlasted the 2-3 real seconds it got). Now the session share is 5s and the
# stub below sleeps 6s, so it is exhausted; the quota pass gets its own 6s (5+ real
# seconds for a sub-second walk). Were the quota pass on the session's clock it
# would have no time left and miss old-big, which is what this cell must catch.
CLAUDE_TMP_BUDGET_S=11; CLAUDE_TMP_QUOTA_BUDGET_S=6
eval "real_$(declare -f any_recent)"
any_recent() { if [ "$1" = 48 ]; then sleep 6; fi; real_any_recent "$@"; }   # the session pass outlasts its share and the quota budget
out=$(reap_claude_tmp "$R" 48 0)
eval "$(declare -f real_any_recent | sed 's/^real_any_recent/any_recent/')"
CLAUDE_TMP_BUDGET_S=$saved_b; CLAUDE_TMP_QUOTA_BUDGET_S=$saved_q
check "an idle entry is still removed by the quota pass" gone "$(has "$S/old-big")"

echo "11. a session pass that runs out of time resumes there next tick, so every dir is reached"
build
rm -rf -- "${R:?}/proj"
for n in a b c d e f; do mb "$R/rot/$n/scratchpad/x" 1; touch "$P/rot.$n.jsonl"; done
mkdir -p "$P/rot"; for n in a b c d e f; do mv "$P/rot.$n.jsonl" "$P/rot/$n.jsonl"; done
age "$R/rot" "$P/rot"
touch "$P/rot/a.jsonl" "$P/rot/b.jsonl" "$P/rot/c.jsonl" "$P/rot/d.jsonl"   # a-d are live and always kept: they are the time sink at the front
CLAUDE_TMP_QUOTA_KB=0
saved_b=$CLAUDE_TMP_BUDGET_S saved_q=$CLAUDE_TMP_QUOTA_BUDGET_S
CLAUDE_TMP_BUDGET_S=3; CLAUDE_TMP_QUOTA_BUDGET_S=0
eval "real_$(declare -f any_recent)"
any_recent() { if [ "$1" = 48 ]; then sleep 1; fi; real_any_recent "$@"; }   # each judged session costs a second
reached=""
for tick in 1 2 3 4 5 6; do
  reap_claude_tmp "$R" 48 0 > "$FIX/rot.out"
  reached="$reached$(grep -o 'removed [^ ]* [^ ]*/rot/[a-f] ' "$FIX/rot.out" | sed 's#.*/rot/##' | tr -d ' ' || true)"
  [ "$(ls "$R/rot" 2>/dev/null | tr -d '\n')" = abcd ] && break
done
eval "$(declare -f real_any_recent | sed 's/^real_any_recent/any_recent/')"
CLAUDE_TMP_BUDGET_S=$saved_b; CLAUDE_TMP_QUOTA_BUDGET_S=$saved_q
check "both idle dirs behind the live ones are removed within six short ticks" abcd "$(ls "$R/rot" 2>/dev/null | tr -d '\n')"
check "the last dir in order is reached, not starved" yes "$(printf '%s' "$reached" | grep -q f && echo yes || echo no)"
check "the live dirs are kept" abcd "$(ls "$R/rot" 2>/dev/null | tr -d '\n')"
check "a pass that stops early says where the next one starts" yes "$(grep -q 'the next tick starts at' "$FIX/rot.out" && echo yes || echo no)"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-claude-tmp — all checks passed"; exit 0; fi
echo "mac-cleanup-claude-tmp: $fails check(s) FAILED"; exit 1
