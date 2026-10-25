#!/usr/bin/env bash
# Amuxhelper: the fleet's one shared, non-LLM executor for pre-approved,
# named runbooks. No new board state — it only ever claims cards already
# reassigned to it (`session=amuxhelper`, `status=todo`) and drives them
# through the SAME todo -> doing -> done/blocked lifecycle every other
# worker uses. See scripts/amuxhelper-runbooks/README.md for the contract.
#
# Deliberately not an agent: there is no LLM in this loop, so there is
# nothing here that can improvise. The only thing this script can ever do
# is run a script that already exists in scripts/amuxhelper-runbooks/, named
# exactly by the card. Adding a new runbook is a reviewed code change, not
# something any session can trigger at runtime.
#
# Run by com.amux.amuxhelper-poll every 5 minutes (see amuxhelper-poll.plist).
set -euo pipefail

export PATH="$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

REPO="${AMUX_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
RUNBOOK_DIR="${AMUXHELPER_RUNBOOK_DIR:-$REPO/scripts/amuxhelper-runbooks}"
LOG="${AMUXHELPER_LOG:-$HOME/.amux/logs/amuxhelper.log}"
LOCK="${AMUXHELPER_LOCK:-$HOME/.amux/amuxhelper.lock}"
AMUX_URL="${AMUX_URL:-$(amux url 2>/dev/null || echo https://localhost:8824)}"
SESSION="${AMUXHELPER_SESSION_NAME:-amuxhelper}"
MAX_PER_TICK="${AMUXHELPER_MAX_PER_TICK:-5}"
CMD_TIMEOUT_S="${AMUXHELPER_TIMEOUT_S:-60}"

# Every `amux board` write below is attributed via this — without it, every
# mutation lands as api-anonymous/unattributed (the exact AMUX-1812 shape).
export AMUX_URL
export AMUX_SESSION="$SESSION"
export AMUX_WORKER="$SESSION"

mkdir -p "$(dirname "$LOG")"
exec >>"$LOG" 2>&1

# ONE run at a time. A tick that is still executing a runbook when the next
# tick fires must not double-claim.
#
# mkdir, not flock: flock is a Linux utility and does not exist on macOS,
# where this runs (same reasoning as scripts/rust-auto-build.sh's own lock,
# AMUX-2927). mkdir is atomic on POSIX and needs no helper binary.
LOCK_HELD=""
cleanup() { [ -n "$LOCK_HELD" ] && rm -rf "$LOCK"; }
trap cleanup EXIT

if ! mkdir "$LOCK" 2>/dev/null; then
  owner="$(cat "$LOCK/pid" 2>/dev/null || echo "")"
  if [ -n "$owner" ] && kill -0 "$owner" 2>/dev/null; then
    echo "$(date -Iseconds) amuxhelper: previous tick still running (pid $owner), skipping"
    exit 0
  fi
  echo "$(date -Iseconds) amuxhelper: breaking stale lock (pid ${owner:-unknown} is gone)"
  rm -rf "$LOCK"
  mkdir "$LOCK" 2>/dev/null || { echo "$(date -Iseconds) amuxhelper: lost the lock race, skipping"; exit 0; }
fi
LOCK_HELD=1
echo $$ >"$LOCK/pid"

echo "$(date -Iseconds) amuxhelper: tick start"

board_json="$(curl -sk --max-time 20 "$AMUX_URL/api/board?all=1&slim=0")"
if [ -z "$board_json" ]; then
  echo "$(date -Iseconds) amuxhelper: board unreachable, measured=false, skipping tick"
  exit 0
fi

# Candidate ids: agent-owned todo cards assigned to us. python does the JSON
# work; everything after this is plain bash + curl, no other dependency.
#
# JSON goes in on STDIN, never as an argv element — the full board (all
# lanes, unslimmed) is well past ARG_MAX as a single argument (measured:
# "Argument list too long" against a ~1600-card board). stdin has no such
# ceiling.
#
# Not `mapfile` — this box's /bin/bash is 3.2 (macOS froze it there over the
# GPLv3 switch), and mapfile/readarray are bash-4+ builtins. A while-read
# loop is the portable equivalent.
candidates=()
while IFS= read -r line; do
  [ -n "$line" ] && candidates+=("$line")
