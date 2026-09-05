#!/usr/bin/env bash
# Canonical status-hook installation and payload regression cells.
set -euo pipefail
cd "$(dirname "$0")/.."
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
SETTINGS="$TMP/settings.json"

printf '%s\n' '{
  "model": "keep-me",
  "hooks": {
    "Stop": [{"hooks": [
      {"type":"command","command":"echo unrelated"},
      {"type":"command","command":"curl $AMUX_URL/api/sessions/$AMUX_SESSION/report"}
    ]}],
    "PostToolUse": [{"matcher":"Write","hooks":[
      {"type":"command","command":"bash check-format.sh"}
    ]}]
  }
}' > "$SETTINGS"

for _ in 1 2; do
  /usr/bin/python3 scripts/hooks/install-claude-status-hooks.py \
    --settings "$SETTINGS" --hook-path '$HOME/.amux/hook-report.sh' >/dev/null
done

/usr/bin/python3 - "$SETTINGS" <<'PY'
import json, sys
v=json.load(open(sys.argv[1]))
assert v["model"] == "keep-me"
hooks=v["hooks"]
required={"SessionStart","UserPromptSubmit","PostToolUse","Stop","SubagentStart","SubagentStop"}
assert required <= set(hooks)
rows=[]
for event, groups in hooks.items():
    for group in groups:
        for hook in group.get("hooks", []):
            rows.append((event, group.get("matcher"), hook.get("command", "")))
reports=[r for r in rows if "hook-report.sh" in r[2]]
assert len(reports) == 6, reports
assert len([r for r in reports if r[0] == "PostToolUse" and r[1] == ".*"]) == 1
assert any(r[2] == "echo unrelated" for r in rows)
assert any(r[2] == "bash check-format.sh" for r in rows)
assert not any("/api/sessions/" in r[2] and "hook-report.sh" not in r[2] for r in rows)
print("ok   installer is idempotent and preserves unrelated hooks/settings")
PY

# Drive the shipped hook against a disposable HTTP endpoint. The endpoint can
# refuse requests or drop a response after reading the body, which exercises
# the durable producer rather than a paraphrase of its JSON encoder.
mkdir -p "$TMP/home/.amux/logs"
CAPTURE="$TMP/requests.jsonl" PORT_FILE="$TMP/port" DOWN="$TMP/down" LOSS="$TMP/loss"
cat > "$TMP/server.py" <<'PY'
import http.server,json,os,socketserver,sys
capture,port_file,down,loss=sys.argv[1:]
class Handler(http.server.BaseHTTPRequestHandler):
    transient_seen=set()
    def do_POST(self):
        raw=self.rfile.read(int(self.headers.get("content-length","0")))
        try: body=json.loads(raw)
        except Exception: body={"invalid":raw.decode(errors="replace")}
        session=self.headers.get("X-Amux-Session","")
        expected=f"/api/sessions/{session}/report"
        row={"body":body,"down":os.path.exists(down),"path":self.path,
             "expected_path":expected,"session":session}
        with open(capture,"a") as stream:
            stream.write(json.dumps(row,separators=(",",":"))+"\n"); stream.flush()
        if self.path != expected:
            self.send_response(405); self.end_headers(); return
        if os.path.exists(down):
            self.send_response(503); self.end_headers(); return
        if body.get("agent_id")=="poison-agent":
            self.send_response(400); self.end_headers(); return
        agent=str(body.get("agent_id", ""))
        if agent.startswith("transient-") and agent not in self.transient_seen:
            self.transient_seen.add(agent)
            self.send_response(int(agent.split("-",1)[1])); self.end_headers(); return
        if os.path.exists(loss):
            os.unlink(loss)
            self.connection.shutdown(2)
            self.connection.close()
            return
        self.send_response(200); self.send_header("content-type","application/json")
        self.end_headers(); self.wfile.write(b'{"ok":true}')
    def log_message(self,*args): pass
class Server(socketserver.TCPServer): allow_reuse_address=True
with Server(("127.0.0.1",0),Handler) as server:
    with open(port_file,"w") as stream: stream.write(str(server.server_address[1]))
    server.serve_forever()
