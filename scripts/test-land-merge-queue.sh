#!/usr/bin/env bash
# test-land-merge-queue.sh [amux]: `amux land` with amux.landMergeQueue (MC-92,
# Mixpeek's research/cicd/MERGE-QUEUE.md). A stand-in gh judges a candidate by
# its tree: a file `bad` makes its run red, a file `lost` makes the first run of
# a candidate red on a lost runner only. Each case uses a fresh bare origin.
#   off       lands directly and never calls gh
#   enforce   green: main is fast-forwarded to the dispatched candidate, the
#             mq ref is gone, and the base reaches the workflow input
#   enforce   red alone: rc 6, main unchanged
#   enforce   red batch: bisected, the culprit waiter ejected, the prefix that
#             already passed lands, the waiters after the culprit keep tickets;
#             a culprit in first place lands the holder alone
#   enforce   red batch while main moves: the passing prefix is carried onto
#             the new main and lands when nothing overlaps; re-tested when a
#             file is shared
#   enforce   red on the holder's own commit in a batch: rc 6, waiters keep
#             their tickets
#   enforce   a green bisect step's mq ref stands while the next step runs
#             and is gone once its prefix lands; a killed holder's kept ref
#             and an orphaned step ref are dropped by the next holder
#   enforce   lost runner: dispatched once more, lands
#   enforce   dispatch fails: rc 6, nothing pushed to main
#   enforce   only no-code paths: lands with no candidate; an excluded path or code
#             beside it gets one; its check can still refuse
#   enforce   main moves under a green candidate: carried when nothing overlaps or
#             the new commit is no-code, re-composed when a file is shared
#   enforce   carry check (amux.landMergeQueueCarryCheck): carried when it names
#             no member, re-tested when it names one or fails, on both carry paths
#   shadow    candidate dispatched, main pushed directly even when it is red
#   misconfigured (no workflow): lands directly with a WARN
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
# [part]: a or b runs that half of the cases (checks.yml runs the halves as two
# checks, in parallel); none runs them all.
PART="${2:-}"; part() { [ -z "$PART" ] || [ "$PART" = "$1" ]; }
case "$PART" in ''|a|b) ;; *) echo "FAIL unknown part '$PART' (a, b or none)"; exit 2 ;; esac
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
  # A stand-in deployd: the candidates it could boot ahead while this one runs.
  git --git-dir="$O" for-each-ref --format='%(refname:short)' refs/heads/mq/ > "$H/seen-${ref##*/}"
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
plant() { # <name> <file> [content] -> a ticket behind the holder for a commit on origin/main
  git checkout -q -b "$1" origin/main; commit_file "$2" "${3:-x}"
  local sha pid common; sha="$(git rev-parse HEAD)"; git checkout -q main
  sleep 300 & pid=$!; PIDS="${PIDS:-} $pid"
  common="$(cd "$(git rev-parse --git-common-dir)" && pwd -P)"
  printf '%s\n%s\n%s\n1\nx\n\n' "$1" "$sha" "$common" > "$q/$(printf '%012d' $(( $(date +%s) + 100 + ${#PIDS} )))-$pid"
}

if part a; then
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

# --- enforce, green, while another land's young mq ref is on origin
# (mixpeek-ops-server, 2026-11-17: the sweep's loop status ended the land under
# set -e with exit 1 and no message.)
setup; queue_on; commit_file g1 x
young="mq/$(date +%s)-424242-a1"
git push -q origin "HEAD:refs/heads/$young" 2>/dev/null
land; rc=$?
{ [ "$rc" = 0 ] && main_has g1; } && ok "enforce green beside another land's young mq ref: lands" || bad "young mq ref rc=$rc; $(tail -5 "$H/out")"
git --git-dir="$H/origin.git" rev-parse -q --verify "refs/heads/$young" >/dev/null && ok "the young mq ref is left for its own land" || bad "the young mq ref was swept"
done_case

# --- enforce, red alone
setup; queue_on; base="$(git rev-parse origin/main)"; commit_file bad x
land; rc=$?
{ [ "$rc" = 6 ] && [ "$(git --git-dir="$H/origin.git" rev-parse main)" = "$base" ]; } && ok "enforce red alone: rc 6 and main unchanged" || bad "enforce red rc=$rc"
grep -q "candidate red (https://ci/runs/[0-9]*); this lane's change is refused" "$H/.amux/logs/land.log" && ok "enforce red alone: land.log names the run" || bad "no refusal line"
done_case

