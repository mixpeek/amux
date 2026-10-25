#!/usr/bin/env bash
# test-land-guards.sh [amux]: the land guards and outcome reporting added after
# the mxp-gs12 review (2026-10-18), against throwaway repositories under a temp
# HOME, so no live ~/.amux state is read or written.
#
#   1. a path freeze in .amux/land.conf refuses a land that changes a frozen path
#   2. a Freeze-Override trailer lands it, and the override is logged
#   3. an expired freeze is ignored
#   4. a commit reusing a landed commit's exact message over other files is
#      refused (812b83ca5d8), and AMUX_LAND_ALLOW_REUSED_MESSAGE=1 lands it
#   5. a repeated message over the SAME file (a ledger append) still lands
#   6. --json reports landed/on_origin from the remote, for a land and a refusal
#   7. a key read from .git/config logs verdict=land_config_unversioned; the
#      same key in .amux/land.conf does not
#   8. a regenerate command from .amux/land.conf that fails three lands in a
#      row refuses the third (exit 5) after an ERROR streak line
# Each refusal case has an allow twin, so a guard that refuses everything fails.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"; AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
T="$(mktemp -d "${TMPDIR:-/tmp}/lguard.XXXXXX")"
trap 'rm -rf -- "${T:?}"' EXIT
export HOME="$T/home"; mkdir -p "$HOME/.amux/logs"
unset AMUX_SESSION AMUX_URL AMUX_API GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
export AMUX_LAND_CLIENT=1 AMUX_LAND_NOTIFY=0 AMUX_WORKER=lguard GIT_CONFIG_NOSYSTEM=1
LOG="$HOME/.amux/logs/land.log"
fail=0; n=0
ok()  { n=$((n + 1)); echo "ok   $*"; }
bad() { n=$((n + 1)); fail=1; echo "FAIL $*"; }