PY
/usr/bin/python3 "$TMP/server.py" "$CAPTURE" "$PORT_FILE" "$DOWN" "$LOSS" &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true; rm -rf "$TMP"' EXIT
for _ in $(seq 1 100); do [ -s "$PORT_FILE" ] && break; sleep .02; done
URL="http://127.0.0.1:$(cat "$PORT_FILE")"

wait_for() {
  local expr="$1"
  for _ in $(seq 1 300); do
    /usr/bin/python3 - "$CAPTURE" "$expr" <<'PY' && return 0
import json,sys
try: rows=[json.loads(line) for line in open(sys.argv[1]) if line.strip()]
except FileNotFoundError: rows=[]
raise SystemExit(0 if eval(sys.argv[2],{"rows":rows}) else 1)
PY
    sleep .05
  done
  echo "timed out waiting for hook requests: $expr" >&2
  return 1
}

HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
  bash scripts/hooks/hook-report.sh subagent-start subagent-start-hook \
  <<<'{"session_id":"abc-123","agent_id":"explore-1"}'
wait_for 'any(r["body"].get("agent_id")=="explore-1" for r in rows)'
/usr/bin/python3 - "$CAPTURE" <<'PY'
import json,sys
v=next(json.loads(line)["body"] for line in open(sys.argv[1]) if "explore-1" in line)
assert v["subagent"] == "start" and "state" not in v, v
assert v["source"] == "subagent-start-hook" and v["session_id"] == "abc-123", v
assert v["event_id"] == "abc-123:explore-1:start", v
assert v["delivery_attempt"] == 1, v
print("ok   SubagentStart carries session, agent and stable event identity")
PY
/usr/bin/python3 - "$CAPTURE" <<'PY'
import json,sys
rows=[json.loads(line) for line in open(sys.argv[1])]
assert all(r["path"]==r["expected_path"] for r in rows),rows
assert rows[-1]["path"]=="/api/sessions/probe/report",rows[-1]
print("ok   durable lifecycle delivery uses the exact session report route")
PY

# Two identity-less callbacks with identical payloads are two real agents, not
# duplicate delivery. Only retries of each queued row reuse its minted id.
for _ in 1 2; do
  HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
    bash scripts/hooks/hook-report.sh subagent-start subagent-start-hook <<<'{}'
done
wait_for 'len([r for r in rows if str(r["body"].get("event_id","")).startswith("anonymous:")]) >= 2'
/usr/bin/python3 - "$CAPTURE" <<'PY'
import json,sys
ids=[json.loads(line)["body"].get("event_id","") for line in open(sys.argv[1])]
ids=[i for i in ids if i.startswith("anonymous:")]
assert len(ids)>=2 and len(set(ids[-2:]))==2, ids
print("ok   two empty-payload starts mint distinct identities")
PY

# Malformed provider payloads are still distinct hook invocations. The hook
# fails open, emits valid lifecycle JSON, and never falls back to an empty key.
for _ in 1 2; do
  HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
    bash scripts/hooks/hook-report.sh subagent-start malformed-hook <<<'{broken'
done
wait_for 'len([r for r in rows if r["body"].get("source")=="malformed-hook"]) >= 2'
/usr/bin/python3 - "$CAPTURE" <<'PY'
import json,sys
rows=[json.loads(line)["body"] for line in open(sys.argv[1])]
ids=[r.get("event_id","") for r in rows if r.get("source")=="malformed-hook"]
assert len(ids)>=2 and all(i.startswith("anonymous:") for i in ids[-2:]),ids
assert len(set(ids[-2:]))==2,ids
print("ok   malformed lifecycle payloads mint distinct nonempty identities")
PY

# Response loss after the server read the event must retry the same identity.
touch "$LOSS"
HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
  bash scripts/hooks/hook-report.sh subagent-stop subagent-stop-hook \
  <<<'{"session_id":"abc-123","agent_id":"explore-loss"}'
