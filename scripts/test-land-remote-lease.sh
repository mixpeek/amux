#!/usr/bin/env bash
# test-land-remote-lease.sh [amux]: two machines landing on one repo take one
# lease on the remote, refs/amux/land-lock/<branch> (mixpeek-meta-worker,
# 2026-11-06: the mixpeek-dev VM's green batch lost its push twice to this
# Mac's queue, because each machine held only its own local lock). Cases from
# mixpeek-cicd's review:
#   1. a land holds the lease through its gate (the hook sees it), its lease
#      pushes skip the hook, and it releases the lease after the push
#   2. two lands from two checkouts against one bare remote: the second waits
#      for the first's lease and then lands on top
#   3. a fresh lease another machine holds keeps this land out (nothing pushed,
#      the lease untouched), and the wait names the holder and the lease's age
#   4. another machine's lease past its expiry plus the skew margin is taken
#      over; inside the margin it is still held
#   5. a lease left by a killed land on this machine is taken over at once
#   6. a renewal that loses its compare-and-swap stops its own push
#   7. AMUX_LAND_REMOTE_LEASE=off lands without touching the ref
#   8. a remote unreadable for a few beats mid-gate is the network, not a lost
#      lease: the land keeps its lease and lands (land gate review)
# From mixpeek-cicd's review of 9f52808bb..d11c0b645:
#   9. a holder waiting on a held remote lease past --max-hold keeps its local
#      lock: the next local land does not take it over (item 1)
#  10. a lease taken over on the remote while the heartbeat stalls refuses the
#      push, read before the push (item 2)
#  11. back-to-back lands on one machine yield to the other machine's waiter,
#      so the two alternate (item 4); the waiter announces itself on
#      refs/amux/land-wait/<branch>/ and removes the announcement after
#  16. a back-to-back land with no announced waiter takes the lease at once
#      (the unconditional yield cost every reland 20-40 s, 2026-11-12)
#  17. the heartbeat's nap dies with the heartbeat
#  12. a timeout on the remote lease alerts (item 5)
#  14. a renewal pushed between the pre-push check's two reads is our own lease,
#      not someone else's: the land pushes and releases it (mxp-gs12,
#      2026-11-10 12:53:32Z lost a green merge-queue run to this)
#  15. a refusal for someone else's lease leaves that lease alone and says it
#      was not released
#  18. a land that needs another attempt hands the lease to the other machine's
#      announced waiter first, so one machine's queue cannot keep the other's
#      out for a whole hold (mixpeek-meta-worker, 2026-11-22: 31+ min)
#  19. `amux land --lease-check` in a pre-push hook refuses a bare push under
#      another land's lease, not an expired one or land's own push; and a lost
#      push names the commit that moved the branch under the lease
#      (mxp-decomp, 2026-11-23: ci/tip/land.sh landed d0dff040f6 that way)
#  20. a --priority land does not yield to the other machine's ordinary
#      waiter, and still yields to a priority one (mixpeek-meta-worker,
#      2026-11-25: a production S1 fix waited out an ordinary land)
#  21. a heartbeat renewal that lands at the release (rl-sha one behind the
#      remote) is still released: the lease on the remote is our own
#      (mxp-gs12, 2026-11-25 13:54Z: release_failed left it for the TTL)
#  22. a commit whose message is over the exec argument limit (1.3 MB, past
#      Linux's 128 KiB per argument and macOS's 1 MiB per command line) is
#      replayed onto a moved main with its message intact (mixpeek-cicd,
#      2026-11-25 15:52Z: "Argument list too long" on a 189,880-byte message)
#  23. the yield to the other machine's waiter lasts until that waiter takes
#      the lease, however slowly it polls, and a land that itself waits
#      yields only to a waiter that started waiting before it
#      (mixpeek-meta-worker, 2026-11-25: mvs-infra's land on the Mac waited
#      3.5 h behind eight mixpeek-dev lands under a fixed 20-40 s yield)
#  13. `amux land --cancel` (TERM to the land's process group, twice) and an
#      INT to the group both release the lease (mixpeek-cicd, 2026-11-06
#      23:52:20Z: a cancelled land left it held for the TTL)
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
# [part]: a or b runs that half of the cases (checks.yml runs the halves as two
# checks, in parallel); none runs them all.
PART="${2:-}"; part() { [ -z "$PART" ] || [ "$PART" = "$1" ]; }
case "$PART" in ''|a|b) ;; *) echo "FAIL unknown part '$PART' (a, b or none)"; exit 2 ;; esac
H="$(mktemp -d "${TMPDIR:-/tmp}/lrl.XXXXXX")"; mkdir -p "$H/.amux/logs/land"
trap 'cd / && rm -rf -- "${H:?}"' EXIT
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
REF=refs/amux/land-lock/main
mk() { # <dir>: a checkout whose hook records the lease and sleeps $H/gate-s seconds
  git clone -q origin.git "$1" 2>/dev/null
  git -C "$1" config user.email t@t; git -C "$1" config user.name t
  mkdir -p "$1/.git/hooks"
  cat > "$1/.git/hooks/pre-push" <<EOF
#!/bin/sh
echo hook >> "$H/hook-runs"
git ls-remote "$H/origin.git" $REF | awk '{print \$1}' > "$H/lease-during-gate"
sleep "\$(cat "$H/gate-s" 2>/dev/null || echo 0)"
while [ -e "$H/hold" ]; do sleep 0.2; done
# race-once: someone lands from checkout c during this gate, so this push loses
# the race and the land makes another attempt (case 18).
if [ -e "$H/race-once" ]; then rm -f "$H/race-once"; git -C "$H/c" push -q --no-verify origin HEAD:main; fi
exit 0
EOF
  chmod +x "$1/.git/hooks/pre-push"
}
mk a; (cd a && echo a > f && git add f && git commit -qm base && git push -q --no-verify origin HEAD:main 2>/dev/null)
mk b; echo 0 > "$H/gate-s"
fail=0
ok() { echo "ok   $*"; }; bad() { fail=1; echo "FAIL $*"; }
land() { # <checkout> <lane> [VAR=value ...]
  local d="$1" w="$2"; shift 2
  # Checkout b lands from its own HOME, as a second machine would: its own
  # local lock, so only the remote lease can serialize it with a.
  local home="$H"; [ "$d" = b ] && { home="$H/m2"; mkdir -p "$home/.amux/logs/land"; }
  (cd "$H/$d" && HOME="$home" AMUX_LAND_NOTIFY=0 AMUX_WORKER="$w" AMUX_LAND_REMOTE_LEASE_POLL_S=1 env "$@" bash "$AM" land --tries 2 --no-batch >"$H/$w.out" 2>&1)
}
commit() { # <checkout> <file> <content>
  git -C "$H/$1" fetch -q origin; git -C "$H/$1" reset -q --hard origin/main
  echo "$3" > "$H/$1/$2"; git -C "$H/$1" add "$2"; git -C "$H/$1" commit -qm "$2=$3"
}
main_has() { git -C "$H/a" fetch -q origin; git -C "$H/a" show "origin/main:$1" 2>/dev/null; }
lease_now() { git -C "$H/a" ls-remote origin "$REF" | awk '{print $1}'; }
waiters() { git -C "$H/a" ls-remote origin 'refs/amux/land-wait/*'; }   # announced waiters
other_lease() { # <holder> <expires_at>: a lease written by someone else
  local t c; t="$(git -C "$H/a" mktree </dev/null)"
  c="$(printf 'amux land lease\n\nholder=%s\nlane=vm-lane\nhead=abc1234\ntaken_at=%s\nexpires_at=%s\n' "$1" "$(( $(date +%s) - 90 ))" "$2" | git -C "$H/a" commit-tree "$t")"
  git -C "$H/a" push -q --no-verify --force origin "$c:$REF"; echo "$c"
}
L="$H/.amux/logs/land.log"
printf '#!/bin/sh\necho "$*" >> "%s/alert.log"\n' "$H" > "$H/alert"; chmod +x "$H/alert"
# Every case, not only the alert case: a land that times out on the lease runs
# `amux alert`, and with this temp HOME the CLI falls back to localhost:8824,
# the owner's real server. Cases three and four-skew paged Ethan by email and
# iMessage (alerts 488 on 11-12, 493 and 494 on 11-17).
export AMUX_LAND_REMOTE_LEASE_ALERT_CMD="$H/alert"

