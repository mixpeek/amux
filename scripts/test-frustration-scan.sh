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
# A long forwarded body, the thing both G messages carry and neither wrote.
QUOTE = ("Thanks Happy to. The one that would help me most is not really a roadmap "
         "idea it is the direction call on one three four. I posted the inventory and "
         "I am sitting on the follow up until you say whether the legacy dispatcher is "
         "the supported path or the workers api gets completed. Either is fine by me I "
         "would just rather not build one and be told it should have been the other. "
         "One sentence unblocks it. Roadmap wise the thing I would put at the top from "
         "reading your commits is a theme you are already naming yourself outputs that "
         "read the same whether things are healthy or broken.")
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
    # C: SAME text, 31 HOURS apart, DIFFERENT sessions -> still a fan-out, and it
    # reaches the REPEAT branch rather than the double-delivery one. Cell A's pair
    # is 12s apart, so it only ever exercised the sub-60s branch — which was the
    # only branch the session guard was in. The bug lived one branch over, and a
    # cell asserting exactly the right property sat green through it because its
    # fixture could not reach the code that was wrong.
    (5, "whats the status?", "user", "random",      now - 112_000_000, "", "", "direct"),
    (6, "whats the status?", "user", "tubescience", now -   1_000_000, "", "", "direct"),
    # D: SAME text, 31 hours apart, SAME session -> a genuine repeat, and it must
    # survive. Without this, deleting the repeat branch entirely passes cell C.
    (7, "make me a table of all the outstanding items", "user", "ts", now - 112_000_000, "", "", "direct"),
    (8, "make me a table of all the outstanding items", "user", "ts", now -   1_000_000, "", "", "direct"),
    # E: a CONTINUATION 9s later — "and <more work>". Not a chase. The reprompt
    # kind means "a lane went quiet or a delivery did not land"; this is Ethan
    # finishing one thought in two messages. Two real specimens on 2026-08-25
    # (gtm-ticker 9s, primis 8s) both scored as reprompts under `and\?*`.
    (9,  "yes make the first touch land on contextual ad matching", "user", "gt", now - 600_000, "", "", "direct"),
    (10, "and send the email with the ns",                          "user", "gt", now - 591_000, "", "", "direct"),
    # F: a BARE prod 9s later — "and?" IS a chase and must still be caught, or
    # the fix is a hollowing-out that passes cell E by reporting nothing.
    (11, "go over the retriever numbers once more", "user", "pr", now - 400_000, "", "", "direct"),
    (12, "and?",                                    "user", "pr", now - 391_000, "", "", "direct"),
    # G: two DIFFERENT asks that both forward the SAME long email (AF-255). Plain
    # jaccard scored the QUOTE, not the ask, so these read as a repeated request
    # at 0.70-1.00. Live specimen 2026-08-26: three forwards of one contributor
    # email, Ethan's own words "do what u think is best", "did you apply these
    # suggestions", "do we have these captured" — the top TWO candidates of the
    # run, at scores 15 and 12.
    (13, "do what u think is best u have full autonomy\n\n" + QUOTE, "user", "af", now - 300_000, "", "", "direct"),
    (14, "do we have these captured and working where appropriate\n\n" + QUOTE, "user", "af", now - 200_000, "", "", "direct"),
    # H: a GENUINE repeat that also happens to quote the same thing must SURVIVE.
    # Without this, stripping the shared run everywhere passes cell G by
    # reporting nothing — the hollowing-out.
    (15, "whats the status on this\n\n" + QUOTE, "user", "hh", now - 300_000, "", "", "direct"),
    (16, "whats the status on this\n\n" + QUOTE, "user", "hh", now - 200_000, "", "", "direct"),
    # I: a pasted MEETING TRANSCRIPT. Its markers are spoken by participants, not
    # by Ethan. Scored 13 on 2026-08-26 — the second-highest candidate of the run
    # — on still/again/i-said/already/you-didnt/no-response, none of them his.
    (17, "Meeting Title: Sync\nMeeting participants: Ethan, Dan\n"
         "Dan: i said this already and you didnt do it, still no response, again",
         "user", "tt", now - 150_000, "", "", "direct"),
    # J: the SAME marker words, written by ETHAN, must still score. Without this,
    # stripping everything passes cell I by scoring nothing.
    (18, "this still doesnt work and i said already you didnt fix it",
         "user", "jj", now - 140_000, "", "", "direct"),
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

