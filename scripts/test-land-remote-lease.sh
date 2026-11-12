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
#      so the two alternate (item 4)
#  12. a timeout on the remote lease alerts (item 5)
#  14. a renewal pushed between the pre-push check's two reads is our own lease,
#      not someone else's: the land pushes and releases it (mxp-gs12,
#      2026-11-10 12:53:32Z lost a green merge-queue run to this)
#  15. a refusal for someone else's lease leaves that lease alone and says it
#      was not released
#  13. `amux land --cancel` (TERM to the land's process group, twice) and an
#      INT to the group both release the lease (mixpeek-cicd, 2026-11-06
#      23:52:20Z: a cancelled land left it held for the TTL)
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
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
other_lease() { # <holder> <expires_at>: a lease written by someone else
  local t c; t="$(git -C "$H/a" mktree </dev/null)"
  c="$(printf 'amux land lease\n\nholder=%s\nlane=vm-lane\nhead=abc1234\ntaken_at=%s\nexpires_at=%s\n' "$1" "$(( $(date +%s) - 90 ))" "$2" | git -C "$H/a" commit-tree "$t")"
  git -C "$H/a" push -q --no-verify --force origin "$c:$REF"; echo "$c"
}
L="$H/.amux/logs/land.log"
printf '#!/bin/sh\necho "$*" >> "%s/alert.log"\n' "$H" > "$H/alert"; chmod +x "$H/alert"

# 1
commit a f b; : > "$H/hook-runs"
land a one; rc=$?
held="$(cat "$H/lease-during-gate" 2>/dev/null)"
[ "$rc" = 0 ] && [ "$(main_has f)" = b ] && [ -n "$held" ] && ok "a land holds the remote lease through its gate" || bad "1: rc=$rc lease during gate='$held': $(tail -2 "$H/one.out")"
[ -z "$(lease_now)" ] && ok "and releases it after the push" || bad "1: lease left behind"
[ "$(wc -l < "$H/hook-runs" | tr -d ' ')" = 1 ] && ok "the lease pushes skip the hook (it ran once)" || bad "1: hook ran $(wc -l < "$H/hook-runs") times"

# 2
commit a f c; commit b g one; echo 4 > "$H/gate-s"
land a first & pa=$!
for _ in $(seq 1 40); do [ -n "$(lease_now)" ] && break; sleep 0.25; done
land b second; rb=$?; wait "$pa"; ra=$?; echo 0 > "$H/gate-s"
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
commit a f m1-first; commit b g m2; echo 3 > "$H/gate-s"
# An 8 s remote poll (production: 20 s), slower than the 5 s local lock poll,
# as in production: without the yield m1-second takes the lease before m2 polls.
land a m1-first AMUX_LAND_REMOTE_LEASE_POLL_S=8 & pa1=$!
for _ in $(seq 1 40); do [ -n "$(lease_now)" ] && break; sleep 0.25; done
land b m2 AMUX_LAND_REMOTE_LEASE_POLL_S=8 & pb=$!
sleep 1
git -C "$H/c" fetch -q origin 2>/dev/null || mk c
git -C "$H/c" fetch -q origin; git -C "$H/c" reset -q --hard origin/main
echo m1-second > "$H/c/h"; git -C "$H/c" add h; git -C "$H/c" commit -qm "h=m1-second"
(cd "$H/c" && HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=m1-second AMUX_LAND_REMOTE_LEASE_POLL_S=8 bash "$AM" land --tries 3 --no-batch >"$H/m1-second.out" 2>&1) & pa2=$!
wait "$pa1" "$pb" "$pa2"; echo 0 > "$H/gate-s"
order="$(git -C "$H/a" log --format=%s origin/main -3 2>/dev/null | tr '\n' ' ')"
git -C "$H/a" fetch -q origin; order="$(git -C "$H/a" log --format=%s origin/main -3 | tr '\n' ' ')"
case "$order" in "h=m1-second g=m2 f=m1-first "*) ok "back-to-back lands on one machine yield: the other machine's waiter lands in between" ;;
  *) bad "11: order on main (newest first): $order" ;; esac
grep -q 'verdict=land_remote_lease_yield' "$H/m1-second.out" && ok "the yield is logged" || bad "11: no yield line"

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

# 7
commit a f off
mine="$(lease_now)"
land a seven AMUX_LAND_REMOTE_LEASE=off; rc=$?
[ "$rc" = 0 ] && [ "$(main_has f)" = off ] && [ "$(lease_now)" = "$mine" ] && ok "AMUX_LAND_REMOTE_LEASE=off lands and leaves the ref alone" || bad "7: rc=$rc"
exit $fail
