#!/usr/bin/env bash
# test-land-merge-queue.sh [amux]: `amux land` with amux.landMergeQueue (MC-92,
# Mixpeek's research/cicd/MERGE-QUEUE.md). A stand-in gh judges a candidate by
# its tree: a file `bad` makes its run red, a file `lost` makes the first run of
# a candidate red on a lost runner only. Each case uses a fresh bare origin.
#   off       lands directly and never calls gh
#   enforce   green: main is fast-forwarded to the dispatched candidate, the
#             mq ref is gone, and the base reaches the workflow input
#   enforce   red alone: rc 6, main unchanged
#   enforce   red batch: bisected, the culprit waiter ejected, the rest land
#   enforce   red on the holder's own commit in a batch: rc 6, waiters keep
#             their tickets
#   enforce   lost runner: dispatched once more, lands
#   enforce   dispatch fails: rc 6, nothing pushed to main
#   enforce   only no-code paths: lands with no candidate; an excluded path or code
#             beside it gets one; its check can still refuse
#   enforce   main moves under a green candidate: carried when nothing overlaps or
#             the new commit is no-code, re-composed when a file is shared
#   shadow    candidate dispatched, main pushed directly even when it is red
#   misconfigured (no workflow): lands directly with a WARN
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }

setup() { # -> H, lane checkout as cwd
  H="$(mktemp -d "${TMPDIR:-/tmp}/lmq.XXXXXX")"; mkdir -p "$H/.amux/logs/land" "$H/bin"
  cd "$H" || exit 1
  git init -q --bare --initial-branch=main origin.git
  git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
  git config user.email t@t; git config user.name t
  echo a > f; git add f; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
  cat > "$H/bin/gh" <<'GH'
#!/usr/bin/env bash
H="$FAKE_GH_HOME"; O="$H/origin.git"
echo "$*" >> "$H/gh.log"
if [ "$1 $2" = "workflow run" ]; then
  [ -f "$H/dispatch-fails" ] && { echo "HTTP 403: Resource not accessible by integration" >&2; exit 1; }
  ref=""; prev=""; for a in "$@"; do [ "$prev" = --ref ] && ref="$a"; prev="$a"; done
  sha="$(git --git-dir="$O" rev-parse -q --verify "refs/heads/$ref")" || { echo "no ref $ref" >&2; exit 1; }
  echo "$ref $sha" >> "$H/dispatched"
  # Main moves while this candidate runs, once.
  if [ -f "$H/move-main" ]; then
    f="$(cat "$H/move-main")"; rm -f "$H/move-main"
    W="$(mktemp -d)"; git clone -q "$O" "$W" 2>/dev/null
    ( cd "$W" && git config user.email m@m && git config user.name m && mkdir -p "$(dirname "$f")" && echo moved > "$f" && git add -A && git commit -qm "moved $f" && git push -q origin HEAD:main ) 2>/dev/null
    rm -rf "$W"
  fi
  exit 0
fi
[ "$1" = api ] || exit 2
case "$2" in
  */runs\?branch=*)
    b="${2#*branch=}"; b="${b%%&*}"
    sha="$(awk -v b="$b" '$1==b {print $2}' "$H/dispatched" 2>/dev/null | tail -1)"
    [ -n "$sha" ] || { echo '{"workflow_runs":[]}'; exit 0; }
    id="$(printf '%s' "$b" | cksum | cut -d' ' -f1)"
    n="$(cat "$H/polls-$id" 2>/dev/null || echo 0)"; n=$((n + 1)); echo "$n" > "$H/polls-$id"
    st=completed; co='"success"'; kind=green
    if git --git-dir="$O" cat-file -e "$sha:bad" 2>/dev/null; then co='"failure"'; kind=red
    elif git --git-dir="$O" cat-file -e "$sha:lost" 2>/dev/null && [ "${b%-r}" = "$b" ]; then co='"failure"'; kind=lost; fi
    [ "$n" -ge 2 ] || { st=in_progress; co=null; }
    echo "$kind" > "$H/run-$id"
    printf '{"workflow_runs":[{"id":%s,"head_sha":"%s","status":"%s","conclusion":%s,"html_url":"https://ci/runs/%s"}]}\n' "$id" "$sha" "$st" "$co" "$id" ;;
  */jobs*)
    id="${2#*/runs/}"; id="${id%%/*}"
    if [ "$(cat "$H/run-$id" 2>/dev/null)" = lost ]; then
      echo '{"jobs":[{"name":"Unit shard 2 of 6 (retry after a lost runner)","conclusion":"failure"},{"name":"Lint","conclusion":"success"}]}'
    else
      echo '{"jobs":[{"name":"Unit shard 2 of 6","conclusion":"failure"}]}'
    fi ;;
  *) exit 2 ;;
