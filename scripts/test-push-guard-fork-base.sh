#!/usr/bin/env bash
# AF-234 — the append-only push guard must not accuse a FORK contributor of
# deleting work they never touched.
#
# The bug this pins: for a NEW branch the guard has no remote ref to compare
# against, so it uses the push remote's `main` TIP. That is right on the shared
# checkout, where `origin` IS the authoritative history. On a fork, `origin` is
# a MIRROR that lags upstream — 251 commits, for @tsukimiya on PR #157. Entries
# are ARCHIVED out of frustrations.md into frustrations-archive.md upstream, so
# a stale mirror still holds lines that legitimately MOVED, and a branch cut
# from `upstream/main` reads as DELETING all of them. Their branch touched one
# unrelated file and `git diff upstream/main -- frustrations.md` was empty.
#
# THREE CELLS, because the obvious fix is worse than the bug. Making the guard
# quiet on a fork, or comparing against a merge-base, would pass cell A
# perfectly and hollow the guard out — a merge-base is old exactly when the
# graft is old, which is the case the guard exists for. So B pins that a branch
# NOT descending from the authoritative tip is still refused, and C pins the
# shared-checkout path (no `upstream` remote) unchanged. A alone is theatre.
#
# Runs the SHIPPED guard against real throwaway repos, driven through its real
# stdin ref-line protocol — `--check <base> <head>` bypasses the base SELECTION
# that this fix changes, so a test using it could not fail against the old code.
#
# PUSH_GUARD_HOOK points the cells at a different copy, to confirm cell A
# actually fails against the code it was written for:
#   git show <sha>^:scripts/git-hooks/append-only-push-guard > /tmp/pre-guard
#   PUSH_GUARD_HOOK=/tmp/pre-guard bash scripts/test-push-guard-fork-base.sh
#
# Exit 0 = all pass, 1 = a failure.
set -uo pipefail
cd "$(dirname "$0")/.."
GUARD="${PUSH_GUARD_HOOK:-$(pwd)/scripts/git-hooks/append-only-push-guard}"
PASS=0; FAIL=0
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

ok()  { PASS=$((PASS+1)); echo "  ok   — $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL — $1"; }

q() { "$@" >/dev/null 2>&1; }

# ABORT ON A BROKEN FIXTURE, loudly. The first draft of this file expanded
# `q git init` into `git git init`, so no repo was ever created — and cell A
# reported PASS, because "the guard did not block" is equally true when there
# is nothing to guard. A fixture that never built and a fix that works produce
# the same green, which is the failure this whole file is about. Every setup
# step that other cells depend on is asserted, so a zero cannot pass for a pass.
must() {
  local what="$1"; shift
  if ! "$@" >/dev/null 2>&1; then
    echo "  SETUP FAILED — $what (the cells below would report vacuous passes)" >&2
    exit 1
  fi
}
have_commit() {
  [ -n "$(git rev-parse --verify --quiet HEAD 2>/dev/null)" ]
}

# An entry in the shape the guard counts (it keys on real entry lines).
entry() { printf '\n## %s\nAREA: cli\nSTATUS: open\nCARD: X-%s\nCOST: minutes\n' "$1" "$1"; }

# --- build an UPSTREAM repo whose frustrations.md has been archived ---------
UP="$TMP/upstream"
mkdir -p "$UP" && cd "$UP"
must "git init in the upstream fixture" git init -b main
q git config user.email t@t; q git config user.name t
{ echo "# frustrations"; entry one; entry two; entry three; } > frustrations.md
echo "# archive" > frustrations-archive.md
echo "seed" > unrelated.txt
must "stage the upstream seed" git add -A
must "commit the upstream seed" git -c core.hooksPath=/dev/null commit -m seed
must "the upstream fixture has a commit" have_commit
MIRROR_POINT=$(git rev-parse HEAD)   # the fork's stale mirror sits here