# --- enforce, red batch: holder h, waiters w1, w2 (bad), w3
setup; queue_on; commit_file h x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
plant w1 w1; plant w2 bad; plant w3 w3
land; rc=$?
{ [ "$rc" = 0 ] && main_has h && main_has w1 && ! main_has bad && ! main_has w3; } \
  && ok "enforce red batch: the culprit is bisected out and the prefix that passed lands" || bad "red batch rc=$rc; $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -8)"
# Mixpeek 2026-11-15: three red batches each re-ran a passing prefix and landed
# none of it. The prefix is landed as the exact candidate that ran green.
[ "$(dispatches)" = 4 ] && ok "enforce red batch: 4 runs (full, then its 3 prefixes at once); the passing prefix is not run again" || bad "dispatches=$(dispatches)"
[ "$(git --git-dir="$H/origin.git" rev-parse main)" = "$(grep -- '-b1 ' "$H/dispatched" | awk '{print $2}' | tail -1)" ] \
  && ok "enforce red batch: main is exactly the prefix candidate that ran green" || bad "main $(git --git-dir="$H/origin.git" rev-parse main) is not the b1 candidate: $(cat "$H/dispatched")"
grep -q "landing the prefix that already passed (this lane and 1 waiter(s), verdict=mq_prefix_landed)" "$H/.amux/logs/land.log" && ok "enforce red batch: land.log says the prefix landed" || bad "no prefix line: $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -3)"
grep -q "^landed " "$H/.amux/locks/land-$key.r/"*w1* 2>/dev/null || grep -lq "^landed " "$H/.amux/locks/land-$key.r/"* 2>/dev/null && ok "enforce red batch: the prefix waiter is told it landed" || bad "no landed result for w1"
ls "$q" | grep -q . && ok "enforce red batch: the waiter after the culprit keeps its ticket" || bad "w3's ticket is gone: $(ls "$q")"
grep -q "refused the batch on your commit" "$H/.amux/locks/land-$key.r/"* 2>/dev/null && ok "enforce red batch: the culprit's waiter is told it was refused" || bad "no refusal written for w2"
grep -q "bisected to w2's commit" "$H/.amux/logs/land.log" && ok "enforce red batch: land.log names the culprit lane" || bad "culprit not logged"
[ "$(mq_refs)" = 0 ] && ok "enforce red batch: no candidate ref left" || bad "mq refs left: $(mq_refs)"
# mixpeek-meta-worker, 2026-11-25: the green prefix lost its ref when its run
# ended, so deployd could not boot it while the bisect tested the next step.
b1="$(awk '/-b1 /{print $1}' "$H/dispatched" | tail -1)"; b2="$(awk '/-b2 /{print $1}' "$H/dispatched" | tail -1)"
{ [ -n "$b1" ] && [ -n "$b2" ] && grep -qx "$b1" "$H/seen-${b2##*/}"; } \
  && ok "enforce red batch: the green step's ref stands while the next step runs" || bad "b1=$b1 not seen at $b2: $(cat "$H/seen-${b2##*/}" 2>/dev/null)"
grep -q "mq/${b1#mq/} stays on origin until its land is pushed or set aside (verdict=mq_green_ref_kept)" "$H/.amux/logs/land.log" && grep -q "dropped green candidate ${b1} (its land is pushed" "$H/out" \
  && ok "enforce red batch: the kept ref is logged, and dropped once the prefix lands" || bad "no kept/dropped lines: $(grep -a 'green candidate\|green_ref' "$H/out" | tail -4)"
kill $PIDS 2>/dev/null; wait 2>/dev/null
done_case

# --- a kept ref a killed holder left, and a step ref nobody lands, are dropped;
# a step ref of a land announced as waiting stays
setup; queue_on; commit_file g1 x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
now=$(date +%s); mkdir -p "$H/.amux/locks"
for n in "$now-11111-a1" "$now-999999-a2-b1" "$now-424242-a1-b1"; do git push -q origin "HEAD~1:refs/heads/mq/$n" 2>/dev/null; done
echo "$now-11111-a1" > "$H/.amux/locks/land-$key.mq-kept"
wc="$(printf 'amux land wait\n\nholder=othermac:424242\nlane=other-machine\nexpires_at=%s\n' $(( now + 900 )) | git commit-tree "$(git mktree </dev/null)")"
git push -q origin "$wc:refs/amux/land-wait/main/othermac-424242" 2>/dev/null
land; rc=$?
has_ref() { git --git-dir="$H/origin.git" rev-parse -q --verify "refs/heads/mq/$1" >/dev/null; }
{ [ "$rc" = 0 ] && main_has g1 && ! has_ref "$now-11111-a1" && [ ! -e "$H/.amux/locks/land-$key.mq-kept" ]; } \
  && ok "a green ref a killed holder kept is dropped by the next holder" || bad "stale kept rc=$rc: $(git --git-dir="$H/origin.git" for-each-ref refs/heads/mq/)"
{ ! has_ref "$now-999999-a2-b1" && grep -q "verdict=mq_sweep_orphan_step" "$H/.amux/logs/land.log"; } \
  && ok "a green step ref whose holder no longer lands is dropped at once" || bad "orphan step left: $(git --git-dir="$H/origin.git" for-each-ref refs/heads/mq/)"
has_ref "$now-424242-a1-b1" && ok "a step ref of a land announced as waiting stays" || bad "the waiting land's step ref was dropped"
done_case

# --- enforce, red batch whose culprit is the first waiter: the holder lands
# alone (mxp-presales 2026-11-23: BSD head refused -n 0 and the land exited 1).
setup; queue_on; commit_file h x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
plant w1 bad; plant w2 w2
land; rc=$?
{ [ "$rc" = 0 ] && main_has h && ! main_has bad && ! main_has w2; } \
  && ok "red batch, culprit is waiter 1: the holder's prefix of one lands" || bad "culprit-first rc=$rc; $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -6); $(tail -3 "$H/out")"
grep -q "this lane and 0 waiter(s), verdict=mq_prefix_landed" "$H/.amux/logs/land.log" && ok "red batch, culprit is waiter 1: land.log says the holder landed alone" || bad "no prefix-of-one line"
ls "$q" | grep -q . && ok "red batch, culprit is waiter 1: the waiter after it keeps its ticket" || bad "w2's ticket is gone"
kill $PIDS 2>/dev/null; wait 2>/dev/null
done_case

# --- a deep queue takes a bigger batch (Mixpeek 2026-11-23: 36 deep at 7 a batch).
setup; queue_on; commit_file h x
git config amux.landMergeQueueBatchMax 3; git config amux.landMergeQueueDeepQueue 2; git config amux.landMergeQueueBatchMaxDeep 6
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
plant w1 w1; plant w2 w2; plant w3 w3; plant w4 w4
land; rc=$?
{ [ "$rc" = 0 ] && main_has h && main_has w1 && main_has w4 && [ "$(dispatches)" = 1 ]; } \
  && ok "deep queue: 4 waiters past a depth of 2 land in one candidate, not 2 at a time" || bad "deep rc=$rc dispatches=$(dispatches); $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -4)"
grep -q "verdict=mq_batch_deep" "$H/.amux/logs/land.log" && ok "deep queue: land.log says the batch grew" || bad "no mq_batch_deep line"
kill $PIDS 2>/dev/null; wait 2>/dev/null
done_case

# --- a red batch's bisect lets the other machine's waiter land between steps
# while no step has run green; the first step here (h, w1) is red.
handovers() { grep -c 'verdict=land_remote_lease_handover' "$H/out" "$H/.amux/logs/land.log" 2>/dev/null | awk -F: '{s+=$2} END{print s+0}'; }
other_waits() {
  local wc; wc="$(printf 'amux land wait\n\nholder=othermac:4242\nlane=other-machine\nexpires_at=%s\n' $(( $(date +%s) + 900 )) | git commit-tree "$(git mktree </dev/null)")"
  git push -q origin "$wc:refs/amux/land-wait/main/othermac-4242" 2>/dev/null
}
setup; queue_on; commit_file h x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
git config amux.landMergeQueueBisectParallel 1
plant w1 bad; plant w2 w2; plant w3 w3
other_waits
AMUX_LAND_REMOTE_LEASE_POLL_S=1 land; rc=$?
{ [ "$rc" = 0 ] && main_has h && ! main_has bad; } \
  && ok "bisect with a waiter on the other machine: the prefix still lands" || bad "bisect handover rc=$rc; $(grep -a 'merge queue\|lease' "$H/.amux/logs/land.log" | tail -6)"
[ "$(handovers)" -ge 1 ] \
  && ok "bisect: the lease is handed over between steps before any step ran green" || bad "no handover during the bisect: $(grep -a 'lease' "$H/out" | tail -4)"
kill $PIDS 2>/dev/null; wait 2>/dev/null
done_case

# --- once a bisect step ran green the lease is kept to the end (mixpeek-dev
# 2026-11-25: a handover after the green step let main move, the green prefix
# could not be carried, and 40 min of bisect landed nothing). (h, w1) is green.
setup; queue_on; commit_file h x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
git config amux.landMergeQueueBisectParallel 1
plant w1 w1; plant w2 bad; plant w3 w3
other_waits
AMUX_LAND_REMOTE_LEASE_POLL_S=1 land; rc=$?
{ [ "$rc" = 0 ] && main_has h && main_has w1 && ! main_has bad; } \
  && ok "bisect after a green step: the green prefix lands" || bad "green-step bisect rc=$rc; $(grep -a 'merge queue\|lease' "$H/.amux/logs/land.log" | tail -6)"
{ grep -q 'verdict=mq_bisect_keeps_lease' "$H/.amux/logs/land.log" && [ "$(handovers)" = 0 ]; } \
  && ok "bisect after a green step: no handover, and land.log says the lease is kept" || bad "handovers=$(handovers): $(grep -a 'lease\|keeps' "$H/.amux/logs/land.log" | tail -4)"
kill $PIDS 2>/dev/null; wait 2>/dev/null
done_case

# --- a red batch's prefixes all run at once and the lease is held for one
# round (mixpeek-meta-worker, 2026-11-26: a 6-landing binary bisect held main
# over an hour, one full run per step). w2 is the culprit of h, w1..w4: the
# prefix of 4 is cancelled once the prefix of 3 reads red.
setup; queue_on; commit_file h x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
plant w1 w1; plant w2 bad; plant w3 w3; plant w4 w4
other_waits
AMUX_LAND_REMOTE_LEASE_POLL_S=1 land; rc=$?
{ [ "$rc" = 0 ] && main_has h && main_has w1 && ! main_has bad && ! main_has w3; } \
  && ok "parallel bisect: the culprit is ejected and the green prefix lands" || bad "parallel rc=$rc; $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -6)"
[ "$(dispatches)" = 5 ] && [ "$(grep -c -- '-b[0-9]* ' "$H/dispatched")" = 4 ] \
  && ok "parallel bisect: the full run, then all 4 prefixes in one round" || bad "dispatches=$(dispatches): $(cat "$H/dispatched")"
{ grep -q "dispatched 4 prefixes at once.*verdict=mq_bisect_parallel" "$H/.amux/logs/land.log" && grep -q "verdict=mq_bisect_cancel_rest" "$H/.amux/logs/land.log" && grep -q "^run cancel" "$H/gh.log"; } \
  && ok "parallel bisect: land.log says so, and the run past the culprit is cancelled" || bad "no parallel/cancel lines: $(grep -a 'bisect' "$H/.amux/logs/land.log" | tail -4); $(grep cancel "$H/gh.log")"
{ [ "$(handovers)" = 0 ] && ! grep -q "verdict=mq_bisect_keeps_lease" "$H/.amux/logs/land.log"; } \
  && ok "parallel bisect: one round, so no step boundary to hand over or hold at" || bad "handovers=$(handovers)"
[ "$(mq_refs)" = 0 ] && ok "parallel bisect: no candidate ref left" || bad "mq refs left: $(git --git-dir="$H/origin.git" for-each-ref refs/heads/mq/)"
kill $PIDS 2>/dev/null; wait 2>/dev/null
done_case

# --- enforce, red batch while main moves (Mixpeek 2026-11-23: three commits
# reached main during the bisect and the green prefix was re-composed instead).
setup; queue_on; commit_file h x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
plant w1 w1; plant w2 bad; plant w3 w3
echo other.txt > "$H/move-main"
land; rc=$?
{ [ "$rc" = 0 ] && main_has h && main_has w1 && main_has other.txt && ! main_has bad && ! main_has w3; } \
  && ok "red batch, main moved, no shared file: the passing prefix is carried onto the new main and lands" || bad "carried prefix rc=$rc; $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -6)"
[ "$(dispatches)" = 4 ] && ok "red batch, main moved: no run after the bisect" || bad "dispatches=$(dispatches)"
grep -q "verdict=mq_prefix_carried" "$H/.amux/logs/land.log" && grep -q "verdict=mq_prefix_landed" "$H/.amux/logs/land.log" \
  && ok "red batch, main moved: land.log says the prefix was carried and landed" || bad "no carried line: $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -4)"
ls "$q" | grep -q . && ok "red batch, main moved: the waiter after the culprit keeps its ticket" || bad "w3's ticket is gone"
kill $PIDS 2>/dev/null; wait 2>/dev/null
done_case

setup; queue_on; commit_file h x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
plant w1 w1; plant w2 bad; plant w3 w3
echo w1 > "$H/move-main"
land; rc=$?
{ grep -q "verdict=mq_prefix_overlap" "$H/.amux/logs/land.log" && ! grep -q "verdict=mq_prefix_carried" "$H/.amux/logs/land.log"; } \
  && ok "red batch, main moved on a file the prefix changes: not carried, tested again" || bad "overlap prefix rc=$rc; $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -6)"
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

fi
if part b; then
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

# --- the carry check (Mixpeek 2026-11-24: a carried prefix landed a tree no
# candidate built). It prints the members whose tree the replay changes.
setup; queue_on; commit_file g1 x; echo other.txt > "$H/move-main"
git config amux.landMergeQueueCarryCheck 'env | grep ^AMUX_LAND_ | sort > "$FAKE_GH_HOME/carry-env"'
land; rc=$?
{ [ "$rc" = 0 ] && main_has g1 && [ "$(dispatches)" = 1 ]; } && ok "carry check prints nothing: carried with no second run" || bad "empty carry check rc=$rc dispatches=$(dispatches)"
{ grep -q "^AMUX_LAND_REPLAY=$(git --git-dir="$H/origin.git" rev-parse main)$" "$H/carry-env" \
  && grep -q '^AMUX_LAND_CANDIDATE=[0-9a-f]\{40\}$' "$H/carry-env" && grep -q '^AMUX_LAND_ONTO=[0-9a-f]\{40\}$' "$H/carry-env" && grep -q '^AMUX_LAND_BASE=[0-9a-f]\{40\}$' "$H/carry-env"; } \
  && ok "carry check: given the base, the moved main, the candidate and the landed replay as full shas" || bad "carry env: $(cat "$H/carry-env" 2>/dev/null)"
done_case

setup; queue_on; commit_file g1 x; echo other.txt > "$H/move-main"
git config amux.landMergeQueueCarryCheck 'echo engine; echo platform'
land; rc=$?
{ [ "$rc" = 0 ] && main_has g1 && [ "$(dispatches)" = 2 ] && grep -q "verdict=mq_carry_changes_members members=engine,platform" "$H/.amux/logs/land.log" \
  && ! grep -q "replayed onto the new tip" "$H/.amux/logs/land.log"; } \
  && ok "carry check names members: not carried, tested as a new candidate, then lands" || bad "member carry rc=$rc dispatches=$(dispatches); $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -4)"
done_case

setup; queue_on; commit_file g1 x; echo other.txt > "$H/move-main"
git config amux.landMergeQueueCarryCheck 'echo boom >&2; exit 3'
land; rc=$?
{ [ "$rc" = 0 ] && [ "$(dispatches)" = 2 ] && grep -q "verdict=mq_carry_unmeasured" "$H/.amux/logs/land.log"; } \
  && ok "carry check fails: unjudged, so tested as a new candidate" || bad "failing carry check rc=$rc dispatches=$(dispatches); $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -4)"
done_case

setup; queue_on; commit_file h x
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"; PIDS=""
plant w1 w1; plant w2 bad; plant w3 w3
echo other.txt > "$H/move-main"
git config amux.landMergeQueueCarryCheck 'echo engine'
land; rc=$?
{ grep -q "verdict=mq_carry_changes_members members=engine" "$H/.amux/logs/land.log" && ! grep -q "verdict=mq_prefix_carried" "$H/.amux/logs/land.log" \
  && ! grep -q "verdict=mq_prefix_overlap" "$H/.amux/logs/land.log"; } \
  && ok "red batch, main moved, carry check names a member: the prefix is not carried, tested again" || bad "member prefix rc=$rc; $(grep -a 'merge queue' "$H/.amux/logs/land.log" | tail -6)"
kill $PIDS 2>/dev/null; wait 2>/dev/null
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
fi

exit $fail
