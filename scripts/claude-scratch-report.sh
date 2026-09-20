#!/usr/bin/env bash
# Report what /private/tmp/claude-501 is holding, per conversation and per
# owning lane. THIS SCRIPT NEVER DELETES ANYTHING. There is no --apply, and
# there is a test that greps for the absence of a delete path, because the whole
# point is that the lane which owns the bytes decides, not amux (AMUX-4615).
#
# Why this is separate from reap-amux-debris.sh: that reaper deliberately skips
# claude-501 in three places, and it is right to. This is live scratchpad space
# for every running Claude Code session on the machine, so an age-based reaper
# over it moves someone's working files. `reclaim.rs`'s guard refuses the same
# path and says why:
#
#   "Its own top-level mtime can look stale for hours while sessions write deep
#    inside their own subdirs (APFS only bumps a directory's mtime when its
#    direct entries change), so age-based heuristics over it are unreliable in
#    exactly the direction that would move someone's live working files."
#
# So LIVENESS IS NEVER TAKEN FROM A DIRECTORY MTIME here. It comes from the
# conversation's TRANSCRIPT, which is appended on every turn and cannot go
# stale while the session is working.
#
# Usage: scripts/claude-scratch-report.sh [--min-gb N] [--dead-days N] [--tsv]
set -uo pipefail

# This user's Claude scratch root on THIS OS: $TMPDIR/claude-<uid> (so
# /tmp/claude-1000 on Linux), with macOS's /private/tmp form as the fallback.
# It used to default to /private/tmp/claude-501 — the author's uid on his OS —
# so on Linux it measured nothing and this report could never fire.
_default_scratch_root() {
  local uid tmp
  uid=$(id -u)
  tmp=${TMPDIR:-/tmp}; tmp=${tmp%/}
  for c in "$tmp/claude-$uid" "/private/tmp/claude-$uid"; do
    [ -d "$c" ] && { printf '%s\n' "$c"; return; }
  done
  printf '%s\n' "$tmp/claude-$uid"
}
ROOT=${CLAUDE_SCRATCH_ROOT:-$(_default_scratch_root)}
PROJ=${CLAUDE_PROJECTS_DIR:-$HOME/.claude/projects}
SESS=${AMUX_SESSIONS_DIR:-$HOME/.amux/sessions}
MIN_GB=1.0        # a subfolder smaller than this is noise, not a finding
DEAD_DAYS=3       # transcript silent this long = the conversation is over
TSV=0

while [ $# -gt 0 ]; do
  case "$1" in
    --min-gb) MIN_GB=$2; shift 2 ;;
    --dead-days) DEAD_DAYS=$2; shift 2 ;;
    --tsv) TSV=1; shift ;;
    -h|--help) sed -n '1,25p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# Conversations that were escalated to Ethan on 2026-09-14 because their lane
# could not act: mixpeek-ops-server was not running and mixpeek-cicd was paused.
#
# THIS LIST IS HISTORY, NOT A CLASSIFICATION. Reachability is COMPUTED below on
# every run, because "the owner has to decide this" is a fact about whether the
# lane is up right now, and pinning it to a UUID freezes a Sunday afternoon into
# the tool. Measured 2026-09-16: both lanes are running again, so both entries
# would still be reported as owner-held by a hardcoded rule while their owners
# sat there able to act. The list is kept only so the report can say a
# conversation has been escalated before.
ESCALATED_2026_09_14="caceffea-c12d-475e-a18e-26729434a5d8 db837290-839f-4c5a-addc-4aa3c72a4256"

now=$(date +%s)

