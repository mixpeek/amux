#!/usr/bin/env bash
# test-pipeline-bottlenecks-throughput.sh: the throughput checks flag a deploy
# workflow that mostly fails, lanes with no card and nothing eligible, and aged
# owner asks; each is quiet below its threshold, and an unreadable source reads
# unmeasured, never ok (2026-10-05: the landing queue read ok all afternoon
# while 1 deploy in 20 succeeded).
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d "${TMPDIR:-/tmp}/pbt.XXXXXX")"; trap 'rm -rf -- "${T:?}"' EXIT
export AMUX_BOTTLENECKS_OUT="$T/out.jsonl" AMUX_BOTTLENECKS_STATE="$T/state.json"
now=$(date +%s)
python3 - "$T" "$now" <<'PY'
import json,sys
T,now=sys.argv[1],int(sys.argv[2])
runs=[{"id":i,"status":"completed","conclusion":"failure","failed":["platform__promote: Post-promote acceptance (ingest checks)"]} for i in range(5)]
runs+=[{"id":9,"status":"completed","conclusion":"success"},{"id":10,"status":"in_progress","conclusion":None}]
json.dump(runs,open(f"{T}/runs_bad.json","w"))
json.dump([{"id":i,"status":"completed","conclusion":"success"} for i in range(5)],open(f"{T}/runs_good.json","w"))
lanes=[{"session":"gs12-a","card":None,"reason":"blocker-recovery-unchanged","eligible_todos":0},
       {"session":"gs12-b","card":None,"reason":"no-eligible","eligible_todos":0},
       {"session":"gs12-c","card":None,"reason":"mid-turn","eligible_todos":0},
       {"session":"gs12-d","card":"X-1","reason":"active-claim-current","eligible_todos":0},
       {"session":"gs12-e","card":None,"reason":"held","eligible_todos":2},
       {"session":"other","card":None,"reason":"no-eligible","eligible_todos":0}]
json.dump({"last":{"lanes":lanes}},open(f"{T}/drive.json","w"))
ny=[{"id":f"A-{i}","status":"needsyou","ask_actor":"Ethan","session":"gs12-a","entered_state_at":now-5*3600} for i in range(3)]
ny+=[{"id":"A-9","status":"needsyou","ask_actor":"Ethan","session":"gs12-a","entered_state_at":now-600},
     {"id":"B-1","status":"needsyou","ask_actor":"someone","session":"gs12-a","entered_state_at":now-9*3600}]
json.dump(ny,open(f"{T}/ny.json","w"))
PY
run() { python3 "$ROOT/scripts/pipeline-bottlenecks.py" --dry-run --throughput-only "$@" 2>&1 | grep '"throughput' | tail -1; }
fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: expected '$2', got '$3'"; fail=1; fi; }
out=$(run --deploy-repo o/r --deploy-workflow w.yml --runs-file "$T/runs_bad.json" --lane-prefix gs12- --drive-file "$T/drive.json" --owner ethan --needsyou-file "$T/ny.json")
v() { printf '%s' "$out" | python3 -c "import json,sys;d=json.loads(sys.stdin.read());print($1)"; }
check "a mostly failing deploy workflow is a bottleneck" "throughput_bottleneck" "$(v 'd["verdict"]')"
check "it counts 1 of 6 concluded" "1 6" "$(v 'str(d["deploy"]["succeeded"])+" "+str(d["deploy"]["concluded"])')"
check "and names the failing step" "yes" "$(v '"yes" if any("Post-promote acceptance" in k for k,_ in d["deploy"]["top_failing_steps"]) else "no"')"
check "idle lanes are only the prefix's lanes with no card and nothing eligible" "gs12-a gs12-b" "$(v '" ".join(x["lane"] for x in d["idle_lanes"])')"
check "aged owner asks count only the owner's, over the age" "3" "$(v 'len(d["owner_asks_aged"])')"
out=$(run --deploy-repo o/r --deploy-workflow w.yml --runs-file "$T/runs_bad.json")
check "the deploy check alone flags a mostly failing workflow" "throughput_bottleneck" "$(v 'd["verdict"]')"
out=$(run --lane-prefix gs12- --drive-file "$T/drive.json")
check "the idle check alone flags two idle lanes" "throughput_bottleneck" "$(v 'd["verdict"]')"
out=$(run --owner ethan --needsyou-file "$T/ny.json")
check "the owner-ask check alone flags three aged asks" "throughput_bottleneck" "$(v 'd["verdict"]')"
out=$(run --deploy-repo o/r --deploy-workflow w.yml --runs-file "$T/runs_good.json")
check "a green deploy workflow is ok" "throughput_ok" "$(v 'd["verdict"]')"
out=$(run --lane-prefix gs12- --drive-file "$T/missing.json")
check "an unreadable source is unmeasured, not ok-with-zero" "unmeasured" "$(v 'd["idle_lanes"]')"
exit $fail
