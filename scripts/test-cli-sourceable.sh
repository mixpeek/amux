#!/usr/bin/env bash
# shellcheck disable=SC1090,SC1091
# AF-928. amux's own comment claims tests can `source amux` to reach helper
# functions without CLI side effects. That was false until now: the guard
# skipped dispatch as documented, but an unconditional `exit "$?"` two lines
# after it still killed the sourcing shell regardless. Pins the actual
# capability, not just its absence of a crash.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLI="$ROOT/amux"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fails=0
fail() { echo "FAIL: $*" >&2; fails=$((fails + 1)); }
pass() { echo "  ok: $*"; }

# ── A: sourcing survives, and code AFTER the source line actually runs ──────
# The load-bearing assertion. A version of this file with the old shape
# (unconditional trailing exit) makes bash exit "$out" empty and $rc=0 from
# the `exit`'s own status, which reads deceptively like a pass if this only
# checked $rc -- so it must assert the POST-source line's own output landed.
export HOME="$TMP/home1"
mkdir -p "$HOME"
if out="$(source "$CLI" 2>&1; echo "REACHED_AFTER_SOURCE")"; then
  if [[ "$out" == *"REACHED_AFTER_SOURCE"* ]]; then
    pass "A: code after 'source amux' actually runs"
  else
    fail "A: sourcing succeeded (rc=0) but nothing after it ran -- output: $out"
  fi
else
  fail "A: sourcing amux exited non-zero"
fi

# ── B: a real helper function defined LATE in the file (right before the ───
# guard) is callable after sourcing -- not just "sourcing didn't crash".
export HOME="$TMP/home2"
mkdir -p "$HOME"
if out="$(source "$CLI" >/dev/null 2>&1; type -t _warn_stale_amux_url)"; then
  if [[ "$out" == "function" ]]; then
    pass "B: _warn_stale_amux_url (defined right before the guard) is callable post-source"
  else
    fail "B: _warn_stale_amux_url is not a function after sourcing: '$out'"
  fi
else
  fail "B: sourcing amux exited non-zero"
fi

# ── C: an EARLY helper function (defined hundreds of lines up) is ALSO ─────
# callable -- the guard must not accidentally sit ABOVE some function defs.
export HOME="$TMP/home3"
mkdir -p "$HOME"
if out="$(source "$CLI" >/dev/null 2>&1; type -t cmd_start)"; then
  if [[ "$out" == "function" ]]; then
    pass "C: cmd_start (defined far above the guard) is callable post-source"
  else
    fail "C: cmd_start is not a function after sourcing: '$out'"
  fi
else
  fail "C: sourcing amux exited non-zero"
fi

# ── D: direct execution is completely unaffected -- same dispatch, same ────
# exit codes, as a plain run.
export HOME="$TMP/home4"
mkdir -p "$HOME"
out="$(bash "$CLI" version 2>&1)"; rc=$?
# The version is the checkout's `git describe`: `v...` with a release tag, a
# bare sha on CI's tagless checkout, or "(version unknown)" outside a checkout.
if [[ "$rc" -eq 0 ]] && [[ "$out" =~ ^amux\ (v[0-9]|[0-9a-f]{7}|\(version\ unknown\)) ]]; then
  pass "D: direct execution ('amux version') still dispatches normally"
else
  fail "D: direct execution regressed -- rc=$rc out=$out"
fi

if out="$(bash "$CLI" totally-bogus-cmd 2>&1)"; then rc=0; else rc=$?; fi
if [[ "$rc" -eq 1 ]] && [[ "$out" == *"unknown command"* ]]; then
  pass "D: the unknown-command error path still exits 1"
else
  fail "D: unknown-command path regressed -- rc=$rc out=$out"
fi

# ── E: PRE-FIX behaviour (guard removed) must NOT survive sourcing -- the ──
# discriminating control. Same shape as test-install-hooks-worktree.sh's
# case B and lane-worktree-migrate's case D.
sed '/# AF-928.*actual check/,/^fi$/d' "$CLI" > "$TMP/prefix.sh"
if grep -q "AF-928's actual check" "$TMP/prefix.sh"; then
  fail "E: mutation did not remove the guard block -- the sed pattern did not match, E proves nothing"
elif ! diff -q "$CLI" "$TMP/prefix.sh" >/dev/null; then
  export HOME="$TMP/home5"
  mkdir -p "$HOME"
  if out="$(source "$TMP/prefix.sh" 2>&1; echo "REACHED_AFTER_SOURCE")"; then
    if [[ "$out" == *"REACHED_AFTER_SOURCE"* ]]; then
      fail "E: pre-fix file (guard removed) let code after 'source' run anyway -- E is not discriminating"
    else
      pass "E: pre-fix file (guard removed) kills the sourcing shell before 'REACHED_AFTER_SOURCE' -- confirms the fix is load-bearing"
    fi
  else
    pass "E: pre-fix file (guard removed) exits non-zero on sourcing -- confirms the fix is load-bearing"
  fi
else
  fail "E: the sed mutation did not change the file at all -- fix the sed pattern"
fi

if [[ "$fails" -eq 0 ]]; then
  echo "cli-sourceable suite: PASS"
else
  echo "cli-sourceable suite: FAIL ($fails)" >&2
fi
exit "$fails"