# Encode each lane's CC_DIR the way Claude Code names a project directory
# (path separators become dashes). ENCODING IS DETERMINISTIC AND DECODING IS
# NOT: `-Users-ethan-Dev-ai-for-smbs` could be /Users/ethan/Dev/ai-for-smbs or
# /Users/ethan/Dev/ai/for/smbs, so the map is built in the direction that has
# one answer.
lane_map=$(
  for f in "$SESS"/*.env; do
    [ -f "$f" ] || continue
    d=$(grep -h '^CC_DIR=' "$f" 2>/dev/null | head -1 | cut -d= -f2- | tr -d '"')
    [ -n "$d" ] || continue
    printf '%s\t%s\n' "$(printf '%s' "${d%/}" | tr '/' '-')" "$(basename "${f%.env}")"
  done | sort
)

running_lanes=$(tmux list-panes -a -F '#{session_name}' 2>/dev/null | sed 's/^amux-//' | sort -u)

# Lanes whose CC_DIR encodes to this project directory. A SHARED WORKSPACE HAS
# NO SINGLE OWNER: /Users/ethan/Dev/mixpeek is the CC_DIR of 19 lanes and
# /Users/ethan/Dev/amux of 10, so for those the honest answer is the candidate
# list, not a pick. Naming one would be a guess that reads like a fact, and
# that is how the 2026-09-14 delivery routed bytes to lanes that could not act.
attribute() {
  local proj=$1 lanes n running=
  lanes=$(awk -F'\t' -v k="$proj" '$1==k{print $2}' <<<"$lane_map")
  n=$(printf '%s' "$lanes" | grep -c . )
  if [ "$n" = 0 ]; then echo "unattributed"; return; fi
  if [ "$n" = 1 ]; then echo "$lanes"; return; fi
  for l in $lanes; do
    grep -qx "$l" <<<"$running_lanes" && running="${running}${running:+,}$l"
  done
  echo "AMBIGUOUS:${n}-candidates${running:+ running=$running}"
}

# Whether whoever owns these bytes can be told about them right now. The
# 2026-09-14 delivery failed on exactly this and had no way to say so: two of
# three lanes were down, the asks went nowhere, and both scratchpads then grew
# by ~6 GB each while the report still read as delivered.
reachability() {
  local owner=$1
  case "$owner" in
    unattributed) echo "no-owner" ;;
    AMBIGUOUS:*running=*) echo "reachable-via-candidates" ;;
    AMBIGUOUS:*) echo "UNREACHABLE-all-candidates-down" ;;
    *) grep -qx "$owner" <<<"$running_lanes" && echo "reachable" || echo "UNREACHABLE-lane-down" ;;
  esac
}

emit() {  # size_gb  age  class  reach  owner  path
  if [ "$TSV" = 1 ]; then printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$@"
  else printf '%-8s %-11s %-13s %-26s %-30s %s\n' "$@"; fi
}

[ "$TSV" = 1 ] || emit "SIZE_GB" "TRANSCRIPT" "CLASS" "REACH" "OWNER" "CONVERSATION"

tot=0; n_conv=0; n_live=0; n_dead=0; n_notx=0; n_held=0
gb_live=0; gb_dead=0; gb_notx=0; gb_held=0
report=$(
for cdir in "$ROOT"/*/*/; do
  [ -d "$cdir" ] || continue
  conv=$(basename "$cdir"); proj=$(basename "$(dirname "$cdir")")
  kb=$(du -sk "$cdir" 2>/dev/null | awk '{print $1}'); [ -n "$kb" ] || continue
  gb=$(awk -v k="$kb" 'BEGIN{printf "%.2f", k/1048576}')

  tx="$PROJ/$proj/$conv.jsonl"
  if [ -f "$tx" ]; then
    # GNU `stat -c %Y` and BSD `stat -f %m`. Without the GNU arm every
    # transcript reads as mtime 0 on Linux, which would silently classify the
    # whole fleet as DEAD: the wrong answer in the dangerous direction.
    mt=$(stat -c %Y "$tx" 2>/dev/null || stat -f %m "$tx" 2>/dev/null || echo 0)
    age=$(awk -v n="$now" -v m="$mt" 'BEGIN{d=(n-m)/86400; printf "%.1f", (d<0?0:d)}')
    if awk -v a="$age" -v d="$DEAD_DAYS" 'BEGIN{exit !(a>=d)}'; then cls=DEAD; else cls=LIVE; fi
    agestr="${age}d"
  else
    # NO TRANSCRIPT IS NOT DEATH. `bash-edit-diff` holds 20+ directories that
    # never had a conversation, so a rule keyed on transcript age marks every
    # one of them reapable. Unknown liveness is its own class and is never
    # actionable.
    cls=NO-TRANSCRIPT; agestr="absent"
  fi
  # CAN THE OWNER ACT? This is orthogonal to whether the bytes are stale, and
  # folding the two together is what made the 09-14 list go out of date: a
  # scratchpad is the owner's problem when its lane is up, and Ethan's only
  # while it is not. Computed per run from the live pane list.
  owner=$(attribute "$proj")
  reach=$(reachability "$owner")
  # An `if`, not a `case`: bash 3.2 (what /bin/bash is on this Mac) mis-parses a
  # case statement nested inside $( ), taking the pattern's `)` as the end of
  # the command substitution.
  if [[ " $ESCALATED_2026_09_14 " == *" $conv "* ]]; then reach="$reach,escalated-09-14"; fi

  emit "$gb" "$agestr" "$cls" "$reach" "$owner" "$proj/$conv"
done | sort -k1 -rn
)