# Cell C: a cross-session pair with a LARGE gap must not be reported either.
# This is the specimen from the 2026-08-24 sweep: id 31240 to `random` and id
# 31973 to `tubescience`, both "whats the status?", 31.4h apart, reported as
# `random` repeating itself with jaccard 1.00 — the top-scoring candidate of the
# run, and a lane that had never repeated anything.
if echo "$OUT" | python3 -c '
import json,sys
d=json.load(sys.stdin)
ids={m["id"] for f in d["findings"] for m in f["messages"]}
sys.exit(0 if not ({5,6} & ids) else 1)'; then
  ok "C: a cross-session pair 31h apart is NOT a repeat (the branch cell A could not reach)"
else
  bad "C: a cross-session pair fell through to the repeat branch — the comment's promise, unimplemented"
  echo "$OUT" | head -30 | sed 's/^/       /'
fi

# Cell D: and the repeat detector must still fire on a SAME-session repeat.
# Cell C alone is satisfied by a scanner that reports no repeats at all.
if echo "$OUT" | python3 -c '
import json,sys
d=json.load(sys.stdin)
hit=[f for f in d["findings"] if f["kind"]=="repeat"
     and {m["id"] for m in f["messages"]} == {7,8}]
sys.exit(0 if hit else 1)'; then
  ok "D: a same-session repeat 31h apart IS still reported (cell C did not hollow it out)"
else
  bad "D: the same-session repeat was NOT reported — the repeat detector is now inert"
  echo "$OUT" | head -30 | sed 's/^/       /'
fi

# Cell E: "and <more work>" is a CONTINUATION, not a re-prompt.
if echo "$OUT" | python3 -c '
import json,sys
d=json.load(sys.stdin)
ids={m["id"] for f in d["findings"] if f["kind"]=="reprompt" for m in f["messages"]}
sys.exit(0 if 10 not in ids else 1)'; then
  ok "E: \"and <more work>\" is a continuation, not a chase"
else
  bad "E: a continuation scored as a re-prompt — the kind means a lane went quiet"
  echo "$OUT" | head -30 | sed 's/^/       /'
fi

# Cell F: a BARE "and?" must STILL be caught. Without this, deleting the token
# entirely passes cell E perfectly.
if echo "$OUT" | python3 -c '
import json,sys
d=json.load(sys.stdin)
ids={m["id"] for f in d["findings"] if f["kind"]=="reprompt" for m in f["messages"]}
sys.exit(0 if 12 in ids else 1)'; then
  ok "F: a bare \"and?\" IS still a re-prompt (cell E did not hollow it out)"
else
  bad "F: the bare prod stopped being detected — the discriminator is now inert"
  echo "$OUT" | head -30 | sed 's/^/       /'
fi

echo
# Cell G: two different asks sharing one long quote must NOT be a repeat.
if echo "$OUT" | python3 -c '
import json,sys
d=json.load(sys.stdin)
ids={m["id"] for f in d["findings"] for m in f["messages"]}
sys.exit(0 if not ({13,14} & ids) else 1)'; then
  ok "G: two different asks forwarding the SAME email are NOT a repeated request"
else
  bad "G: the quote was scored instead of the ask — the 2026-08-26 top-2 artifact"
  echo "$OUT" | head -40 | sed 's/^/       /'
fi

# Cell H: a genuine repeat that also quotes must still be caught.
if echo "$OUT" | python3 -c '
import json,sys
d=json.load(sys.stdin)
hit=[f for f in d["findings"] if f["kind"]=="repeat"
     and {13,14}.isdisjoint({m["id"] for m in f["messages"]})
     and {15,16} & {m["id"] for m in f["messages"]}]
sys.exit(0 if hit else 1)'; then
  ok "H: an IDENTICAL ask that also quotes IS still a repeat (cell G did not hollow it out)"
else
  bad "H: a genuine repeat was suppressed by the quote-stripping — hollowed out"
  echo "$OUT" | head -40 | sed 's/^/       /'
fi

# Cell I: markers inside a pasted transcript are not Ethan's.
if echo "$OUT" | python3 -c '
import json,sys
d=json.load(sys.stdin)
ids={m["id"] for f in d["findings"] for m in f["messages"]}
sys.exit(0 if 17 not in ids else 1)'; then
  ok "I: markers spoken by MEETING PARTICIPANTS are not scored as Ethan's"
else
  bad "I: a pasted transcript was scored as Ethan's frustration"
  echo "$OUT" | head -40 | sed 's/^/       /'
fi

# Cell J: the same words, written by him, must still score.
if echo "$OUT" | python3 -c '
import json,sys
d=json.load(sys.stdin)
ids={m["id"] for f in d["findings"] for m in f["messages"]}
sys.exit(0 if 18 in ids else 1)'; then
  ok "J: the SAME marker words written by ETHAN still score (cell I did not hollow it out)"
else
  bad "J: marker detection is now inert — stripping went too far"
  echo "$OUT" | head -40 | sed 's/^/       /'
fi

echo "frustration-scan cells: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
