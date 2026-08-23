#!/usr/bin/env bash
# Auto-build for the Rust server (the "server adopts every change" seam).
#
# Run by com.amux.server-rs-builder every 60s: when the committed Rust
# source has moved since the last successful build, rebuild release and
# install the binary; the running server notices its own binary changed and
# exits for launchd to relaunch (self-adoption in amux-server/src/lib.rs).
#
# COMMITTED source only — building the working tree would ship half-typed
# code from any session on this shared checkout. A commit is the unit of
# "there is a change to adopt", mirroring how the Python server's file-save
# reload is bounded by whole-file saves.
set -euo pipefail

# launchd does NOT inherit the shell PATH (the restic lesson in ~/Dev/CLAUDE.md
# — same class, same fix): name the toolchain absolutely.
export PATH="$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

# The repo is wherever this script lives (scripts/ under the checkout), so a
# clone installed via ./install.sh builds ITSELF rather than a hardcoded
# developer path. Env overrides exist for the temp-prefix install self-test.
REPO="${AMUX_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
INSTALL="${AMUX_RS_INSTALL:-$HOME/.local/bin/amux-server-rs}"
STAMP="${AMUX_RS_BUILD_STAMP:-$HOME/.amux/rust-build-stamp}"
LOG="${AMUX_RS_BUILD_LOG:-$HOME/.amux/logs/rust-auto-build.log}"
mkdir -p "$(dirname "$LOG")" "$(dirname "$INSTALL")"

LOCK="${AMUX_RS_BUILD_LOCK:-$HOME/.amux/rust-build.lock}"

# ONE cleanup handler, set once. A second `trap ... EXIT` REPLACES the first
# rather than adding to it, so the worktree trap that used to live inside the
# block below would have silently discarded any lock release installed before
# it — the lock would leak on every run and the second invocation would be
# blocked forever. Everything that needs unwinding goes here.
cleanup() {
  if [ -n "${WORK:-}" ]; then
    git -C "$REPO" worktree remove --force "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
  fi
  [ -n "${BUILD_OUT:-}" ] && rm -f "$BUILD_OUT"
  [ -n "${LOCK_HELD:-}" ] && rm -rf "$LOCK"
  return 0   # a falsey last test must not make the trap itself fail under set -e
}
trap cleanup EXIT

head=$(git -C "$REPO" log -1 --format=%H -- crates/ Cargo.toml Cargo.lock 2>/dev/null || echo none)
last=$(cat "$STAMP" 2>/dev/null || echo "")
[ "$head" = "$last" ] && exit 0

# ── SINGLE-INSTANCE LOCK (AMUX-2927) ────────────────────────────────────────
# Two invocations — the 60s launchd cycle and a human running this by hand —
# built 68d7114 simultaneously and one died with E0432 on a half-evicted
# artifact. Cargo's own build lock is NOT what was missing: it already
# serialises concurrent `cargo build`s (measured in CLAUDE.md: two
# concurrent incremental builds finish in 1.65s vs 1.48s alone, because the
# second waits and then finds the work done). What is outside that lock is the
# DISK GUARD's `rm -rf` of the shared target dir — one invocation can delete
# the tree the other is mid-build against, which is exactly what an
# unresolved-import error on a vanished rlib looks like.
#
# So the lock must cover the guard and the build TOGETHER, which is why it
# wraps the whole block rather than just the cargo call.
#
# mkdir, not flock: flock is a Linux utility and does not exist on macOS, where
# this runs. mkdir is atomic on POSIX and needs no helper binary.
mkdir -p "$(dirname "$LOCK")"
if ! mkdir "$LOCK" 2>/dev/null; then
  owner=$(cat "$LOCK/pid" 2>/dev/null || echo "")
  if [ -n "$owner" ] && kill -0 "$owner" 2>/dev/null; then
    # Log the contention rather than exiting silently: a skip that leaves no
    # trace is indistinguishable from a cycle that found nothing to do, and
    # THAT ambiguity is what made this bug take three occurrences to spot.
    echo "== $(date '+%F %T') SKIP $head — build already running (pid $owner)" >> "$LOG"
    exit 0
  fi
  echo "== $(date '+%F %T') breaking stale lock (pid ${owner:-unknown} is gone)" >> "$LOG"
  rm -rf "$LOCK"
  mkdir "$LOCK" 2>/dev/null || { echo "== $(date '+%F %T') SKIP $head — lost the lock race" >> "$LOG"; exit 0; }
fi
LOCK_HELD=1
echo $$ > "$LOCK/pid"