# Per-conversation totals are not actionable for a LIVE lane: nobody deletes the
# scratchpad they are working in. The actionable unit is the SUBFOLDER that has
# not been written in days while the conversation around it is busy, which is
# where the growth actually sits (mixpeek-homepage was holding gitG, cand3 and
# cand4 at 3.6 GB each). Reported for every conversation big enough to matter,
# LIVE ones included, because those are the ones a lane can still act on.
stale_subfolders() {
  local cdir=$1 sub kb gb reported=
  for sub in "$cdir"*/ "$cdir"*/*/; do
    [ -d "$sub" ] || continue
    # A stale parent already covers everything beneath it, so reporting the
    # child as well would let someone add the two figures and double-count the
    # same bytes. Shallower entries come first in this glob order, so skipping
    # anything under an already-reported path keeps the widest honest unit.
    local skip=
    for r in $reported; do
      case "$sub" in "$r"*) skip=1; break ;; esac
    done
    [ -z "$skip" ] || continue
    kb=$(du -sk "$sub" 2>/dev/null | awk '{print $1}'); [ -n "$kb" ] || continue
    gb=$(awk -v k="$kb" 'BEGIN{printf "%.2f", k/1048576}')
    awk -v g="$gb" -v m="$MIN_GB" 'BEGIN{exit !(g>=m)}' || continue
    # "Has ANY file under here been written in the window?" -print -quit stops
    # at the first hit, so this stays cheap on a 43 GB tree. The question is
    # asked of FILES, never of the directory's own mtime.
    if [ -n "$(find "$sub" -type f -newermt "-${DEAD_DAYS} days" -print -quit 2>/dev/null)" ]; then
      continue   # written recently; the lane is still using it
    fi
    reported="$reported $sub"
    printf '    %6s GB  no writes in %sd  %s\n' "$gb" "$DEAD_DAYS" "${sub#"$ROOT"/}"
  done
}
printf '%s\n' "$report"

if [ "$TSV" != 1 ]; then
  printf '\nSTALE SUBFOLDERS (>= %s GB, no file written in %s days)\n' "$MIN_GB" "$DEAD_DAYS"
  printf 'The part a lane can act on without touching what it is still using.\n\n'
  found=0
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    gb=$(awk '{print $1}' <<<"$line"); path=$(awk '{print $NF}' <<<"$line")
    awk -v g="$gb" -v m="$MIN_GB" 'BEGIN{exit !(g>=m)}' 2>/dev/null || continue
    subs=$(stale_subfolders "$ROOT/$path/")
    [ -n "$subs" ] || continue
    found=$((found+1))
    printf '  %s  [%s]\n%s\n' "$path" "$(awk '{print $3}' <<<"$line")" "$subs"
  done <<<"$report"
  [ "$found" -gt 0 ] || printf '  none: no subfolder >= %s GB has been idle %s days\n' "$MIN_GB" "$DEAD_DAYS"
fi

# Summary computed from the rows above, never written by hand. Every count
# carries the population it is over.
printf '%s\n' "$report" | awk -v min="$MIN_GB" '
  { g=$1+0; c=$3; r=$4; tot+=g; n++
    if (c=="LIVE") { L+=g; nl++ } else if (c=="DEAD") { D+=g; nd++ } else { U+=g; nu++ }
    if (r ~ /^UNREACHABLE/) { X+=g; nx++ } else if (r=="no-owner") { N+=g; nn++ } else { R+=g; nr++ }
    if (g>=min) big++ }
  END {
    printf "\n%d conversation(s), %.1f GB total\n", n, tot
    printf "\nBY LIVENESS (is the data still in use)\n"
    printf "  LIVE           %5.1f GB over %d  transcript written recently, not a target\n", L, nl
    printf "  DEAD           %5.1f GB over %d  transcript silent, the only reapable class\n", D, nd
    printf "  NO-TRANSCRIPT  %5.1f GB over %d  liveness unknown, never actionable\n", U, nu
    printf "\nBY REACH (can the owner be told, right now)\n"
    printf "  reachable      %5.1f GB over %d  ask the lane; it can act today\n", R, nr
    printf "  UNREACHABLE    %5.1f GB over %d  lane is down, so this is Ethans call\n", X, nx
    printf "  no owner       %5.1f GB over %d  no lane CC_DIR encodes to this project\n", N, nn
    printf "\n  %d of %d conversation(s) are >= %s GB\n", big, n, min
    if (tot>0) printf "  reapable share: %.1f%% of all bytes here\n", 100*D/tot
  }'