done < <(printf '%s' "$board_json" | python3 -c "
import json, sys
items = json.load(sys.stdin)
mine = [i['id'] for i in items if i.get('session') == sys.argv[1] and i.get('status') == 'todo']
print('\n'.join(mine))
" "$SESSION" 2>>"$LOG" | head -n "$MAX_PER_TICK")

n_considered=${#candidates[@]}
n_ran=0
n_refused=0

# bash 3.2 treats a ZERO-element array as unset under `set -u`, so
# `"${candidates[@]}"` on an empty array is an "unbound variable" error, not
# a zero-iteration loop (fixed in later bash; this box doesn't have later
# bash). Guard the count instead of trusting the expansion.
i=0
while [ "$i" -lt "$n_considered" ]; do
  id="${candidates[$i]}"
  i=$((i + 1))
  [ -z "$id" ] && continue

  card_json="$(curl -sk --max-time 20 "$AMUX_URL/api/board/$id")"
  desc="$(printf '%s' "$card_json" | python3 -c "import json,sys; print(json.load(sys.stdin).get('desc') or '')" 2>>"$LOG")"

  runbook="$(printf '%s\n' "$desc" | sed -n 's/^AMUXHELPER_RUNBOOK: *//p' | head -1 | tr -d '\r')"
  args_line="$(printf '%s\n' "$desc" | sed -n 's/^AMUXHELPER_ARGS: *//p' | head -1 | tr -d '\r')"

  refuse() {
    local reason="$1"
    echo "$(date -Iseconds) amuxhelper: refusing $id: $reason"
    n_refused=$((n_refused + 1))
    amux board block "$id" --on "amuxhelper refused: $reason" >>"$LOG" 2>&1 || true
  }

  if [ -z "$runbook" ]; then
    refuse "no AMUXHELPER_RUNBOOK: line in desc"
    continue
  fi

  # Name shape FIRST, cheaply, independent of the filesystem: only
  # alnum/dash/underscore. This is the real path-traversal guard — it
  # rejects '/', '..', and anything else that could point `$script` outside
  # the registry before a single filesystem call happens. (BSD readlink -f
  # on macOS prints nothing and exits nonzero for a path that doesn't
  # exist — relying on it alone gave a misleading "outside the registry"
  # refusal for an ordinary typo; measured live, AH-142.)
  case "$runbook" in
    *[!a-zA-Z0-9_-]*)
      refuse "runbook name '$runbook' contains characters outside [a-zA-Z0-9_-]"
      continue
      ;;
  esac

  script="$RUNBOOK_DIR/$runbook.sh"
  if [ ! -x "$script" ]; then
    refuse "unknown runbook '$runbook' (no script at scripts/amuxhelper-runbooks/$runbook.sh)"
    continue
  fi

  # Claim it the normal way BEFORE running anything, so a concurrent tick
  # (or a human) sees `doing`, not a card that silently vanished mid-run.
  #
  # `chore`-type cards gate `doing` on "Scope is clear" / "Has an owner" —
  # both genuinely true by this point, not a rubber stamp: we already
  # confirmed above that a NAMED, EXISTING runbook is what's being claimed
  # (scope) and the card is owned by this session (owner). Amuxhelper cards
  # must be created as `type: chore` (see amuxhelper-runbooks/README.md); any
  # other type's gate is unknown to this script and the claim below will
  # fail loudly rather than guess at criteria that may not apply.
  if ! amux board doing "$id" --checked "Scope is clear" "Has an owner" >>"$LOG" 2>&1; then
    echo "$(date -Iseconds) amuxhelper: could not claim $id (already claimed, or not type=chore?), skipping"
    continue
  fi

  # Args become AMUXHELPER_ARG_<KEY> env vars; each runbook validates its own.
  # Same empty-array caveat as above: build with a while-read, not `read -ra`
  # into a loop trusted to run zero times safely.
  arg_env=()
  IFS=',' read -ra pairs <<<"$args_line"
  n_pairs=${#pairs[@]}
  j=0
  while [ "$j" -lt "$n_pairs" ]; do
    pair="${pairs[$j]}"
    j=$((j + 1))
    [ -z "$pair" ] && continue
    key="${pair%%=*}"
    val="${pair#*=}"
    key_upper="$(echo "$key" | tr '[:lower:]-' '[:upper:]_')"
    arg_env+=("AMUXHELPER_ARG_${key_upper}=${val}")
  done

  echo "$(date -Iseconds) amuxhelper: running $runbook for $id (args: $args_line)"
  out_file="$(mktemp -t "amuxhelper-$id.XXXXXX")"
  # `env` with no var=val arguments still works fine, but an empty
  # "${arg_env[@]}" is the same bash-3.2 unbound-variable trap as above —
  # branch instead of trusting the expansion.
  if [ "${#arg_env[@]}" -gt 0 ]; then
    if env "${arg_env[@]}" timeout "$CMD_TIMEOUT_S" "$script" >"$out_file" 2>&1; then
      exit_code=0
    else
      exit_code=$?
    fi
  else
    if timeout "$CMD_TIMEOUT_S" "$script" >"$out_file" 2>&1; then
      exit_code=0
    else
      exit_code=$?
    fi
  fi
  output="$(tail -c 4000 "$out_file")"
  rm -f "$out_file"

  if [ "$exit_code" -eq 0 ]; then
    # Never claim success without checking: `done` can still be gate-refused
    # (wrong card type, missing criterion), and swallowing that with `|| true`
    # is exactly the silent-success shape this whole mechanism exists to avoid.
    # `done` requires a checkable artifact pointer (board_store::asset_refs —
    # a repo path, URL, sha or #ref; a bare `gs://...` string in the
    # runbook's own stdout does NOT match, it only recognizes https?://).
    # scripts/amuxhelper-runbooks/<name>.sh is a real repo path and always
    # matches, so it doubles as the checkable "what actually ran" pointer.
    if printf '%s\n\nrunbook: scripts/amuxhelper-runbooks/%s.sh\nargs: %s\nexit: 0\n' "$output" "$runbook" "$args_line" \
        | amux board done "$id" --checked "Outcome recorded in the item (what happened, and why it is closed)" --evidence-stdin >>"$LOG" 2>&1; then
      n_ran=$((n_ran + 1))
      echo "$(date -Iseconds) amuxhelper: $id done (runbook $runbook, exit 0)"
    else
      n_refused=$((n_refused + 1))
      printf '%s\n\nrunbook: %s (SUCCEEDED, exit 0) args: %s\n' "$output" "$runbook" "$args_line" \
        | amux board progress "$id" --stdin >>"$LOG" 2>&1 || true
      amux board block "$id" --on "amuxhelper: runbook $runbook succeeded but the board refused done (gate/type mismatch) — see progress note" >>"$LOG" 2>&1 || true
      echo "$(date -Iseconds) amuxhelper: $id runbook OK but done was REFUSED — left blocked, not silently dropped"
    fi
  else
    n_refused=$((n_refused + 1))
    printf '%s\n\nrunbook: %s\nargs: %s\nexit: %s\n' "$output" "$runbook" "$args_line" "$exit_code" \
      | amux board progress "$id" --stdin >>"$LOG" 2>&1 || true
    amux board block "$id" --on "amuxhelper: runbook $runbook exited $exit_code, see log" >>"$LOG" 2>&1 || true
    echo "$(date -Iseconds) amuxhelper: $id FAILED (runbook $runbook, exit $exit_code)"
  fi
done

echo "$(date -Iseconds) amuxhelper: tick done — considered=$n_considered ran=$n_ran refused=$n_refused"
