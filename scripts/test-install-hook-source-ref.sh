#!/usr/bin/env bash
# install_hook_from_head must install what the FLEET runs, not what this
# checkout's HEAD happens to be.
#
# AF-596. Measured 2026-09-08: ~/.amux/hooks/git-shared-guard.py had that day's
# mtime and was 145 lines behind the repo, missing two SHIPPED fixes —
# 09c26abb (`\b` after a literal verb matches a hyphen, so `commit-tree` read
# as `commit`) and a391c1c6 (AF-577, refuse a `git config` write from a linked
# worktree). The installer reads committed bytes, which is right, but took them
# from HEAD; graft-push never advances local HEAD, so HEAD lags origin by an
# unbounded amount while looking authoritative.
#
# Cost: the commit-tree false positive blocked the out-of-tree graft that
# mixpeek's own CLAUDE.md prescribes as THE safe pattern on a shared checkout,
# for every lane on this box, and it was reported as a guard defect when the
# guard had been fixed the day before. Nothing anywhere said the running hook
# was old.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pass=0; fail=0
ok()  { echo "  ok   $1"; pass=$((pass+1)); }
bad() { echo "  FAIL $1"; fail=$((fail+1)); }

T="$(mktemp -d "${TMPDIR:-/tmp}/testinstallref.XXXXXX")" || exit 2
trap 'rm -rf "$T"' EXIT
W="$T/work"
git init -q --bare "$T/origin.git"
git init -q "$W"
git -C "$W" config user.email t@t; git -C "$W" config user.name t
mkdir -p "$W/scripts/git-hooks"

printf 'OLD_VERSION\n' > "$W/scripts/git-hooks/hook.py"
git -C "$W" add -A; git -C "$W" commit -qm old
git -C "$W" remote add origin "$T/origin.git"; git -C "$W" push -q origin HEAD:main
OLD_SHA="$(git -C "$W" rev-parse HEAD)"

printf 'NEW_VERSION_WITH_FIX\n' > "$W/scripts/git-hooks/hook.py"
git -C "$W" add -A; git -C "$W" commit -qm new; git -C "$W" push -q origin HEAD:main

# THE SHAPE THAT MATTERS: local HEAD rewound behind origin/main, which is what
# a graft-push checkout looks like all the time.
git -C "$W" checkout -q "$OLD_SHA"
git -C "$W" fetch -q origin main

sed -n '/^install_hook_from_head()/,/^}/p' "$ROOT/install.sh" > "$T/fn.sh"
if [ ! -s "$T/fn.sh" ]; then
  bad "install_hook_from_head not found in install.sh — did it get renamed?"
  echo "  $pass passed, $fail failed"; exit 1
fi
SCRIPT_DIR="$W"; . "$T/fn.sh"
install_hook_from_head scripts/git-hooks/hook.py "$T/installed" >"$T/out" 2>&1

if grep -q NEW_VERSION_WITH_FIX "$T/installed"; then
  ok "installs origin/main's bytes when local HEAD lags"
else
  bad "installed the lagging HEAD version: $(cat "$T/installed")"
fi

# It must SAY which ref it used. An install that silently picks a source is how
# this went unnoticed for a day.
if grep -q "from origin/main" "$T/out"; then
  ok "names the ref it installed from"
else
  bad "install is silent about its source ref"
fi

# THE CONTROL. With no origin at all it must still install, from HEAD, and say
# so — otherwise this "fix" just breaks every non-shared checkout.
W2="$T/solo"
git init -q "$W2"; git -C "$W2" config user.email t@t; git -C "$W2" config user.name t
mkdir -p "$W2/scripts/git-hooks"; printf 'SOLO\n' > "$W2/scripts/git-hooks/hook.py"
git -C "$W2" add -A; git -C "$W2" commit -qm solo
SCRIPT_DIR="$W2"
install_hook_from_head scripts/git-hooks/hook.py "$T/installed2" >"$T/out2" 2>&1
if grep -q SOLO "$T/installed2" && grep -q "from HEAD" "$T/out2"; then
  ok "falls back to HEAD with no origin, and says so"
else
  bad "no-origin checkout broke: $(cat "$T/out2")"
fi

echo "  $pass passed, $fail failed"
[ "$fail" -eq 0 ]
