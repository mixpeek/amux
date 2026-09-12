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

# A concurrency slot bounds invocations, not rustc's internal parallelism or
# libtest's threads. Cargo otherwise defaults to every CPU on the host, per
# invocation. Avoid full DWARF and incremental trees for routine fleet checks.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-2}"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"
export CARGO_PROFILE_TEST_DEBUG="${CARGO_PROFILE_TEST_DEBUG:-0}"

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

# --- CONCURRENCY THROTTLE (AMUX-4288, 2026-09-09) ---------------------------
# Confirmed live: enough lanes invoking this wrapper AT ONCE (each a full
# workspace check/clippy/test) pinned every core on this 4-thread box and ran
# it into swap -- load average hit 60-67. That was not just a slow build: it
# starved the LIVE production amux-server's DB read pool (every store.read()
# has to actually get scheduled on a CPU), turning a local build storm into
# `read_pool_exhausted` on the real running service. CLAUDE.md already tells
# every lane to run local cargo through this wrapper instead of bare cargo;
# this is the enforcement point, not a new rule anyone has to remember.
#
# mkdir-based lock, not `flock` -- this box (macOS) does not ship the flock(1)
# binary, and there is no portable way to hold a flock'd fd across the `exec`
# this script used to do unconditionally (bash 3.2 here has no `exec {fd}>`
# dynamic allocation either). So this wrapper no longer execs at all: it runs
# the guarded command as a child and releases the slot in a trap after it
# exits. One extra bash frame in the process tree is a smaller cost than the
# thing it prevents.
#
# STALE-SLOT RECLAIM: an mkdir lock does not release itself if its holder is
# SIGKILLed (an OOM kill is exactly the failure mode the rest of this script
# exists to contain) -- unlike flock, the kernel does not clean it up. Each
# slot records its holder's pid; a blocked acquirer that finds a dead pid
# reclaims the slot instead of queuing behind a lock nobody will ever release.
_throttle_max="${AMUX_CARGO_MAX_CONCURRENT:-2}"
case "$_throttle_max" in
  ''|*[!0-9]*|0) echo 'cargo_budget_refused: AMUX_CARGO_MAX_CONCURRENT must be positive' >&2; exit 75 ;;
esac
[ "$_throttle_max" -gt 0 ] || { echo 'cargo_budget_refused: concurrency must be positive' >&2; exit 75; }
_throttle_dir="$HOME/.amux/cargo-throttle"
mkdir -p "$_throttle_dir" 2>/dev/null || true
_throttle_slot=""
_throttle_announced=0
while [ -z "$_throttle_slot" ]; do
  _n=1
  while [ "$_n" -le "$_throttle_max" ]; do
    _cand="$_throttle_dir/slot-$_n"
    _pidfile="$_cand.pid"
    if mkdir "$_cand" 2>/dev/null; then
      _throttle_slot="$_cand"
      break
    elif [ -f "$_pidfile" ]; then
      _holder="$(cat "$_pidfile" 2>/dev/null || echo)"
      if [ -n "$_holder" ] && ! kill -0 "$_holder" 2>/dev/null; then
        rmdir "$_cand" 2>/dev/null || true
        rm -f "$_pidfile" 2>/dev/null || true
        if mkdir "$_cand" 2>/dev/null; then
          _throttle_slot="$_cand"
          break
        fi
      fi
    fi
    _n=$((_n + 1))
  done
  if [ -z "$_throttle_slot" ]; then
    if [ "$_throttle_announced" -eq 0 ]; then
      echo "safe-cargo.sh: $_throttle_max concurrent local cargo build(s) already running" \
           "through this wrapper -- waiting for a slot (AMUX-4288: unthrottled concurrent" \
           "builds drove this box's load average past 60 and starved the production DB" \
           "pool). Set AMUX_CARGO_MAX_CONCURRENT to change the cap." >&2
      _throttle_announced=1
    fi
    sleep 2
  fi
done
# The pid marker is a SIBLING file, not inside the slot dir: `rmdir` only
# removes EMPTY directories, so a pid file living inside it would make every
# release (and every stale-slot reclaim, which runs the identical rmdir) a
# silent no-op forever -- confirmed live in testing before this shipped, the
# exact bug this comment is here to stop someone re-introducing.
echo $$ > "$_throttle_slot.pid" 2>/dev/null || true
_throttle_release() { rmdir "$_throttle_slot" 2>/dev/null || true; rm -f "$_throttle_slot.pid" 2>/dev/null || true; }
trap '_throttle_release' EXIT

_receipt=""
if [ "${1:-}" = "test" ] && [ -z "${_TC_RECEIPT:-}" ]; then
  _receipt="$(cd "$(dirname "$0")" && pwd)/write-test-receipt.sh"
  [ -x "$_receipt" ] || _receipt=""
fi

_target_guard="$(cd "$(dirname "$0")" && pwd)/cargo-target-guard.py"
_budget="$(cd "$(dirname "$0")" && pwd)/cargo-budget.py"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.amux/rust-build-target}"
_guard_cmd=(python3 "$_target_guard" run --target "$CARGO_TARGET_DIR")
# Cargo's explicit --target-dir wins over the environment. Lease both roots.
_next_target=0
for _arg in "$@"; do
  if [ "$_next_target" = 1 ]; then _guard_cmd+=(--target "$_arg"); _next_target=0; fi
  case "$_arg" in
    --target-dir) _next_target=1 ;;
    --target-dir=*) _guard_cmd+=(--target "${_arg#--target-dir=}") ;;
  esac
done

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
       -- "${_guard_cmd[@]}" -- python3 "$_budget" -- cargo "$@")
else
  echo "safe-cargo.sh: no systemd on this host — running cargo directly." \
       "There is no pane scope for an OOM to cascade into here." >&2
  CMD=("${_guard_cmd[@]}" -- python3 "$_budget" -- cargo "$@")
fi

# No more `exec` here (see the throttle comment above) -- the EXIT trap that
# releases this invocation's concurrency slot has to actually run, and `exec`
# would replace this process before it could.
rc=0
"${CMD[@]}" || rc=$?
if [ -n "$_receipt" ]; then
  "$_receipt" "$rc" "$@"
fi
exit "$rc"