wait_for 'len([r for r in rows if r["body"].get("agent_id")=="explore-loss"]) >= 2'
/usr/bin/python3 - "$CAPTURE" <<'PY'
import json,sys
rows=[json.loads(line)["body"] for line in open(sys.argv[1])]
rows=[r for r in rows if r.get("agent_id")=="explore-loss"]
assert rows[-2]["event_id"]==rows[-1]["event_id"], rows[-2:]
assert rows[-2]["delivery_attempt"]==1 and rows[-1]["delivery_attempt"]==2, rows[-2:]
print("ok   response-loss retry preserves event identity")
PY

# Outage recovery is FIFO: start reaches the rebuilt endpoint before stop.
touch "$DOWN"
for mode in start stop; do
  HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
    bash scripts/hooks/hook-report.sh "subagent-$mode" "subagent-$mode-hook" \
    <<EOF
{"session_id":"abc-123","agent_id":"ordered-agent"}
EOF
done
sleep .1; rm "$DOWN"
wait_for 'len([r for r in rows if not r["down"] and r["body"].get("agent_id")=="ordered-agent"]) >= 2'
/usr/bin/python3 - "$CAPTURE" <<'PY'
import json,sys
rows=[json.loads(line) for line in open(sys.argv[1])]
events=[r["body"]["subagent"] for r in rows if not r["down"] and r["body"].get("agent_id")=="ordered-agent"]
assert events[:2]==["start","stop"], events
print("ok   lifecycle start/stop replay in FIFO order after outage")
PY

# A permanent 4xx is a poison event, not an outage: dead-letter it with full
# identity and continue to the valid row behind it.
for agent in poison-agent after-poison; do
  HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
    bash scripts/hooks/hook-report.sh subagent-start subagent-start-hook \
    <<EOF
{"session_id":"abc-123","agent_id":"$agent"}
EOF
done
wait_for 'any(r["body"].get("agent_id")=="after-poison" for r in rows)'
grep -q 'lifecycle_queue=dead_letter.*verdict=non_retryable_http.*agent_id=poison-agent.*event=start' \
  "$TMP/home/.amux/logs/hook-report-failures.log"
echo "ok   poison 4xx dead-letters and does not head-of-line block the next event"

# Transient 4xx classes stay at the FIFO head and retry with the same identity.
for code in 408 409 425 429; do
  HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
    bash scripts/hooks/hook-report.sh subagent-start subagent-start-hook \
    <<EOF
{"session_id":"abc-123","agent_id":"transient-$code"}
EOF
  wait_for "len([r for r in rows if r[\"body\"].get(\"agent_id\")==\"transient-$code\"]) >= 2"
done
/usr/bin/python3 - "$CAPTURE" <<'PY'
import json,sys
rows=[json.loads(line)["body"] for line in open(sys.argv[1])]
for code in (408,409,425,429):
    got=[r for r in rows if r.get("agent_id")==f"transient-{code}"]
    assert [r["delivery_attempt"] for r in got[:2]]==[1,2],got
    assert got[0]["event_id"]==got[1]["event_id"],got
print("ok   transient 408/409/425/429 responses retry without reordering")
PY

# Main-turn reports are latest-wins, but still durable. Reproduce Primis at
# 15:22: active was already stored, Stop got http=000 during a rebuild, and the
# prompt stayed WORKING until the active trust window expired. The detached
# drain must replay the idle row without a later hook, while the older active
# attempt may never land after the endpoint returns.
touch "$DOWN"
HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=state-replay \
  bash scripts/hooks/hook-report.sh active state-outage-hook <<<'{}'
sleep .15
HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=state-replay \
  bash scripts/hooks/hook-report.sh idle state-outage-hook <<<'{}'
sleep .15
rm "$DOWN"
wait_for 'any(not r["down"] and r["body"].get("source")=="state-outage-hook" for r in rows)'
/usr/bin/python3 - "$CAPTURE" "$TMP/home/.amux/hook-report-queue/state-replay.state.json" <<'PY'
import json,sys,time
rows=[json.loads(line) for line in open(sys.argv[1])]
delivered=[r["body"]["state"] for r in rows
           if not r["down"] and r["body"].get("source")=="state-outage-hook"]
