#!/usr/bin/env bash
# test-append-only-inplace-edit.sh — AF-528: the append-only guard's FOURTH cause.
#
# An entry REWRITTEN IN PLACE loses its old lines while the entry survives under
# the same heading. That is what merging main FORWARD into a stale branch does,
# and the guard used to report it as a stale republish — the opposite direction,
# with a prescribed remedy (`git checkout origin/main -- <file>`) that would have
# reinstated the older text.
#
# Two cases, and the CONTROL is the one that matters: a real deletion must NOT
# acquire the new wording, or the guard would excuse every deletion as an edit.
set -u
# Fixtures are HERMETIC: no template from the developer's own git config.
# `git init` copies $HOME's init.templatedir into every new repo, so a global
# commit-msg hook (a Conventional Commits enforcer, say) lands in the throwaway
# repos below and rejects their fixture commits. 17 of this repo's test scripts
# failed that way on a machine that had one, while CI stayed green because the
# runner has no template — a test that passes only on machines configured like
# the author's. Empty means "no template", and git then creates no .git/hooks,
# so a test that installs a hook makes that directory itself.
export GIT_TEMPLATE_DIR=


GUARD="$(cd "$(dirname "$0")" && pwd)/git-hooks/append-only-push-guard"
T=$(mktemp -d) || exit 2
trap 'rm -rf "$T"' EXIT
FAILS=0
fail() { echo "FAIL: $1" >&2; FAILS=$((FAILS + 1)); }

cd "$T" || exit 2
git init -q .
git config user.email t@test && git config user.name tester
printf 'FRUSTRATIONS.md\n' > .append-only-files

entry_open() {
  printf '## the thing that keeps happening\nSTATUS: open\nCARD: X-1\nFIX: replace the literal match with a structural check\n  mirroring what the pre-question clause already does\n'
}
printf -- '# ledger\n\n' > FRUSTRATIONS.md
entry_open >> FRUSTRATIONS.md
printf '\n## an unrelated entry\nSTATUS: open\nCARD: X-2\n' >> FRUSTRATIONS.md
git add -A && git commit -qm base
BASE=$(git rev-parse HEAD)

# ---- CASE 1: the entry is REWRITTEN IN PLACE (open -> fixed, FIX replaced).
printf -- '# ledger\n\n' > FRUSTRATIONS.md
printf '## the thing that keeps happening\nSTATUS: fixed\nCARD: X-1\nFIX: done in abc1234 by splitting the tail on the same connectors\n  the existing verb check already trusts\n' >> FRUSTRATIONS.md
printf '\n## an unrelated entry\nSTATUS: open\nCARD: X-2\n' >> FRUSTRATIONS.md
git add -A && git commit -qm 'rewrite the entry in place'
HEAD1=$(git rev-parse HEAD)
OUT=$("$GUARD" --check "$BASE" "$HEAD1" 2>&1)
if printf '%s' "$OUT" | grep -q 'AN IN-PLACE EDIT'; then
  echo "  ok   an in-place rewrite is named as an in-place edit"
else
  fail "an in-place rewrite was NOT named; the reader still gets only the delete-shaped causes"
  printf '%s\n' "$OUT" | head -20 | sed 's/^/       /'
fi
if printf '%s' "$OUT" | grep -q 'the thing that keeps happening'; then
  echo "  ok   it NAMES the surviving heading, so the claim is checkable"
else
  fail "the surviving heading was not named — 'an edit happened somewhere' is not actionable"
fi

# ---- CONTROL: a real deletion. The entry is GONE, heading and all.
git reset -q --hard "$BASE"
printf -- '# ledger\n\n## an unrelated entry\nSTATUS: open\nCARD: X-2\n' > FRUSTRATIONS.md
git add -A && git commit -qm 'delete the entry outright'
HEAD2=$(git rev-parse HEAD)
OUT2=$("$GUARD" --check "$BASE" "$HEAD2" 2>&1)
if printf '%s' "$OUT2" | grep -q 'PUSH BLOCKED'; then
  echo "  ok   a real deletion is still refused"
else
  fail "a real deletion was not refused — the deletion half regressed"
fi
if printf '%s' "$OUT2" | grep -q 'AN IN-PLACE EDIT'; then
  fail "a real DELETION was excused as an in-place edit — this is the direction that loses work"
  printf '%s\n' "$OUT2" | head -20 | sed 's/^/       /'
else
  echo "  ok   a real deletion is NOT excused as an edit (the control that makes case 1 mean something)"
fi

if [ "$FAILS" -eq 0 ]; then echo "ok: in-place-edit cause — all 4 checks pass"; else echo "$FAILS check(s) failed" >&2; fi
exit $((FAILS > 0))