esac
GH
  chmod +x "$H/bin/gh"
}
queue_on() { # [mode]
  git config amux.landMergeQueue "${1:-enforce}"
  git config amux.landMergeQueueWorkflow fast.yml
  git config amux.landMergeQueueRepo o/r
  git config amux.landMergeQueueBaseInput base_sha
  git config amux.landMergeQueueLostJob 'retry after a lost runner'
}
land() { # extra args...
  HOME="$H" FAKE_GH_HOME="$H" AMUX_LAND_MQ_GH="$H/bin/gh" AMUX_LAND_MQ_POLL_S=0 AMUX_LAND_NET_BACKOFF_S=0 \
    AMUX_LAND_NOTIFY=0 AMUX_WORKER=lmq bash "$AM" land --tries 6 "$@" > "$H/out" 2>&1
}
commit_file() { mkdir -p "$(dirname "$1")"; echo "$2" > "$1"; git add "$1"; git commit -qm "add $1"; }
main_has() { git --git-dir="$H/origin.git" cat-file -e "main:$1" 2>/dev/null; }
mq_refs() { git --git-dir="$H/origin.git" for-each-ref --format='%(refname)' refs/heads/mq/ | grep -c . ; }
dispatches() { cat "$H/dispatched" 2>/dev/null | grep -c . ; }
done_case() { cd / && rm -rf -- "${H:?}"; }

# --- off: unchanged behaviour
setup; commit_file g1 x
land; rc=$?
{ [ "$rc" = 0 ] && main_has g1 && [ ! -f "$H/gh.log" ]; } && ok "off: lands directly and never calls gh" || bad "off rc=$rc gh=$(cat "$H/gh.log" 2>/dev/null)"
done_case

# --- enforce, green
setup; queue_on; base="$(git rev-parse origin/main)"; commit_file g1 x
land; rc=$?
cand="$(awk '{print $2}' "$H/dispatched" 2>/dev/null | tail -1)"
{ [ "$rc" = 0 ] && [ -n "$cand" ] && [ "$(git --git-dir="$H/origin.git" rev-parse main)" = "$cand" ]; } \
  && ok "enforce green: main is fast-forwarded to exactly the candidate the workflow ran on" || bad "enforce green rc=$rc cand=$cand main=$(git --git-dir="$H/origin.git" rev-parse main); $(tail -5 "$H/out")"
grep -q "workflow run fast.yml --repo o/r --ref mq/[0-9]*-[0-9]*-a1 -f base_sha=$base" "$H/gh.log" && ok "enforce green: dispatched on mq/<batch> with the base sha as the input" || bad "dispatch args: $(grep 'workflow run' "$H/gh.log")"
[ "$(mq_refs)" = 0 ] && ok "enforce green: the candidate ref is deleted" || bad "mq refs left: $(mq_refs)"
grep -q '"phase": "landed"' "$H/.amux/logs/land-mq.jsonl" && ok "enforce green: the ledger records the landing" || bad "no landed ledger line"
done_case

# --- enforce, red alone
setup; queue_on; base="$(git rev-parse origin/main)"; commit_file bad x
land; rc=$?
{ [ "$rc" = 6 ] && [ "$(git --git-dir="$H/origin.git" rev-parse main)" = "$base" ]; } && ok "enforce red alone: rc 6 and main unchanged" || bad "enforce red rc=$rc"
grep -q "candidate red (https://ci/runs/[0-9]*); this lane's change is refused" "$H/.amux/logs/land.log" && ok "enforce red alone: land.log names the run" || bad "no refusal line"
done_case

