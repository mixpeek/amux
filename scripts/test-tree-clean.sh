#!/usr/bin/env bash
# AF-150 (AREA silent-partial). Assert that running a command leaves the CHECKOUT
# exactly as it found it — no residue at any path git would report.
#
# Why this exists. A test fixture called create_dir_all on a literal relative path
# ("some/relative/path-that-does-not-exist") to prove the code REFUSES it. The
# refusal was later removed, the mutant ran, and the directory it made outlived the
# revert. From then on is_dir() short-circuited to Ok and the test was red on green
# code, on every machine with history, for everyone sharing this checkout (67137cc).
#
# THE GUARD IS AT THE RIGHT LAYER AND CI IS THE RIGHT PLACE, which is the opposite of
# what 67137cc concluded ("CI never sees this class (fresh checkout)"). A fresh
# checkout is where this is EASIEST to see: the residue has no history to hide in, so
# the test's own run is the only thing that could have made it. The per-fixture fix
# ("clean your own residue") only ever covers the fixture somebody already noticed.
#
# WHAT IT CAN SEE, and this is measured rather than assumed:
#   - `git status --porcelain`  -> modified/deleted tracked files, untracked FILES
#   - `git clean -nd`           -> untracked DIRECTORIES, INCLUDING EMPTY ONES
# Both are required and neither is redundant. git does not track empty directories,
# so `git status --porcelain` reports ZERO LINES for the AF-150 residue above --
# verified, as is `-uall`, which is also blind to it. A guard built on git status
# alone is exactly the check ethos rule 7 warns about: green, plausible, and unable
# to fail on the incident it was written for. `git clean -nd` sees it; `git clean`
# sees no modification to a tracked file. Hence the union.
#
# WHERE IT IS SOUND, and this is a real limit rather than a caveat. The guard
# attributes every before/after difference to the wrapped command. That is TRUE on a
# fresh single-tenant checkout (CI) and FALSE on this shared one, where ~50 lanes edit
# the same working tree concurrently. Measured on the first baseline run: the diff came
# back naming ` M crates/amux-server/src/api/alerts.rs`, which the test suite never
# touched -- a peer had edited it mid-run. So run it locally to reproduce a specific
# suspicion, and gate on it only where the checkout has one writer. This is the second
# reason CI is the right home for it, independent of the first.
#
# WHAT IT CANNOT SEE, on purpose: anything gitignored. cargo writes to target/ on
# every run, so including ignored paths would make the gate pure noise. The scope is
# residue at a path git would report -- which is the AF-150 shape, a fixture path
# inside the source tree.
#
# Usage:  scripts/test-tree-clean.sh <command...>     # wrap the thing under test
#         exit 3 = command succeeded but left residue (see the contract below)
#         scripts/test-tree-clean.sh --self-test      # prove the guard CAN fail
#
# It WRAPS rather than runs after, so the gate cannot be separated from the thing it
# guards, and it propagates the wrapped command's exit code -- a wrapper that
# swallowed a red test to report a clean tree would be its own silent-partial.

set -uo pipefail

REPO="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"

snapshot() {
  {
    git -C "$REPO" status --porcelain
    git -C "$REPO" clean -nd
  } 2>/dev/null | LC_ALL=C sort
}

# EXIT CODE CONTRACT -- read this before wrapping the script in anything.
#   0            the command succeeded AND left no residue
#   3            the command SUCCEEDED but left residue (RESIDUE_RC below)
#   <cmd rc>     the command itself failed; propagated unchanged
#
# Residue gets its own code because of a defect amux found in the first version,
# which was this file's own subject matter turned on itself. v1 returned 1 for
# residue and the wrapped command's rc otherwise -- both non-zero, indistinguishable
# at the CI step level. The plan was to land the guard `continue-on-error: true` for a
# trial period, on the sound reasoning that a detector never observed SILENT on healthy
# runs should not gate the whole fleet's pushes. With v1's contract that step would have
# made a REAL `cargo test --workspace` FAILURE non-blocking too: an advisory wrapper
# silently disarming the gate it wrapped, taking its signal from one part of a compound
# operation. Caught by reading the contract, before it ran.
#
# A FAILING COMMAND ALWAYS WINS over residue, so an advisory 3 can never mask a red
# test. The residue diff still prints in that case. This also removes the ambiguity of a
# command whose own exit code is 3: 3 means residue ONLY when the command exited 0, so a
# genuine rc=3 always propagates as itself.
RESIDUE_RC=3