if part a; then
# 1
commit a f b; : > "$H/hook-runs"
land a one; rc=$?
held="$(cat "$H/lease-during-gate" 2>/dev/null)"
[ "$rc" = 0 ] && [ "$(main_has f)" = b ] && [ -n "$held" ] && ok "a land holds the remote lease through its gate" || bad "1: rc=$rc lease during gate='$held': $(tail -2 "$H/one.out")"
[ -z "$(lease_now)" ] && ok "and releases it after the push" || bad "1: lease left behind"
[ "$(wc -l < "$H/hook-runs" | tr -d ' ')" = 1 ] && ok "the lease pushes skip the hook (it ran once)" || bad "1: hook ran $(wc -l < "$H/hook-runs") times"

# 16: a back-to-back land with nobody waiting takes the lease at once (it
# used to yield 20-40 s on every reland). A 30 s poll keeps case 1's release
# inside the yield window, so only the missing waiter can explain no yield.
commit a f b2
nap=$(( 40000 + $$ % 9000 ))   # a renewal interval no other run uses, so pgrep counts only ours
land a sixteen AMUX_LAND_REMOTE_LEASE_POLL_S=30 AMUX_LAND_REMOTE_LEASE_RENEW_S=$nap; rc=$?
[ "$rc" = 0 ] && [ "$(main_has f)" = b2 ] && grep -q 'verdict=land_remote_lease_no_yield' "$H/sixteen.out" && ! grep -q 'verdict=land_remote_lease_yield' "$H/sixteen.out" \
  && ok "a back-to-back land with no other waiter does not yield" || bad "16: rc=$rc f=$(main_has f): $(grep 'lease_' "$H/sixteen.out" | head -3)"
