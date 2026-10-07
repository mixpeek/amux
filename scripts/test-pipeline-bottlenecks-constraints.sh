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
json.dump([{"name":"gs12-a","status":"idle"},{"name":"gs12-b","status":"active"}],open(f"{T}/sessions.json","w"))
json.dump({"wait_p95_min":5,"n_considered":3},open(f"{T}/land.json","w"))
json.dump({"deploy_sha":"04bec9a4f2b8"},open(f"{T}/prod.json","w"))
PY
run() { python3 "$ROOT/scripts/pipeline-bottlenecks.py" --constraints-only --orchestrator-asks "$T/asks.txt" \
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
out=$(run --board-file "$T/board_ok.json" --dry-run)
check "proof on pace is not the constraint" "no" "$(v '"yes" if d["name"]=="proof_stalled" else "no"')"
out=$(run --board-file "$T/missing.json" --dry-run)
check "an unreadable board is reported as unmeasured" "yes" "$(v '"yes" if "board" in d.get("why_unmeasured","") else "no"')"
exit $fail
