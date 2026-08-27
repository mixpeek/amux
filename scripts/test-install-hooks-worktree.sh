#!/bin/bash
# scripts/install-hooks.sh must work FROM A LINKED WORKTREE, and nothing tested it.
#
# CLAUDE.md tells every session to develop in a worktree and never in the checkout
# the builder watches. install-hooks.sh resolved its destination as
# "$ROOT/.git/hooks" — and in a linked worktree `.git` is a FILE, not a directory,
# so every install died with
#
#     install: cannot stat '<worktree>/.git/hooks/pre-commit': Not a directory
#
# AFTER the script had already printed its first `ok` line. So the sanctioned way
# to work was also the one way you could not install the secret scan, the
# staged-guard, the push guard or the Amux-Session stamp — and the half-run reads
# as having started fine.
#
# The script already knew the shape: its --all sweep skips linked worktrees BY
# NAME, and its own WARN tells OTHER hooks to resolve guards with
# `git rev-parse --git-path hooks`. Only its own install path still spelled the
# path by hand, which is why a test belongs here rather than a comment.
#
# CASE B IS THE LOAD-BEARING HALF. A test that only asserts "installing works"
# passes against the pre-fix script the moment it is run from a NORMAL checkout,
# which is how this went unnoticed: the defect is invisible unless the fixture is
# a worktree. So B rebuilds the pre-fix behaviour (the hardcoded path) and asserts
# it FAILS on the same fixture. Confirmed before wiring in: with B's mutation
# applied to the shipped script, A fails and B passes.
set -uo pipefail

SRC_REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail=0

note() { printf '  %s\n' "$*"; }
ok()   { printf '  ok   %s\n' "$*"; }
bad()  { printf '  FAIL %s\n' "$*" >&2; fail=1; }

# ── the fixture: a real repo with a real linked worktree ─────────────────────
# A COPY, not the amux checkout itself: this test must not install into the tree
# it is run from (a test with a side effect on the developer's hooks is one
# nobody runs twice), and mktemp gives us a `.git` we can safely inspect.
repo="$TMP/origin"
mkdir -p "$repo/scripts"
cp -p "$SRC_REPO/scripts/install-hooks.sh" "$repo/scripts/"
cp -pr "$SRC_REPO/scripts/git-hooks" "$repo/scripts/"
git -C "$repo" init -q -b main
git -C "$repo" config user.email t@example.com
git -C "$repo" config user.name  t
git -C "$repo" add -A
# `-c core.hooksPath=/dev/null` so the fixture's OWN commit does not run the
# hooks under test — the fixture must not depend on what it is checking.
git -C "$repo" -c core.hooksPath=/dev/null commit -qm "fixture"

# check_tracked_guard_mode asserts the guard is tracked 100755. If the copy lost
# the bit, this test would report the SCRIPT as broken when the fixture is.
mode=$(git -C "$repo" ls-files -s scripts/git-hooks/amux-staged-guard | awk '{print $1}')
if [ "$mode" != "100755" ]; then
  bad "fixture lost the guard's exec bit (tracked $mode) — fix the fixture, not the script"
  exit 1
fi

wt="$TMP/wt"
git -C "$repo" worktree add -q -b feature "$wt"
# The precondition the whole defect rests on. If this ever stops being true the
# test is measuring nothing, so assert it rather than assume it.
#
# TEST FOR THE PROPERTY, NOT FOR ITS NEGATION (Copilot, #158). This was written
# as `[ -d "$wt/.git" ] && fail`, which is a DIFFERENT claim from the `ok` line
# it guards: a `.git` that is missing entirely — a fixture whose `worktree add`
# half-failed, or a path typo — is also "not a directory", so the check printed
# "whose .git is a file" about a path with nothing at it, and every case below
# would then fail for a reason that has nothing to do with the defect. Require
# the regular file the linked-worktree shape actually produces, and name what
# was found instead, so a broken fixture reads as a broken fixture.
if [ ! -f "$wt/.git" ]; then
  bad "fixture worktree's .git is not a regular FILE — precondition gone, this test is vacuous"
  note "  found instead: $(ls -ld "$wt/.git" 2>&1)"
  exit 1