# 17: the heartbeat's nap dies with it: an orphaned `sleep RENEW_S` held the
# land's descriptors for up to five minutes after it exited.
n17="$(pgrep -f "^sleep $nap\$" | wc -l | tr -d ' ')"
[ "$n17" = 0 ] && ok "the heartbeat leaves no sleeping child behind" || bad "17: $n17 'sleep $nap' process(es) outlived the land"

# 2
# The first's gate holds until the second is waiting on its lease: a fixed
# 4 s gate let a loaded box start the second only after the first was done.
commit a f c; commit b g one; : > "$H/hold"
land a first & pa=$!
for _ in $(seq 1 240); do [ -n "$(lease_now)" ] && break; sleep 0.25; done
land b second & pb2=$!
for _ in $(seq 1 480); do grep -q 'verdict=land_remote_lease_wait' "$H/second.out" 2>/dev/null && break; sleep 0.25; done
rm -f "$H/hold"; wait "$pb2"; rb=$?; wait "$pa"; ra=$?
if [ "$ra" = 0 ] && [ "$rb" = 0 ] && [ "$(main_has f)" = c ] && [ "$(main_has g)" = one ]; then ok "two checkouts: both land, the second after the first"
else bad "2: ra=$ra rb=$rb f=$(main_has f) g=$(main_has g)"; fi
grep -q 'verdict=land_remote_lease_wait' "$H/second.out" && ok "the second waited on the first's lease" || bad "2: the second never waited: $(tail -3 "$H/second.out")"

# 3
commit a f d
theirs="$(other_lease vm-host:4242 $(( $(date +%s) + 600 )))"
land a three AMUX_LAND_REMOTE_LEASE_WAIT_S=3; rc=$?
[ "$rc" != 0 ] && [ "$(main_has f)" = c ] && [ "$(lease_now)" = "$theirs" ] && ok "a fresh lease held elsewhere keeps this land out and is left alone" || bad "3: rc=$rc f=$(main_has f)"
grep -q 'verdict=land_remote_lease_wait: main is being landed elsewhere by vm-host:4242 (vm-lane, head abc1234), lease age 9[0-9]s' "$L" \
  && ok "the wait names the holder and the lease's age" || bad "3: wait line: $(grep lease_wait "$L" | tail -1)"
[ -z "$(waiters)" ] && ok "a waiter that gave up removed its announcement" || bad "3: waiter refs left: $(waiters)"

# 4
other_lease vm-host:4242 $(( $(date +%s) - 5 )) >/dev/null
land a four-skew AMUX_LAND_REMOTE_LEASE_WAIT_S=2; rc=$?
[ "$rc" != 0 ] && [ "$(main_has f)" = c ] && ok "inside the skew margin an expired lease is still held" || bad "4: skew not honoured rc=$rc"
land a four AMUX_LAND_REMOTE_LEASE_SKEW_S=0; rc=$?
[ "$rc" = 0 ] && [ "$(main_has f)" = d ] && [ -z "$(lease_now)" ] && ok "past expiry and the margin it is taken over" || bad "4: rc=$rc f=$(main_has f)"

# 5
commit a f e
other_lease "$(hostname -s 2>/dev/null || hostname):999999" $(( $(date +%s) + 600 )) >/dev/null
land a five AMUX_LAND_REMOTE_LEASE_WAIT_S=2; rc=$?
[ "$rc" = 0 ] && [ "$(main_has f)" = e ] && grep -q 'whose process on this machine is gone' "$L" && ok "a killed land's lease on this machine is taken over at once" || bad "5: rc=$rc f=$(main_has f)"

# 6
commit a f lost; echo 6 > "$H/gate-s"; rm -f "$H/lease-during-gate"
land a six AMUX_LAND_REMOTE_LEASE_RENEW_S=1 & p6=$!
for _ in $(seq 1 80); do [ -s "$H/lease-during-gate" ] && break; sleep 0.25; done   # inside the gate
other_lease vm-host:4242 $(( $(date +%s) + 600 )) >/dev/null
wait "$p6"; rc=$?; echo 0 > "$H/gate-s"
[ "$rc" != 0 ] && [ "$(main_has f)" = e ] && ok "a renewal that loses its CAS stops its own push" || bad "6: rc=$rc f=$(main_has f)"
# Whichever guard saw it first, the heartbeat (lost) or the read before the
# push (not_ours), the refusal names the lease.
grep -qE 'verdict=land_remote_lease_(lost|not_ours)' "$H/six.out" && ok "the lost lease is logged" || bad "6: no lost line: $(grep -m2 lease_ "$H/six.out")"

