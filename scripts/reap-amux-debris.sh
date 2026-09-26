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
#   - worktrees only under a temp root or $REPO/scratch, and only when tracked
#     files are clean, every untracked entry is a regenerable build dir
#     (node_modules, target, .venv), HEAD is reachable from a remote-tracking
#     ref (a clean detached worktree can hold the ONLY ref to its commits), and
#     no live process has its cwd inside. `--force` is passed only in the state
#     where the regenerable dirs are all that remain; the main worktree is never
#     a candidate
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
# A per-worktree CARGO_TARGET_DIR is only debris once nothing is building into
# it. 24h because a lane's build touches its target constantly, so a full idle
# day is a strong signal, and the cost of being wrong is a rebuild.
TARGET_IDLE_HOURS=24
APPLY=0
REPO="${AMUX_REPO_DIR:-$HOME/Dev/amux}"
while [ $# -gt 0 ]; do
  case "$1" in
    --apply) APPLY=1 ;;
    --age-hours) AGE_HOURS="${2:?--age-hours needs a value}"; shift ;;
    --target-idle-hours) TARGET_IDLE_HOURS="${2:?--target-idle-hours needs a value}"; shift ;;
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
wt_removed=0; wt_dirty=0; wt_considered=0; wt_local_only=0; wt_in_use=0; wt_unprobed=0
# Temp roots a finished session's detached worktree lands under. Overridable so
# the test can prove the $REPO/scratch rule on its own: a fixture repo lives
# under a temp root, which would otherwise match first.
WT_TEMP_ROOTS="${AMUX_DEBRIS_WT_TEMP_ROOTS-/tmp:/private/tmp:/var/folders}"
# Every live process's working directory, read once. A worktree somebody is
# standing in is in use whatever its mtime says. launchd's PATH lacks /usr/sbin
# (AF-861), so name lsof absolutely. An empty listing is a failed probe, never
# "nobody is anywhere", and it fails closed: no worktree is removed.
LSOF=/usr/sbin/lsof; [ -x "$LSOF" ] || LSOF=$(command -v lsof 2>/dev/null || true)
cwds=""
[ -n "$LSOF" ] && cwds=$("$LSOF" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p')
if [ -d "$REPO/.git" ] || [ -f "$REPO/.git" ]; then
  repo_real=$(cd "$REPO" 2>/dev/null && pwd -P)
  while IFS= read -r wt; do
    [ -n "$wt" ] || continue
    # git records a worktree by its REAL path, so a mktemp worktree is listed
    # under /private/var/folders while TMPDIR says /var/folders. Match both
    # spellings of both sides, or no temp-root worktree is ever a candidate:
    # the arm considered 0 worktrees on this Mac on 2026-09-14 for exactly that.
    wt_real=$(cd "$wt" 2>/dev/null && pwd -P) || continue
    in_root=no
    for cand in "$wt" "$wt_real"; do case "$cand" in "$REPO"/scratch/*|"$repo_real"/scratch/*) in_root=yes ;; esac; done
    if [ "$in_root" = no ]; then
      while IFS= read -r r; do
        [ -n "$r" ] || continue
        r_real=$(cd "$r" 2>/dev/null && pwd -P) || r_real="$r"
        for cand in "$wt" "$wt_real"; do
          case "$cand" in "${r%/}"/*|"${r_real%/}"/*) in_root=yes ;; esac
        done
        [ "$in_root" = yes ] && break
      done < <(printf '%s\n' "$WT_TEMP_ROOTS" | tr ':' '\n')
    fi
    [ "$in_root" = yes ] || continue
    case "$wt:$wt_real" in *claude-501*) continue ;; esac
    [ -d "$wt" ] || continue
    # Older than the age floor? `find -maxdepth 0` tests the path itself.
    [ -n "$(find "$wt" -maxdepth 0 -mmin "+$((AGE_HOURS*60))" 2>/dev/null)" ] || continue
    [ "$wt_real" = "$repo_real" ] && continue
    wt_considered=$((wt_considered+1))
    # Tracked changes, or any untracked entry other than a regenerable build
    # dir, is somebody's work: leave it and count it.
    tracked=$(git -C "$wt" status --porcelain --untracked-files=no 2>/dev/null)
    other=$(git -C "$wt" status --porcelain 2>/dev/null | grep '^??' | grep -v -E '^\?\? (node_modules|target|\.venv)/?$')
    if [ -n "$tracked" ] || [ -n "$other" ]; then
      wt_dirty=$((wt_dirty+1)); continue
    fi
    # Clean is not the same as safe. A detached worktree can hold the ONLY ref
    # to its commits, and removing it strands them for gc (DESKT-39 found 9 clean
    # local-only worktrees on 2026-09-14). So HEAD must be contained in a branch,
    # a tag or a remote-tracking ref of the repo, all of which outlive the
    # worktree. Requiring a REMOTE ref alone was stricter than losslessness: on
    # 2026-09-26 every scheduled run kept the same 8 clean scratch worktrees whose
    # HEADs sat on local branches, and reclaimed 0 of 14 (DESKT-56).
    head=$(git -C "$wt" rev-parse HEAD 2>/dev/null)
    if [ -z "$head" ] || [ -z "$(git -C "$REPO" for-each-ref --count=1 --contains "$head" refs/heads refs/tags refs/remotes 2>/dev/null)" ]; then
      wt_local_only=$((wt_local_only+1)); continue
    fi
    if [ -z "$cwds" ]; then
      wt_unprobed=$((wt_unprobed+1)); continue
    fi
    if printf '%s\n' "$cwds" | awk -v a="$wt" -v b="$wt_real" '$0==a || $0==b || index($0, a"/")==1 || index($0, b"/")==1 {f=1} END {exit !f}'; then
      wt_in_use=$((wt_in_use+1)); continue
    fi
    # Only regenerable untracked dirs can remain here, and plain removal
    # refuses over them, so --force is reached only in that state.
    force=""
    [ -n "$(git -C "$wt" status --porcelain 2>/dev/null | grep '^??')" ] && force="--force"
    if [ "$APPLY" = "1" ]; then
      git -C "$REPO" worktree remove $force "$wt" >/dev/null 2>&1 && wt_removed=$((wt_removed+1))
    else
      wt_removed=$((wt_removed+1))
    fi
  done < <(git -C "$REPO" worktree list --porcelain 2>/dev/null | awk '/^worktree /{print $2}')
  [ "$APPLY" = "1" ] && git -C "$REPO" worktree prune >/dev/null 2>&1
fi

# ── stale side cargo target dirs ─────────────────────────────────────────────
# A per-worktree CARGO_TARGET_DIR outlives the worktree that made it. Measured
# 2026-09-15: 76.6 GB across four of them, and rust-build-target-rr0052-wt was
# back at 19.9 GB hours after 45 GB of it was deleted by hand, which is why this
# is an arm and not a cleanup someone runs when they notice (AMUX-4614).
#
# THE SHARED TARGET IS NEVER A CANDIDATE. CLAUDE.md mandates one shared
# CARGO_TARGET_DIR for the whole fleet; reaping it would make every lane rebuild
# at once. Two layers protect it, and they cover different cases: the glob below
# never matches the default name (~/.amux/rust-build-target has no suffix), and
# the explicit guard covers a shared target CONFIGURED to a matching sibling
# name. Only siblings are eligible, and only when nothing has written to them
# for --target-idle-hours and no live process is inside or names them.
targets_removed=0; targets_kb=0; targets_kept=0
shared_target="${AMUX_SHARED_TARGET:-$HOME/.amux/rust-build-target}"
target_root="${AMUX_DEBRIS_TARGET_ROOT:-$HOME/.amux}"
# WHERE IT LOOKED, in the summary. The first scheduled run of this arm printed
# "0 (0 MB), 0 kept" while three side targets sat on the same machine: under the
# scheduler $HOME resolves elsewhere, so the glob matched nothing. A no-op and a
# wrong root render identically unless the root is named beside the count
# (ethos rule 4), and the fix for the root is a different fix than the fix for
# a busy target.
targets_root_state=present
[ -d "$target_root" ] || targets_root_state=MISSING
for t in "$target_root"/rust-build-target-*; do
  [ -d "$t" ] || continue
  [ "$t" = "$shared_target" ] && continue
  # Written inside the idle window? A build in flight touches its target
  # constantly, so this is the cheap half of "is anyone using it".
  if [ -n "$(find "$t" -newermt "-${TARGET_IDLE_HOURS} hours" -print -quit 2>/dev/null)" ]; then
    targets_kept=$((targets_kept+1)); continue
  fi
  # Named by a live process (cargo/rustc read it from argv or the environment),
  # or someone is standing in it. Same fail-closed rule as the worktree arm:
  # an empty cwd listing means the probe failed, so nothing is removed.
  if ps -Ao command= 2>/dev/null | grep -v grep | grep -qF -- "$t"; then
    targets_kept=$((targets_kept+1)); continue
  fi
  if [ -z "$cwds" ] || printf '%s\n' "$cwds" | grep -qF -- "$t"; then
    targets_kept=$((targets_kept+1)); continue
  fi
  kb=$(du -sk "$t" 2>/dev/null | cut -f1); kb=${kb:-0}
  if [ "$APPLY" = "1" ]; then
    rm -rf -- "$t" 2>/dev/null && { targets_removed=$((targets_removed+1)); targets_kb=$((targets_kb+kb)); }
  else
    targets_removed=$((targets_removed+1)); targets_kb=$((targets_kb+kb))
  fi
done

# Every number below is COMPUTED (CLAUDE.md: a summary line you hardcode cannot
# disagree with the run, so it reads as measured to every reader including you).
mode=$([ "$APPLY" = "1" ] && echo applied || echo "dry-run (pass --apply to reclaim)")
mb=$((dirs_bytes / 1024))
echo "amux-debris: mode=$mode age_floor=${AGE_HOURS}h"
echo "amux-debris: temp dirs ${dirs_removed} (${mb} MB), kept ${dirs_kept_fresh} younger than the floor"
echo "amux-debris: worktrees ${wt_removed} of ${wt_considered} considered, ${wt_dirty} left alone as dirty, ${wt_local_only} kept because no branch, tag or remote holds HEAD, ${wt_in_use} kept in use"
echo "amux-debris: side cargo targets ${targets_removed} ($((targets_kb / 1024)) MB), ${targets_kept} kept as busy or fresh (idle floor ${TARGET_IDLE_HOURS}h; the shared target is never a candidate)"
echo "amux-debris: side-target root ${target_root} ${targets_root_state}"
[ -n "$cwds" ] || echo "amux-debris: cwd probe unavailable (lsof missing or empty), so ${wt_unprobed} worktree(s) were not removed"
# Non-zero only on a real failure, so a scheduler run that reclaims nothing is
# still a success. Reclaiming nothing is the healthy steady state.
exit 0
