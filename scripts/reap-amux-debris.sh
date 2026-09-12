#!/bin/bash
# reap-amux-debris.sh — investigate and reclaim AMUX'S OWN filesystem debris.
#
# WHAT THIS IS NOT. It is not a disk cleaner for the machine. `disk_watch`
# (crates/amux-server/src/runtime_jobs/disk_watch.rs) deliberately REPORTS and
# deletes nothing, and that decision stands: of 337 GiB in nominally-regenerable
# paths on this box only 17.3 GiB was untouched for 30 days, so an age cap
# reclaims a rounding error and a size cap deletes build artifacts ~50 lanes are
# actively using. The human's caches stay the human's call (ethos rule 8).
#
# WHAT THIS IS. The debris amux itself leaves behind, where the owner is
# unambiguous and the blast radius is amux's own: temp roots minted by the e2e
# harnesses, snapshot copies taken by the push gates, and scratch git worktrees
# abandoned by finished sessions. Nobody has to weigh whether to keep these —
# the run that made them is over. Measured 2026-09-12 before the first pass:
# 7,774 directories and ~3 GB, and it had been accumulating for weeks because
# `~/.tmp-reaper.sh` defaults to --min-size-mb 200 and 2,389 of them were zero
# bytes.
#
# SAFE BY CONSTRUCTION
#   - only names amux itself mints (exact prefixes below), never a glob of /tmp
#   - only entries idle longer than --age-hours (default 6; these runs finish in
#     minutes, so a live run's root is always far younger)
#   - worktrees only under a scratch root, only when `git status` is clean, and
#     removed through `git worktree remove` WITHOUT --force so git's own refusal
#     is the last guard
#   - /private/tmp/claude-501 is never touched: live scratchpad space for every
#     running Claude Code session on this machine
#   - dry run by default; --apply is required to delete anything
#
# Usage:
#   scripts/reap-amux-debris.sh              # investigate, delete nothing
#   scripts/reap-amux-debris.sh --apply      # reclaim
#   scripts/reap-amux-debris.sh --apply --age-hours 24
set -uo pipefail

AGE_HOURS=6
APPLY=0
REPO="${AMUX_REPO_DIR:-$HOME/Dev/amux}"
while [ $# -gt 0 ]; do
  case "$1" in
    --apply) APPLY=1 ;;
    --age-hours) AGE_HOURS="${2:?--age-hours needs a value}"; shift ;;
    --repo) REPO="${2:?--repo needs a value}"; shift ;;
    -h|--help) sed -n '2,36p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

# The temp roots amux actually writes to. On macOS these differ and BOTH are
# used: os.tmpdir() is the per-user /var/folders/.../T, while any run whose
# TMPDIR was forced to /tmp (the lifecycle harness does, because the per-user
# path overflows the Unix socket limit) lands in /private/tmp instead. Watching
# only one root is how 349 dirs hid behind a reaper that was already running.
#
# AMUX_DEBRIS_ROOTS (colon-separated) REPLACES the defaults. This exists so the
# test can run against a fixture instead of the machine's real temp roots: a
# reaper whose only test target is /tmp is one nobody can safely test.
ROOTS=()
if [ -n "${AMUX_DEBRIS_ROOTS:-}" ]; then
  # printf '%s\n', NOT '%s': without the terminator `read` returns non-zero on
  # the final unterminated line, the loop body never runs for it, and a
  # single-root override silently reaps nothing. The test caught exactly that.
  while IFS= read -r r; do [ -n "$r" ] && ROOTS+=("${r%/}"); done < <(printf '%s\n' "$AMUX_DEBRIS_ROOTS" | tr ':' '\n')
else
  [ -n "${TMPDIR:-}" ] && ROOTS+=("${TMPDIR%/}")
  ROOTS+=(/private/tmp)
fi
# Prefixes amux mints. Keep this list exact — a wildcard here is how a reaper
# starts deleting somebody else's files.
PREFIXES=(amux-lc- amux-e2e- amux-rs-build. gp- gp_ push-check)

dirs_removed=0; dirs_bytes=0; dirs_kept_fresh=0
# macOS ships bash 3.2, which has no associative arrays. A newline-delimited
# string is the portable way to remember which real paths we already walked.
seen_roots=$'\n'