run_guarded() {
  local before after
  before="$(snapshot)"
  "$@"
  local rc=$?
  after="$(snapshot)"

  if [ "$before" != "$after" ]; then
    echo ""
    echo "RESIDUE: the checkout changed while running: $*"
    echo "      Residue at paths git reports. On a shared checkout this outlives the"
    echo "      run and poisons every later one, while a fresh CI checkout stays green."
    echo ""
    diff <(printf '%s\n' "$before") <(printf '%s\n' "$after") | sed 's/^/      /'
    echo ""
    echo "      Fix the FIXTURE (write under a tempdir, or clear its own residue"
    echo "      before it runs) -- do not add the path to .gitignore, which only"
    echo "      makes the next occurrence invisible to this gate too."
    if [ "$rc" -ne 0 ]; then
      echo "      NOTE: the wrapped command ALSO failed (rc=$rc). Propagating that rc,"
      echo "      not the residue code, so a red test is never downgraded to advisory."
      return $rc
    fi
    return $RESIDUE_RC
  fi
  return $rc
}

self_test() {
  # Negative control (ethos rule 7): a fixture I built myself is a CLAIM that it is
  # broken, not a premise. This runs the guard around a command that leaves an EMPTY
  # directory -- the AF-150 shape, the one `git status` cannot see -- and fails if the
  # guard reports success.
  local probe="$REPO/.tree-clean-selftest-residue/nested/leaf"
  trap 'rm -rf "$REPO/.tree-clean-selftest-residue"' EXIT
  rm -rf "$REPO/.tree-clean-selftest-residue"

  run_guarded mkdir -p "$probe" >/dev/null 2>&1
  local rc=$?
  if [ "$rc" -ne "$RESIDUE_RC" ]; then
    echo "SELF-TEST FAIL: expected $RESIDUE_RC after a command created $probe, got $rc"
    echo "                An empty untracked directory is the AF-150 residue shape."
    return 1
  fi
  rm -rf "$REPO/.tree-clean-selftest-residue"

  # Positive control: the guard must NOT fire on a command that touches nothing,
  # or it is an unbounded match that would fail every build for free.
  run_guarded true >/dev/null 2>&1
  rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "SELF-TEST FAIL: the guard returned $rc on a no-op -- it matches everything."
    return 1
  fi

  # Contract control, and the one that would have caught v1. A command that BOTH fails
  # and leaves residue must propagate the command's own rc, never the advisory residue
  # code -- otherwise a CI step treating 3 as advisory disarms the gate it wraps.
  # Asserting merely "non-zero" here is what let v1 pass its own self-test, which is the
  # AF-150 shape one level up: a check that cannot see the distinction it exists to make.
  rm -rf "$REPO/.tree-clean-selftest-residue"
  run_guarded bash -c 'mkdir -p "$1"; exit 42' _ "$probe" >/dev/null 2>&1
  rc=$?
  if [ "$rc" -ne 42 ]; then
    echo "SELF-TEST FAIL: a command that failed (42) AND left residue returned $rc."
    echo "                A real failure must never be downgraded to the residue code."
    return 1
  fi
  rm -rf "$REPO/.tree-clean-selftest-residue"

  echo "self-test ok: residue=$RESIDUE_RC on a clean-exit residue, 0 on a no-op, and a"
  echo "failing command's own rc (42) wins over residue"
  return 0
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

if [ $# -eq 0 ]; then
  echo "usage: $0 <command...>   |   $0 --self-test" >&2
  exit 2
fi

run_guarded "$@"
exit $?