# 8
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null   # case 6 left the other machine's lease
commit a f outage; echo 5 > "$H/gate-s"; rm -f "$H/lease-during-gate"
land a eight AMUX_LAND_REMOTE_LEASE_RENEW_S=1 & p8=$!
for _ in $(seq 1 80); do [ -s "$H/lease-during-gate" ] && break; sleep 0.25; done   # inside the gate
mv "$H/origin.git" "$H/origin.away"; sleep 2.5; mv "$H/origin.away" "$H/origin.git"
wait "$p8"; rc=$?; echo 0 > "$H/gate-s"
[ "$rc" = 0 ] && [ "$(main_has f)" = outage ] && ! grep -q 'land_remote_lease_lost' "$H/eight.out" && ok "an unreadable remote for a few beats is not a lost lease" || bad "8: rc=$rc f=$(main_has f): $(grep -m1 lease_lost "$H/eight.out")"


# 23: the waiter comes round after 6 s, several poll intervals past the old
# fixed yield (one to two polls, POLL_S=1 here); the land holds off until it
# has taken the lease, waits on it, and lands after it.
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
vm23() { # <wait_since or empty>: the other machine's announcement
  local t c; t="$(git -C "$H/a" mktree </dev/null)"
  c="$(printf 'amux land lease\n\nholder=vm:2323\nlane=vm-slow\nhead=abc1234\ntaken_at=\nexpires_at=%s\npriority=0\nwait_since=%s\n' "$(( $(date +%s) + 600 ))" "$1" | git -C "$H/a" commit-tree "$t")"
  git -C "$H/a" push -q --no-verify --force origin "$c:refs/amux/land-wait/main/vm-2323"
}
vm23 ""
commit a f t23
land a t23 AMUX_LAND_REMOTE_LEASE_YIELD_MAX_S=60 AMUX_LAND_REMOTE_LEASE_SKEW_S=0 & p23=$!
sleep 6
other_lease vm:2323 $(( $(date +%s) + 4 )) >/dev/null
git -C "$H/a" push -q --no-verify origin ":refs/amux/land-wait/main/vm-2323"
wait "$p23"; rc=$?
[ "$rc" = 0 ] && [ "$(main_has f)" = t23 ] && grep -q 'verdict=land_remote_lease_yield_done: vm:2323 (vm-slow) took it after' "$H/t23.out" \
  && grep -q 'verdict=land_remote_lease_wait: .*vm:2323' "$H/t23.out" && [ -z "$(lease_now)" ] \
  && ok "the yield lasts until a slow waiter on the other machine takes the lease" || bad "23: rc=$rc f=$(main_has f): $(grep 'lease_' "$H/t23.out" | head -4)"
# A land waiting on a held lease announced first; the other machine's waiter
# announced later, so when the lease frees this land takes it without yielding.
other_lease vm:9999 $(( $(date +%s) + 3 )) >/dev/null
commit a f t23b
land a t23b AMUX_LAND_REMOTE_LEASE_SKEW_S=0 & p23=$!
for _ in $(seq 1 40); do grep -q 'lease_wait' "$H/t23b.out" 2>/dev/null && break; sleep 0.25; done
vm23 $(( $(date +%s) + 1000 ))
wait "$p23"; rc=$?
git -C "$H/a" push -q --no-verify origin ":refs/amux/land-wait/main/vm-2323" 2>/dev/null
[ "$rc" = 0 ] && [ "$(main_has f)" = t23b ] && ! grep -q 'verdict=land_remote_lease_yield:' "$H/t23b.out" \
  && ok "a waiter yields only to a waiter that started waiting before it" || bad "23b: rc=$rc f=$(main_has f): $(grep 'lease_' "$H/t23b.out" | head -4)"
fi
if part b; then
# 9
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
commit a f nine
theirs="$(other_lease vm-host:4242 $(( $(date +%s) + 600 )))"
(cd "$H/a" && HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=nine-a AMUX_LAND_REMOTE_LEASE_POLL_S=1 AMUX_LAND_REMOTE_LEASE_WAIT_S=10 \
  AMUX_LAND_REMOTE_LEASE_ALERT_CMD="$H/alert" bash "$AM" land --tries 1 --no-batch --max-hold 2 >"$H/nine-a.out" 2>&1) & p9=$!
sleep 4
(cd "$H/a" && HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=nine-b AMUX_LAND_REMOTE_LEASE_POLL_S=1 AMUX_LAND_REMOTE_LEASE_WAIT_S=3 \
  AMUX_LAND_REMOTE_LEASE_ALERT_CMD="$H/alert" bash "$AM" land --tries 1 --no-batch --max-hold 2 >"$H/nine-b.out" 2>&1) & p9b=$!
