#!/usr/bin/env bash
# test-pipeline-bottlenecks-constraints.sh: the hourly detector (AH-398) names
# the top GS-12 constraint with its numbers and acts on it. Proof stalled ranks
# first when it costs the most finish-date hours; the former tripwire rule
# (three stalled runs, then Ethan, once a day) holds; orchestrator-owned
# constraints queue one line for the batched message and never send; an
# unreadable source is reported, not read as healthy.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d "${TMPDIR:-/tmp}/pbc.XXXXXX")"; trap 'rm -rf -- "${T:?}"' EXIT
export AMUX_BOTTLENECKS_OUT="$T/out.jsonl" AMUX_BOTTLENECKS_STATE="$T/state.json"
# A stub amux on PATH: the detector alerts Ethan and messages lanes through
# `amux`, and a test must never reach the real one (2026-10-07: a mutation run
# of this test sent Ethan a real alert carrying fixture numbers).
mkdir -p "$T/bin"
printf '#!/bin/sh\necho "$*" >> "%s/amux-calls.log"\n' "$T" > "$T/bin/amux"; chmod +x "$T/bin/amux"
export PATH="$T/bin:$PATH"
now=$(date +%s)
python3 - "$T" "$now" <<'PY'
import json,sys
T,now=sys.argv[1],int(sys.argv[2])
b=[]
# 20 proof cards on the hub, 3 verified in the last 24 h, 2 active on lanes.
for i in range(20):
    b.append({"id":f"MO-{i}","session":"mixpeek-override","title":f"GS12 proof {i}: x","status":"backlog","depends_on":[]})
for i in range(3):
    b.append({"id":f"GE-{i}","session":"gs12-extra-1","title":f"GS12 proof v{i}","status":"verified","closed_at":now-3600})
for i in range(2):
    b.append({"id":f"GD-{i}","session":"gs12-deputy","title":f"GS12 proof a{i}","status":"doing"})
b.append({"id":"MO-99","session":"mixpeek-override","title":"GS12 proof blocked","status":"backlog","depends_on":["GX-1"]})
b.append({"id":"GX-1","session":"gs12-data","title":"plan item","status":"doing"})
json.dump(b,open(f"{T}/board_stalled.json","w"))
good=[{"id":f"V-{i}","session":"gs12-extra-1","title":f"GS12 proof {i}","status":"verified","closed_at":now-3600} for i in range(10)]
good+=[{"id":f"A-{i}","session":"gs12-extra-2","title":f"GS12 proof a{i}","status":"doing"} for i in range(12)]
json.dump(good,open(f"{T}/board_ok.json","w"))
# Live shape, 2026-10-07 12:25Z: a good last 24 h (10 verified) but only 2 proof
# cards in work, and a review backlog that outranks proof.
mix=[{"id":f"V-{i}","session":"gs12-extra-1","title":f"GS12 proof {i}","status":"verified","closed_at":now-3600} for i in range(10)]
mix+=[{"id":f"A-{i}","session":"gs12-extra-2","title":f"GS12 proof a{i}","status":"doing"} for i in range(2)]
mix+=[{"id":f"R-{i}","session":"gs12-model","title":f"card {i}","status":"done","entered_state_at":now-30*3600} for i in range(200)]
json.dump(mix,open(f"{T}/board_mixed.json","w"))
json.dump([{"name":"gs12-a","status":"idle"},{"name":"gs12-b","status":"active"}],open(f"{T}/sessions.json","w"))
json.dump({"wait_p95_min":5,"n_considered":3},open(f"{T}/land.json","w"))
json.dump({"deploy_sha":"04bec9a4f2b8"},open(f"{T}/prod.json","w"))
PY
python3 - "$T" <<'PY'
import sqlite3,sys
c=sqlite3.connect(sys.argv[1]+"/db.sqlite")
c.execute("CREATE TABLE card_contracts (card TEXT PRIMARY KEY, review_state TEXT)")
c.commit()
PY
run() { python3 "$ROOT/scripts/pipeline-bottlenecks.py" --constraints-only --orchestrator-asks "$T/asks.txt" --db "$T/db.sqlite" \
        --sessions-file "$T/sessions.json" --land-file "$T/land.json" --prod-file "$T/prod.json" --rules-off "6,7" "$@" 2>&1 | grep '"top_constraint"\|"no_constraint"' | tail -1; }
fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: expected '$2', got '$3'"; fail=1; fi; }
out=$(run --board-file "$T/board_stalled.json" --dry-run)
v() { printf '%s' "$out" | python3 -c "import json,sys;d=json.loads(sys.stdin.read());print($1)"; }
check "stalled proof is the top constraint" "proof_stalled" "$(v 'd["name"]')"
check "with its numbers: 3 in 24 h, 2 active, 20 ready on the hub" "3 2 20" "$(v '" ".join(str(d["numbers"][k]) for k in ("verified_24h","active_on_lanes","ready_on_hub"))')"
check "a card with an open dependency is not ready" "20" "$(v 'd["numbers"]["ready_on_hub"]')"
check "the run reports what it considered" "yes" "$(v '"yes" if d["measured"] and d["n_considered"]>0 else "no"')"
# The tripwire rule: queue for the orchestrator every run, alert Ethan from the third.
rm -f "$T/state.json"
for k in 1 2; do out=$(run --board-file "$T/board_stalled.json"); done
check "two stalled runs queue for the orchestrator and do not alert" "no" "$(v '"yes" if "alert" in d["action"] else "no"')"
check "the orchestrator line is queued once an hour, not per run" "1" "$(grep -c 'GS-12 proof' "$T/asks.txt")"
check "and nothing alerted yet" "0" "$(cat "$T/amux-calls.log" 2>/dev/null | grep -c '^alert')"
out=$(run --board-file "$T/board_stalled.json")
check "the third stalled run alerts Ethan" "1" "$(grep -c '^alert' "$T/amux-calls.log")"
out=$(run --board-file "$T/board_stalled.json")
check "and not again within a day" "1" "$(grep -c '^alert' "$T/amux-calls.log")"
rm -f "$T/state.json" "$T/amux-calls.log"
for k in 1 2 3; do out=$(run --board-file "$T/board_mixed.json"); done
check "with review ranked first, too few active proof cards still trip the alert" "review_backlog 1" "$(v 'd["name"]') $(cat "$T/amux-calls.log" 2>/dev/null | grep -c '^alert')"
out=$(run --board-file "$T/board_ok.json" --dry-run)
check "proof on pace is not the constraint" "no" "$(v '"yes" if d["name"]=="proof_stalled" else "no"')"
out=$(run --board-file "$T/missing.json" --dry-run)
check "an unreadable board is reported as unmeasured" "yes" "$(v '"yes" if "board" in d.get("why_unmeasured","") else "no"')"

