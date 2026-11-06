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
commit a f lost; echo 6 > "$H/gate-s"
land a six AMUX_LAND_REMOTE_LEASE_RENEW_S=1 & p6=$!
for _ in $(seq 1 40); do [ -n "$(lease_now)" ] && break; sleep 0.25; done
sleep 0.5; other_lease vm-host:4242 $(( $(date +%s) + 600 )) >/dev/null
wait "$p6"; rc=$?; echo 0 > "$H/gate-s"
[ "$rc" != 0 ] && [ "$(main_has f)" = e ] && ok "a renewal that loses its CAS stops its own push" || bad "6: rc=$rc f=$(main_has f)"
grep -q 'verdict=land_remote_lease_lost' "$L" && ok "the lost lease is logged" || bad "6: no lost line"

# 7
commit a f off
mine="$(lease_now)"
land a seven AMUX_LAND_REMOTE_LEASE=off; rc=$?
[ "$rc" = 0 ] && [ "$(main_has f)" = off ] && [ "$(lease_now)" = "$mine" ] && ok "AMUX_LAND_REMOTE_LEASE=off lands and leaves the ref alone" || bad "7: rc=$rc"
exit $fail