# RE-READ THE STAMP NOW THAT WE HOLD THE LOCK. The check above ran before the
# wait, so a build we queued behind may have just installed this very sha —
# rebuilding it is the wasted-cycle half of the reported bug ("each next SOLO
# cycle built the identical sha fine").
last=$(cat "$STAMP" 2>/dev/null || echo "")
[ "$head" = "$last" ] && exit 0

# PROVENANCE (AEAB-12). This builder rebuilds whenever $REPO's local HEAD moves
# and does not care whether HEAD is on main or on somebody's feature branch. The
# server then self-adopts within 5s. That permissiveness is CORRECT and must stay:
# this machine survived weeks deliberately pinned to an unmerged fix branch, and
# "only build main" would delete the rollback mechanism.
#
# The defect is that a deliberate pin and an ACCIDENTAL feature branch are
# byte-identical to the builder, and the accidental one is announced nowhere. On
# 2026-08-17 a commit made on a branch inside this checkout was serving the whole
# fleet 76 seconds later and stayed there 9h42m — no CI had run on it, no review —
# while the machine also could not track upstream, so the daily update schedule
# silently did nothing. Everything looked healthy the entire time.
#
# So: say it. NOT a refusal, a fact, written where consumers can find it.
#
# The predicate is "HEAD is contained in main OR origin/main". Checking only
# origin/main would false-positive right after a merge, because this script
# deliberately does not fetch (it must not reach the network on a 60s timer) and
# the local remote-tracking ref lags. Local `main` moves on the merge itself, so
# the pair covers both orders.
on_main=no
if git -C "$REPO" merge-base --is-ancestor HEAD main 2>/dev/null \
   || git -C "$REPO" merge-base --is-ancestor HEAD origin/main 2>/dev/null; then
  on_main=yes
fi
head_ref=$(git -C "$REPO" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "?")
# The sha reported is the one actually BUILT — the worktree below is created from
# `rev-parse HEAD`. `$head` is a different thing: the last commit that touched the
# build inputs, used as the rebuild stamp key. Reporting the stamp key here would
# name a commit that is not what is running.
built_sha=$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo "$head")
# A file rather than only a log line, because ~/.amux/logs/rust-auto-build.log is
# not somewhere anyone looks — a tag in a store the reader never opens is the same
# failure as no tag. The SessionStart freshness hook reads this and says it at the
# one moment a session is about to build on it.
#
# AEAB-50: COMPUTED here, WRITTEN after a successful install. It used to be written
# right here, before the build, and nowhere else — so it recorded the sha this run
# was ABOUT TO ATTEMPT, not the one running. Three consequences, and the third is
# the one that matters:
#   1. wrong for the whole duration of every build. Every build on this machine is
#      cold right now (free space sits under the cache threshold), so that window
#      is ~1.5-2 min out of every 60s tick.
#   2. observed 2026-08-23: the file read {"sha":"9ef46b1f","on_main":"yes"} while
#      both servers reported commit 23ddb8d1d91d, an unmerged branch, because the
#      correcting build was still running. I read it as "the fleet is corrected"
#      and it was not.
#   3. a FAILED build left the file permanently asserting a deploy that never
#      happened, with nothing to correct it. The failure branch below already says
#      "running server keeps the last good build" — so the file must keep
#      describing THAT build, and now it does: on failure it is not touched.
# The freshness hook quotes this as "the RUNNING SERVER is N behind; built <sha>",
# the line a session uses to decide whether its merge has deployed. That sentence
# was false whenever the last build failed or was in flight.
PROV_FILE="${AMUX_RS_BUILD_PROVENANCE:-$HOME/.amux/rust-build-provenance.json}"
PROV_JSON=$(printf '{"sha":"%s","ref":"%s","on_main":"%s","built_at":"%s"}' \
  "$built_sha" "$head_ref" "$on_main" "$(date '+%F %T')")

# A seam so the predicate above is TESTABLE against real repos rather than
# restated in a test that could not notice it changing. Everything before this
# point is cheap and touches no network; a cargo build is neither, which is why
# scripts/test-build-provenance.sh stops here instead of asserting on a copy of
# the logic.
if [ "${AMUX_RS_BUILD_PROVENANCE_ONLY:-}" = "1" ]; then
  # The seam still WRITES, because what it exists to test is the predicate that
  # produces these fields, and a test that cannot read the output tests nothing.
  # The real path deliberately does not write here — see AEAB-50 above.
  printf '%s\n' "$PROV_JSON" > "$PROV_FILE" 2>/dev/null || true
  [ "$on_main" = "yes" ] || echo "OFF-MAIN $head_ref $built_sha"
  exit 0
fi