sleep 4
if grep -q 'taking over a lock' "$H/nine-b.out"; then bad "9: the next local land took over a lock whose holder waits on the remote lease"
else ok "a holder waiting on the remote lease past --max-hold keeps its local lock"; fi
wait "$p9" "$p9b" 2>/dev/null
[ "$(lease_now)" = "$theirs" ] && ok "and nobody on this machine took the other machine's lease" || bad "9: lease $(lease_now) theirs $theirs"

# 12 (case 9's holder timed out on the remote lease)
grep -q 'remote land lease' "$H/alert.log" 2>/dev/null && ok "a remote lease timeout alerts" || bad "12: no alert"

# 10
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
commit a f ten2
land a ten AMUX_LAND_REMOTE_LEASE_RENEW_S=1000 AMUX_LAND_TEST_PRE_PUSH_PAUSE_S=5 & p10=$!
for _ in $(seq 1 60); do [ -n "$(lease_now)" ] && break; sleep 0.25; done
sleep 1; other_lease vm-host:4242 $(( $(date +%s) + 600 )) >/dev/null
wait "$p10"; rc=$?
[ "$rc" != 0 ] && [ "$(main_has f)" != ten2 ] && grep -q 'verdict=land_remote_lease_not_ours' "$H/ten.out" \
  && ok "a lease taken over on the remote while the heartbeat stalls refuses the push" || bad "10: rc=$rc f=$(main_has f): $(grep -m1 'lease_' "$H/ten.out")"

# 15 (case 10's land was refused on someone else's lease)
[ "$(lease_now)" != "" ] && grep -q 'verdict=land_remote_lease_not_released' "$H/ten.out" && ! grep -q 'verdict=land_remote_lease_released' "$H/ten.out" \
  && ok "a refusal on someone else's lease leaves it and says it was not released" || bad "15: lease '$(lease_now)': $(grep 'lease_' "$H/ten.out" | tail -2)"

# 11
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
commit a f m1-first; commit b g m2; echo 0 > "$H/gate-s"
# An 8 s remote poll (production: 20 s), slower than the 5 s local lock poll,
# as in production: without the yield m1-second takes the lease before m2 polls.
# m1-first's gate holds until m2 has announced that it waits, as a real
# waiter has long before a 10-minute gate ends (on a loaded box m2 could
# reach the lease only after m1-first had released it).
: > "$H/hold"
land a m1-first AMUX_LAND_REMOTE_LEASE_POLL_S=8 & pa1=$!
for _ in $(seq 1 240); do [ -n "$(lease_now)" ] && break; sleep 0.25; done
land b m2 AMUX_LAND_REMOTE_LEASE_POLL_S=8 & pb=$!
for _ in $(seq 1 240); do [ -n "$(waiters)" ] && break; sleep 0.25; done
[ -n "$(waiters)" ] && ok "a land waiting on the lease announces itself" || bad "11: m2 never announced: $(grep 'lease_' "$H/m2.out" | head -3)"
git -C "$H/c" fetch -q origin 2>/dev/null || mk c
git -C "$H/c" fetch -q origin; git -C "$H/c" reset -q --hard origin/main
echo m1-second > "$H/c/h"; git -C "$H/c" add h; git -C "$H/c" commit -qm "h=m1-second"
(cd "$H/c" && HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=m1-second AMUX_LAND_REMOTE_LEASE_POLL_S=8 bash "$AM" land --tries 3 --no-batch >"$H/m1-second.out" 2>&1) & pa2=$!
# m1-second queues on the local lock behind m1-first, then that gate ends.
for _ in $(seq 1 120); do [ -s "$H/m1-second.out" ] && break; sleep 0.25; done
sleep 2; rm -f "$H/hold"
wait "$pa1" "$pb" "$pa2"; echo 0 > "$H/gate-s"
order="$(git -C "$H/a" log --format=%s origin/main -3 2>/dev/null | tr '\n' ' ')"
git -C "$H/a" fetch -q origin; order="$(git -C "$H/a" log --format=%s origin/main -3 | tr '\n' ' ')"
case "$order" in "h=m1-second g=m2 f=m1-first "*) ok "back-to-back lands on one machine yield: the other machine's waiter lands in between" ;;
  *) bad "11: order on main (newest first): $order" ;; esac
# m2 polls every 8 s and m1-second's local lock every 5 s, so m2 can take the
# lease before m1-second reaches it; then m1-second waits on m2, and there is
# nothing to yield. Otherwise m1-second must have yielded, naming m2.
if grep -q 'verdict=land_remote_lease_wait: .*(m2, ' "$H/m1-second.out"; then ok "m2 took the lease first; m1-second waited on it"
else grep -q 'verdict=land_remote_lease_yield: .* and .*(m2) waits' "$H/m1-second.out" && ok "the yield is logged and names the waiter" || bad "11: no yield line: $(grep 'lease_' "$H/m1-second.out" | head -3)"; fi
[ -z "$(waiters)" ] && ok "every waiter removed its announcement" || bad "11: waiter refs left: $(waiters)"

