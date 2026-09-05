#!/usr/bin/env bash
# AMUX-2626 / AMUX-2670 — the unstamped-send ledger must actually reconcile.
#
# When the server is unreachable, `amux send` falls back to raw tmux, which
# drops provenance, the audit row and delivery verification. AMUX-2670 covers
# that: record the send to a LOCAL ledger and flush it to POST /api/history as
# `raw-tmux-fallback` on the next send that reaches the server, so an unaudited
# send and a send that never happened stop looking identical.
#
# WHY THIS TEST EXISTS. That mechanism had never fired: zero `raw-tmux-fallback`
# rows in cmd_history, all time, and no pending file. Zero is the one-output/
# two-states shape — it reads the same for "the fallback genuinely never
# happens" (good) and "the ledger is broken and every fallback send is lost"
# (the exact failure it was built to prevent). A safety net nobody has ever seen
# catch anything is a safety net nobody has tested.
#
# It cannot be exercised by sending: a real fallback TYPES INTO A PEER'S PANE.
# So this drives the two shipped functions directly, against an isolated
# CC_HOME, and asserts the row lands server-side with the right type and origin.
#
# Exit 0 = pass, 1 = failure, 2 = skipped (server unreachable — the test needs a
# live /api/history and says so rather than passing vacuously).
set -uo pipefail
cd "$(dirname "$0")/.."

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   — $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL — $1"; }

API="${AMUX_API:-https://localhost:8824}"
if ! curl -sk -m 5 -o /dev/null "$API/health" 2>/dev/null; then
  echo "SKIP: $API/health unreachable — this test needs a live server to reconcile into."
  echo "      Skipping is deliberate: passing without a server would assert nothing."
  exit 2
fi

# EXTRACT THE SHIPPED FUNCTIONS rather than reimplementing them. A copy here
# would pass forever while the real ones rot — the failure this file exists to
# prevent, one level up.
_fn_range() {  # $1 = function name -> "start,end" of its definition
  awk -v want="$1" '
    $0 ~ "^"want"\\(\\) *\\{" { start=NR; depth=0 }
    start && /\{/ { depth += gsub(/\{/,"{") }
    start && /\}/ { depth -= gsub(/\}/,"}"); if (depth<=0) { print start","NR; exit } }
  ' amux
}
# TRANSITIVELY. Extracting a function without its callees is the same rot this
# file was built to catch, one layer down, and it had already happened: AMUX-40
# (978645c0) swapped the bare `curl` inside _flush_unstamped_ledger for the
# hang-guarded `_curl` helper, which this test did not extract. `_curl` was then
# an undefined command, its rc-127 hit the flush's own "server went away
# mid-flush: KEEP the row" branch, and three assertions went red reading:
#
#   FAIL — the row never reached /api/history — every fallback send would be lost
#
# A green server, a working POST (verified by hand), and a test insisting the
# audit trail was broken. The diagnosis pointed at the mechanism; the defect was
# in the harness reading it. So walk the call graph instead of naming three
# functions and hoping the list stays complete.
SRC=""; HAVE=" "; NEED="_unstamped_ledger _record_unstamped_send _flush_unstamped_ledger"
while [ -n "$NEED" ]; do
  NEXT=""
  for f in $NEED; do
    case "$HAVE" in *" $f "*) continue ;; esac
    r=$(_fn_range "$f")
    if [ -z "$r" ]; then
      bad "could not locate $f in ./amux — the CLI moved and this test is now blind"
      echo; echo "$PASS passed, $FAIL failed"; exit 1
    fi
    body=$(sed -n "${r}p" amux)
    SRC="$SRC$body"$'\n'
    HAVE="$HAVE$f "
    for dep in $(printf '%s\n' "$body" | grep -oE '\b_[a-z0-9_]+' | sort -u); do
      case "$HAVE$NEXT " in *" $dep "*) continue ;; esac
      grep -q "^$dep() {" amux && NEXT="$NEXT $dep"
    done
  done
  NEED="$NEXT"
