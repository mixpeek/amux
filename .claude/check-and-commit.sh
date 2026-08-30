#!/bin/bash
# PostToolUse hook: validate dashboard client JS after every edit.
#
# History: this hook used to ast.parse amux-server.py and node --check its inline
# <script> blocks. The Python server was removed 2026-08-09; the client now lives
# as real static files under crates/amux-dashboard/static/. Rust edits are NOT
# checked here — `cargo check` on every single Edit would outlast the hook timeout
# and stack up on the shared target dir; the builder + CI (rust.yml) own that gate,
# and .claude/rules/single-file.md tells sessions to run `cargo check` themselves.
set -euo pipefail

# The repo root is wherever THIS script lives (<repo>/.claude/check-and-commit.sh),
# never a hardcoded path. A hardcoded checkout path matches on exactly one machine;
# everywhere else the path comparison below fails and the script exits 0 — reporting
# success while checking nothing. That is ethos rule 7 ("can your check actually
# fail?") in its worst form, because the silence looks like a pass.
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
STATIC_REAL=$(python3 -c "import os, sys; print(os.path.realpath(sys.argv[1]))" "$REPO/crates/amux-dashboard/static")

# Read the edited path from hook input, resolved, so a symlinked checkout or a
# relative path still matches the directory we gate on.
FILE_PATH=$(cat | python3 -c "
import sys, json, os
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
p = d.get('tool_input', {}).get('file_path', '')
print(os.path.realpath(p) if p else '')
" 2>/dev/null || echo "")

# THE BASH CLI SHIPS ON SAVE, NOT ON COMMIT — so it needs a gate HERE or it has
# none that fires in time (AMUX-3464, 2026-08-26). ~/.local/bin/amux is a 26-byte
# SYMLINK to <repo>/amux, so an uncommitted edit is live to every session in the
# fleet the instant the file is written: no install step, no builder cycle, no CI.
# A comment containing an apostrophe was added inside a `python3 -c '...'` block,
# which closed the quote early and left bash parsing garbage; the CLI died at load
# with `syntax error near unexpected token ;;` for EVERY subcommand, fleet-wide,
# until a peer reported it over the HTTP API.
#
# The failure mode is what makes this worth a hook rather than a test: a parse
# error at LOAD means the CLI cannot print its own help, so a session that has
# only ever used `amux send` has no way to discover that POST /api/sessions/<n>/send
# exists. It is not degraded, it is mute. `bash -n` costs milliseconds and catches
# the whole class structurally, apostrophes included.
case "$FILE_PATH" in
  "$REPO"/amux)
    if ! ERR=$(bash -n "$FILE_PATH" 2>&1); then
      echo "BASH SYNTAX ERROR in amux — the CLI is a symlink target, so this is LIVE" >&2
      echo "to the whole fleet right now and every subcommand dies at load:" >&2
      echo "$ERR" >&2
      echo "Common cause: an apostrophe or single quote inside a python3 -c '...' block." >&2
      exit 2
    fi ;;
esac

# Shell artifacts whose ONLY gate is checks.yml get their paired CI suite run
# at edit time. Rust has the builder and cargo; dashboard JS has node --check
# below; but a hook script outside crates/ has no local gate at all, so its
# first red is post-push CI (AMUX-3494: the freshness hook's pathspec fix
# shipped, its fixture suite went red twice on origin/main, and the editor
# only learned the suite existed from the CI failure). Same rule as the JS
# gate: run the check the CI will run, where the edit happens. One line per
# pair; add the pair when a checks.yml suite bites the same way.
SUITE=""
case "$FILE_PATH" in
  "$REPO"/.claude/session-freshness.sh|"$REPO"/scripts/test-session-freshness.sh)
    SUITE="scripts/test-session-freshness.sh" ;;
esac
if [ -n "$SUITE" ]; then
  if ! (cd "$REPO" && "./$SUITE" >/tmp/amux-hook-suite.$$ 2>&1); then
    echo "CI suite $SUITE FAILS after this edit — checks.yml will go red on push:" >&2
    tail -20 "/tmp/amux-hook-suite.$$" >&2
    rm -f "/tmp/amux-hook-suite.$$"
    exit 2  # surface the failure to the editor now, not post-push
  fi
  rm -f "/tmp/amux-hook-suite.$$"
fi

# Only gate client JS under the dashboard's static dir.
case "$FILE_PATH" in
  "$STATIC_REAL"/*.js|"$STATIC_REAL"/*.mjs) ;;
  *) exit 0 ;;
esac

# node --check proves the script PARSES, not that every name it calls exists
# (the closePeek() lesson, ethos rule 7) — but a parse error is the failure mode
# that bricks the whole SPA for every client at once, so it blocks immediately.
if ! node --check "$FILE_PATH"; then
  echo "JS syntax error in $FILE_PATH (see above)" >&2
  exit 2  # blocks the action
fi

exit 0
