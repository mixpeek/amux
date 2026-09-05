#!/usr/bin/env bash
# Cells for the installed-hook drift notice in .claude/session-freshness.sh.
#
# WHY. The notice printed one fixed sentence for every kind of drift:
# "a stale guard does not announce itself — it just stops guarding". On
# 2026-08-30 the only difference between the installed amux-staged-guard and the
# checkout's was a COMMENT rewrite; both read `GUARD_VERSION = 11`, so nothing
# had stopped guarding, and the notice said it had.
#
# That is ethos rule 4 failing in the direction that costs trust: a byte diff
# fires on any edit to a hook, docs included, and a start-of-session banner that
# cries wolf every time trains lanes to skip the line — including the MISSING
# case, which is the one where a guard really is off.
#
# So the DETECTOR is unchanged (bytes are still the only thing that can see
# "this is not the file in the checkout"); what changed is that the notice now
# says which of the three situations it measured. The cells below pin all three
# plus the anti-vacuity control, because a banner nobody can fail is a banner
# nobody should believe.
set -uo pipefail
cd "$(dirname "$0")/.."
SRC_REPO="$(pwd)"
PASS=0; FAIL=0

# THE VERSION IS DERIVED, NEVER TYPED (AF-447).
#
# These cells hardcoded `11` in three places: the expected "GUARD_VERSION 11 on
# both", the sed that manufactures a regression, and the expected "installed
# GUARD_VERSION 8 < checkout 11". GUARD_VERSION then moved to 12 (f36b35f1) and
# 13 (349a9ce4) and CI went red on all three — the sed matched nothing, so cell 3
# never even created the regression it was asserting about.
#
# Bumping 11 to 13 would defer this to the next bump. The number belongs to
# scripts/git-hooks/amux-staged-guard, so read it from there: a fixture that
# hand-types a producer's output is a copy of a BELIEF about the producer and
# cannot survive it changing (AF-437).
GV="$(grep -m1 -E '^GUARD_VERSION[[:space:]]*=' "$SRC_REPO/scripts/git-hooks/amux-staged-guard" \
      | grep -oE '[0-9]+' | head -1)"
GV_OLD=$(( GV > 3 ? GV - 3 : 1 ))
# THE CONTROL FOR THE DERIVATION ITSELF. An empty GV makes every expectation
# below a substring match against "GUARD_VERSION  on both", which some output
# could satisfy by accident and which no cell would report as a problem — the
# vacuous pass this file exists to prevent, arriving in its own harness.
if [ -z "$GV" ]; then
  printf '  FAIL could not read GUARD_VERSION from the checkout — every cell below would be vacuous\n'
  exit 1
fi
ok(){ PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
no(){ FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

# Build a scratch checkout that looks like amux to the script: it resolves REPO
# from its OWN location, so the copy has to carry the same shape.
scratch() {
  local tmp; tmp="$(mktemp -d)"
  local R="$tmp/repo"
  mkdir -p "$R/.claude" "$R/scripts/git-hooks"
  git init -q "$R"
  cp "$SRC_REPO/.claude/session-freshness.sh" "$R/.claude/"
  chmod +x "$R/.claude/session-freshness.sh"
  for h in pre-commit pre-push prepare-commit-msg amux-staged-guard; do
    cp "$SRC_REPO/scripts/git-hooks/$h" "$R/scripts/git-hooks/$h"
    cp "$SRC_REPO/scripts/git-hooks/$h" "$R/.git/hooks/$h"
  done
  echo "$R"
}
# Run the hook and echo only the drift stanza (name line + its verdict line).
drift() {
  AMUX_URL="http://127.0.0.1:9" bash "$1/.claude/session-freshness.sh" 2>/dev/null \
    | grep -A1 'installed git hooks differ'
}

echo "hook-drift notice cells"

# 1. CONTROL, and it has to come first: identical hooks say NOTHING. A notice
#    that fires on a clean checkout is the noise problem in its purest form.
R="$(scratch)"
if [ -z "$(drift "$R")" ]; then
  ok "identical installed hooks produce no drift line"
else
  no "a clean checkout must not print a drift notice" "$(drift "$R")"
fi
rm -rf "$(dirname "$R")"

# 2. THE CASE THAT WAS MISREPORTED: a comment-only edit, same GUARD_VERSION.
R="$(scratch)"
printf '\n# a comment added by the drift cells\n' >> "$R/.git/hooks/amux-staged-guard"
out="$(drift "$R")"
case "$out" in
  *"GUARD_VERSION $GV on both"*) ok "comment-only drift names the version on both sides" ;;
  *) no "same-version drift must report the version, not a bare name" "$out" ;;
esac
case "$out" in
  *"just stops guarding"*) no "must not claim the guard stopped guarding when it did not" "$out" ;;
  *"cannot tell a comment edit"*) ok "and says what a byte diff can and cannot distinguish" ;;
  *) no "the verdict line is missing" "$out" ;;
esac
rm -rf "$(dirname "$R")"

# 3. A GENUINELY OLDER INSTALLED COPY. This is the case the old sentence was
#    written for, and it must still read as serious.
R="$(scratch)"
sed -i.bak "s/^GUARD_VERSION = $GV/GUARD_VERSION = $GV_OLD/" "$R/.git/hooks/amux-staged-guard"
rm -f "$R/.git/hooks/amux-staged-guard.bak"
out="$(drift "$R")"
case "$out" in
  *"installed GUARD_VERSION $GV_OLD < checkout $GV"*) ok "an older installed guard names both numbers" ;;
  *) no "a version regression must be called out as such" "$out" ;;
esac
case "$out" in
  *"guarding by the previous rules"*) ok "and says the installed copy is running the old rules" ;;
  *) no "the older-copy verdict line is missing" "$out" ;;
esac
rm -rf "$(dirname "$R")"

# 4. NO VERSION LINE AT ALL — the pre-versioning era, which is older than any
#    number. Without this arm it would fall through to the reassuring branch.
R="$(scratch)"
grep -v '^GUARD_VERSION' "$R/.git/hooks/amux-staged-guard" > "$R/tmp.$$" \
  && mv "$R/tmp.$$" "$R/.git/hooks/amux-staged-guard"
out="$(drift "$R")"
case "$out" in
  *"carries no GUARD_VERSION"*) ok "a pre-versioning installed copy is reported as older" ;;
  *) no "an unversioned installed hook must not read as merely 'differs'" "$out" ;;
esac
rm -rf "$(dirname "$R")"

# 5. MISSING is off, not stale, and keeps the strongest wording.
R="$(scratch)"
rm -f "$R/.git/hooks/amux-staged-guard"
out="$(drift "$R")"
case "$out" in
  *"(MISSING)"*) ok "an absent hook is reported as MISSING" ;;
  *) no "a missing hook must be named as missing" "$out" ;;
esac
case "$out" in
  *"not installed is not running"*) ok "and is stated as off rather than stale" ;;
  *) no "MISSING must keep the strong verdict" "$out" ;;
esac
rm -rf "$(dirname "$R")"

# 6. MISSING WINS OVER a same-version comment edit. Precedence matters: the
#    softest branch must never absorb a real one when both are true at once.
R="$(scratch)"
rm -f "$R/.git/hooks/pre-push"
printf '\n# comment\n' >> "$R/.git/hooks/amux-staged-guard"
out="$(drift "$R")"
case "$out" in
  *"not installed is not running"*) ok "MISSING outranks a comment-only diff in the same run" ;;
  *) no "a missing hook must not be softened by a benign one beside it" "$out" ;;
esac
rm -rf "$(dirname "$R")"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
