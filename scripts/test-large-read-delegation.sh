#!/usr/bin/env bash
# Large-read routing: the real hook, installer-facing command, and CLI wire shape.
set -euo pipefail
cd "$(dirname "$0")/.."

TMP="$(mktemp -d)"
SERVER_PID=""
cleanup() {
  [[ -z "$SERVER_PID" ]] || kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

# The PASS line must be COMPUTED. It read "PASS (9 outcome cells)" as a literal,
# and stayed 9 after five checks were added — a verdict you hardcode cannot
# disagree with the run, so it reads as measured to every reader including the
# author. The test is what input would change this output; for a constant there
# is none.
CELLS=0
ok() { CELLS=$((CELLS + 1)); printf "ok   %s\n" "$*"; }
mkdir -p "$TMP/home/.amux/logs"

python3 - "$TMP/small.txt" "$TMP/large.txt" <<'PY'
import sys
open(sys.argv[1], "w").write("one\ntwo\n")
open(sys.argv[2], "w").write("".join(f"line {i}\n" for i in range(1, 9)))
PY

hook() {
  HOME="$TMP/home" AMUX_HOME="$TMP/home/.amux" AMUX_SESSION=test-lane \
    CC_ISOLATED="${HOOK_ISOLATED:-0}" \
    AMUX_BULK_READ_LINES=3 python3 scripts/hooks/large-read-guard.py
}

read_payload() {
  FILE="$1" LIMIT="${2:-}" python3 - <<'PY'
import json,os
tool={"file_path":os.environ["FILE"]}
if os.environ.get("LIMIT"):
    tool["limit"]=int(os.environ["LIMIT"])
print(json.dumps({"tool_name":"Read","cwd":"/","tool_input":tool}))
PY
}

# Heredoc bodies are not shell argv. Valid prose must not become probe failures.
python3 - <<'PYTEST'
import importlib.util
spec=importlib.util.spec_from_file_location("read_guard", "scripts/hooks/large-read-guard.py")
guard=importlib.util.module_from_spec(spec);spec.loader.exec_module(guard)
for command in ["python3 - <<'EOF'\nprint(\"don't parse this as shell\")\nEOF", "cat small.txt && python3 - <<'EOF'\nowner's note\nEOF"]:
    assert guard.simple_shell_argv(command)==[]
assert guard.simple_shell_argv("cat 'file with spaces.txt'")==['cat','file with spaces.txt']
try:
    guard.simple_shell_argv("cat 'invalid")
except ValueError:
    pass
else:
    raise AssertionError("malformed simple commands must remain diagnostic failures")
PYTEST
ok "complex shell and heredoc bodies stop before unsupported lexical parsing"

# GMA-123: an IMAGE must NOT be routed to `amux delegate read`. The delegate is a
# text summarizer and died on the first byte of any PNG (0x89) with a raw
# UnicodeDecodeError, while this guard was actively recommending it — the guard's
# remedy could not open the file type the guard blocks. For a lane that ships
# images the blocked artifact IS the deliverable.
printf '\x89PNG\r\n\x1a\n' > "$TMP/big.png"
for i in 1 2 3 4 5 6 7 8; do printf 'binary junk %s\n' "$i" >> "$TMP/big.png"; done
binary_out="$(read_payload "$TMP/big.png" | hook 2>&1 || true)"
case "$binary_out" in
  *"looks BINARY"*) ok "an image is named as binary, not sent to the text helper" ;;
  *) echo "FAIL image not identified as binary: $binary_out" >&2; exit 1 ;;
esac
# The binary message MENTIONS `amux delegate read` on purpose, to say not to use
# it, so "does the string appear" is the wrong test — it fails on the correct
# message. What must be absent is the RECOMMENDATION, whose shape is the
# `--question` invocation the text arm prints.
case "$binary_out" in
  *"--question"*)
    echo "FAIL the refusal still hands an image to the delegate — that path ends in a decode error" >&2
    exit 1 ;;
  *) ok "it does NOT hand an image to the delegate" ;;