done

TMPHOME=$(mktemp -d)
trap 'rm -rf "$TMPHOME"' EXIT
export CC_HOME="$TMPHOME" AMUX_SESSION="ledger-selftest" AMUX_API="$API"
GREEN=""; RESET=""
eval "$SRC"

for f in _unstamped_ledger _record_unstamped_send _flush_unstamped_ledger; do
  if ! type -t "$f" >/dev/null; then
    bad "$f did not survive extraction — the assertions below would be vacuous"
    echo; echo "$PASS passed, $FAIL failed"; exit 1
  fi
done
ok "ledger functions + callees extracted from the shipped CLI:$HAVE"
# The closure is only worth having if a missing callee is LOUD. Without this,
# the next helper swap reproduces AMUX-40 exactly: an undefined command, a
# silent rc, and a red assertion pointing at the wrong subsystem.
MISSING=""
for dep in $(printf '%s\n' "$SRC" | grep -oE '\b_[a-z0-9_]+' | sort -u); do
  grep -q "^$dep() {" amux || continue          # not a CLI function, not our problem
  type -t "$dep" >/dev/null 2>&1 || MISSING="$MISSING $dep"
done
if [ -z "$MISSING" ]; then
  ok "every CLI helper the extracted code calls is defined here (no silent rc-127)"
else
  bad "extracted code calls undefined CLI helper(s):$MISSING — assertions below would blame the ledger for a harness gap"
fi

MARK="__ledgerselftest$(date +%s)$$__"
LEDGER=$(_unstamped_ledger)

# 1. A fallback send is RECORDED locally, because the server cannot be told.
_record_unstamped_send "ledger-selftest-target" "$MARK body"
if [ "$(wc -l < "$LEDGER" 2>/dev/null || echo 0)" -ge 1 ]; then
  ok "a fallback send is recorded to the local ledger"
else
  bad "nothing was recorded — a fallback send would leave no trace at all"
fi

# 2. The flush RECONCILES it into the audit trail and clears the local file.
_flush_unstamped_ledger >/dev/null 2>&1
if [ ! -s "$LEDGER" ]; then
  ok "the ledger is cleared after a successful flush"
else
  bad "rows remain after flush — they would be re-sent forever"
fi

