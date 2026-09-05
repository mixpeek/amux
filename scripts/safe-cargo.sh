#!/usr/bin/env bash
# Run cargo in its OWN systemd scope, isolated from whatever pane invoked it.
#
# Root cause this exists for (AMUX-70, frustrations.md 2026-09-01, confirmed
# live via journalctl + dmesg): every process in an interactive amux pane —
# including the Claude Code session itself — shares ONE systemd scope,
# `tmux-spawn-<uuid>.scope`. When a `cargo check`/`clippy`/`build`/`test` run
# directly in that pane gets OOM-killed, systemd does not just reap the
# offending process — it marks the WHOLE SCOPE `Failed with result
# 'oom-kill'`, and whatever supervises the pane tears it down and starts a
# brand-new one. The entire interactive session restarts mid-conversation,
# not just the build.
#
# `systemd-run --user --scope` gives the cargo invocation a SIBLING scope
# instead (verified: `run-p<pid>-i<id>.scope`, distinct from
# `tmux-spawn-*.scope`) — an OOM kill inside it can no longer cascade into
# the pane hosting the session.
#
# This does NOT replace remote offload (see CLAUDE.md / the offload-builds
# convention) — always prefer building on remote hardware for anything
# beyond a quick syntax check. Use this script only for the cases that
# genuinely need to run locally, so a local run is contained instead of
# risky by default.
#
# Usage: scripts/safe-cargo.sh <cargo subcommand and args...>
#   scripts/safe-cargo.sh check -p amux-server
#   scripts/safe-cargo.sh clippy -p amux-server --all-targets -- -D warnings
set -euo pipefail

# NO SYSTEMD AT ALL means the hazard above cannot happen (AMUX-4022).
#
# The whole reason this wrapper exists is that a cargo OOM inside the pane's
# `tmux-spawn-*.scope` makes systemd fail the WHOLE SCOPE and tear the session
# down. A machine that does not run systemd has no scope to fail, so there is
# nothing to isolate from and running cargo directly is the CORRECT behaviour
# rather than a compromise.
#
# This mattered: the unconditional refusal below took the macOS auto-builder
# down from the moment it shipped. `rust-auto-build.sh` builds through this
# script, so every release build failed with "systemd-run not found" and NO
# COMMIT FROM ANY LANE DEPLOYED — silently, because the builder's failure only
# shows up as /health's `commit` quietly not moving.
#
# `/run/systemd/system` is the canonical "is systemd the init system" test, so a
# Linux box that HAS systemd but is missing systemd-run still gets the refusal:
# that is a real misconfiguration and the original judgement about it stands.
# A `test` run writes a RECEIPT, and a receipt can only be written after the
# run — so this wrapper does not `exec` for `test`. Every other subcommand keeps
# exec: rust-auto-build.sh builds through this script, and an extra shell in the
# builder's process tree is a change nobody asked for.
#
# WHY THIS WRAPPER WRITES ONE AT ALL (AF-478). CLAUDE.md names two sanctioned
# local paths and they were in conflict: run tests with `test-contended.sh`, and
# put any local cargo run through this script. Only the first wrote a receipt,
# so following the safety instruction produced a commit whose pre-commit hook
# reported the bytes as untested and cited a run from twenty hours earlier.
# There was no sequence of sanctioned commands that made the hook right.
#
# `_TC_RECEIPT` is set by test-contended.sh, which writes its own receipt at the
# end of its run. Two identical receipts would be harmless and confusing.
_receipt=""
if [ "${1:-}" = "test" ] && [ -z "${_TC_RECEIPT:-}" ]; then
  _receipt="$(cd "$(dirname "$0")" && pwd)/write-test-receipt.sh"
  [ -x "$_receipt" ] || _receipt=""
fi

if [ -d /run/systemd/system ]; then
  if ! command -v systemd-run >/dev/null 2>&1; then
    echo "safe-cargo.sh: systemd-run not found on a systemd host — refusing to run cargo unisolated." \
         "Offload remotely instead, or run systemd-run --user --scope by hand." >&2
    exit 1
  fi
  CMD=(systemd-run --user --scope --quiet
       --working-directory="$(pwd)"
       --setenv=CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.amux/rust-build-target}"
       --setenv=PATH="$PATH"
       --setenv=HOME="$HOME"
       -- cargo "$@")
else
  echo "safe-cargo.sh: no systemd on this host — running cargo directly." \
       "There is no pane scope for an OOM to cascade into here." >&2
  CMD=(cargo "$@")
fi

if [ -z "$_receipt" ]; then
  exec "${CMD[@]}"
fi

rc=0
"${CMD[@]}" || rc=$?
"$_receipt" "$rc" "$@"
exit "$rc"
