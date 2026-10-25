#!/bin/bash
# `amux sync-checkout --override` makes a checkout match origin/main exactly
# and saves everything it replaces under refs/amux/pre-override/ first, so the
# "pull from remote" step of /amux-contribute can never lose work for good.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
AMUX="$HERE/amux"
T=$(mktemp -d); trap 'rm -rf -- "${T:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
g() { git -c user.email=t@t -c user.name=t -c init.defaultBranch=main "$@"; }

g init -q --bare "$T/up.git"
g clone -q "$T/up.git" "$T/peer" 2>/dev/null
(cd "$T/peer" && g checkout -q -b main && echo one > f && g add f && g commit -qm one && g push -q origin main)
g clone -q -b main "$T/up.git" "$T/me" 2>/dev/null
(cd "$T/peer" && echo two > f && g commit -qam two && g push -q origin main)
cd "$T/me"
echo mine >> f && g commit -qam "local commit"
echo dirty >> f; echo new > untracked.txt; echo ignored > junk.log; echo '*.log' > .git/info/exclude
# Empty name variables make git refuse every identity, the way a CI runner
# whose account has no full name does (2026-10-24: checks red on "empty ident
# name"). The fixture's own commits above use `g`, which runs outside this.
run() { HOME="$T/home" GIT_AUTHOR_NAME="" GIT_COMMITTER_NAME="" "$AMUX" sync-checkout "$@" 2>&1; }

echo "1. --check reports and changes nothing"
out=$(run --override --check)
check "reports the local commit"  1 "$(grep -c 'local commits not on origin/main: 1' <<<"$out")"
check "head unchanged"            "local commit" "$(git log -1 --format=%s)"
check "no backup ref yet"         0 "$(git for-each-ref refs/amux/pre-override | wc -l | tr -d ' ')"

echo "2. plain sync-checkout still refuses (it never drops work)"
set +e; run >/dev/null; rc=$?; set -e
check "refused with 3"            3 "$rc"

echo "3. --override matches origin/main"
out=$(run --override) || { echo "$out"; exit 1; }
check "verdict logged"            1 "$(grep -c verdict=overridden "$T/home/.amux/logs/sync-checkout.log")"
check "head is origin/main"       "$(git rev-parse origin/main)" "$(git rev-parse HEAD)"
check "file is remote's"          two "$(cat f)"
check "untracked removed"         no "$([ -e untracked.txt ] && echo yes || echo no)"
check "ignored file kept"         yes "$([ -e junk.log ] && echo yes || echo no)"
check "tree clean"                "" "$(git status --porcelain)"

echo "4. everything replaced is in the backup ref"
ref=$(git for-each-ref --format='%(refname)' refs/amux/pre-override)
check "one backup ref"            1 "$(wc -l <<<"$ref" | tr -d ' ')"
check "dirty edit saved"          "$(printf 'one\nmine\ndirty')" "$(git show "$ref:f")"
check "untracked saved"           new "$(git show "$ref:untracked.txt")"
check "local commit is its parent" "local commit" "$(git log -1 --format=%s "$ref^")"
check "restore line printed"      1 "$(grep -c "git switch -c restored-local $ref" <<<"$out")"

echo "5. a second run on a matching checkout is a no-op"
run --override >/dev/null
check "still one backup ref"      1 "$(git for-each-ref refs/amux/pre-override | wc -l | tr -d ' ')"

echo "6. from a detached head on an old commit"
g checkout -q --detach HEAD~1
run --override >/dev/null
check "head is origin/main"       "$(git rev-parse origin/main)" "$(git rev-parse HEAD)"

[ "$fails" = 0 ] && echo "PASS: sync-checkout --override" || { echo "FAIL: $fails check(s)"; exit 1; }