# 13
# cancel: `amux land --cancel` on a detached land. INT: a land started the way a
# terminal starts one (its own process group, SIGINT at its default, which a
# background job in a non-interactive shell does not have), then INT to the group.
for sig in cancel INT; do
  git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
  commit a f "sig-$sig"; echo 30 > "$H/gate-s"; rm -f "$H/lease-during-gate"
  if [ "$sig" = cancel ]; then
    (cd "$H/a" && HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER="sig-$sig" AMUX_LAND_REMOTE_LEASE_POLL_S=1 bash "$AM" land --detach --no-batch >/dev/null 2>&1)
  else
    (cd "$H/a" && HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER="sig-$sig" AMUX_LAND_REMOTE_LEASE_POLL_S=1 python3 -c '
import os, signal, subprocess, sys
def fg(): os.setsid(); signal.signal(signal.SIGINT, signal.SIG_DFL)
p = subprocess.Popen(["bash", sys.argv[1], "land", "--no-batch"], preexec_fn=fg, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
open(sys.argv[2], "w").write(str(p.pid))' "$AM" "$H/int.pgid" &)
  fi
  for _ in $(seq 1 80); do [ -s "$H/lease-during-gate" ] && break; sleep 0.25; done   # inside the gate
  if [ "$sig" = cancel ]; then
    (cd "$H/a" && HOME="$H" AMUX_WORKER="sig-$sig" bash "$AM" land --cancel >/dev/null 2>&1)
  else
    kill -INT -- "-$(cat "$H/int.pgid")" 2>/dev/null
  fi
  for _ in $(seq 1 40); do [ -z "$(lease_now)" ] && break; sleep 0.25; done
  echo 0 > "$H/gate-s"
  [ -z "$(lease_now)" ] && grep -q "sig-$sig verdict=land_remote_lease_released" "$L" \
    && ok "a land stopped by $sig releases the remote lease" || bad "13 $sig: lease $(lease_now): $(grep "sig-$sig" "$L" | tail -2)"
done
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null

# 14 runs after 11 and 13 on purpose: it lands from machine a, and case 11's
# first land from a then counted as a's back-to-back land and yielded to m2
# (CI: "order on main (newest first): h=m1-second f=m1-first g=m2", 2026-11-10
# and 11-11; it passed on a fast Mac). Case 11 needs a quiet machine a.
# 14: re-push our own lease with a later expiry while rl-sha still names the old one
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
commit a f fourteen
land a fourteen AMUX_LAND_REMOTE_LEASE_RENEW_S=1000 AMUX_LAND_TEST_PRE_PUSH_PAUSE_S=5 & p14=$!
for _ in $(seq 1 60); do [ -n "$(lease_now)" ] && break; sleep 0.25; done
sleep 1; ours="$(lease_now)"
renewed="$(git -C "$H/a" log -1 --format=%B "$ours" | sed "s/^expires_at=.*/expires_at=$(( $(date +%s) + 1200 ))/" | git -C "$H/a" commit-tree "$(git -C "$H/a" mktree </dev/null)")"
git -C "$H/a" push -q --no-verify "--force-with-lease=$REF:$ours" origin "$renewed:$REF"
wait "$p14"; rc=$?
[ "$rc" = 0 ] && [ "$(main_has f)" = fourteen ] && grep -q 'verdict=land_remote_lease_renewed_under_check' "$H/fourteen.out" \
  && ok "a renewal between the check's reads is our own lease: the land pushes" || bad "14: rc=$rc f=$(main_has f): $(grep 'lease_' "$H/fourteen.out" | tail -3)"
[ -z "$(lease_now)" ] && ok "and the renewed lease is released after the push" || bad "14: lease left: $(lease_now)"

# 18
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
commit a f e1; commit b g e2; echo 0 > "$H/gate-s"
[ -d "$H/c" ] || mk c
git -C "$H/c" fetch -q origin; git -C "$H/c" reset -q --hard origin/main
echo race > "$H/c/h"; git -C "$H/c" add h; git -C "$H/c" commit -qm "h=race"
: > "$H/hold"
land a e1 & pa=$!
for _ in $(seq 1 240); do [ -n "$(lease_now)" ] && break; sleep 0.25; done
land b e2 & pb=$!
for _ in $(seq 1 240); do [ -n "$(waiters)" ] && break; sleep 0.25; done
# e1's gate ends with a push from c landing first: e1 must make a second attempt.
: > "$H/race-once"; rm -f "$H/hold"
wait "$pa"; ra=$?; wait "$pb"; rb=$?
git -C "$H/a" fetch -q origin; order="$(git -C "$H/a" log --format=%s origin/main -3 | tr '\n' ' ')"
[ "$ra" = 0 ] && [ "$rb" = 0 ] && grep -q 'lost a race to a push outside land (attempt 1)' "$H/e1.out" \
  && ok "the holder lost its first push and made another attempt" || bad "18: ra=$ra rb=$rb: $(tail -3 "$H/e1.out")"
grep -q 'verdict=land_remote_lease_handover: .*(e2) waits' "$H/e1.out" \
  && ok "before it, the holder handed the lease to the other machine's waiter" || bad "18: no handover: $(grep 'lease_' "$H/e1.out" | tail -4)"
case "$order" in "f=e1 g=e2 h=race "*) ok "the waiter landed between the holder's attempts" ;;
  *) bad "18: order on main (newest first): $order" ;; esac