# 3. THE ONE THAT MATTERS: it landed server-side, typed so an unstamped
#    injection is distinguishable from an audited send.
FOUND=$(curl -sk -m 10 "$API/api/history?limit=50" 2>/dev/null \
  | MARK="$MARK" python3 -c "
import json,os,sys
mark=os.environ['MARK']
try: d=json.load(sys.stdin)
except Exception: print('0|'); sys.exit(0)
rows=d if isinstance(d,list) else (d.get('rows') or d.get('items') or [])
hit=[r for r in rows if mark in json.dumps(r)]
print(f\"{len(hit)}|{hit[0].get('type','') if hit else ''}\")" 2>/dev/null)
N="${FOUND%%|*}"; TYPE="${FOUND##*|}"
if [ "${N:-0}" -ge 1 ] && [ "$TYPE" = "raw-tmux-fallback" ]; then
  ok "reconciled into the audit trail as type=raw-tmux-fallback"
elif [ "${N:-0}" -ge 1 ]; then
  bad "row landed but typed '$TYPE' — an unstamped send must be distinguishable"
else
  bad "the row never reached /api/history — every fallback send would be lost"
fi

# 4. THE UNIT. cmd_history.ts is MILLISECONDS (declared in
#    invariants/checks.rs TIMESTAMP_COLUMNS). The ledger wrote SECONDS, so
#    every reconciled row landed dated 1970-01-21 and sorted below every real
#    row — /api/history?limit=50 never returned one. The send was recorded and
#    was unfindable where anyone looks, which is the failure this ledger exists
#    to prevent, reached by the ledger itself.
#
#    Asserted against the NEWEST row the endpoint returns rather than a literal,
#    so it stays true as the clock moves and cannot pass by coincidence.
NEWEST=$(curl -sk -m 10 "$API/api/history?limit=1" 2>/dev/null \
  | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(0); sys.exit(0)
rows=d if isinstance(d,list) else []
print(rows[0].get('ts') or 0 if rows else 0)" 2>/dev/null)
if [ "${NEWEST:-0}" -gt 100000000000 ]; then
  ok "the endpoint's newest row is in milliseconds (control: the column's unit is what we think)"
else
  bad "control failed — the newest history row is not millis, so the check below proves nothing"
fi
MYTS=$(curl -sk -m 10 "$API/api/history?limit=50" 2>/dev/null \
  | MARK="$MARK" python3 -c "
import json,os,sys
mark=os.environ['MARK']
try: d=json.load(sys.stdin)
except Exception: print(0); sys.exit(0)
rows=d if isinstance(d,list) else []
hit=[r for r in rows if mark in json.dumps(r)]
print(hit[0].get('ts') or 0 if hit else 0)" 2>/dev/null)
if [ "${MYTS:-0}" -gt 100000000000 ]; then
  ok "the reconciled row carries a MILLISECOND ts, so it sorts with real rows"
else
  bad "the reconciled row's ts is ${MYTS:-0} — seconds into a millis column dates it 1970 and \
hides it from every time-ordered view"
fi

# 5. WHAT THE FALLBACK TELLS ITS USER (AF-454). Sections 1-4 prove the ledger
#    reconciles. This one proves the tool SAYS SO, which is a separate claim and
#    was false for as long as the ledger has existed: the closing message ended
#    "no origin stamp, no audit" and was printed one line AFTER
#    _record_unstamped_send wrote the audit row.
#
#    That cost a real measurement pass. gtm-engine hit a server flap on
#    2026-09-03, read "no audit" literally, and filed a provenance gap that
#    AMUX-2670 had already closed; their own send was in the trail the whole
#    time as MSG-40621 (type raw-tmux-fallback, origin "unstamped-fallback from
#    gtm-engine"). A mechanism the user is told does not exist is a mechanism
#    that does not reach them (ethos rule 1).
#
#    Asserted against the SOURCE, deliberately. This block cannot be executed
#    without typing into a peer's live pane, which is the same reason sections
#    1-4 drive the functions directly instead of sending. A static assertion
#    that can fail beats a dynamic one that cannot run.
#    And it reads ONLY the `echo` lines. The first draft of this check matched
#    the whole block and went red against the FIXED code, because the comment
#    explaining the old wording quotes "no audit" verbatim. A check that reads
#    the prose around a line instead of the line is pinned to the wrong layer,
#    and would have passed just as happily on a revert that kept the comment.
BLOCK=$(sed -n '/^  _record_unstamped_send /,/^  return 0$/p' amux | grep '^ *echo ')
if [ -z "$BLOCK" ]; then
  bad "could not locate the fallback's closing block in ./amux — this check proves nothing"
else
  case "$BLOCK" in
    *"no audit"*) bad "the fallback still says 'no audit' one line after recording the audit row (AF-454)" ;;
    *) ok "the fallback no longer claims 'no audit' while writing the audit row" ;;
  esac
  case "$BLOCK" in
    *"reconciles into the audit trail"*) ok "it tells the sender the send is recorded and will reconcile" ;;
    *) bad "nothing tells the sender the send was recorded — they will read the warning as loss" ;;
  esac
  # The remedy must not depend on the server whose unreachability is the only
  # reason this branch runs (ethos rule 3). tmux must come BEFORE the curl.
  T_POS=$(printf '%s' "$BLOCK" | grep -n 'tmux capture-pane' | head -1 | cut -d: -f1)
  C_POS=$(printf '%s' "$BLOCK" | grep -n 'curl -sk' | head -1 | cut -d: -f1)
  if [ -n "$T_POS" ] && [ -n "$C_POS" ] && [ "$T_POS" -lt "$C_POS" ]; then
    ok "the server-independent remedy (tmux) is offered before the curl"
  elif [ -z "$T_POS" ]; then
    bad "the only verification offered is a curl at the server that was just proved unreachable"
  else
    bad "the curl is printed above the tmux remedy — the reader tries the dead one first"
  fi
  # $tname, not $name: gtm-engine ran `tmux has-session -t gtm-ticker` against a
  # session actually called amux-gtm-ticker, found nothing, and briefly read a
  # DELIVERED message as lost (the 2026-07-27 shape). And the trailing colon is
  # load-bearing: `-t "=$tname"` fails with "can't find pane".
  CAP=$(printf '%s\n' "$BLOCK" | grep 'capture-pane' | head -1)
  case "$CAP" in
    *'=$tname:'*) ok "the tmux remedy uses the REAL prefixed session name, with the colon capture-pane needs" ;;
    *'$tname'*)   bad "the tmux remedy names \$tname but drops the trailing colon — capture-pane answers \"can't find pane\"" ;;
    *'$name'*)    bad "the tmux remedy interpolates the FLEET name; the tmux session is prefixed and it will find nothing" ;;
    *)            bad "the tmux remedy does not name the session at all" ;;
  esac
  # Scrollback, not the viewport. A bare capture-pane returns the current frame,
  # which is the trap CLAUDE.md documents for peek: a full-screen picker clears
  # the screen and the message being looked for scrolls off.
  case "$CAP" in
    *'capture-pane -p -S -'*) ok "the tmux remedy reads scrollback, not just the viewport" ;;
    *) bad "capture-pane without -S returns the viewport only — the peek/output trap, in the remedy" ;;
  esac
