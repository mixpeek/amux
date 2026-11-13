#!/usr/bin/env bash
# Who must you ask before `git push origin main`, and who CANNOT be asked?
#
# CLAUDE.md's Deploy block says "If foreign commits exist, ask their author
# before pushing." That instruction has no truthful path when the author is an
# ISOLATED lane: an isolated worker refuses sends carrying a worker origin, so
# only the owner can reach it from the dashboard. The state is completely
# legitimate — an isolated lane is supposed to commit to shared main and
# somebody else is supposed to push it — and the two available moves are to
# push unasked while a MANDATORY rule says otherwise, or to never push. Everyone
# picks the first and nobody records that they did (AF-548, found live
# 2026-09-07 when amux-homepage's consent poll named 16 of 23 commits and missed
# 5 belonging to an isolated `amux`).
#
# A NAMED EXEMPTION IS A TRUTHFUL PATH; SILENCE IS NOT (ethos rule 1: when you
# exempt something from a loop, name what still reaches it). So this prints the
# unaskable authors as their own section, by name and with the reason, for the
# pusher to state rather than not know.
#
# It also prints, per lane, how many commits touch NO Rust — because the gates
# people offer as push evidence (`cargo clippy --workspace`, `cargo test -p
# amux-server`) are scoped to a LANGUAGE and get quoted as a verdict on a RANGE.
# In the run that produced this script, 10 of 23 commits touched no .rs file at
# all, including the commit belonging to the lane that asked for consent. A
# workspace clippy said nothing whatsoever about it, and "exit 0" looks total.
#
# Usage: scripts/push-consent.sh [<base>] [<tip>]     defaults: origin/main HEAD
set -uo pipefail

BASE="${1:-origin/main}"
# THE TIP DEFAULTS TO HEAD, NOT `main`, AND THAT IS THE WHOLE POINT.
#
# CLAUDE.md tells every lane to run its gates on a DETACHED worktree, because a
# check in the shared checkout reads peers' uncommitted files. On a detached
# worktree `main` is a stale local branch ref that has nothing to do with the
# commits you are about to push, so the old default silently answered about the
# wrong range and printed "Nothing to push.", a FALSE CLEAR on the one gate
# whose entire job is to stop a push that should have asked somebody first.
# Measured 2026-09-16 on a worktree holding one unpushed commit: `commits 0`.
#
# HEAD is right in both shapes: on a checkout sitting on main, HEAD IS main.
TIP="${2:-HEAD}"
ME="${AMUX_SESSION:-}"
API="${AMUX_URL:-}"

git rev-parse --verify --quiet "$BASE" >/dev/null || { echo "no such ref: $BASE" >&2; exit 2; }
git rev-parse --verify --quiet "$TIP"  >/dev/null || { echo "no such ref: $TIP"  >&2; exit 2; }

shas=$(git rev-list "$BASE..$TIP")
total=$(printf '%s\n' "$shas" | grep -c . || true)

tip_sha=$(git rev-parse --short "$TIP")
on=$(git symbolic-ref --quiet --short HEAD 2>/dev/null || echo "detached HEAD")

echo "range        $BASE..$TIP"
echo "tip          $tip_sha (on $on)"
echo "commits      $total"
echo "you          ${ME:-<AMUX_SESSION unset>}"
# Say it when the old default would have answered about something else, so a
# reader who remembers `main` is told which range this verdict covers.
if git rev-parse --verify --quiet main >/dev/null 2>&1; then
  main_sha=$(git rev-parse --short main)
  if [ "$main_sha" != "$tip_sha" ]; then
    echo "note         local 'main' is $main_sha, a DIFFERENT commit; this verdict is about $tip_sha"
  fi
fi
echo

if [ "$total" -eq 0 ]; then
  echo "Nothing to push."
  exit 0
fi

# ---- per-commit facts -------------------------------------------------------
# This server's id, the same one agent commits carry as Amux-Host.
HERE_HOST="${AMUX_HOST_ID:-}"
if [ -z "$HERE_HOST" ] && [ -s "${AMUX_HOME:-$HOME/.amux}/ledger/host-id" ]; then
  HERE_HOST=$(head -1 "${AMUX_HOME:-$HOME/.amux}/ledger/host-id" | tr -cd '0-9a-f')
fi
# lane<TAB>sha<TAB>rust_files<TAB>total_files
facts=""
for c in $shas; do
  lane=$(git log -1 --format='%(trailers:key=Amux-Session,valueonly,separator=)' "$c")
  [ -z "$lane" ] && lane="<no Amux-Session trailer>"
  # A same-named lane on ANOTHER amux server is not you (governance PR 15):
  # its Amux-Host differs, so it is listed as <lane>@<host8> and needs consent.
  host=$(git log -1 --format='%(trailers:key=Amux-Host,valueonly,separator=)' "$c" | tr -cd '0-9a-f')
  if [ -n "$host" ] && [ -n "$HERE_HOST" ] && [ "$host" != "$HERE_HOST" ]; then
    lane="${lane}@$(printf '%s' "$host" | cut -c1-8)"
  fi
  files=$(git show --name-only --format= "$c")
  nrs=$(printf '%s\n' "$files" | grep -c '\.rs$' || true)
  ntot=$(printf '%s\n' "$files" | grep -c . || true)
  facts="${facts}${lane}	$(git rev-parse --short "$c")	${nrs}	${ntot}
"
done