[ -z "$(lease_now)" ] && [ -z "$(waiters)" ] && ok "and the lease and every announcement are gone after" || bad "18: left: lease=$(lease_now) waiters=$(waiters)"
grep -q 'verdict=land_branch_moved_under_lease: main moved to [0-9a-f]* by .*h=race' "$H/e1.out" \
  && ok "the lost push names the commit that moved main under the lease" || bad "18: no moved-under-lease line: $(grep 'lost a race' -A1 "$H/e1.out" | tail -2)"

# 21: a renewal at the release is our own lease, and it is released
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
commit a f twentyone; echo 0 > "$H/gate-s"
land a twentyone AMUX_LAND_TEST_RENEW_AT_RELEASE=1; rc=$?
[ "$rc" = 0 ] && [ "$(main_has f)" = twentyone ] && [ -z "$(lease_now)" ] \
  && grep -q 'verdict=land_remote_lease_release_own_renewal' "$H/twentyone.out" && grep -q 'verdict=land_remote_lease_released' "$H/twentyone.out" \
  && ! grep -q 'land_remote_lease_release_failed' "$H/twentyone.out" \
  && ok "a renewal that lands at the release is our own lease and is released" \
  || bad "21: rc=$rc f=$(main_has f) lease=$(lease_now): $(grep 'lease_' "$H/twentyone.out" | tail -3)"

# 22: a message too long for one exec argument is replayed intact
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
commit a f before22; echo 0 > "$H/gate-s"
land a before22
git -C "$H/a" fetch -q origin; git -C "$H/a" reset -q --hard origin/main
commit b g moved22                             # main moves under a's next commit
git -C "$H/b" push -q --no-verify origin HEAD:main 2>/dev/null
echo big22 > "$H/a/f"; git -C "$H/a" add f
{ echo "f=big22"; echo; awk 'BEGIN { for (i = 0; i < 26000; i++) printf "staged path %06d of a large codegen commit\n", i }'; } > "$H/msg22"
git -C "$H/a" commit -q -F "$H/msg22"
want22="$(git -C "$H/a" log -1 --format=%B HEAD | wc -c | tr -d ' ')"
land a twentytwo; rc=$?
[ "$rc" = 0 ] || cp "$H/twentytwo.out" "${AMUX_TEST_KEEP_OUT:-/dev/null}" 2>/dev/null
git -C "$H/a" fetch -q origin
got22="$(git -C "$H/a" log -1 --format=%B origin/main | wc -c | tr -d ' ')"
[ "$rc" = 0 ] && [ "$(main_has f)" = big22 ] && [ "$(main_has g)" = moved22 ] && [ "$want22" -gt 1100000 ] && [ "$got22" = "$want22" ] \
  && ! grep -q 'Argument list too long' "$H/twentytwo.out" \
  && ok "a ${want22}-byte message is replayed onto a moved main intact" \
  || bad "22: rc=$rc f=$(main_has f) g=$(main_has g) bytes want=$want22 got=$got22: $(grep -m2 -i 'argument\|error' "$H/twentytwo.out")"

# 19: amux land --lease-check in a pre-push hook keeps a push outside land
# (mixpeek's ci/tip/land.sh, 2026-11-23 03:36:37Z) from landing under a lease
# another land holds, and lets land's own push through.
mkchk() { # <dir>: a checkout whose pre-push runs only the lease check
  git clone -q origin.git "$H/$1" 2>/dev/null
  git -C "$H/$1" config user.email t@t; git -C "$H/$1" config user.name t
  printf '#!/bin/sh\nbash "%s" land --lease-check "$1"\n' "$AM" > "$H/$1/.git/hooks/pre-push"; chmod +x "$H/$1/.git/hooks/pre-push"
}
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
[ -d "$H/k" ] || mkchk k
git -C "$H/k" fetch -q origin; git -C "$H/k" reset -q --hard origin/main
echo bare > "$H/k/i"; git -C "$H/k" add i; git -C "$H/k" commit -qm "i=bare"
theirs="$(other_lease vm-host:4242 $(( $(date +%s) + 600 )))"
git -C "$H/k" push origin HEAD:main >"$H/k19.out" 2>&1; rc=$?
[ "$rc" != 0 ] && [ -z "$(main_has i)" ] && grep -q 'verdict=land_lease_push_refused: main is being landed by vm-host:4242 (vm-lane)' "$H/k19.out" \
  && ok "a bare push under another land's lease is refused by --lease-check, naming the holder" || bad "19: rc=$rc i=$(main_has i): $(tail -2 "$H/k19.out")"