cd "$T" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git seed 2>/dev/null; cd seed || exit 1
git config user.email t@t; git config user.name t
mkdir -p .amux proofs
echo base > a.txt; echo p > proofs/row1.json; echo l > ledger.md
cat > .amux/land.conf <<'EOF'
[freeze "rows"]
	path = proofs/*
	until = 2099-01-01T00:00:00Z
	owner = test-owner
	reason = proofs are pinned
[freeze "old"]
	path = old/*
	until = 2001-01-01T00:00:00Z
EOF
git add -A; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
echo a2 > a.txt; git commit -qam "ci: a message that describes a.txt"; git push -q origin HEAD:main 2>/dev/null
cd "$T" || exit 1; git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t

land() { bash "$AM" land "$@" >"$T/out" 2>&1; echo $?; }
on_origin() { git fetch -q origin main; git merge-base --is-ancestor "$1" origin/main 2>/dev/null || [ -z "$(git cherry origin/main "$1" | grep '^+')" ]; }

# 1. freeze refuses
echo x >> proofs/row1.json; git commit -qam "proofs: touch row1"
c1="$(git rev-parse HEAD)"
rc="$(land --sha "$c1")"
if [ "$rc" = 4 ] && grep -q "frozen by freeze 'rows'" "$T/out" && ! on_origin "$c1"; then ok "a frozen path is refused (rc 4) and names the freeze"
else bad "frozen path: rc=$rc, on origin? $(on_origin "$c1" && echo yes || echo no): $(tail -3 "$T/out")"; fi

# 2. override trailer lands it and logs the override
git commit -q --amend -m "proofs: touch row1

Freeze-Override: rows: re-run of row 1 at the freeze commit"
c2="$(git rev-parse HEAD)"
rc="$(land --sha "$c2")"
if [ "$rc" = 0 ] && on_origin "$c2" && grep -aq "verdict=land_freeze_override freeze=rows" "$LOG"; then ok "a Freeze-Override trailer lands it and the override is logged"
else bad "override: rc=$rc: $(tail -3 "$T/out")"; fi
git fetch -q origin main; git reset -q --hard origin/main

# 3. expired freeze ignored
mkdir -p old; echo o > old/f.txt; git add old/f.txt; git commit -qm "old: add f"
c3="$(git rev-parse HEAD)"
rc="$(land --sha "$c3")"
if [ "$rc" = 0 ] && on_origin "$c3" && grep -aq "expired_names=old" "$LOG"; then ok "an expired freeze is ignored and counted"
else bad "expired freeze: rc=$rc: $(tail -3 "$T/out")"; fi
git fetch -q origin main; git reset -q --hard origin/main

# 4. reused message over other files refused; the allow switch lands it
echo b > b.txt; git add b.txt; git commit -qm "ci: a message that describes a.txt"
c4="$(git rev-parse HEAD)"
rc="$(land --sha "$c4")"
if [ "$rc" = 4 ] && grep -q "land_message_reused" "$T/out" && ! on_origin "$c4"; then ok "a reused message over other files is refused (812b83ca5d8 shape)"
else bad "reused message: rc=$rc: $(tail -3 "$T/out")"; fi
rc="$(AMUX_LAND_ALLOW_REUSED_MESSAGE=1 land --sha "$c4")"
if [ "$rc" = 0 ] && on_origin "$c4" && grep -aq "land_message_reused_allowed" "$LOG"; then ok "AMUX_LAND_ALLOW_REUSED_MESSAGE=1 lands it, logged"
else bad "reused message allow: rc=$rc: $(tail -3 "$T/out")"; fi
git fetch -q origin main; git reset -q --hard origin/main

# 5. same message over the same file passes (ledger appends)
echo l2 >> ledger.md; git commit -qam "ledger: lane lines (auto-land)"; land --sha HEAD >/dev/null
git fetch -q origin main; git reset -q --hard origin/main
echo l3 >> ledger.md; git commit -qam "ledger: lane lines (auto-land)"
c5="$(git rev-parse HEAD)"
rc="$(land --sha "$c5")"
if [ "$rc" = 0 ] && on_origin "$c5"; then ok "a repeated message over the same file still lands"
else bad "ledger repeat: rc=$rc: $(tail -3 "$T/out")"; fi
git fetch -q origin main; git reset -q --hard origin/main

# 6. --json
echo c > c.txt; git add c.txt; git commit -qm "c: add c.txt"
c6="$(git rev-parse HEAD)"
j="$(bash "$AM" land --sha "$c6" --json 2>/dev/null)"; rc=$?
if [ "$rc" = 0 ] && printf '%s' "$j" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d["landed"] is True and d["on_origin"] is True and d["measured"] and d["n_considered"]==1 and d["sha"] else 1)'; then ok "--json reports a land as landed and on_origin, measured"
else bad "--json land: rc=$rc json=$j"; fi
git fetch -q origin main; git reset -q --hard origin/main
echo y >> proofs/row1.json; git commit -qam "proofs: again"
j="$(bash "$AM" land --sha HEAD --json 2>/dev/null)"; rc=$?
if [ "$rc" = 4 ] && printf '%s' "$j" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d["landed"] is False and d["on_origin"] is False and d["rc"]==4 and d["not_on_origin"] else 1)'; then ok "--json reports a refusal as not landed, with the commits not on origin"
else bad "--json refusal: rc=$rc json=$j"; fi
git reset -q --hard origin/main

# 7. unversioned config is named; versioned is not
git config amux.landBatchMax 4
echo d > d.txt; git add d.txt; git commit -qm "d: add"; land --sha HEAD >/dev/null
if grep -aq "verdict=land_config_unversioned key=amux.landBatchMax" "$LOG"; then ok "a key read from .git/config logs land_config_unversioned"
else bad "no land_config_unversioned line for a .git/config key"; fi
git fetch -q origin main; git reset -q --hard origin/main
git config --file .amux/land.conf amux.landBatchMax 4; git commit -qam "land.conf: batch max"; land --sha HEAD >/dev/null
git fetch -q origin main; git reset -q --hard origin/main
: > "$LOG"
echo e > e.txt; git add e.txt; git commit -qm "e: add"; land --sha HEAD >/dev/null
if ! grep -aq "land_config_unversioned key=amux.landBatchMax" "$LOG"; then ok "the same key in .amux/land.conf reads without the warning"
else bad "versioned key still warned"; fi
git config --unset amux.landBatchMax
git fetch -q origin main; git reset -q --hard origin/main

# 8. regenerate failure streak from the versioned file refuses the third
git config --file .amux/land.conf amux.landRegenerate 'exit 3'
git config --file .amux/land.conf amux.landRegenerateInput gen
git config --file .amux/land.conf amux.landRegenerable 'gen/*'
mkdir -p gen; echo 0 > gen/in; git add -A; git commit -qm "regen: declare a broken generator"; land --sha HEAD >/dev/null
rcs=""
for i in 1 2; do
  git fetch -q origin main; git reset -q --hard origin/main
  echo "$i" > gen/in; git commit -qam "gen: input $i"
  rcs="$rcs $(land --sha HEAD)"
done
if [ "$rcs" = " 0 5" ] && grep -aq "ERROR verdict=land_regenerate_failing consecutive=2 source=.amux/land.conf" "$LOG"; then ok "a failing versioned regenerate is ERROR from the 2nd land in a row and refuses the 3rd (rc 5)"
else bad "regenerate streak: rcs=$rcs: $(grep -a regenerate "$LOG" | tail -3)"; fi

echo "$n case(s), $([ "$fail" = 0 ] && echo all passed || echo FAILURES above)"
exit "$fail"
