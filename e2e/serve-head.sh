#!/usr/bin/env bash
# Build and run amux-server for the e2e harness from COMMITTED HEAD (AMUX-2924).
#
# WHY NOT THE WORKING TREE. This is a SHARED checkout — CLAUDE.md's deploy
# section is about several sessions committing to it at once — and playwright's
# webServer used to run `cargo run -p amux-server` right here in it. So any peer
# who happened to be mid-edit when your suite started failed YOUR run, with a
# Rust compile error, against changes that might be JS-only. That happened on
# 2026-08-11: E0432 out of session_verbs.rs during a run whose diff touched only
# app.js and a .spec.ts. `cargo check` passed 40s later, once the peer saved.
#
# The lost minutes are not the point. The danger is that a red e2e run caused by
# a stranger's half-saved file is INDISTINGUISHABLE from a red run caused by
# your own change, so the natural response — doubt your patch — is aimed at the
# wrong thing. Committed source is also what actually ships (the builder deploys
# HEAD, not the tree), so HEAD is the honest thing to test.
#
# THE TRAP THIS COULD HAVE ADDED, and why the notice below is not decoration:
# defaulting to HEAD means uncommitted work is NOT under test. A developer
# iterating on a fix would watch their change have no effect, which is a
# quieter failure than the one being fixed. So a dirty tree is announced
# loudly, by name, every run — and AMUX_E2E_WORKING_TREE=1 opts back in for
# exactly that workflow.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# One shared target dir, per CLAUDE.md: per-session scratch dirs cost 10-15GB
# each and filled the volume on 2026-08-10. Cargo's build lock makes a
# concurrent build WAIT and then find the work done, which is cheap.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.amux/rust-build-target}"

# AEAB-52. THIS SCRIPT EXISTS TO PIN A SPECIFIC BUILD, so a server that hot-swaps
# itself mid-suite is running a different binary from the one the suite chose.
#
# playwright.config.ts starts THREE of these (desktop 18823, mobile 18833,
# ios-safari 18843). Each builds, every build rewrites the shared binary, and the
# server's 5s SELF_ADOPT loop sees its own mtime move and exec's — refusing
# connections for ~1s. Whichever specs are mid-`page.goto` fail with
# ERR_CONNECTION_REFUSED. That was three days of "flaky" desktop failures: 3, then
# 9 on a re-run of the SAME commit, then 2, then 2.
#
# The signature, execs per port against start order in run 32645871348:
#     18823 desktop (1st) -> 2      18833 mobile (2nd) -> 1      18843 ios (3rd) -> 0
# Each server exec's once per server started after it; the last never does.
#
# Set here rather than in playwright.config.ts because THIS is the script that
# pins the build — anything else launching a pinned server gets it for free, and
# the config cannot forget.
#
# A SECOND COPY WAS ADDED TO playwright.config.ts AND REMOVED AGAIN (AF-185). It
# was written from a CI log that predated this line by twenty minutes, which is
# the trap worth naming: a failure you can read in a job log is not a failure the
# current tree has, and the log gives no hint that the tree moved. The copy was
# also INERT — this `export` runs inside the process the config launches, so it
# overrides whatever the config passed in, and the redundant setting could never
# have taken effect even if the two disagreed. If you are here because a run
# shows `binary changed on disk`, check the run's created_at against this line's
# commit before patching anything.
export AMUX_NO_SELF_ADOPT=1

dirty="$(git -C "$REPO" status --porcelain -- crates/ Cargo.toml Cargo.lock 2>/dev/null || true)"

if [ "${AMUX_E2E_WORKING_TREE:-0}" = "1" ]; then
  echo "[e2e] SOURCE: the WORKING TREE at $REPO — NOT committed HEAD (AMUX_E2E_WORKING_TREE=1)."
  echo "[e2e] A peer mid-edit in this shared checkout can fail this run; that is the trade you asked for."
  exec cargo run -p amux-server
fi

if [ -n "$dirty" ]; then
  echo "[e2e] ─────────────────────────────────────────────────────────────────"
  echo "[e2e] BUILDING FROM COMMITTED HEAD ($(git -C "$REPO" rev-parse --short HEAD))."
  echo "[e2e] These UNCOMMITTED rust changes are NOT under test:"
  echo "$dirty" | sed 's/^/[e2e]   /'
  echo "[e2e] Commit them, or re-run with AMUX_E2E_WORKING_TREE=1 to test the tree."
  echo "[e2e] ─────────────────────────────────────────────────────────────────"
fi

# A STABLE worktree, re-pointed at HEAD each run rather than created per run:
# `worktree add` costs real time and a per-run temp dir leaks when playwright
# kills this process (it kills the server, it does not run our cleanup).
WT="${AMUX_E2E_WORKTREE_DIR:-$HOME/.amux/e2e-worktree}"
SHA="$(git -C "$REPO" rev-parse HEAD 2>/dev/null || true)"

# FALL BACK TO THE TREE RATHER THAN FAILING TO START. `set -e` plus a git that
# cannot make a worktree — a shallow CI checkout, a read-only $HOME, a git too
# old — would otherwise kill this script, and playwright reports that as
# "Process from config.webServer was not able to start", which reads as a
# broken harness rather than a git problem. On CI the tree IS HEAD, so the
# fallback is exact there; locally it is the old behaviour with a loud line
# saying so. Degrading to the previous behaviour beats refusing to run.

