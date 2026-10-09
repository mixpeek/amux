#!/usr/bin/env bash
# test-land-demote-sticks.sh [amux]: a demoted --priority waiter stays demoted
# across a re-exec. Every re-exec (self-upgrade on a new install, a lost lock)
# replayed the original arguments, so --priority came back: gs12-templates'
# pid 83291 re-queued with priority three times after two demotes (2026-10-03).
# The waiter queues behind a fake holder, is demoted, then a newer copy of land
# is swapped in so it re-execs; its ticket must stay in ordinary order.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lds.XXXXXX")"; mkdir -p "$H/.amux/logs/land"
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
echo a > f; git add f; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
# A gate path, so --priority is granted.
mkdir -p scripts/ci; echo b > scripts/ci/x.sh; git add scripts/ci/x.sh; git commit -qm change -- scripts/ci/x.sh
cp "$AM" "$H/amux-copy"
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
lock="$H/.amux/locks/land-$key"; mkdir -p "$lock"
sleep 300 & holder=$!
echo "$holder" > "$lock/pid"; echo other > "$lock/who"; date +%s > "$lock/since"
HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=tl bash "$H/amux-copy" land --priority --reason "test" > "$H/waiter.out" 2>&1 &
waiter=$!
tk() { local t; for t in "$lock.q"/*-"$waiter"; do [ -e "$t" ] && { basename "$t"; return; }; done; }
for _ in $(seq 1 30); do [ -n "$(tk)" ] && break; sleep 1; done
fail=0
case "$(tk)" in 000*) echo "ok   queued with priority" ;; *) echo "FAIL setup: no priority ticket ($(tk))"; fail=1 ;; esac
HOME="$H" AMUX_WORKER=orch bash "$H/amux-copy" land --demote tl >/dev/null 2>&1
for _ in $(seq 1 40); do case "$(tk)" in 000*|"") sleep 1 ;; *) break ;; esac; done
case "$(tk)" in 000*|"") echo "FAIL the demote did not take ($(tk))"; fail=1 ;; *) echo "ok   demoted to ordinary order" ;; esac
# A newer land is installed: the waiter must re-exec within the 10 s poll
# budget, with 30 s for CI scheduling noise. This deadline must fail if the
# old one-minute poll returns; otherwise this test can quietly cost a minute.
sed 's/^LAND_BEHAVIOR_VERSION="\([^"]*\)"/LAND_BEHAVIOR_VERSION="\1-next"/' "$H/amux-copy" > "$H/amux-next"
mv -f "$H/amux-next" "$H/amux-copy"
for _ in $(seq 1 30); do grep -q 're-executing at its place' "$H/.amux/logs/land.log" 2>/dev/null && break; sleep 1; done
grep -q 're-executing at its place' "$H/.amux/logs/land.log" 2>/dev/null && echo "ok   the waiter re-executed within 30s" || { echo "FAIL setup: no re-exec within 30s"; fail=1; }
grep -q 're-executing at its place.*verdict=land_self_upgrade_reexec' "$H/.amux/logs/land.log" 2>/dev/null && echo "ok   the re-exec has a named verdict" || { echo "FAIL no measured re-exec verdict"; fail=1; }
sleep 6
n_prio="$(sed -n '/re-executing at its place/,$p' "$H/.amux/logs/land.log" | grep -c 'queued with --priority')"
[ "$n_prio" = 0 ] && echo "ok   no 'queued with --priority' after the re-exec" || { echo "FAIL the re-exec claimed priority again ($n_prio line(s))"; fail=1; }
case "$(tk)" in 000*) echo "FAIL --priority came back after the re-exec ($(tk))"; fail=1 ;; "") echo "FAIL the ticket vanished"; fail=1 ;; *) echo "ok   still in ordinary order after the re-exec" ;; esac
kill "$waiter" "$holder" 2>/dev/null; pkill -f "$H/amux-copy" 2>/dev/null; wait 2>/dev/null
cd / && rm -rf -- "${H:?}"
exit $fail