# --- Lever feedback loop and the two 2026-10-07 probes ---------------------
python3 - "$T" "$now" <<'PY'
import json,sqlite3,sys
T,now=sys.argv[1],int(sys.argv[2])
# 12 open proof cards on the hub chained into one web through plan items.
w=[{"id":f"PL-{i}","session":"mixpeek-override","title":f"plan {i}","status":"backlog","depends_on":[f"PL-{i-1}"] if i else []} for i in range(12)]
w+=[{"id":f"WP-{i}","session":"mixpeek-override","title":f"GS12 proof w{i}","status":"backlog","depends_on":[f"PL-{i}"]} for i in range(12)]
# Done cards 10 h old: D-1 is under review, D-2 has no review state, D-3 has no row, D-4 is an epic.
w+=[{"id":f"D-{i}","session":"gs12-data","title":f"d{i}","status":"done","type":"epic" if i==4 else "code","entered_state_at":now-36000} for i in range(1,5)]
json.dump(w,open(f"{T}/board_web.json","w"))
c=sqlite3.connect(f"{T}/db.sqlite")
c.executemany("INSERT INTO card_contracts VALUES (?,?)",[("D-1","pending"),("D-2",None)])
c.commit()
PY
rm -f "$T/state.json" "$T/amux-calls.log" "$T/asks.txt"
web() { run --board-file "$T/board_web.json" --now "$1" | python3 -c "import json,sys;d=json.loads(sys.stdin.read());r={x['name']:x['est_hours'] for x in d['ranked']};print($2)"; }
check "proof cards inside a web over the cap are counted" "6.0" "$(web $now 'r["proof_blocked_by_web"]')"
out=$(run --board-file "$T/board_web.json" --now $now --dry-run)
check "done cards with no review state and no row are unreachable; review in progress and epics are not" "0.5" "$(v '{x["name"]:x["est_hours"] for x in d["ranked"]}["review_unreachable"]')"
check "a lever reaches the web and the harness gap, not only the top constraint" "yes yes" "$(v '("yes" if "; proof_blocked_by_web: " in d["action"] else "no")+" "+("yes" if "; review_unreachable: " in d["action"] else "no")')"
rm -f "$T/state.json" "$T/amux-calls.log" "$T/asks.txt" "$T/out.jsonl"
for k in 0 1 2 3; do out=$(run --board-file "$T/board_stalled.json" --now $((now + k * 4 * 3600))); done
check "each unmoved lever is judged no_effect after the eval window" "3" "$(grep '"name": "proof_stalled"' "$T/out.jsonl" | grep -c '"outcome": "no_effect"')"
check "a proof escalation shares the tripwire's one alert a day" "1 1" "$(grep -c '^alert' "$T/amux-calls.log") $(grep -c 'deduped: the proof tripwire' "$T/out.jsonl")"
check "the run publishes the lever record" "0/3" "$(v 'd["lever_record"].get("proof_stalled")')"
python3 - "$T" "$now" <<'PY'
import json,sys
T,now=sys.argv[1],int(sys.argv[2])
r=[{"id":f"V-{i}","session":"gs12-extra-1","title":f"GS12 proof {i}","status":"verified","closed_at":now-3600} for i in range(10)]
r+=[{"id":f"A-{i}","session":"gs12-extra-2","title":f"GS12 proof a{i}","status":"doing"} for i in range(12)]
r+=[{"id":f"R-{i}","session":"gs12-model","title":f"card {i}","status":"done","entered_state_at":now-30*3600} for i in range(40)]
json.dump(r,open(f"{T}/board_review.json","w"))
c=__import__("sqlite3").connect(f"{T}/db.sqlite")
c.executemany("INSERT INTO card_contracts VALUES (?,?)",[(f"R-{i}","pending") for i in range(40)])
c.commit()
PY
rm -f "$T/state.json" "$T/amux-calls.log" "$T/asks.txt"
for k in 0 1 2 3; do out=$(run --board-file "$T/board_review.json" --now $((now + k * 4 * 3600))); done
check "three no_effect levers on an orchestrator constraint escalate it to Ethan" "1" "$(grep -c '^alert GS-12 review_backlog: three levers' "$T/amux-calls.log")"
check "the review lever names the action, oldest first" "yes" "$(grep -q 'verify or reopen them oldest first' "$T/asks.txt" && echo yes)"
rm -f "$T/state.json" "$T/amux-calls.log" "$T/asks.txt"
for k in 0 1; do out=$(run --board-file "$T/board_stalled.json" --now $((now + k * 4 * 3600))); done
# A moved number is recorded as moved and resets the run.
python3 - "$T" "$now" <<'PY'
import json,sys
T,now=sys.argv[1],int(sys.argv[2])
b=json.load(open(f"{T}/board_stalled.json"))
b+=[{"id":f"NV-{i}","session":"gs12-extra-1","title":f"GS12 proof n{i}","status":"verified","closed_at":now+15*3600} for i in range(2)]
json.dump(b,open(f"{T}/board_moved.json","w"))
PY
out=$(run --board-file "$T/board_moved.json" --now $((now + 16 * 3600)))
check "a lever whose number moved is recorded as moved" "1" "$(grep -c '"outcome": "moved"' "$T/out.jsonl")"
exit $fail