# bash 3.2 + `set -u`: expanding an EMPTY array is an unbound-variable error.
for root in ${ROOTS[@]+"${ROOTS[@]}"}; do
  [ -d "$root" ] || continue
  # /tmp and /private/tmp are the same volume on macOS; don't count twice.
  real=$(cd "$root" 2>/dev/null && pwd -P) || continue
  case "$seen_roots" in *$'\n'"$real"$'\n'*) continue ;; esac
  seen_roots="${seen_roots}${real}"$'\n'
  for prefix in "${PREFIXES[@]}"; do
    while IFS= read -r path; do
      [ -n "$path" ] || continue
      case "$path" in */claude-501*) continue ;; esac
      # -F: a name may legitimately begin with a dash; never let it parse as a flag.
      sz=$(du -sk "$path" 2>/dev/null | cut -f1); sz=${sz:-0}
      if [ "$APPLY" = "1" ]; then
        rm -rf -- "$path" 2>/dev/null && { dirs_removed=$((dirs_removed+1)); dirs_bytes=$((dirs_bytes+sz)); }
      else
        dirs_removed=$((dirs_removed+1)); dirs_bytes=$((dirs_bytes+sz))
      fi
    done < <(find "$real" -maxdepth 1 -name "${prefix}*" -mmin "+$((AGE_HOURS*60))" 2>/dev/null)
    fresh=$(find "$real" -maxdepth 1 -name "${prefix}*" -mmin "-$((AGE_HOURS*60))" 2>/dev/null | wc -l | tr -d ' ')
    dirs_kept_fresh=$((dirs_kept_fresh + fresh))
  done
done

# ── abandoned scratch worktrees ──────────────────────────────────────────────
# A finished session's detached worktree under a scratch root. Clean only: a
# worktree with uncommitted work is somebody's in-flight change, and `git
# worktree remove` (no --force) refuses it for us as the final guard.
wt_removed=0; wt_dirty=0; wt_considered=0
if [ -d "$REPO/.git" ] || [ -f "$REPO/.git" ]; then
  while IFS= read -r wt; do
    [ -n "$wt" ] || continue
    case "$wt" in
      /tmp/*|/private/tmp/*|/var/folders/*) ;;
      *) continue ;;
    esac
    case "$wt" in */claude-501*) continue ;; esac
    [ -d "$wt" ] || continue
    # Older than the age floor? `find -maxdepth 0` tests the path itself.
    [ -n "$(find "$wt" -maxdepth 0 -mmin "+$((AGE_HOURS*60))" 2>/dev/null)" ] || continue
    wt_considered=$((wt_considered+1))
    if [ -n "$(git -C "$wt" status --porcelain 2>/dev/null)" ]; then
      wt_dirty=$((wt_dirty+1)); continue
    fi
    if [ "$APPLY" = "1" ]; then
      git -C "$REPO" worktree remove "$wt" >/dev/null 2>&1 && wt_removed=$((wt_removed+1))
    else
      wt_removed=$((wt_removed+1))
    fi
  done < <(git -C "$REPO" worktree list --porcelain 2>/dev/null | awk '/^worktree /{print $2}')
  [ "$APPLY" = "1" ] && git -C "$REPO" worktree prune >/dev/null 2>&1
fi

# Every number below is COMPUTED (CLAUDE.md: a summary line you hardcode cannot
# disagree with the run, so it reads as measured to every reader including you).
mode=$([ "$APPLY" = "1" ] && echo applied || echo "dry-run (pass --apply to reclaim)")
mb=$((dirs_bytes / 1024))
echo "amux-debris: mode=$mode age_floor=${AGE_HOURS}h"
echo "amux-debris: temp dirs ${dirs_removed} (${mb} MB), kept ${dirs_kept_fresh} younger than the floor"
echo "amux-debris: worktrees ${wt_removed} of ${wt_considered} considered, ${wt_dirty} left alone as dirty"
# Non-zero only on a real failure, so a scheduler run that reclaims nothing is
# still a success. Reclaiming nothing is the healthy steady state.
exit 0
