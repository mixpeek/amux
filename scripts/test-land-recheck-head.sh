#!/usr/bin/env bash
# test-land-recheck-head.sh [amux]: a queued ticket that follows HEAD names
# only a commit the land precheck passed. mixpeek-dev, 2026-11-24: a waiter's
# precheck passed at 19:21Z, it committed twice while waiting, and another
# holder's batch replayed those commits unchecked into two red CI runs. A
# waiter queues behind a fake holder; a clean commit moves its ticket, a
# commit the precheck refuses does not, and that refusal runs once.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lrh.XXXXXX")"; mkdir -p "$H/.amux/logs/land"
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
mk() { git clone -q origin.git "$1" 2>/dev/null; git -C "$1" config user.email t@t; git -C "$1" config user.name t; }
mk seed; (cd seed && echo a > f && git add f && git commit -qm base && git push -q origin HEAD:main 2>/dev/null)
mk lw; (cd lw && echo 1 > g && git add g && git commit -qm one -- g)
# The precheck refuses any tip whose tree has a file named "bad".
printf '#!/bin/sh\necho "$2" >> "%s/runs"\nif git cat-file -e "$2:bad" 2>/dev/null; then echo "leg: REFUSED bad"; exit 1; fi\necho "land-precheck: passed"; exit 0\n' "$H" > "$H/chk.sh"
chmod +x "$H/chk.sh"
key="$(printf '%s %s' "$(git -C lw remote get-url origin)" main | shasum | cut -c1-16)"
lock="$H/.amux/locks/land-$key"; mkdir -p "$lock"
sleep 300 & holder=$!
echo "$holder" > "$lock/pid"; echo other > "$lock/who"; date +%s > "$lock/since"
(cd lw && HOME="$H" AMUX_LAND_PRECHECK="$H/chk.sh" AMUX_LAND_NOTIFY=0 AMUX_WORKER=lw bash "$AM" land >/dev/null 2>&1) & P=$!
tk() { for f in "$lock.q"/*; do [ "$(sed -n 1p "$f" 2>/dev/null)" = lw ] && sed -n 2p "$f"; done; }
waitfor() { for _ in $(seq 1 40); do [ "$(tk)" = "$1" ] && return 0; sleep 1; done; return 1; }
fail=0
c1="$(git -C lw rev-parse HEAD)"
waitfor "$c1" && echo "ok   the ticket names the enqueued commit" || { echo "FAIL setup: ticket $(tk), want $c1"; fail=1; }
(cd lw && echo 2 > g && git commit -qm two -- g); c2="$(git -C lw rev-parse HEAD)"
waitfor "$c2" && echo "ok   a commit the precheck passes moves the ticket" || { echo "FAIL ticket $(tk), want $c2"; fail=1; }
(cd lw && echo x > bad && git add bad && git commit -qm bad -- bad); c3="$(git -C lw rev-parse HEAD)"
for _ in $(seq 1 40); do grep -q "result=refused" "$H/.amux/logs/land.log" && break; sleep 1; done
sleep 8
[ "$(tk)" = "$c2" ] && echo "ok   a commit the precheck refuses leaves the ticket on the last checked commit" || { echo "FAIL ticket $(tk), want $c2 (c3 $c3)"; fail=1; }
grep -q "verdict=land_head_rechecked result=refused" "$H/.amux/logs/land.log" && echo "ok   the refusal is logged with its verdict" || { echo "FAIL no refused verdict in land.log"; fail=1; }
n="$(grep -c "$c3" "$H/runs" 2>/dev/null)"
[ "$n" = 1 ] && echo "ok   the refused HEAD is checked once, not every poll" || { echo "FAIL the refused HEAD ran $n times"; fail=1; }
(cd lw && git rm -q bad && git commit -qm fix -- bad); c4="$(git -C lw rev-parse HEAD)"
waitfor "$c4" && echo "ok   a fixed HEAD is checked again and moves the ticket" || { echo "FAIL ticket $(tk), want $c4"; fail=1; }
kill "$holder" 2>/dev/null; pkill -f "$H" 2>/dev/null; kill "$P" 2>/dev/null; wait 2>/dev/null
[ "$fail" = 0 ] && { cd / && rm -rf -- "${H:?}"; } || echo "kept $H"
exit $fail