assert delivered and delivered[0]=="idle",delivered
assert "active" not in delivered,delivered
for _ in range(100):
    try:
        if json.load(open(sys.argv[2]))==[]: break
    except FileNotFoundError: break
    time.sleep(.02)
else: raise AssertionError("state replay row did not clear")
print("ok   lost Stop report replays latest idle without a later hook")
PY
grep -q 'state-replay source=state-outage-hook.*state_queue=retrying.*state=active' \
  "$TMP/home/.amux/logs/hook-report-failures.log"
grep -q 'state-replay source=state-outage-hook.*state_queue=delivered.*verdict=replayed_state.*state=idle' \
  "$TMP/home/.amux/logs/hook-report-failures.log"
echo "ok   successful state replay is sweep-visible with identity and verdict"

# A queue left by an expired drain is awakened by an ordinary prompt hook.
QF="$TMP/home/.amux/hook-report-queue/probe.json"
/usr/bin/python3 - "$QF" "$URL" <<'PY'
import json,os,sys
os.makedirs(os.path.dirname(sys.argv[1]),exist_ok=True)
body={"subagent":"start","source":"survivor","session_id":"abc-123",
      "agent_id":"surviving-queue","event_id":"surviving:event","event_ts":1.0}
json.dump([{"event_id":"surviving:event","body":body,"url":sys.argv[2],"attempts":90}],open(sys.argv[1],"w"))
PY
HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
  bash scripts/hooks/hook-report.sh active prompt-hook <<<'{}'
wait_for 'any(r["body"].get("agent_id")=="surviving-queue" for r in rows)'
wait_for 'any(r["body"].get("state")=="active" for r in rows)'
echo "ok   ordinary state hook wakes and route-corrects a surviving legacy queue"

# Corruption is evidence, not an empty queue. Preserve the exact bad bytes,
# announce the verdict, and let the new event proceed in a fresh atomic file.
printf '%s' '{not-json' > "$QF"
HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
  bash scripts/hooks/hook-report.sh subagent-start corrupt-successor \
  <<<'{"session_id":"abc-123","agent_id":"after-corrupt"}'
wait_for 'any(r["body"].get("agent_id")=="after-corrupt" for r in rows)'
CORRUPT=$(find "$TMP/home/.amux/hook-report-queue" -name 'probe.json.corrupt.*' -print -quit)
test -n "$CORRUPT"
test "$(cat "$CORRUPT")" = '{not-json'
printf '%s' '{}' > "$QF"
HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
  bash scripts/hooks/hook-report.sh subagent-start corrupt-schema-successor \
  <<<'{"session_id":"abc-123","agent_id":"after-schema-corrupt"}'
wait_for 'any(r["body"].get("agent_id")=="after-schema-corrupt" for r in rows)'
test "$(find "$TMP/home/.amux/hook-report-queue" -name 'probe.json.corrupt.*' | wc -l | tr -d ' ')" -ge 2
grep -q 'lifecycle_queue=corrupt.*verdict=preserved_corrupt_queue.*preserved=' \
  "$TMP/home/.amux/logs/hook-report-failures.log"
echo "ok   corrupt queue bytes and schemas are preserved and diagnosed before recovery"

# A replacement drain that starts while another process owns the drain lock
# waits for the bounded handoff instead of losing the only wakeup.
RACE_Q="$TMP/home/.amux/hook-report-queue/race.json"
RACE_MARK="$TMP/race-lock-held"
/usr/bin/python3 - "$RACE_Q.drain.lock" "$RACE_MARK" <<'PY' &
import fcntl,sys,time
with open(sys.argv[1],"a+") as lock:
    fcntl.flock(lock,fcntl.LOCK_EX)
    open(sys.argv[2],"w").close()
    time.sleep(.5)
PY
RACE_HOLDER=$!
for _ in $(seq 1 100); do [ -e "$RACE_MARK" ] && break; sleep .01; done
HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=race \
  bash scripts/hooks/hook-report.sh subagent-start race-hook \
  <<<'{"session_id":"race-session","agent_id":"race-agent"}'