esac
case "$binary_out" in
  *"cannot open it"*) ok "and it says why, rather than silently omitting the option" ;;
  *) echo "FAIL the binary refusal does not explain why the delegate is wrong here" >&2; exit 1 ;;
esac
case "$binary_out" in
  *"limit="*) ok "it names the remedy that works (Read with an explicit limit)" ;;
  *) echo "FAIL no usable remedy named: $binary_out" >&2; exit 1 ;;
esac

# THE CONTROL. A large TEXT file must still be delegated — without this the binary
# branch could swallow everything and the guard would stop doing its job.
text_out="$(read_payload "$TMP/large.txt" | hook 2>&1 || true)"
case "$text_out" in
  *"amux delegate read"*) ok "a large TEXT read is still delegated (control)" ;;
  *) echo "FAIL the binary branch swallowed the text case: $text_out" >&2; exit 1 ;;
esac
case "$text_out" in
  *"looks BINARY"*) echo "FAIL a .txt was called binary" >&2; exit 1 ;;
  *) ok "a .txt is not called binary (control)" ;;
esac

# Positive control: a small full read is allowed.
read_payload "$TMP/small.txt" | hook
ok "small full Read stays with the primary model"

# The actual threshold crossing must block with the sanctioned alternative.
set +e
blocked="$(read_payload "$TMP/large.txt" | hook 2>&1)"
rc=$?
set -e
[[ "$rc" -eq 2 ]]
[[ "$blocked" == *"amux delegate read --question"* ]]
[[ "$blocked" == *"explicit limit of 3 lines or fewer"* ]]
python3 - "$TMP/home/.amux/logs/read-delegation.jsonl" <<'PY'
import json,sys
rows=[json.loads(line) for line in open(sys.argv[1]) if line.strip()]
row=rows[-1]
assert row["event"]=="bulk_read_guard" and row["verdict"]=="delegation_required",row
assert row["threshold_lines"]==3 and row["lines_seen_min"]==4,row
assert row["session"]=="test-lane",row
PY
ok "oversized Read blocks and writes a measured audit verdict"

# Editing/debugging keeps a truthful bounded path on the primary model.
read_payload "$TMP/large.txt" 3 | hook
ok "explicit bounded Read remains available"

# The ordinary shell bypass is covered, while a narrowing pipe remains legal.
set +e
shell_blocked="$(CMD="cat $TMP/large.txt" python3 - <<'PY' | hook 2>&1
import json,os
print(json.dumps({"tool_name":"Bash","cwd":"/","tool_input":{"command":os.environ["CMD"]}}))
PY
)"
shell_rc=$?
set -e
[[ "$shell_rc" -eq 2 && "$shell_blocked" == *"bulk-read router"* ]]
CMD="cat $TMP/large.txt | rg 'line 8'" python3 - <<'PY' | hook
import json,os
print(json.dumps({"tool_name":"Bash","cwd":"/","tool_input":{"command":os.environ["CMD"]}}))
PY
ok "direct cat blocks and a narrowing pipeline passes"

# Global Claude settings also apply outside amux and to isolated raw agents.
# Neither may inherit this fleet policy merely because the hook is installed.
read_payload "$TMP/large.txt" | HOOK_ISOLATED=1 hook
read_payload "$TMP/large.txt" | HOME="$TMP/home" AMUX_HOME="$TMP/home/.amux" \
  CC_ISOLATED=0 AMUX_SESSION= AMUX_WORKER= AMUX_BULK_READ_LINES=3 \
  python3 scripts/hooks/large-read-guard.py
ok "isolated and unmanaged Claude processes remain untouched"

# A bad hook payload must fail open and announce that its probe did not run.
printf '%s' '{broken' | hook
grep -q '"verdict":"probe_failed"' "$TMP/home/.amux/logs/read-delegation.jsonl"
ok "malformed hook input fails open with probe_failed evidence"

