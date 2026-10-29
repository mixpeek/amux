#!/usr/bin/env bash
# Compile main's tip while its CI runs, so going live is not a build.
#
#   scripts/rust-prebuild.sh <repo> <sha>
#
# Owner, 2026-10-29: "we need to speed up how this works". The activation
# authority deploys the newest commit CI passed, and only then did the builder
# compile it: 4 to 6 minutes (267s and 374s that day) between CI going green
# and the server running it. The authority now starts this script for a tip
# that is still pending. It compiles that commit in the builder's own stable
# directory with the builder's own settings, so when the commit goes green the
# builder's `cargo build` there finds nothing left to do (the stable directory
# keeps cargo's fingerprints; scripts/rust-auto-build.sh "ONE STABLE BUILD
# DIRECTORY"). It installs nothing, writes no stamp and changes no live state:
# the worst a bad prebuild costs is CPU, niced.
#
# It takes the builder's lock (the same mkdir lock, same pid file), so it never
# compiles beside the builder or the disk guard, and skips when the lock is
# held. It skips when disk is below the builder's floor, and it never starts a
# second time for the same commit (the done file). Every outcome is one log
# line with a verdict: prebuild_started, prebuild_done, prebuild_failed,
# prebuild_skipped. A commit is marked done only once it compiled; a failure
# is retried after AMUX_PREBUILD_RETRY_S (600).
set -euo pipefail

REPO="${1:?repo}"
SHA="${2:?sha}"
LOG="${AMUX_RS_BUILD_LOG:-$HOME/.amux/logs/rust-auto-build.log}"
LOCK="${AMUX_RS_BUILD_LOCK:-$HOME/.amux/rust-build.lock}"
STABLE="${AMUX_DEPLOY_SRC:-$HOME/.amux/deploy-build-src}"
TARGET="${AMUX_DEPLOY_TARGET_DIR:-$HOME/.amux/rust-build-target}"
DONE="${AMUX_PREBUILD_DONE:-$HOME/.amux/prebuild-done}"
export PATH="$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "$(dirname "$LOG")"

say() { echo "== $(date '+%F %T') PREBUILD $SHA $*" >> "$LOG"; }

[ "$(cat "$DONE" 2>/dev/null)" != "$SHA" ] || exit 0
# A failure is retried, but not every pass: a commit that does not compile would
# otherwise recompile every minute until its CI resolves (land review F2).
FAILED="$DONE.failed"
if [ "$(cat "$FAILED" 2>/dev/null)" = "$SHA" ] \
   && [ $(( $(date +%s) - $(stat -f %m "$FAILED" 2>/dev/null || stat -c %Y "$FAILED" 2>/dev/null || echo 0) )) -lt "${AMUX_PREBUILD_RETRY_S:-600}" ]; then
  exit 0
fi
free_gb=$(df -Pk "$HOME" | awk 'NR==2{print int($4/1048576)}')
if [ "${free_gb:-0}" -lt "${AMUX_BUILD_MIN_FREE_GB:-25}" ]; then
  say "skipped: ${free_gb}GB free is under the builder's floor verdict=prebuild_skipped reason=disk"
  exit 0
fi
if ! mkdir "$LOCK" 2>/dev/null; then
  say "skipped: the builder's lock is held (pid $(cat "$LOCK/pid" 2>/dev/null || echo '?')) verdict=prebuild_skipped reason=lock"
  exit 0
fi
OUT=$(mktemp "${TMPDIR:-/tmp}/amux-prebuild.XXXXXX")
trap 'rm -rf "$LOCK"; rm -f "$OUT"' EXIT
echo $$ > "$LOCK/pid"

# The same snapshot the builder makes: hard-reset and cleaned, then the commit.
if [ -d "$STABLE" ] && git -C "$STABLE" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  git -C "$STABLE" reset -q --hard && git -C "$STABLE" clean -qfdx && git -C "$STABLE" checkout -q --detach "$SHA" \
    || { echo "$SHA" > "$FAILED"; say "skipped: could not check out $SHA in $STABLE verdict=prebuild_skipped reason=checkout"; exit 0; }
else
  git -C "$REPO" worktree add --detach "$STABLE" "$SHA" >/dev/null 2>&1 \
    || { echo "$SHA" > "$FAILED"; say "skipped: could not create $STABLE verdict=prebuild_skipped reason=checkout"; exit 0; }
fi

# The builder's settings (rust-auto-build.sh "THE DEPLOY PROFILE ONLY PAYS OFF
# WITH ITS OWN SETTINGS"): every core on a Mac, niced so the fleet wins.
if [ -n "${AMUX_DEPLOY_JOBS:-}" ]; then JOBS="$AMUX_DEPLOY_JOBS"
elif [ "$(uname -s)" = Darwin ]; then JOBS="$(/usr/sbin/sysctl -n hw.ncpu 2>/dev/null || echo 2)"
else JOBS="${CARGO_BUILD_JOBS:-2}"; fi
NICE="${AMUX_DEPLOY_NICE:-10}"
# The committed wrapper at this commit, never the shared checkout's.
CARGO="${AMUX_PREBUILD_CARGO:-$STABLE/scripts/safe-cargo.sh}"
say "started jobs=$JOBS nice=$NICE verdict=prebuild_started"
t0=$(date +%s)
if (cd "$STABLE" && CARGO_TARGET_DIR="$TARGET" CARGO_INCREMENTAL=1 CARGO_BUILD_JOBS="$JOBS" nice -n "$NICE" "$CARGO" build --profile deploy -p amux-server) > "$OUT" 2>&1; then
  echo "$SHA" > "$DONE"; rm -f "$FAILED"
  say "done elapsed_s=$(( $(date +%s) - t0 )) verdict=prebuild_done"
else
  echo "$SHA" > "$FAILED"
  say "failed elapsed_s=$(( $(date +%s) - t0 )) verdict=prebuild_failed (retried after ${AMUX_PREBUILD_RETRY_S:-600}s): $(grep -m1 -E '^error' "$OUT" | cut -c1-200)"
fi
