#!/usr/bin/env bash
# AF-195 — the staged-guard must refuse a commit whose staged bytes are not the
# bytes on disk, and must NOT refuse the three forms where they cannot differ.
#
# WHY A SHELL TEST AND NOT A PYTHON UNIT TEST. The property under test is about
# what GIT does before the hook runs: `git commit -a` and `git commit -- <path>`
# build a TEMPORARY index from the working tree and point GIT_INDEX_FILE at it,
# so `git diff` inside the hook compares the worktree against that temp index
# and finds nothing. That is the whole reason those forms are safe, and it is a
# claim about git's behaviour that no amount of reading the hook can settle.
# Calling the function directly would test a paraphrase of the shipped path.
#
# It drives the SHIPPED scripts/git-hooks/amux-staged-guard, installed as a real
# pre-commit hook in a scratch repo, invoked by real `git commit`.
#
# AMUX_VERIFIED_SOLO is set throughout. That is not a workaround: the divergence
# check is specified to run BEFORE the foreign-work overrides and to outrank
# them, so setting it isolates this check from the server-backed half AND
# asserts the ordering claim at the same time. If the ordering ever regresses,
# every blocking cell here goes green and the test fails loudly.
#
# Exit 0 = pass, 1 = failure.
set -uo pipefail
cd "$(dirname "$0")/.."
GUARD="$PWD/scripts/git-hooks/amux-staged-guard"
[ -f "$GUARD" ] || { echo "FAIL — $GUARD not found; this test is now blind"; exit 1; }

command -v python3 >/dev/null 2>&1 || { echo "SKIP: python3 not installed"; exit 2; }

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
export AMUX_HOME="$TMP/amux-home"; mkdir -p "$AMUX_HOME"
export AMUX_SESSION="test-lane"
export AMUX_VERIFIED_SOLO=1
unset AMUX_PARTIAL_STAGE

pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  ok   — $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL — $1"; }

# A fresh repo per cell: a commit that lands changes HEAD and would leak into
# the next cell's staged set.
fresh_repo() {
  rm -rf "$TMP/repo"; mkdir -p "$TMP/repo"; cd "$TMP/repo"
  git init -q .
  git config user.email t@t; git config user.name t
  git config commit.gpgsign false
  mkdir -p .git/hooks
  printf '#!/bin/sh\nexec python3 "%s"\n' "$GUARD" > .git/hooks/pre-commit
  chmod +x .git/hooks/pre-commit
  echo "original" > a.txt; echo "other" > b.txt
  git add a.txt b.txt
  # The seed commit must not be judged by the hook under test.
  git -c core.hooksPath=/dev/null commit -qm seed
}

# staged content and worktree content deliberately disagree on a.txt
diverge() {
  echo "STAGED" > a.txt
  git add a.txt
  echo "ON DISK, NOT STAGED" > a.txt
}

echo "AF-195 index/worktree divergence, against the shipped guard:"

# 1. THE INCIDENT'S OWN FORM: plain `git commit` over a hand-staged index.
fresh_repo; diverge
out=$(git commit -qm "plain" 2>&1); rc=$?
[ $rc -ne 0 ] && ok "plain git commit with divergence is REFUSED" \
              || bad "plain git commit with divergence was ALLOWED (rc=$rc)"
case "$out" in
  *a.txt*) ok "the refusal names the offending path" ;;
  *)       bad "the refusal does not name a.txt: $out" ;;
esac
case "$out" in
  *"git add a.txt"*) ok "it prints the escape that commits what you tested" ;;
  *) bad "no 'git add' escape in the message" ;;
esac
# The refusal must not have written a commit.
[ "$(git rev-list --count HEAD)" = "1" ] && ok "no commit was created" \
                                         || bad "a commit landed despite the refusal"

# 2. THE NEGATIVE CONTROL that matters most: staged, but NOT divergent. If this
#    goes red the guard fires on every ordinary commit and will be bypassed.
fresh_repo
echo "clean change" > a.txt; git add a.txt
git commit -qm "clean" 2>&1 >/dev/null
[ "$(git rev-list --count HEAD)" = "2" ] && ok "an ordinary staged commit is allowed" \
                                         || bad "an ordinary staged commit was refused"

# 3. Dirty file that is NOT being committed — the second half of the
#    intersection. On a shared checkout this is the common state, and flagging
#    it would make the guard fire constantly.
fresh_repo
echo "unstaged noise" > b.txt          # dirty, never staged
echo "staged and clean" > a.txt; git add a.txt
git commit -qm "unrelated dirt" 2>&1 >/dev/null
[ "$(git rev-list --count HEAD)" = "2" ] && ok "unrelated dirty files do not block" \
                                         || bad "an unrelated dirty file blocked the commit"

# 4/5. The two forms git resolves through a TEMPORARY index. These are the
#      cells that make the whole design safe, and they are assertions about
#      git, not about the hook.
fresh_repo; diverge
git commit -qam "commit -a" 2>&1 >/dev/null
[ "$(git rev-list --count HEAD)" = "2" ] && ok "git commit -a is allowed (temp index == worktree)" \
                                         || bad "git commit -a was refused"
if [ "$(git show -s --format=%H HEAD 2>/dev/null)" ]; then
  [ "$(git show HEAD:a.txt)" = "ON DISK, NOT STAGED" ] \
    && ok "and -a really committed the DISK copy, not the staged one" \
    || bad "-a committed something other than the disk copy"
fi

fresh_repo; diverge
git commit -qm "pathspec" -- a.txt 2>&1 >/dev/null
[ "$(git rev-list --count HEAD)" = "2" ] && ok "git commit -- <path> is allowed" \
                                         || bad "git commit -- <path> was refused"
if [ "$(git rev-list --count HEAD)" = "2" ]; then
  [ "$(git show HEAD:a.txt)" = "ON DISK, NOT STAGED" ] \
    && ok "and -- <path> really committed the DISK copy" \
    || bad "-- <path> committed something other than the disk copy"
fi

# 6. The honest escape: a DECLARED partial stage proceeds, and is recorded.
fresh_repo; diverge
AMUX_PARTIAL_STAGE=1 git commit -qm "declared" 2>&1 >/dev/null
[ "$(git rev-list --count HEAD)" = "2" ] && ok "AMUX_PARTIAL_STAGE proceeds" \
                                         || bad "AMUX_PARTIAL_STAGE did not proceed"
[ "$(git show HEAD:a.txt)" = "STAGED" ] \
  && ok "and it committed the STAGED copy, which is what the declaration claims" \
  || bad "the declared partial stage committed the wrong copy"

# 7. THE LEDGER. A refusal nobody can count is the AF-182 shape itself: five
#    instances were visible only because two lanes happened to be talking.
LEDGER="$AMUX_HOME/staged-guard-divergence.jsonl"
if [ -f "$LEDGER" ]; then
  ok "the divergence ledger exists ($LEDGER)"
  python3 - "$LEDGER" <<'PY' && ok "ledger carries both a blocked and a declared row" || bad "ledger is missing one of the two resolutions"
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
res = {r.get("resolution") for r in rows}
need = {"blocked", "declared-partial"}
missing = need - res
if missing:
    print("    missing resolutions: %s (saw %s)" % (sorted(missing), sorted(res)))
    sys.exit(1)
# The paths must be NAMED, not counted: a count cannot be acted on.
if not any(r.get("paths") for r in rows):
    print("    ledger rows carry no paths")
    sys.exit(1)
PY
else
  bad "no divergence ledger was written"
fi

cd "$TMP"
echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1
