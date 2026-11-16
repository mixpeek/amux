#!/usr/bin/env bash
# Proof for scripts/push-consent.sh (AF-548).
#
# The range c6876cf1..8a701877 is the real push this script was built from and
# it is permanent history on origin, so every number below is checkable by
# anyone. Deliberately asserts on the PURE-GIT half (commit counts, Rust/no-Rust
# split, which shas carry no Rust) rather than on live /api/sessions isolation,
# because a lane's isolation is a fact about today and would make this a test of
# the fleet rather than of the script.
#
# The PASS line is COMPUTED from the cell count, never written: a summary a
# script hardcodes cannot disagree with its own run, so it reads as measured
# while being unable to fail.
# `-e` IS THE GUARD, and it is the only one that works here (AF-560).
#
# A missing command returns 127, and `set -e` aborts on it with bash's own
# message naming the helper AND the line:
#     scripts/test-push-consent.sh: line 103: check_not: command not found
# then exit 127. No verdict is printed, so there is no false PASS.
set -euo pipefail
cd "$(dirname "$0")/.."

BASE=c6876cf1
TIP=8a701877
CELLS=0
FAILED=0

check() {  # check <label> <expected-substring> <<< haystack is $3
  CELLS=$((CELLS + 1))
  if printf '%s' "$3" | grep -qF -- "$2"; then
    echo "  ok    $1"
  else
    echo "  FAIL  $1"
    echo "        expected to find: $2"
    FAILED=$((FAILED + 1))
  fi
}

check_not() {  # check_not <label> <forbidden-substring> <haystack>
  CELLS=$((CELLS + 1))
  if printf '%s' "$3" | grep -qF -- "$2"; then
    echo "  FAIL  $1"
    echo "        must NOT contain: $2"
    FAILED=$((FAILED + 1))
  else
    echo "  ok    $1"
  fi
}

# WHY NOT `command_not_found_handle`, which is the obvious answer and was my
# first one: it does not exist before bash 4.0, and this machine runs
# 3.2.57 (the macOS system bash, which `#!/usr/bin/env bash` resolves to).
# I shipped that handler in 40938593 and it was INERT — deleting a check
# helper still produced "PASS (9 outcome cells)" with two cells never run,
# which is the original bug surviving its own fix. Measured, not assumed:
#     bash -c 'command_not_found_handle(){ echo FIRED; }; nosuch' -> no FIRED
# Kept out rather than kept as a no-op: a guard that cannot fire on the
# platform it ships to is worse than an absent one, because the next reader
# believes the case is covered.
#
# The history: two `check_not` calls were added here before that helper
# existed in this file. bash printed "check_not: command not found" to stderr
# and the suite reported PASS with two of nine assertions never executed.
# The verdict was COMPUTED from $CELLS, so even the honest-summary discipline
# did not catch it — nobody reads stderr when the last line says PASS.

git rev-parse --verify --quiet "$BASE^{commit}" >/dev/null || { echo "SKIP: $BASE not in this clone"; exit 0; }
git rev-parse --verify --quiet "$TIP^{commit}"  >/dev/null || { echo "SKIP: $TIP not in this clone";  exit 0; }

out=$(scripts/push-consent.sh "$BASE" "$TIP" 2>&1)

echo "range $BASE..$TIP"
check "counts the range"                 "commits      23"                                   "$out"
check "names the Rust-covered half"      "13 of 23 commit(s) touch Rust"                     "$out"
check "names the half it cannot cover"   "10 of 23 commit(s) touch NO .rs file"              "$out"
check "lists a no-Rust commit by sha"    "8a701877"                                          "$out"
check "warns against quoting a Rust gate" "Do not offer a green Rust gate"                   "$out"
check "reports a reachability section"   "commit(s),"                                        "$out"

# An empty range must say so and exit clean, not fall through the report.
empty=$(scripts/push-consent.sh "$TIP" "$TIP" 2>&1)
check "empty range says nothing to push" "Nothing to push"                                   "$empty"
check "empty range reports zero"         "commits      0"                                    "$empty"

# A LANE NAME WITH SPACES MUST STAY ONE LANE (found by AF-559's probe). The
# untrailered placeholder is `<no Amux-Session trailer>` — three words — and
# `for lane in $lanes` word-split it into three fake lanes, each reported as
# needing consent. Needs a throwaway repo: the real range has no untrailered
# commit, so this cell is unreachable from origin history, and reading the
# script did not reveal it. Running it did.
probe=$(mktemp -d)
here=$(pwd)
git -C "$probe" init -q -b main . 2>/dev/null
git -C "$probe" config user.email t@t
git -C "$probe" config user.name t
echo a > "$probe/f"
git -C "$probe" add f
git -C "$probe" commit -q -m "untrailered" -- f 2>/dev/null
git -C "$probe" branch -f base HEAD 2>/dev/null
echo b >> "$probe/f"
git -C "$probe" add f
git -C "$probe" commit -q -m "second, also untrailered" -- f 2>/dev/null
pout=$(cd "$probe" && AMUX_URL= AMUX_SESSION=probe bash "$here/scripts/push-consent.sh" base main 2>&1)
rm -rf "$probe"

check "an untrailered commit stays ONE lane" "<no Amux-Session trailer> — 1 commit(s)" "$pout"
check_not "does not split into '<no'"        "  <no — "                                "$pout"
check_not "does not split into 'trailer>'"   "  trailer> — "                           "$pout"

# A SAME-NAMED LANE ON ANOTHER AMUX SERVER IS NOT YOU (governance PR 15). On
# 2026-11-10 three commits a Mac's `amux-helper` made were read on a sandbox as
# its own `amux-helper`'s. Amux-Host tells them apart.
probe=$(mktemp -d)
git -C "$probe" init -q -b main . 2>/dev/null
git -C "$probe" config user.email t@t
git -C "$probe" config user.name t
echo a > "$probe/f"; git -C "$probe" add f
git -C "$probe" commit -q -m "base" -- f 2>/dev/null
git -C "$probe" branch -f base HEAD 2>/dev/null
echo b >> "$probe/f"; git -C "$probe" add f
git -C "$probe" commit -q -m "$(printf 'here\n\nAmux-Session: probe\nAmux-Host: aaaaaaaaaaaaaaaa')" -- f 2>/dev/null
echo c >> "$probe/f"; git -C "$probe" add f
git -C "$probe" commit -q -m "$(printf 'there\n\nAmux-Session: probe\nAmux-Host: bbbbbbbbbbbbbbbb')" -- f 2>/dev/null
hout=$(cd "$probe" && AMUX_URL= AMUX_SESSION=probe AMUX_HOST_ID=aaaaaaaaaaaaaaaa bash "$here/scripts/push-consent.sh" base main 2>&1)
rm -rf "$probe"
check "another host's same-named lane is its own lane" "probe@bbbbbbbb — 1 commit(s)" "$hout"
check_not "this host's commit is not tagged"            "probe@aaaaaaaa"                "$hout"

echo
if [ "$FAILED" -eq 0 ]; then
  echo "PASS ($CELLS outcome cells)"
  exit 0
fi
echo "FAIL ($FAILED of $CELLS outcome cells)"
exit 1