# Upstream ARCHIVES two entries: they move OUT of frustrations.md and INTO the
# archive. Nothing is lost; a set-difference over one file cannot see that.
{ echo "# frustrations"; entry three; } > frustrations.md
{ echo "# archive"; entry one; entry two; } > frustrations-archive.md
must "stage the archive move" git add -A
must "commit the archive move" git -c core.hooksPath=/dev/null commit -m "archive one+two"
UPSTREAM_TIP=$(git rev-parse HEAD)
[ "$UPSTREAM_TIP" != "$MIRROR_POINT" ] || { echo "  SETUP FAILED — the archive commit did not move HEAD" >&2; exit 1; }

# --- the FORK: origin is a mirror pinned at the pre-archive commit ----------
FORK="$TMP/fork"
must "clone the fork" git clone "$UP" "$FORK"
cd "$FORK"
q git config user.email c@c; q git config user.name c
must "add the upstream remote" git remote add upstream "$UP"
must "fetch upstream/main" git fetch upstream main
must "upstream/main is present in the fork" git rev-parse --verify --quiet refs/remotes/upstream/main
# Pin origin/main to the STALE mirror point — this is what a fork that has not
# synced looks like from inside.
must "pin origin/main to the stale mirror point" git update-ref refs/remotes/origin/main "$MIRROR_POINT"

echo "AF-234 — fork base selection"

# --- CELL A: the reported case. Branch is current with upstream, touches one
#     unrelated file, and must NOT be accused of deleting the archived lines.
must "branch from upstream tip" git checkout -b feature "$UPSTREAM_TIP"
echo "a test change" >> unrelated.txt
must "stage the one-file change" git add -A
must "commit the one-file change" git -c core.hooksPath=/dev/null commit -m "one-file test change"
# The premise of this cell, asserted rather than assumed: the branch must not
# touch the guarded file at all. If it did, a pass would mean nothing.
[ -z "$(git diff "$UPSTREAM_TIP" --name-only -- frustrations.md)" ] \
  || { echo "  SETUP FAILED — cell A branch touched frustrations.md" >&2; exit 1; }
HEAD_A=$(git rev-parse HEAD)
ZERO=0000000000000000000000000000000000000000
if printf 'refs/heads/feature %s refs/heads/feature %s\n' "$HEAD_A" "$ZERO" \
     | "$GUARD" origin >"$TMP/a.out" 2>"$TMP/a.err"; then
  ok "A: a fork branch current with upstream/main is not blocked"
else
  bad "A: fork branch BLOCKED — the AF-234 false positive is live"
  sed 's/^/        /' "$TMP/a.err" | head -8
fi

# --- CELL B: the dodge must STILL be caught. Same fork, but the branch does
#     NOT descend from upstream/main and genuinely drops lines relative to the
#     base the guard falls back to. If the descent test were missing (or the
#     fix reached for a merge-base), this would sail through.
must "branch from the stale mirror point" git checkout -b graft "$MIRROR_POINT"
{ echo "# frustrations"; entry one; } > frustrations.md   # drops two + three
must "stage the stale republish" git add -A
must "commit the stale republish" git -c core.hooksPath=/dev/null commit -m "stale republish"
HEAD_B=$(git rev-parse HEAD)
# The premise of cell B: this branch must NOT descend from upstream/main, or it
# is testing cell A again under a different name.
! git merge-base --is-ancestor "$UPSTREAM_TIP" "$HEAD_B" 2>/dev/null \
  || { echo "  SETUP FAILED — cell B branch descends from upstream tip" >&2; exit 1; }
if printf 'refs/heads/graft %s refs/heads/graft %s\n' "$HEAD_B" "$ZERO" \
     | "$GUARD" origin >/dev/null 2>"$TMP/b.err"; then
  bad "B: a branch NOT descending from upstream/main dropped lines and was ALLOWED"
else
  ok "B: a non-descending branch that drops lines is still refused"
fi

