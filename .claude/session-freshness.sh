#!/bin/bash
# SessionStart hook: say out loud, at the one moment it matters, whether this
# session is about to build on something stale.
#
# Two DIFFERENT staleness axes bit in one session on 2026-08-05, which is why
# this checks both:
#
#   1. The CHECKOUT was ~110 commits behind origin/main. Work got built on a
#      stale base; one fix turned out to duplicate a fix upstream already had,
#      and the rebase that followed conflicted twice.
#   2. The INSTALLED CLI (~/.local/bin/amux) was a Jul-31 copy missing the
#      `status-update` verb. It fell through to help and exited 0, so three of
#      the owner's status requests were silently swallowed (AMUX-2140 shape).
#
# Design constraints, each one deliberate:
#
#   * It FETCHES, never pulls. This is a shared checkout — CLAUDE.md records a
#     peer's `git pull --rebase` replaying another session's unpushed commit
#     onto origin. A hook that rewrites the working tree can destroy in-flight
#     work belonging to a session that is not even running right now. Report
#     and recommend; the human decides (ethos rule 8).
#   * It FAILS OPEN. Offline, no remote, detached HEAD, missing files — every
#     failure path exits 0 silently. A freshness check that blocks a session is
#     worse than the staleness it detects.
#   * It is SILENT when everything is current, so the one time it speaks is
#     signal rather than another banner to scroll past.
set -uo pipefail

[ "${AMUX_SKIP_FRESHNESS:-}" = "1" ] && exit 0

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." 2>/dev/null && pwd -P)" || exit 0
cd "$REPO" 2>/dev/null || exit 0
git rev-parse --git-dir >/dev/null 2>&1 || exit 0

out=""

# ── Axis 1: is the checkout behind its remote? ───────────────────────────────
# Bounded: a hook that hangs on a slow network is a hook that gets deleted.
if git remote get-url origin >/dev/null 2>&1; then
  timeout 10 git fetch -q origin 2>/dev/null
  base="$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || echo origin/main)"
  if git rev-parse --verify -q "$base" >/dev/null 2>&1; then
    behind="$(git rev-list --count "HEAD..$base" 2>/dev/null || echo 0)"
    if [ "${behind:-0}" -gt 0 ]; then
      # Name the files that actually matter here, not just a number: "110
      # commits behind" reads as bookkeeping, "amux-server.py changed upstream"
      # reads as "your edit is going to conflict".
      hot="$(git diff --name-only "HEAD..$base" 2>/dev/null \
             | grep -E '^(amux-server\.py|amux|CLAUDE\.md)$' | tr '\n' ' ')"
      out+="  - checkout is ${behind} commit(s) behind ${base}"
      [ -n "$hot" ] && out+=" — including: ${hot}"
      out+=$'\n'
      out+=$'    git pull --rebase origin main   (review first: this checkout is SHARED)\n'
    fi
  fi
fi

# ── Axis 2: does what is INSTALLED match this checkout? ──────────────────────
# The repo copy is the source; install.sh copies it. Editing the repo alone
# changes nothing that a session or the dashboard actually executes.
live_cli="$(command -v amux 2>/dev/null || true)"
if [ -n "$live_cli" ] && [ -f "$REPO/amux" ]; then
  if ! diff -q "$REPO/amux" "$live_cli" >/dev/null 2>&1; then
    out+="  - installed CLI differs from this checkout: ${live_cli}"$'\n'
    out+="    an unknown verb there may print help and exit 0 — a silent no-op"$'\n'
    out+="    cp \"$REPO/amux\" \"$live_cli\""$'\n'
  fi
fi

# Compare against the server that is actually RUNNING, resolved from the
# process itself rather than a hardcoded path — a hardcoded path matches on one
# machine and silently checks nothing everywhere else (ethos rule 7).
run_srv="$(ps -axo command= 2>/dev/null | grep -oE '[^ ]*/amux-server\.py' | head -1 || true)"
if [ -n "$run_srv" ] && [ -f "$run_srv" ] && [ -f "$REPO/amux-server.py" ]; then
  if ! diff -q "$REPO/amux-server.py" "$run_srv" >/dev/null 2>&1; then
    out+="  - the RUNNING server is not this checkout: ${run_srv}"$'\n'
    out+="    edits here are not live until installed + restarted"$'\n'
    out+="    cp \"$REPO/amux-server.py\" \"$run_srv\" && launchctl kickstart -k gui/\$(id -u)/com.amux.serve"$'\n'
  fi
fi

[ -z "$out" ] && exit 0

printf 'amux freshness — this session may be building on something stale:\n\n%s\n' "$out"
printf 'Reconcile before starting work, or say so in your first message if you are deliberately not.\n'
exit 0