# --- enforce, red batch: holder h, waiters w1, w2 (bad), w3
plant() { # <name> <file> [content] -> a ticket behind the holder for a commit on origin/main
  git checkout -q -b "$1" origin/main; commit_file "$2" "${3:-x}"
  local sha pid common; sha="$(git rev-parse HEAD)"; git checkout -q main
  sleep 300 & pid=$!; PIDS="${PIDS:-} $pid"
  common="$(cd "$(git rev-parse --git-common-dir)" && pwd -P)"
  printf '%s\n%s\n%s\n1\nx\n\n' "$1" "$sha" "$common" > "$q/$(printf '%012d' $(( $(date +%s) + 100 + ${#PIDS} )))-$pid"
}
setup; queue_on; commit_file h x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
plant w1 w1; plant w2 bad; plant w3 w3
land; rc=$?
{ [ "$rc" = 0 ] && main_has h && main_has w1 && main_has w3 && ! main_has bad; } \
  && ok "enforce red batch: the culprit is bisected out and the rest lands" || bad "red batch rc=$rc; $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -8)"
[ "$(dispatches)" = 4 ] && ok "enforce red batch: 4 runs (full, 2 bisect prefixes, the re-composed batch)" || bad "dispatches=$(dispatches)"
grep -q "refused the batch on your commit" "$H/.amux/locks/land-$key.r/"* 2>/dev/null && ok "enforce red batch: the culprit's waiter is told it was refused" || bad "no refusal written for w2"
grep -q "bisected to w2's commit" "$H/.amux/logs/land.log" && ok "enforce red batch: land.log names the culprit lane" || bad "culprit not logged"
[ "$(mq_refs)" = 0 ] && ok "enforce red batch: no candidate ref left" || bad "mq refs left: $(mq_refs)"
kill $PIDS 2>/dev/null; wait 2>/dev/null
done_case

# --- enforce, the holder's own commit is the culprit of a batch
setup; queue_on; base="$(git rev-parse origin/main)"; commit_file bad x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
plant w1 w1
land; rc=$?
{ [ "$rc" = 6 ] && [ "$(git --git-dir="$H/origin.git" rev-parse main)" = "$base" ] && [ "$(ls "$q" | grep -c .)" = 1 ]; } \
  && ok "enforce holder culprit: rc 6, main unchanged, the waiter keeps its ticket" || bad "holder culprit rc=$rc tickets=$(ls "$q")"
kill $PIDS 2>/dev/null; wait 2>/dev/null
done_case

# --- enforce, lost runner
setup; queue_on; commit_file lost x
land; rc=$?
{ [ "$rc" = 0 ] && main_has lost && [ "$(dispatches)" = 2 ] && grep -q -- '-r ' "$H/dispatched"; } \
  && ok "enforce lost runner: dispatched once more, never bisected, lands" || bad "lost runner rc=$rc dispatches=$(dispatches)"
done_case

# --- enforce, dispatch fails
setup; queue_on; base="$(git rev-parse origin/main)"; commit_file g1 x; touch "$H/dispatch-fails"
land; rc=$?
{ [ "$rc" = 6 ] && [ "$(git --git-dir="$H/origin.git" rev-parse main)" = "$base" ] && [ "$(mq_refs)" = 0 ]; } \
  && ok "enforce dispatch failure: rc 6, nothing on main, no ref left" || bad "dispatch failure rc=$rc refs=$(mq_refs)"
grep -q "candidate not measured (unmeasured - - dispatch_failed" "$H/.amux/logs/land.log" && ok "enforce dispatch failure: land.log says why" || bad "no unmeasured line"
done_case

# --- enforce, a stack of only no-code paths
nocode() { git config --add amux.landMergeQueueNoCodePath 'research/*'; git config --add amux.landMergeQueueNoCodePath '!research/cicd/*'; }
setup; queue_on; nocode; commit_file research/notes.md x
land; rc=$?
{ [ "$rc" = 0 ] && main_has research/notes.md && [ "$(dispatches)" = 0 ]; } \
  && ok "no-code: lands with no candidate" || bad "no-code rc=$rc dispatches=$(dispatches)"
done_case

setup; queue_on; nocode; commit_file research/cicd/runs.py x
land; rc=$?
{ [ "$rc" = 0 ] && main_has research/cicd/runs.py && [ "$(dispatches)" = 1 ]; } \
  && ok "no-code: an excluded path still gets a candidate" || bad "excluded rc=$rc dispatches=$(dispatches)"
done_case

setup; queue_on; nocode; commit_file research/notes.md x; commit_file code.py y
land; rc=$?
{ [ "$rc" = 0 ] && [ "$(dispatches)" = 1 ]; } && ok "no-code: code beside it still gets a candidate" || bad "mixed rc=$rc dispatches=$(dispatches)"
done_case