{
  echo "== $(date '+%F %T') building $head (was: ${last:-none})"
  if [ "$on_main" != "yes" ]; then
    echo "== !! OFF-MAIN: $built_sha is on '$head_ref', which is not contained in main."
    echo "==    Installing it makes it the live build for the WHOLE FLEET within ~5s,"
    echo "==    with no CI and no review. Intentional pin? fine. Accident? put"
    echo "==    $REPO back on main — develop in a git worktree, not the build source."
  fi
  # DISK GUARD (AMUX-2754). Runs BEFORE the worktree checkout below, because
  # that checkout writes 1000+ files — freeing space after consuming it is the
  # wrong order when the whole point is that the volume is nearly full. The shared target dir has no GC — cargo never
  # reclaims — so it grows without bound, and on 2026-08-10 the volume hit
  # 741MB free with a 50-session fleet and writes failing with ENOSPC.
  #
  # The trigger is FREE DISK, deliberately not target-dir size. Disk-full is
  # the thing that actually broke the fleet; dir size is a proxy for it whose
  # threshold would be a guess, and the same 28GB is fine on this volume and
  # fatal on a smaller one. Free space is the condition that is absent in the
  # healthy state, which is the signal worth tripping on.
  #
  # Clearing the cache costs one cold build (~3min, once). That is the cheap
  # side of this trade by a wide margin: the expensive side is every lane
  # failing to write.
  # CLEAR THE IDLE CACHE FIRST, AND THE ONE THIS BUILD NEEDS ONLY AS A LAST
  # RESORT. Until 2026-08-19 this deleted `rust-build-target` unconditionally —
  # the cache it was ABOUT TO FILL on the very next line — while
  # `rust-build-target-e2e-head` sat untouched beside it at 4.2GB. Measured that
  # day: the clear fired 16 times, free space still reached 1GB, and each pass
  # freed ~2GB that the immediately-following cold build put straight back. A
  # treadmill that burns a cold build every 60s and never relieves the pressure,
  # while four times as much reclaimable space sat one directory over.
  #
  # So: order by what this build does NOT need, re-measure between steps, and
  # stop as soon as the floor is cleared. `-e2e-head` belongs to e2e/serve-head.sh
  # and is regenerable; clearing it costs a cold e2e build the next time someone
  # runs e2e locally, which is far rarer than this 60s tick.
  # `df -Pk` / `du -sk`, NOT `df -g` / `du -sg`. The -g forms are BSD-only: GNU
  # coreutils rejects them with "df: invalid option -- 'g'", FREE_GB comes back
  # EMPTY, `${FREE_GB:-999}` substitutes 999, and the guard silently never fires.
  # A disk guard that is a no-op on Linux while reporting nothing is the shape
  # this repo keeps finding — it does not fail, it just quietly does not run.
  # Caught by scripts/test-build-disk-clear.sh on its first CI run; the Rust side
  # already had it right (storage::disk_free_bytes uses `df -Pk`), so this was
  # one convention drifting from another inside the same codebase.
  # TWO THRESHOLDS, BECAUSE THERE ARE TWO QUESTIONS (AEAB-35). The first cut of
  # this used one number for both and the keep-warm exit below became DEAD CODE:
  # it fired only once free space reached AMUX_BUILD_MIN_FREE_GB (25GB), on a
  # volume sitting at 4GB, so after reclaiming the idle cache the condition was
  # still false and the shared cache was destroyed anyway. Measured: zero
  # "reclaimed to" lines, ever. A stop condition above the achievable maximum is
  # not a stop condition — the mirror of "a threshold below the baseline is not a
  # detector", and it reads perfectly sensibly in review.
  #
  #   AMUX_BUILD_MIN_FREE_GB (25)             — is the FLEET at ENOSPC risk?
  #                                             Right number for that; AMUX-2754
  #                                             is 741MB free with lanes failing
  #                                             to write. It decides whether to
  #                                             reclaim AT ALL.
  #   AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB (8) — can THIS BUILD proceed while
  #                                             keeping its cache? A cargo build
  #                                             plus a ~5GB target dir needs
  #                                             single-digit GB, not 25.
  #
  # Between the two, reclaim the idle caches and let the build stay warm. Below
  # the lower one, the shared cache genuinely is worth a cold build.
  FREE_GB=$(df -Pk "$HOME" | awk 'NR==2{print int($4/1048576)}')
  if [ "${FREE_GB:-999}" -lt "${AMUX_BUILD_MIN_FREE_GB:-25}" ]; then
    for cand in "$HOME/.amux/rust-build-target-e2e-head" "$HOME/.amux/rust-build-target"; do
      [ -d "$cand" ] || continue
      if [ "$cand" = "$HOME/.amux/rust-build-target" ]; then
        # LAST RESORT. Only sacrifice the cache this build needs when free space
        # is below the level at which the build could keep it.
        if [ "${FREE_GB:-0}" -ge "${AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB:-8}" ]; then
          echo "== reclaimed to ${FREE_GB}GB free (>= ${AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB:-8}GB) — keeping the shared target dir, so this build stays warm."
          break
        fi
      fi
      CAND_GB=$(du -sk "$cand" 2>/dev/null | awk '{print int($1/1048576)}')
      if [ "$cand" = "$HOME/.amux/rust-build-target" ]; then
        echo "== DISK LOW: ${FREE_GB}GB free (< ${AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB:-8}GB). Clearing the ${CAND_GB:-?}GB SHARED target dir — this build goes cold."
      else
        echo "== DISK LOW: ${FREE_GB}GB free. Clearing the ${CAND_GB:-?}GB idle e2e target dir first (this build does not need it)."
      fi
      # A seam so the ORDERING is testable without a 4GB fixture or a real build.
      [ "${AMUX_RS_DISK_CLEAR_DRYRUN:-}" = "1" ] || rm -rf "$cand"
      # Re-measure between candidates: what the idle cache freed is what decides
      # whether the shared one survives. Under dry-run there is nothing to
      # re-measure, so keep listing candidates to show the full order.
      if [ "${AMUX_RS_DISK_CLEAR_DRYRUN:-}" != "1" ]; then
        FREE_GB=$(df -Pk "$HOME" | awk 'NR==2{print int($4/1048576)}')
        if [ "${FREE_GB:-999}" -ge "${AMUX_BUILD_MIN_FREE_GB:-25}" ]; then
          echo "== reclaimed to ${FREE_GB}GB free — above the fleet floor, nothing further to clear."
          break
        fi
      fi
    done
  fi
  if [ "${AMUX_RS_DISK_CLEAR_ONLY:-}" = "1" ]; then exit 0; fi

  # Build from a clean, committed snapshot: a worktree of HEAD, so nobody's
  # uncommitted edits (or a mid-edit broken tree) can poison the deploy.
  WORK=$(mktemp -d /tmp/amux-rs-build.XXXXXX)
  git -C "$REPO" worktree add --detach "$WORK" "$(git -C "$REPO" rev-parse HEAD)" >/dev/null
  # Shared target dir: incremental rebuilds (~15s) instead of cold ones
  # (~3min) — the worktree isolates SOURCE, the cache is content-keyed.
  # CAPTURE THE WHOLE BUILD, THEN DECIDE WHAT TO KEEP (AMUX-2927).
  #
  # This was `cargo build ... 2>&1 | tail -3`, and on a FAILURE cargo's last
  # three lines are the summary — "error: could not compile ... due to N
  # previous errors" and a warning — while the line that names the actual
  # problem (`error[E0432]: unresolved import ...`, with its file and line) is
  # thousands of lines earlier and was thrown away. Every build failure was
  # therefore undiagnosable from the log it wrote, which is ethos rule 4: the
  # instrument could not express the discriminator.
  #
  # Success still logs three lines — terseness there is the point, and this
  # runs every 60s. Only the failing path pays for detail, which is the path
  # that needs it.
  BUILD_OUT=$(mktemp /tmp/amux-rs-buildout.XXXXXX)
  if (cd "$WORK" && CARGO_TARGET_DIR="$HOME/.amux/rust-build-target" cargo build --release -p amux-server) > "$BUILD_OUT" 2>&1; then
    tail -3 "$BUILD_OUT"
    install -m 0755 "$HOME/.amux/rust-build-target/release/amux-server" "$INSTALL"
    echo "$head" > "$STAMP"
    # AEAB-50: only NOW is this true. Written after the install so the file means
    # "what is installed" rather than "what was attempted". On the failure branch
    # below it is left alone, so it keeps naming the last good build — which is
    # exactly what that branch says is still running.
    printf '%s\n' "$PROV_JSON" > "$PROV_FILE" 2>/dev/null || true
    echo "== installed; running server will self-adopt within 5s"
  else
    echo "== BUILD FAILED for $head — running server keeps the last good build"
    echo "-- diagnostics (every error, with context) ---------------------------"
    grep -nE '^error(\[E[0-9]+\])?:|^error: ' -A 8 "$BUILD_OUT" | head -200 || true
    echo "-- last 20 lines of cargo output -------------------------------------"
    tail -20 "$BUILD_OUT"
    echo "-- end diagnostics ($(wc -l < "$BUILD_OUT" | tr -d ' ') lines total) ---"
    # Stamp is NOT updated: the next cycle retries. A failed build never
    # takes the fleet down (the AC-309 class: a bad save must not crash-loop
    # the server).
  fi
} >> "$LOG" 2>&1