# --- CELL C: the shared checkout is untouched. No `upstream` remote at all,
#     so the new code path must not fire and the old behaviour must stand.
SHARED="$TMP/shared"
must "clone the shared checkout fixture" git clone "$UP" "$SHARED"
cd "$SHARED"
q git config user.email s@s; q git config user.name s
must "branch the shared lane" git checkout -b lane "$UPSTREAM_TIP"
# The premise of cell C: no `upstream` remote, so the new path cannot fire.
[ -z "$(git rev-parse --verify --quiet refs/remotes/upstream/main || true)" ] \
  || { echo "  SETUP FAILED — cell C fixture has an upstream remote" >&2; exit 1; }
{ echo "# frustrations"; } > frustrations.md              # drops entry three
must "stage the shared stale republish" git add -A
must "commit the shared stale republish" git -c core.hooksPath=/dev/null commit -m "stale republish"
HEAD_C=$(git rev-parse HEAD)
if printf 'refs/heads/lane %s refs/heads/lane %s\n' "$HEAD_C" "$ZERO" \
     | "$GUARD" origin >/dev/null 2>"$TMP/c.err"; then
  bad "C: shared checkout (no upstream remote) allowed a stale republish"
else
  ok "C: shared checkout behaviour unchanged — stale republish still refused"
fi

# --- CELL D: the refusal must NAME the fork case, or a contributor is left
#     choosing between an untrue override and giving up (the ethos rule 3 half
#     of this card). Asserted on the text a blocked contributor actually reads.
if grep -q "STALE BASE (you are on a FORK)" "$TMP/c.err" \
   && grep -q "git diff upstream/main" "$TMP/c.err"; then
  ok "D: the refusal documents the fork case and how to check it"
else
  bad "D: the refusal does not name the fork case as a third state"
fi

# --- CELL E: the union rule must SEE a hyphen-separated archive.
#
# Found while writing this file (AF-234 follow-up). `archive_for` probed only
# `<stem>_ARCHIVE.md` / `<stem>_archive.md`; this repo's archive is
# `frustrations-archive.md` with a HYPHEN, so the lookup returned "none" and
# the union rule — the entire mechanism for not reporting a MOVE as a deletion
# — had never fired here. 30 archived entries were invisible to it, and the
# refusal text stated the consequence as a fact about the repo ("this repo has
# none") which was false the whole time.
#
# This is the SAME class as the fork bug above and as CD-78's correction on
# AMUX-3367: a set-difference over one file cannot see a MOVE. Three subsystems
# now.
q git checkout -b archiver "$UPSTREAM_TIP"
{ echo "# frustrations"; } > frustrations.md                        # three moves out
{ echo "# archive"; entry one; entry two; entry three; } > frustrations-archive.md
must "stage the archive move" git add -A
must "commit the archive move" git -c core.hooksPath=/dev/null commit -m "archive three"
HEAD_E=$(git rev-parse HEAD)
# Premise: the entry really must still exist, in the companion file.
grep -q '^## three$' frustrations-archive.md \
  || { echo "  SETUP FAILED — cell E did not move the entry into the archive" >&2; exit 1; }
if printf 'refs/heads/archiver %s refs/heads/archiver %s\n' "$HEAD_E" "$ZERO" \
     | "$GUARD" origin >/dev/null 2>"$TMP/e.err"; then
  ok "E: a MOVE into a hyphen-separated archive is not counted as a deletion"
else
  bad "E: archive_for cannot see <stem>-archive.md — a move reads as a deletion"
fi

# --- CELL F: and a REAL deletion must still be refused, or E was bought by
#     making the guard blind rather than by making it see the archive.
q git checkout -b deleter "$UPSTREAM_TIP"
{ echo "# frustrations"; } > frustrations.md          # entry three vanishes entirely
must "stage the real deletion" git add -A
must "commit the real deletion" git -c core.hooksPath=/dev/null commit -m "delete three"
HEAD_F=$(git rev-parse HEAD)
# Premise: the entry must exist in NEITHER file, or this is cell E again.
! grep -q '^## three$' frustrations-archive.md \
  || { echo "  SETUP FAILED — cell F's entry is still in the archive" >&2; exit 1; }
if printf 'refs/heads/deleter %s refs/heads/deleter %s\n' "$HEAD_F" "$ZERO" \
     | "$GUARD" origin >/dev/null 2>"$TMP/f.err"; then
  bad "F: a line deleted from BOTH files was allowed — the union rule is blind, not seeing"
else
  ok "F: a line that exists nowhere is still refused"
fi

echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
