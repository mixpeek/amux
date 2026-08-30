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
SRC=""
for f in _unstamped_ledger _record_unstamped_send _flush_unstamped_ledger; do
  r=$(_fn_range "$f")
  if [ -z "$r" ]; then
    bad "could not locate $f in ./amux — the CLI moved and this test is now blind"
    echo; echo "$PASS passed, $FAIL failed"; exit 1
  fi
  SRC="$SRC$(sed -n "${r}p" amux)"$'\n'
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
ok "all three ledger functions extracted from the shipped CLI"

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

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