setup; queue_on; nocode; git config amux.landMergeQueueNoCodeCheck 'echo planted secret; exit 1'; base="$(git rev-parse origin/main)"; commit_file research/notes.md x
land; rc=$?
{ [ "$rc" = 6 ] && [ "$(git --git-dir="$H/origin.git" rev-parse main)" = "$base" ]; } \
  && ok "no-code: its check still refuses, nothing on main" || bad "no-code check rc=$rc"
done_case

setup; queue_on; nocode; git config amux.landMergeQueueNoCodeCheck 'git cat-file -e "$AMUX_LAND_TIP:research/notes.md"'; commit_file research/notes.md x
land; rc=$?
{ [ "$rc" = 0 ] && main_has research/notes.md; } && ok "no-code: its check sees the stack's tip" || bad "no-code tip rc=$rc"
done_case

setup; queue_on; nocode; mkdir research; git mv f research/f; git commit -qm "move f"
land; rc=$?
{ [ "$rc" = 0 ] && [ "$(dispatches)" = 1 ]; } && ok "no-code: code renamed under a no-code path still gets a candidate" || bad "rename rc=$rc dispatches=$(dispatches)"
done_case

# The check refuses a waiter's file: the holder still lands, alone, and the waiter keeps its place.
setup; queue_on; nocode; git config amux.landMergeQueueNoCodeCheck '! git cat-file -e "$AMUX_LAND_TIP:research/planted.md"'; commit_file research/notes.md x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
plant w1 research/planted.md
land; rc=$?
{ [ "$rc" = 0 ] && main_has research/notes.md && ! main_has research/planted.md && [ "$(ls "$q" | grep -c .)" = 1 ]; } \
  && ok "no-code: a batch its check refuses lands the holder alone, the waiter keeps its ticket" || bad "no-code batch rc=$rc tickets=$(ls "$q")"
kill $PIDS 2>/dev/null; wait 2>/dev/null
done_case

# --- enforce, main moves under a green candidate
setup; queue_on; commit_file g1 x; echo other.txt > "$H/move-main"
land; rc=$?
{ [ "$rc" = 0 ] && main_has g1 && main_has other.txt && [ "$(dispatches)" = 1 ]; } \
  && ok "moved main, no shared file: the green candidate lands, replayed, with no second run" || bad "carried rc=$rc dispatches=$(dispatches)"
done_case

setup; queue_on; commit_file g1 x; echo g1 > "$H/move-main"
land; rc=$?
{ grep -q "re-composing on the new tip" "$H/.amux/logs/land.log" && ! grep -q "replayed onto the new tip" "$H/.amux/logs/land.log"; } \
  && ok "moved main on the same file: not carried" || bad "overlap rc=$rc dispatches=$(dispatches)"
done_case

setup; queue_on; nocode; commit_file g1 x; echo research/notes.md > "$H/move-main"
land; rc=$?
{ [ "$rc" = 0 ] && main_has g1 && [ "$(dispatches)" = 1 ]; } && ok "moved main by a no-code commit: carried" || bad "no-code move rc=$rc dispatches=$(dispatches)"
done_case

# --- shadow
setup; queue_on shadow; commit_file bad x
land; rc=$?
{ [ "$rc" = 0 ] && main_has bad && [ "$(dispatches)" = 1 ] && ! grep -q 'runs?branch' "$H/gh.log"; } \
  && ok "shadow: the candidate is dispatched and main is pushed directly without waiting" || bad "shadow rc=$rc dispatches=$(dispatches)"
grep -q '"phase": "shadow"' "$H/.amux/logs/land-mq.jsonl" && ok "shadow: the ledger records the candidate" || bad "no shadow ledger line"
done_case

# --- misconfigured: on, but no workflow
setup; git config amux.landMergeQueue enforce; git config amux.landMergeQueueRepo o/r; commit_file g1 x
land; rc=$?
{ [ "$rc" = 0 ] && main_has g1 && [ ! -f "$H/gh.log" ] && grep -q "verdict=mq_misconfigured" "$H/.amux/logs/land.log"; } \
  && ok "misconfigured: lands directly and logs verdict=mq_misconfigured" || bad "misconfigured rc=$rc"
done_case

exit $fail