wait "$RACE_HOLDER"
wait_for 'any(r["body"].get("agent_id")=="race-agent" for r in rows)'
echo "ok   drain-lock handoff cannot lose the final enqueue wakeup"

# Compact is not a process reset; startup is. Pin both paths.
before=$(wc -l < "$CAPTURE")
HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
  bash scripts/hooks/hook-report.sh subagent-reset session-start-hook <<<'{"source":"compact","session_id":"abc-123"}'
sleep .1
test "$(wc -l < "$CAPTURE")" -eq "$before"
HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=probe \
  bash scripts/hooks/hook-report.sh subagent-reset session-start-hook <<<'{"source":"startup","session_id":"abc-123"}'
wait_for 'any(r["body"].get("subagent")=="reset" for r in rows)'
echo "ok   startup resets subagents while compact preserves them"

# The producer queue remains bounded under a prolonged outage and announces
# overflow in the hook failure log. Recover afterward so no drain is orphaned.
touch "$DOWN"
for _ in $(seq 1 12); do
  HOME="$TMP/home" AMUX_URL="$URL" AMUX_SESSION=bounded AMUX_HOOK_QUEUE_LIMIT=8 \
    bash scripts/hooks/hook-report.sh subagent-start subagent-start-hook <<<'{}'
done
/usr/bin/python3 - "$TMP/home/.amux/hook-report-queue/bounded.json" <<'PY'
import json,sys
rows=json.load(open(sys.argv[1])); assert len(rows)==8,len(rows)
assert len({r["event_id"] for r in rows})==8
print("ok   durable lifecycle queue obeys its bounded cap with distinct events")
PY
grep -q 'lifecycle_queue=overflow.*limit=8' "$TMP/home/.amux/logs/hook-report-failures.log"
rm "$DOWN"
for _ in $(seq 1 400); do
  pending=$(/usr/bin/python3 - "$TMP/home/.amux/hook-report-queue/bounded.json" <<'PY'
import json,sys
try: print(len(json.load(open(sys.argv[1]))))
except Exception: print(0)
PY
  )
  [ "$pending" -eq 0 ] && break
  sleep .05
done
test "$pending" -eq 0

# Successes and dead letters do not consume the retry budget: one drain must
# empty every row allowed by the production queue bound, not stop at row 90.
BULK_Q="$TMP/home/.amux/hook-report-queue/bulk.json"
/usr/bin/python3 - "$BULK_Q" "$URL/api/sessions/bulk/report" <<'PY'
import json,os,sys
rows=[]
for i in range(128):
    event_id=f"bulk:{i}"
    body={"subagent":"start","source":"bulk-test","session_id":"bulk-session",
          "agent_id":f"bulk-{i}","event_id":event_id,"event_ts":i+1}
    rows.append({"event_id":event_id,"body":body,"url":sys.argv[2],"attempts":0})
json.dump(rows,open(sys.argv[1],"w"))
PY
HOME="$TMP/home" bash scripts/hooks/hook-report.sh --drain-subagents \
  "$BULK_Q" "$URL/api/sessions/bulk/report" bulk
/usr/bin/python3 - "$BULK_Q" "$CAPTURE" <<'PY'
import json,sys
assert json.load(open(sys.argv[1]))==[]
rows=[json.loads(line) for line in open(sys.argv[2])]
got=[r for r in rows if r["body"].get("source")=="bulk-test"]
assert len(got)==128,len(got)
assert all(r["path"]=="/api/sessions/bulk/report" for r in got)
print("ok   one drain empties all 128 bounded FIFO rows")
PY

/usr/bin/python3 - "$CAPTURE" <<'PY'
import json,sys
rows=[json.loads(line) for line in open(sys.argv[1])]
bad=[r for r in rows if r["path"]!=r["expected_path"]]
assert not bad,bad
print("ok   every captured hook request used its exact worker report path")
PY

echo "ok   all shipped status-hook durability regressions passed"
