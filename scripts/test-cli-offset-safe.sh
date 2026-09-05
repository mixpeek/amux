#!/usr/bin/env bash
# AMUX-3891: the bash CLI must be readable in one parse, because it is rewritten
# in place underneath running interpreters as a matter of routine.
#
# THE BUG THIS PINS. bash does not slurp a script. It reads and executes
# incrementally, holding an open fd and a byte offset. `~/.local/bin/amux` is a
# symlink straight into the working tree (deliberately — that liveness is the
# point), ~50 lanes invoke it constantly, and the file is edited constantly. Every
# editor write, `git checkout`, `git stash` and branch switch rewrites it in place
# while somebody is mid-invocation, and bash then resumes at an offset pointing
# into different bytes.
#
# Observed 2026-08-29 20:28 by mixpeek-homepage-claude: a `board retitle` whose
# write SUCCEEDED, then died with `line 3784: syntax error near unexpected token
# ';'` and exit 2. `bash -n` on the same path passed immediately after — the file
# was fine, the PARSE was not.
#
# The spurious exit is the benign outcome. A caller checking exit codes reads 2 as
# a failed write and retries, and `board retitle`/`status-update` are not
# idempotent for a reader. A shifted offset can also land mid-function and execute
# a fragment of a command, which is unbounded.
#
# THE SHAPE. Everything after the preamble lives inside one `{ ... }` compound
# command, and the last two statements are `exit "$?"` and `}`. bash must then
# parse to the closing brace before executing anything, and the trailing `exit`
# means it never seeks back to the file afterwards.
#
# WHY A SHAPE TEST AND NOT ONLY A RACE TEST. The race repro is real and was run
# (30 trials each: unwrapped 6 BROKEN / 24 clean, wrapped 0 BROKEN / 30 clean) but
# it is a RACE — a green race run is consistent with "the shape is gone and we got
# lucky", which is exactly the silent-green this repo keeps getting bitten by. The
# shape is the invariant; assert the invariant. The race proved the shape matters,
# once. This proves the shape is still there, every run.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLI="$ROOT/amux"
fails=0
fail() { echo "FAIL: $*" >&2; fails=$((fails + 1)); }

[[ -f "$CLI" ]] || { echo "FAIL: $CLI not found" >&2; exit 1; }

# 1. It still parses at all. A shape test that passes on a broken file is worse
#    than no shape test.
bash -n "$CLI" || fail "amux does not pass 'bash -n'"

# 2. The wrapper opens. Anchored to a line that is exactly `{` so a `{` inside
#    some function body cannot satisfy this.
if ! grep -qx '{' "$CLI"; then
  fail "no bare '{' line — the whole-script wrapper is gone, so bash will read this file incrementally again (AMUX-3891)"
fi

# 3. The last two non-blank statements are `exit "$?"` then `}`. This is the half
#    that stops bash seeking back to the file after the block completes; without
#    it the wrapper alone still leaves a window.
# No `mapfile` here: macOS ships bash 3.2 as /bin/bash and this file must run
# under whatever `env bash` resolves to on a contributor's machine, not only
# under a Homebrew bash 4+. A test that dies on the runner's shell reports a
# missing builtin as a broken CLI, which is what the first version of this did.
penult="$(grep -vE '^[[:space:]]*$' "$CLI" | tail -2 | head -1)"
last="$(grep -vE '^[[:space:]]*$' "$CLI" | tail -1)"
if [[ "$penult" != 'exit "$?"' ]]; then
  fail "second-to-last statement is '${penult:-<none>}', expected 'exit \"\$?\"' — without the trailing exit bash seeks back to the file after the block and can read shifted bytes (AMUX-3891)"
fi
if [[ "$last" != '}' ]]; then
  fail "last statement is '${last:-<none>}', expected '}' — a top-level command appended after the wrapper is read at a stale offset, which re-opens AMUX-3891 for whatever was appended"
fi

# 4. Vacuity guard. If the file were tiny or unreadable the checks above could all
#    pass against nothing meaningful.
lines="$(wc -l < "$CLI" | tr -d ' ')"
if (( lines < 500 )); then
  fail "amux is only $lines lines — this test's assumptions do not hold, so its greens are meaningless"
fi

if (( fails > 0 )); then
  echo "test-cli-offset-safe: $fails failure(s)" >&2
  exit 1
fi
echo "test-cli-offset-safe: PASS (wrapper present, trailing exit+brace intact, $lines lines)"