# Exercise the public CLI contract against a real HTTP listener. This catches
# quoting, JSON construction, bearer forwarding and receipt parsing together.
CAPTURE="$TMP/request.json" PORT_FILE="$TMP/port"
python3 - "$CAPTURE" "$PORT_FILE" <<'PY' &
import http.server,json,socketserver,sys
capture,port_file=sys.argv[1:]
class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        raw=self.rfile.read(int(self.headers.get("content-length","0")))
        row={"path":self.path,"authorization":self.headers.get("authorization"),
             "session":self.headers.get("x-amux-session"),"body":json.loads(raw)}
        open(capture,"w").write(json.dumps(row))
        if row["body"]["question"] == "force helper error":
            response={"error":"helper provider is rate-limited","measured":True,
                      "n_considered":1,"board_evidence":{"measured":True,
                      "n_considered":1,"verdict":"attached","card_id":"AMUX-T"}}
            status=429
        else:
            response={"text":"- Found it (sample.txt:2)","via":"api:haiku",
                      "measured":True,"n_considered":1,"input_lines":2,
                      "input_bytes":11,"elapsed_ms":17,
                      "board_evidence":{"measured":True,"n_considered":1,
                                        "verdict":"attached","card_id":"AMUX-T"}}
            status=200
        body=json.dumps(response).encode()
        self.send_response(status); self.send_header("content-type","application/json")
        self.send_header("content-length",str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self,*args): pass
class Server(socketserver.TCPServer): allow_reuse_address=True
with Server(("127.0.0.1",0),Handler) as server:
    open(port_file,"w").write(str(server.server_address[1]))
    server.handle_request(); server.handle_request()
PY
SERVER_PID=$!
for _ in $(seq 1 100); do [[ -s "$PORT_FILE" ]] && break; sleep .02; done
printf '%s\n' 'auth-test-token' > "$TMP/home/.amux/auth_token"
printf 'alpha\nbeta\n' > "$TMP/sample.txt"
set +e
cli_output="$(HOME="$TMP/home" AMUX_API="http://127.0.0.1:$(<"$PORT_FILE")" \
  AMUX_SESSION=test-lane bash ./amux delegate read --question 'Where is beta?' \
  --for AMUX-T -- "$TMP/sample.txt" 2>"$TMP/cli.err")"
cli_rc=$?
set -e
[[ "$cli_rc" -eq 0 && "$cli_output" == *"Found it"* ]]
grep -q 'verdict=completed via=api:haiku files=1 lines=2 bytes=11 elapsed_ms=17' "$TMP/cli.err"
grep -q 'board=attached:AMUX-T' "$TMP/cli.err"
python3 - "$CAPTURE" "$TMP/sample.txt" <<'PY'
import json,sys
row=json.load(open(sys.argv[1]))
assert row["path"]=="/api/lookup/bulk",row
assert row["authorization"]=="Bearer auth-test-token",row
assert row["session"]=="test-lane",row
assert row["body"]["question"]=="Where is beta?",row
assert row["body"]["task"]=="AMUX-T",row
assert row["body"]["files"]==[{"path":sys.argv[2],"content":"alpha\nbeta\n"}],row
PY

# JSON mode must preserve the measured failure receipt AND return nonzero. A
# machine-readable error with exit 0 is still a false success to automation.
set +e
json_error="$(HOME="$TMP/home" AMUX_API="http://127.0.0.1:$(<"$PORT_FILE")" \
  AMUX_SESSION=test-lane bash ./amux delegate read --json \
  --question 'force helper error' --for AMUX-T -- "$TMP/sample.txt" 2>"$TMP/json.err")"
json_rc=$?
set -e
[[ "$json_rc" -eq 1 ]]
JSON_ERROR="$json_error" python3 - <<'PY'
import json,os
row=json.loads(os.environ["JSON_ERROR"])
assert row["error"]=="helper provider is rate-limited",row
assert row["board_evidence"]=={"measured":True,"n_considered":1,
                              "verdict":"attached","card_id":"AMUX-T"},row
PY
wait "$SERVER_PID"
SERVER_PID=""
ok "CLI sends caller-read bytes and prints the helper receipt"

ok "JSON errors keep their receipt and return nonzero"

echo "test-large-read-delegation: PASS ($CELLS outcome cells)"
