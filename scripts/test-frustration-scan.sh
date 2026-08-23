#!/usr/bin/env bash
# Cells for scripts/frustration_scan.py's double-delivery classifier.
#
# WHY THIS EXISTS. The detector fired on `gap < 60s AND similarity > 0.98` with no
# session check, so ONE INSTRUCTION FANNED TO TWO LANES read as a delivery defect.
# Measured on the real store 2026-08-23: ids 31157/31158, identical text 12s apart,
# to nissan and autodesk. The finding text told the reader "this is a DELIVERY
# defect, not frustration: check send_dedup" — a confident wrong answer about a bug
# that does not exist, which is worse than silence because it is actionable and the
# action is wasted.
#
# The cells run the SHIPPED scanner against a synthetic store via AMUX_DB, rather
# than restating its logic: simulating what you believe a classifier does cannot
# catch it doing something else.
#
# Cell B is the one that stops the "fix" being a hollowing-out: a scanner that
# classified NOTHING as double-delivery would pass cell A perfectly.
set -uo pipefail
cd "$(dirname "$0")/.."
# Overridable so the pre-fix copy can be run through the SAME cells (rule 7:
# a check that cannot fail on the case that motivated it is theatre).
SCAN="${FRUSTRATION_SCAN:-$(pwd)/scripts/frustration_scan.py}"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
DB="$TMP/t.db"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   — $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL — $1"; }

NOW_MS=$(python3 -c 'import time;print(int(time.time()*1000))')
python3 - "$DB" "$NOW_MS" <<'PY'
import sqlite3, sys
db, now = sys.argv[1], int(sys.argv[2])
c = sqlite3.connect(db)
c.execute("CREATE TABLE cmd_history (id INTEGER PRIMARY KEY, text TEXT, type TEXT, "
          "session TEXT, ts INTEGER, origin TEXT, card_id TEXT, delivery TEXT)")
rows = [
    # A: SAME text, 12s apart, DIFFERENT sessions -> a fan-out, not a defect.
    (1, "do them all nothing needed for me.", "user", "nissan",   now - 600_000,      "", "", "direct"),
    (2, "do them all nothing needed for me.", "user", "autodesk", now - 588_000,      "", "", "direct"),
    # B: SAME text, 8s apart, SAME session -> a real double-delivery candidate.
    (3, "test it with all the gsuite shit for a b and c", "user", "amux", now - 500_000, "", "", "direct"),
    (4, "test it with all the gsuite shit for a b and c", "user", "amux", now - 492_000, "", "", "direct"),
]
c.executemany("INSERT INTO cmd_history VALUES (?,?,?,?,?,?,?,?)", rows)
c.commit()
PY

OUT=$(AMUX_DB="$DB" python3 "$SCAN" 2>&1)
echo "$OUT" | python3 -c 'import json,sys; json.load(sys.stdin)' 2>/dev/null || {
  echo "  FAIL — scanner did not emit JSON:"; echo "$OUT" | head -5; exit 1; }

# Cell A: the cross-session pair must not be reported at all, under ANY kind.
if echo "$OUT" | python3 -c '
import json,sys
d=json.load(sys.stdin)
ids={m["id"] for f in d["findings"] for m in f["messages"]}
sys.exit(0 if not ({1,2} & ids) else 1)'; then
  ok "A: one instruction fanned to two lanes is NOT reported (not a defect, not a repeat)"
else
  bad "A: a cross-session fan-out was reported — the false positive this file exists for"
  echo "$OUT" | head -30 | sed 's/^/       /'
fi

# Cell B: the same-session pair MUST still be classified double-delivery.
if echo "$OUT" | python3 -c '
import json,sys
d=json.load(sys.stdin)
hit=[f for f in d["findings"] if f["kind"]=="double-delivery"
     and {m["id"] for m in f["messages"]} == {3,4}]
sys.exit(0 if hit else 1)'; then
  ok "B: a same-session identical pair IS still double-delivery (the fix did not hollow it out)"
else
  bad "B: the same-session pair was NOT classified double-delivery — detector is now inert"
  echo "$OUT" | head -30 | sed 's/^/       /'
fi

echo
echo "frustration-scan cells: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
