#!/usr/bin/env bash
# Pre-commit section 2b: a staged migration whose version number another
# migration holds, in the index or on origin/main, is refused with the next
# free number. Runs the real hook in a scratch repo.
#   scripts/test-pre-commit-migration-guard.sh
set -uo pipefail
HOOK="$(cd "$(dirname "$0")" && pwd)/git-hooks/pre-commit"
TMP=$(mktemp -d "${TMPDIR:-/tmp}/mig-guard.XXXXXX")
trap 'rm -rf "${TMP:?}"' EXIT
pass=0 fail=0
expect() { # name, want (refused|clear), exit status, output
  local got=clear
  [ "$3" -ne 0 ] && got=refused
  if [ "$got" = "$2" ] && { [ "$2" = clear ] || grep -q "verdict=migration_number_taken" <<<"$4"; }; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1)); echo "FAIL $1: want $2, got $got (exit $3)"; echo "$4" | tail -8
  fi
}
run() { (cd "$TMP/r" && bash "$HOOK" 2>&1); }

git init -q "$TMP/r" && cd "$TMP/r" || exit 1
git config user.email t@t && git config user.name t
D=crates/amux-server/migrations
mkdir -p "$D"
echo "-- a" >"$D/0115_a.sql"
git add -A && git commit -qm base --no-verify
base=$(git rev-parse HEAD)
echo "-- b" >"$D/0116_b.sql"
git add -A && git commit -qm b --no-verify
git update-ref refs/remotes/origin/main HEAD

# The incident: a checkout that does not have origin's 0116 yet.
git checkout -q --detach "$base"
echo "-- new" >"$D/0116_new.sql" && git add "$D/0116_new.sql"
out=$(run); expect "stale checkout reuses origin's number" refused $? "$out"
grep -q "next free number (index and origin/main) is 0117" <<<"$out" || { fail=$((fail + 1)); echo "FAIL next free number not named"; }
git rm -q --cached "$D/0116_new.sql" && rm "$D/0116_new.sql"

git checkout -q --detach refs/remotes/origin/main
# Two new files with one number: only the index can see this, origin has neither.
echo "-- x" >"$D/0119_x.sql" && echo "-- y" >"$D/0119_y.sql" && git add "$D/0119_x.sql" "$D/0119_y.sql"
out=$(run); expect "two staged migrations share a number" refused $? "$out"
git rm -q --cached "$D/0119_x.sql" "$D/0119_y.sql" && rm "$D/0119_x.sql" "$D/0119_y.sql"

echo "-- d" >"$D/0117_d.sql" && git add "$D/0117_d.sql"
out=$(run); expect "the next free number" clear $? "$out"
git rm -q --cached "$D/0117_d.sql" && rm "$D/0117_d.sql"

git checkout -q --detach "$base"
git mv "$D/0115_a.sql" "$D/0118_a.sql"
out=$(run); expect "a rename to a free number" clear $? "$out"

echo "migration-number-guard: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