# NUL-DELIMITED, and the loop below reads it line by line. `for lane in $lanes`
# WORD-SPLITS, and the untrailered placeholder is three words — so one commit
# with no Amux-Session trailer minted three fake lanes named `<no`,
# `Amux-Session` and `trailer>`, each reported as needing consent. Found by
# running this script against a throwaway repo rather than by reading it
# (AF-559's probe).
lanes=$(printf '%s' "$facts" | cut -f1 | sort -u)

# ---- reachability -----------------------------------------------------------
# An UNKNOWN reachability is reported as unknown, never as reachable: a probe
# that could not run must not read as a clean answer (ethos rule 4).
lane_state() {  # -> "isolated" | "reachable" | "unknown:<why>"
  local lane="$1"
  [ -z "$API" ] && { echo "unknown:AMUX_URL unset"; return; }
  # BUDGET SIZED FROM THE MEASURED BASELINE, not guessed (AC-422, amux-cloud).
  # Three back-to-back probes to this endpoint took 2.83s, 2.74s and 3.73s. The
  # old --max-time 5 therefore spent 55-75% of its budget on a good day, so a
  # false "lookup failed" was built in rather than rare, and each one routes the
  # pusher to must-ask for a lane that refuses the send. 12s plus one retry, so
  # a spike has to be sustained across two attempts to degrade the verdict.
  #
  # The fail DIRECTION is unchanged and is the safety property: a timeout can
  # only move a lane isolated -> unknown, never to "reachable", because reachable
  # requires a parsed payload with isolated=false and no timeout can manufacture
  # one. More budget lowers the false-unknown rate; it cannot create a false
  # clear.
  local body attempt rc=1
  for attempt in 1 2; do
    if body=$(curl -sk --max-time 12 "$API/api/sessions/$lane" 2>/dev/null); then rc=0; break; fi
  done
  [ "$rc" -ne 0 ] && { echo "unknown:session lookup failed (2 attempts, 12s each)"; return; }
  [ -z "$body" ] && { echo "unknown:empty response"; return; }
  printf '%s' "$body" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: print("unknown:unparseable response"); sys.exit()
if not isinstance(d,dict) or "name" not in d: print("unknown:no such session"); sys.exit()
if "isolated" not in d: print("unknown:payload has no isolated field"); sys.exit()
print("isolated" if d["isolated"] else "reachable")
'
}

ask=""; cannot=""; unknown=""; mine=""
while IFS= read -r lane; do
  [ -z "$lane" ] && continue
  n=$(printf '%s' "$facts" | awk -F'\t' -v l="$lane" '$1==l' | grep -c . || true)
  nors=$(printf '%s' "$facts" | awk -F'\t' -v l="$lane" '$1==l && $3==0' | grep -c . || true)
  row="  $lane — $n commit(s), $nors with no Rust"
  if [ -n "$ME" ] && [ "$lane" = "$ME" ]; then mine="${mine}${row}
"; continue; fi
  state=$(lane_state "$lane")
  case "$state" in
    isolated)   cannot="${cannot}${row}   [isolated: refuses worker-origin sends]
" ;;
    reachable)  ask="${ask}${row}
" ;;
    unknown:*)  unknown="${unknown}${row}   [${state#unknown:}]
" ;;
  esac
done <<EOF
$lanes
EOF

[ -n "$mine" ]   && { echo "YOURS (no consent needed):"; printf '%s' "$mine"; echo; }
[ -n "$ask" ]    && { echo "MUST ASK — reachable, send them 'amux send <lane>':"; printf '%s' "$ask"; echo; }
[ -n "$cannot" ] && {
  echo "CANNOT BE ASKED — state this in your push, do not imply you asked:"
  printf '%s' "$cannot"
  echo "  An isolated lane committing to shared main has already accepted that a peer"
  echo "  will push it. Pushing is defensible; claiming consent is not. (AF-548)"
  echo
}
[ -n "$unknown" ] && {
  echo "REACHABILITY UNKNOWN. Treat as must-ask; it is not a clear answer:"
  printf '%s' "$unknown"
  # Closes the loop for the flaky-probe case (AC-422). Without this line the
  # pusher tries the send the section just told them to make, it is REFUSED, and
  # they are back at AF-548's dead end for that one commit with nothing telling
  # them why.
  # SELF-CONTAINED ON PURPOSE. This block prints when the lookup FAILED, and in
  # that run nothing was classified isolated, so there is no CANNOT BE ASKED
  # section above it to point at. An earlier draft said "state the exemption
  # above" and referred to text that is absent on exactly the path that prints
  # this. Caught by running the forced-failure case rather than reading it.
  echo "  If you send and the send is REFUSED as isolated, that is the answer:"
  echo "  the lookup was down, not the lane. Push, and state this in your push:"
  echo "  the author is an isolated lane that refuses worker-origin sends, so"
  echo "  consent could not be obtained. Do not imply you asked. (AF-548)"
  echo
}

# ---- what the Rust gates cannot speak for -----------------------------------
# Computed, never written: the test of a summary line is what input would change
# it. A constant has none, so it reads as measured while being unable to fail.
no_rust=$(printf '%s' "$facts" | awk -F'\t' '$3==0' | grep -c . || true)
with_rust=$((total - no_rust))
echo "GATE SCOPE:"
echo "  $with_rust of $total commit(s) touch Rust — 'cargo clippy --workspace' / 'cargo test -p amux-server' speak for these."
echo "  $no_rust of $total commit(s) touch NO .rs file — those gates say nothing about them."
if [ "$no_rust" -gt 0 ]; then
  echo "  Do not offer a green Rust gate as push-readiness for these:"
  printf '%s' "$facts" | awk -F'\t' '$3==0 {printf "    %s [%s] %s file(s), 0 Rust\n", $2, $1, $4}'
fi