other_lease vm-host:4242 $(( $(date +%s) - 600 )) >/dev/null
git -C "$H/k" push origin HEAD:main >"$H/k19b.out" 2>&1; rc=$?
[ "$rc" = 0 ] && [ "$(main_has i)" = bare ] && ok "past its expiry and the skew, the lease no longer refuses" || bad "19: expired lease: rc=$rc: $(tail -2 "$H/k19b.out")"
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
git -C "$H/k" fetch -q origin; git -C "$H/k" reset -q --hard origin/main
echo own > "$H/k/i"; git -C "$H/k" add i; git -C "$H/k" commit -qm "i=own"
land k nineteen; rc=$?
[ "$rc" = 0 ] && [ "$(main_has i)" = own ] && [ -z "$(lease_now)" ] && ! grep -q land_lease_push_refused "$H/nineteen.out" \
  && ok "land's own push passes the check under its own lease" || bad "19: own land: rc=$rc i=$(main_has i): $(tail -3 "$H/nineteen.out")"

# 20: a --priority land takes the lease at once past an ordinary waiter
# announced by the other machine, and still yields to a priority waiter, so
# two priority lands alternate (mixpeek-meta-worker, 2026-11-25 04:09Z:
# mxp-gs12's production S1 fix yielded main to an ordinary land on the Mac).
# A 30 s poll keeps each land inside the window after the previous release.
git -C "$H/a" push -q --no-verify origin ":$REF" 2>/dev/null
vm_waiter() { # <lane> <priority>: an announcement pushed by the other machine
  local t c; t="$(git -C "$H/a" mktree </dev/null)"
  c="$(printf 'amux land lease\n\nholder=vm:4242\nlane=%s\nhead=abc1234\ntaken_at=%s\nexpires_at=%s\npriority=%s\n' "$1" "$(date +%s)" "$(( $(date +%s) + 600 ))" "$2" | git -C "$H/a" commit-tree "$t")"
  git -C "$H/a" push -q --no-verify --force origin "$c:refs/amux/land-wait/main/vm-4242"
}
pland() { # <lane>: a --priority land from checkout a
  (cd "$H/a" && HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER="$1" AMUX_LAND_REMOTE_LEASE_POLL_S=30 AMUX_LAND_REMOTE_LEASE_YIELD_S=2 \
    bash "$AM" land --tries 2 --no-batch --priority --reason "grant: test case 20" >"$H/$1.out" 2>&1)
}
commit a f p20; land a p20-first AMUX_LAND_REMOTE_LEASE_POLL_S=30   # this machine has just released
vm_waiter vm-plain 0
commit a f p20-prio; pland p20-prio; rc=$?
[ "$rc" = 0 ] && [ "$(main_has f)" = p20-prio ] && grep -q 'verdict=land_remote_lease_priority_passes: vm:4242 (vm-plain) waits without --priority' "$H/p20-prio.out" \
  && grep -q 'verdict=land_remote_lease_no_yield: .*no other --priority land waits' "$H/p20-prio.out" && ! grep -q 'verdict=land_remote_lease_yield' "$H/p20-prio.out" \
  && ok "a --priority land does not yield to an ordinary waiter on the other machine" || bad "20: rc=$rc f=$(main_has f): $(grep 'lease_' "$H/p20-prio.out" | head -4)"
commit a f p20-plain; land a p20-plain AMUX_LAND_REMOTE_LEASE_POLL_S=30 AMUX_LAND_REMOTE_LEASE_YIELD_S=2; rc=$?
[ "$rc" = 0 ] && grep -q 'verdict=land_remote_lease_yield: .*vm:4242 (vm-plain) waits' "$H/p20-plain.out" \
  && ok "an ordinary land still yields to that waiter" || bad "20: ordinary rc=$rc: $(grep 'lease_' "$H/p20-plain.out" | head -3)"
vm_waiter vm-prio 1
commit a f p20-both; pland p20-both; rc=$?
[ "$rc" = 0 ] && [ "$(main_has f)" = p20-both ] && grep -q 'verdict=land_remote_lease_yield: .*vm:4242 (vm-prio) waits' "$H/p20-both.out" \
  && ok "two --priority lands still alternate: a priority land yields to a priority waiter" || bad "20: both rc=$rc: $(grep 'lease_' "$H/p20-both.out" | head -3)"
git -C "$H/a" push -q --no-verify origin ":refs/amux/land-wait/main/vm-4242" 2>/dev/null

# 7
commit a f off
mine="$(lease_now)"
land a seven AMUX_LAND_REMOTE_LEASE=off; rc=$?
[ "$rc" = 0 ] && [ "$(main_has f)" = off ] && [ "$(lease_now)" = "$mine" ] && ok "AMUX_LAND_REMOTE_LEASE=off lands and leaves the ref alone" || bad "7: rc=$rc"
fi
exit $fail