fi

# 6. WHAT THE RECEIVER SEES (AF-455). Sections 1-5 are all about the SENDER:
#    what is recorded, and what the sender is told. This one is the other side.
#
#    A send that reaches the server arrives stamped "[amux-origin: <lane> —
#    server-verified ...]". An injection used to arrive with no prefix at all,
#    making it shape-identical to a prompt typed by the OWNER — whose turns
#    carry standing authority the sending peer does not have.
KEYS=$(grep -n 'tmux send-keys .* -l "' amux | head -1)
case "$KEYS" in
  *'-l "$marked"'*) ok "the injected body carries a marker, not the bare text" ;;
  *'-l "$text"'*)   bad "the injection is sent bare — the receiver cannot tell it from an owner prompt (AF-455)" ;;
  *)                bad "could not find the fallback's send-keys body line; this check proves nothing" ;;
esac
# The marker must assert the ABSENCE of verification. A marker that claimed
# identity would be the body signature AMUX-1768 forbids.
MARKER=$(grep -n 'local marked=' amux | head -1)
if [ -z "$MARKER" ]; then
  bad "no marker is constructed for fallback injections"
else
  case "$MARKER" in
    *'NOT server-verified'*) ok "the marker asserts the absence of verification, not an identity (AMUX-1768)" ;;
    *) bad "the marker does not say it is unverified — a prefix that merely names a sender is the forgeable kind AMUX-1768 forbids" ;;
  esac
  case "$MARKER" in
    *'\n'*) bad "the marker embeds a newline — send-keys -l would submit it as a prompt of its own" ;;
    *) ok "the marker is a single line, so the separately-sent Enter still submits body and marker together" ;;
  esac
fi
# The AUDIT row keeps the original text. The marker is for the human reading the
# pane; a trail that stored the decorated string would drift from what was sent.
case "$(grep -n '_record_unstamped_send "' amux | tail -1)" in
  *'_record_unstamped_send "$name" "$text"'*) ok "the audit row records the ORIGINAL body, undecorated" ;;
  *) bad "the audit row no longer records \$text — the trail and the pane would disagree" ;;
esac

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