fi
ok "fixture: linked worktree whose .git is a file"

common_hooks="$(git -C "$wt" rev-parse --git-path hooks)"
case "$common_hooks" in /*) ;; *) common_hooks="$wt/$common_hooks" ;; esac

# ── A: the shipped script installs when run FROM the worktree ────────────────
out="$(cd "$wt" && bash scripts/install-hooks.sh 2>&1)"; rc=$?
if [ "$rc" -ne 0 ]; then
  bad "A: install-hooks.sh from a worktree exited $rc"
  printf '%s\n' "$out" | sed 's/^/       /' >&2
else
  ok "A: install-hooks.sh from a worktree exited 0"
fi

missing=""
for h in pre-commit pre-push prepare-commit-msg amux-staged-guard; do
  [ -x "$common_hooks/$h" ] || missing="$missing $h"
done
if [ -n "$missing" ]; then
  bad "A: hooks absent from $common_hooks:$missing"
else
  ok "A: all four hooks landed in the git COMMON dir, executable"
fi

# The destination is the COMMON dir, shared by the main checkout and every
# worktree — which is WHY the --all sweep may skip worktrees: one install covers
# them. Asserted so a future "fix" that installs into a per-worktree directory
# (silently giving each worktree its own copy to drift) fails here.
if [ -e "$wt/.git/hooks" ]; then
  bad "A: something created $wt/.git/hooks — the destination must be the common dir"
else
  ok "A: nothing was written under the worktree's own .git"
fi

# ── B: the PRE-FIX behaviour must fail on the same fixture ───────────────────
# Rebuilt from the incident rather than from a convenient case: the one line that
# resolved the destination by hand. Anything else about the script is unchanged,
# so a pass here would mean case A proves nothing about worktrees.
# INTO THE WORKTREE, not into $repo/scripts. The first cut wrote it to the main
# checkout and ran it from the worktree, so bash exited 127 "No such file or
# directory" — a FAILURE, which this case reads as success, so it reported a pass
# while measuring nothing. That is the loud-wrong probe: it answered, and the
# answer was the one being hoped for. Hence both the path and the assertion below.
sed 's|^HOOKS="\$(git -C .*|HOOKS="$ROOT/.git/hooks"|' \
  "$wt/scripts/install-hooks.sh" > "$wt/scripts/install-hooks-prefix.sh"
if ! grep -q 'HOOKS="\$ROOT/.git/hooks"' "$wt/scripts/install-hooks-prefix.sh"; then
  # An unapplied mutation and a working fix produce the same green. Check the
  # mutation LANDED before reading its result.
  bad "B: the mutation did not apply — B proves nothing, fix the sed"
else
  bout="$(cd "$wt" && bash scripts/install-hooks-prefix.sh 2>&1)"; brc=$?
  if [ "$brc" -eq 0 ]; then
    bad "B: the pre-fix path SUCCEEDED from a worktree — case A is not discriminating"
  elif printf '%s' "$bout" | grep -q 'Not a directory'; then
    ok "B: the pre-fix path still dies with 'Not a directory' (rc=$brc)"
  else
    # ANY OTHER FAILURE IS NOT EVIDENCE. A missing file, a syntax error or a
    # permission problem all exit non-zero and would let this case certify a
    # fixture that never exercised the defect.
    bad "B: pre-fix path failed (rc=$brc) but NOT with 'Not a directory' — B did not"
    bad "   exercise the defect; its own fixture is broken:"
    printf '%s\n' "$bout" | tail -3 | sed 's/^/       /' >&2
  fi
fi

[ "$fail" -eq 0 ] && echo "install-hooks worktree suite: PASS" || echo "install-hooks worktree suite: FAIL" >&2
exit "$fail"