# SERIALIZE THE GIT OPS — this script now runs THREE TIMES CONCURRENTLY, once
# per playwright project (AF-46 gave each target its own server and AMUX_HOME).
# The worktree below is deliberately SHARED and stable, so three simultaneous
# copies would `worktree add` / `checkout --detach` / in the worst case `rm -rf`
# the same directory at the same moment. That race was already reachable — two
# sessions running e2e at once on this shared checkout — it was just rare enough
# to look like nothing.
#
# mkdir, not flock: `flock(1)` is util-linux and is NOT on macOS, where most of
# this fleet runs. mkdir is atomic on every POSIX filesystem.
#
# The lock covers ONLY the git operations. The cargo build stays outside it on
# purpose: cargo has its own build lock, and CLAUDE.md's measurement is that a
# second builder waits and then finds the work already done (1.48s alone, 1.65s
# for two). Holding this lock across the build would serialize three cold builds
# for no gain.
LOCK="${WT}.lock"
acquire_lock() {
  local waited=0
  while ! mkdir "$LOCK" 2>/dev/null; do
    # A run killed by playwright never reaches the release below, so a stale
    # lock must expire rather than wedge every future run at this line.
    if [ -n "$(find "$LOCK" -maxdepth 0 -mmin +5 2>/dev/null)" ]; then
      echo "[e2e] stale worktree lock (>5min), reclaiming: $LOCK"
      rm -rf "$LOCK"
      continue
    fi
    sleep 1
    waited=$((waited + 1))
    if [ "$waited" -ge 120 ]; then
      # Proceed unlocked rather than refuse to boot: the common case is the
      # same SHA, where the git ops are idempotent no-ops. Say so out loud —
      # a silent degrade here is how a corrupt worktree gets blamed on cargo.
      echo "[e2e] WARNING: worktree lock held >120s — proceeding WITHOUT it. Concurrent git ops possible."
      return 1
    fi
  done
  return 0
}

setup_worktree() {
  [ -n "$SHA" ] || return 1
  local locked=0
  acquire_lock && locked=1
  # Release on ANY exit from this function, including the `return 1` paths —
  # the caller falls back to the working tree there and must not leave the
  # lock held for the next two servers booting one second behind it.
  _unlock() { [ "$locked" = "1" ] && rmdir "$LOCK" 2>/dev/null; locked=0; }
  if [ ! -e "$WT/.git" ]; then
    rm -rf "$WT"
    git -C "$REPO" worktree add --detach "$WT" "$SHA" >/dev/null 2>&1 || { _unlock; return 1; }
  else
    # Already exists: move it to this HEAD. Concurrent e2e runs on this
    # checkout share ONE HEAD, so they converge on the same sha rather than
    # fighting over the directory.
    git -C "$WT" checkout --detach "$SHA" >/dev/null 2>&1 || {
      git -C "$REPO" worktree remove --force "$WT" >/dev/null 2>&1 || rm -rf "$WT"
      git -C "$REPO" worktree add --detach "$WT" "$SHA" >/dev/null 2>&1 || { _unlock; return 1; }
    }
  fi
  _unlock
  [ -f "$WT/Cargo.toml" ]
}

if setup_worktree; then
  cd "$WT"
  # SAY WHICH SOURCE THIS RUN ACTUALLY BUILT, ALWAYS (AF-182, third instance).
  #
  # The banner above prints only `if [ -n "$dirty" ]`, so a run against a clean
  # tree announced NOTHING about its source — and this script has three of them:
  # committed HEAD, the working tree via AMUX_E2E_WORKING_TREE=1, and the working
  # tree via the fallback below. All three produce identical output in the common
  # case, so "did I test HEAD or the tree?" is unanswerable from the run's own log.
  #
  # That is not hypothetical. AF-182's third instance is a run killed by a peer's
  # UNCOMMITTED mid-edit, filed fifteen days after this script started building
  # from HEAD, over an import (`crate::worker::WorkerId`) that `git log -S --all`
  # shows was never committed. One of the two tree paths must have been taken and
  # the record cannot say which, so the incident is unreconstructable today.
  #
  # Ethos rule 4: an output that can be in two states publishes which one it was,
  # in the same place someone already looks.
  echo "[e2e] SOURCE: committed HEAD $(git -C "$REPO" rev-parse --short HEAD) (worktree $WT)."
  # The worktree build gets its OWN target dir (AMUX-2961). Sharing the fleet's
  # dir looked like the rule — but cargo dep-info records ABSOLUTE source paths,
  # so after a worktree build, a `cargo build` from the repo compares mtimes of
  # the WORKTREE's files, sees them unchanged, and no-ops in 0.13s — silently
  # handing back HEAD's binary while the caller believes they built their tree.
  # That manufactured three consecutive "my fix doesn't work" verdicts on
  # 2026-08-12, and would make the auto-builder install a STALE binary for the
  # next commit while reporting success. One extra bounded cache dir is the
  # price of the two source trees never sharing fingerprints.
  export CARGO_TARGET_DIR="${AMUX_E2E_HEAD_TARGET_DIR:-$HOME/.amux/rust-build-target-e2e-head}"
else
  echo "[e2e] WARNING: could not prepare a HEAD worktree at $WT — falling back to the WORKING TREE."
  echo "[e2e] On CI the tree is HEAD so this is exact; on a shared checkout a peer mid-edit can fail this run."
  # Same line as the success branch, deliberately — a reader grepping SOURCE finds
  # one of the two in every run, rather than finding it only when things went well.
  echo "[e2e] SOURCE: the WORKING TREE at $REPO — NOT committed HEAD."
fi

exec cargo run -p amux-server
