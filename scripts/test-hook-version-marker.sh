#!/usr/bin/env bash
# test-hook-version-marker.sh — AF-431 / MR-44. install-hooks.sh classified a
# divergent vendored hook by grepping its `# guard-features:` tokens as bare,
# case-insensitive SUBSTRINGS anywhere in the file. mixpeek-research measured
# the cost: mixpeek's GUARD_VERSION 4 copy of amux-staged-guard, ~215 lines and
# five versions behind, read as "carries every canonical feature — left
# untouched" because the literal AMUX-2946 sits at its line 75 in an unrelated
# comment about retired ports. A token a file can satisfy by TALKING about a
# feature cannot certify it HAS the feature, and it fails in the reassuring
# direction.
#
# Marker format mirrors mixpeek's dc21a0d489 so both repos agree: exactly one
# anchored integer, `^GUARD_VERSION = N` or `^# guard-version: N`.
#
# THE CELL THAT MATTERS is 2: a stale copy carrying EVERY token must still read
# STALE. Without it this suite would pass with the numeric check deleted, since
# a stale copy usually lacks tokens too.
set -u
SRC="$(cd "$(dirname "$0")/.." && pwd)"
T=$(mktemp -d) || exit 2
trap 'rm -rf "$T"' EXIT
FAILS=0
fail() { echo "FAIL: $1" >&2; FAILS=$((FAILS + 1)); }

# A repo with a TRACKED hooks dir, which is the only path that classifies
# rather than overwrites.
mkrepo() {
  local d="$T/$1"; mkdir -p "$d/.githooks"; cd "$d" || exit 2
  git init -q .; git config user.email t@test; git config user.name tester
  git config core.hooksPath .githooks
  printf 'x\n' > f; git add -A; git commit -qm init
  printf '%s' "$d"
}

run() { ( cd "$SRC" && bash scripts/install-hooks.sh "$1" ) 2>&1; }

CANON="$SRC/scripts/git-hooks/amux-staged-guard"
CANON_V=$(grep -m1 -E '^GUARD_VERSION[[:space:]]*=[[:space:]]*[0-9]+' "$CANON" | grep -oE '[0-9]+' | head -1)
[ -n "$CANON_V" ] || { echo "FATAL: canonical carries no GUARD_VERSION"; exit 1; }
TOKENS=$(grep -m1 '^# guard-features:' "$CANON" | cut -d: -f2-)

# 1) A copy behind on the number reads STALE.
D=$(mkrepo behind)
sed "s/^GUARD_VERSION *= *[0-9]*/GUARD_VERSION = 1/" "$CANON" > "$D/.githooks/amux-staged-guard"
chmod +x "$D/.githooks/amux-staged-guard"
out=$(run "$D")
case "$out" in *"is STALE"*) ;; *) fail "a copy at version 1 against $CANON_V did not read STALE" ;; esac
case "$out" in *"version 1 against a canonical of $CANON_V"*) ;; *) fail "the warning did not name both numbers" ;; esac

# 2) THE MR-44 SPECIMEN, and the cell this suite exists for: behind on the
#    number while carrying EVERY token. The token loop is satisfied; the
#    number is not. Must still read STALE.
D=$(mkrepo tokens_present)
{ sed "s/^GUARD_VERSION *= *[0-9]*/GUARD_VERSION = 1/" "$CANON"
  echo "# a comment that merely mentions:$TOKENS"; } > "$D/.githooks/amux-staged-guard"
chmod +x "$D/.githooks/amux-staged-guard"
out=$(run "$D")
case "$out" in *"is STALE"*) ;; *) fail "MR-44's specimen passed: stale version, all tokens present" ;; esac
case "$out" in *"Tokens all PRESENT"*) ;; *) fail "the warning did not say the tokens were the misleading half" ;; esac

# 3) A declaring canonical against a target with NO marker fails CLOSED. Every
#    copy predating this convention looks exactly like this.
D=$(mkrepo no_marker)
grep -v -E '^GUARD_VERSION[[:space:]]*=' "$CANON" > "$D/.githooks/amux-staged-guard"
chmod +x "$D/.githooks/amux-staged-guard"
out=$(run "$D")
case "$out" in *"no version marker at all"*) ;; *) fail "a target with no marker did not read STALE" ;; esac

# 4) THE CONTROL. A copy AT the canonical version with a local addition is a
#    deliberate merge and must be left alone. A check that flagged every
#    divergent file would be worth nothing.
D=$(mkrepo current_diverged)
{ cat "$CANON"; echo "# local addition this repo wants kept"; } > "$D/.githooks/amux-staged-guard"
chmod +x "$D/.githooks/amux-staged-guard"
out=$(run "$D")
case "$out" in *"amux-staged-guard is STALE"*) fail "a CURRENT copy with a local addition was called stale" ;; esac
case "$out" in *"reads as a deliberate local merge"*) ;; *) fail "a current divergent copy was not recognised as a merge" ;; esac

# 5) A copy AHEAD of canonical is not stale. Version compare must be <, not !=.
D=$(mkrepo ahead)
sed "s/^GUARD_VERSION *= *[0-9]*/GUARD_VERSION = 9999/" "$CANON" > "$D/.githooks/amux-staged-guard"
chmod +x "$D/.githooks/amux-staged-guard"
out=$(run "$D")
case "$out" in *"amux-staged-guard is STALE"*) fail "a copy AHEAD of canonical was called stale" ;; esac

# 6) Every canonical hook that DECLARES guard-features carries a marker. The
#    convention is worthless if the canonical files do not follow it.
for f in "$SRC"/scripts/git-hooks/*; do
  [ -f "$f" ] || continue
  grep -q '^# guard-features:' "$f" || continue
  grep -qE '^(GUARD_VERSION[[:space:]]*=[[:space:]]*|# guard-version:[[:space:]]*)[0-9]+' "$f" \
    || fail "$(basename "$f") declares guard-features and carries no anchored version marker"
done

if [ "$FAILS" -eq 0 ]; then
  echo "ok: hook version markers — all 6 cases pass"
  exit 0
fi
echo "$FAILS case(s) failed" >&2
exit 1
