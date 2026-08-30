#!/usr/bin/env bash
# AMUX-3682. A hook must install from the COMMITTED blob, never the worktree.
#
# Both hooks' comments in install.sh said "installed from the repo, so the
# committed copy is authoritative" and then copied `$SCRIPT_DIR/...` — the
# working tree. On a shared checkout somebody is nearly always mid-edit, so
# `./install.sh` shipped whatever uncommitted bytes were on disk to the fleet.
#
# Measured 2026-08-24, two incidents on the same arc:
#   hooks.report_hook_matches_committed   08-20 06:18 -> 08-24 16:25  4d10h
#   hooks.shared_guard_matches_committed  08-20 06:51 -> 08-24 12:36  4d05h
# 32 minutes apart, four days each, both resolved only when a DIFFERENT lane
# committed that file for an unrelated reason.
#
# THE DIRTY-WORKTREE CELL IS THE ONE THAT MATTERS. A clean checkout installs the
# same bytes either way, so a test that only covers it passes against the OLD
# code and proves nothing — that is the wrong-layer trap, and it is the easy
# test to write. The cell below dirties the file first, which is the state both
# incidents were in.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

pass=0; fail=0
ok()  { printf '  ok   — %s\n' "$1"; pass=$((pass+1)); }
bad() { printf '  FAIL — %s\n' "$1"; fail=$((fail+1)); }

# Exercise the REAL function out of install.sh rather than a copy of it. Sourcing
# the whole installer would build and install amux, so the function is extracted
# by name — if it is renamed or deleted this fails loudly instead of silently
# testing nothing.
FN="$(awk '/^install_hook_from_head\(\) \{/,/^\}/' "$ROOT/install.sh")"
if [[ -z "$FN" ]]; then
  echo "hook-install: FAIL — install_hook_from_head() not found in install.sh"
  echo "  The installer must install hooks from HEAD (AMUX-3682); if the function"
  echo "  was renamed, update this test rather than deleting the property."
  exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
REPO="$TMP/repo"; mkdir -p "$REPO/scripts/hooks"
git -C "$REPO" init -q
git -C "$REPO" config user.email t@t; git -C "$REPO" config user.name t
printf 'committed\n' > "$REPO/scripts/hooks/h.sh"
git -C "$REPO" add -A >/dev/null; git -C "$REPO" commit -qm "hook"

run_install() {
  SCRIPT_DIR="$REPO" bash -c "
    $FN
    install_hook_from_head scripts/hooks/h.sh '$1'
  " 2>&1
}

# THE SPECIMEN: worktree dirty, exactly as both incidents were.
printf 'HAND EDIT nobody committed\n' > "$REPO/scripts/hooks/h.sh"
out="$(run_install "$TMP/out-dirty")"
got="$(cat "$TMP/out-dirty" 2>/dev/null)"
if [[ "$got" == "committed" ]]; then
  ok "a dirty worktree still installs the COMMITTED bytes"
else
  bad "installed the worktree copy: got '$got', want 'committed'"
fi
# NOT SILENTLY: a lane that just edited the hook must be told their edit is not
# live, or they debug a hook that is not the one running.
if grep -q "differs from HEAD" <<<"$out"; then
  ok "and says so, so the editor is not left believing their edit shipped"
else
  bad "substituted the committed bytes silently: $out"
fi

# CONTROL: clean worktree installs the same bytes and says nothing extra.
git -C "$REPO" checkout -q -- scripts/hooks/h.sh
out="$(run_install "$TMP/out-clean")"
[[ "$(cat "$TMP/out-clean")" == "committed" ]] \
  && ok "a clean worktree installs the committed bytes" \
  || bad "clean worktree produced '$(cat "$TMP/out-clean")'"
grep -q "differs from HEAD" <<<"$out" \
  && bad "warned about drift on a CLEAN worktree — that warning would become noise" \
  || ok "no drift warning when there is no drift"

# CONTROL: outside a git checkout (tarball, container image) it must still
# install, loudly. Refusing there would be worse than installing.
NOGIT="$TMP/nogit"; mkdir -p "$NOGIT/scripts/hooks"
printf 'tarball\n' > "$NOGIT/scripts/hooks/h.sh"
out="$(SCRIPT_DIR="$NOGIT" bash -c "$FN; install_hook_from_head scripts/hooks/h.sh '$TMP/out-nogit'" 2>&1)"
if [[ "$(cat "$TMP/out-nogit" 2>/dev/null)" == "tarball" ]] && grep -q "not a git checkout" <<<"$out"; then
  ok "falls back to the file outside a checkout, and names the degradation"
else
  bad "no-git fallback: file='$(cat "$TMP/out-nogit" 2>/dev/null)' out='$out'"
fi

printf 'hook-install: %d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
