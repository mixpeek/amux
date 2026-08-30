#!/usr/bin/env bash
# AEAB-17 — `amux board add` must REFUSE an unknown flag, not fold it into the title.
#
# The defect: `add` parsed its arguments as `else title="$*"`, so every flag became
# part of the title with exit 0 and no warning. One log-review run filed five cards
# that way — titles up to 259 chars carrying raw
# `--type blocker --desc-file /private/tmp/.../c26.md` (a scratch path that would not
# exist the next day), empty descs, and the DEFAULT type, so two cards about a
# production outage were gated on "Implemented and merged" + "Tests / lint pass".
# Separately `amux board add --help` FILED A CARD TITLED "--help": the one action taken
# to discover the interface was the action that polluted the board.
#
# These run the REAL dispatch path as a subprocess. Every case below is decided
# BEFORE any network call, which is deliberate and is what makes this runnable in CI
# with no server: a parser that only refused after a successful POST would still have
# created the card. AMUX_API is pointed at a closed port so that if any case ever does
# reach curl, it fails loudly instead of quietly mutating a real board.
#
# Exit 0 = all pass, 1 = a failure. Wired into .github/workflows/checks.yml.
set -uo pipefail
cd "$(dirname "$0")/.."
AMUX_BIN="${AMUX_BIN:-./amux}"
PASS=0; FAIL=0

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
export AMUX_SESSION="addflagtest"
# AMUX_API, *not* AMUX_URL. The board verbs read `AMUX_API` (`local
# AMUX_API="${AMUX_API:-https://localhost:8824}"`); AMUX_URL only feeds the `url`
# resolver. The first cut of this file exported AMUX_URL, so every case that
# reached the network hit the developer's REAL board on :8824 and filed three junk
# cards (ADDFL-1..3) — while cases (a)-(c) stayed green, because a parse refusal
# never gets that far. Case (d) is what caught it. Both belt and braces below:
# port 9 is reserved/discard so nothing can listen.
export AMUX_API="https://127.0.0.1:9"
export AMUX_URL="https://127.0.0.1:9"

# Run the shipped CLI with a controlled HOME. stdout+stderr captured together
# because refusals print to stderr; the exit code is the load-bearing part.
run() { HOME="$TMP" "$AMUX_BIN" board add "$@" 2>&1; }

check_rc() { # label expected_rc actual_rc
  if [ "$2" = "$3" ]; then PASS=$((PASS+1));
  else FAIL=$((FAIL+1)); echo "FAIL: $1 — expected exit $2, got $3"; fi
}
check_has() { # label needle haystack
  case "$3" in
    *"$2"*) PASS=$((PASS+1)) ;;
    *) FAIL=$((FAIL+1)); echo "FAIL: $1 — output did not contain '$2'"; echo "  got: $(printf '%s' "$3" | head -3)" ;;
  esac
}
check_lacks() { # label needle haystack
  case "$3" in
    *"$2"*) FAIL=$((FAIL+1)); echo "FAIL: $1 — output should NOT contain '$2'"; echo "  got: $(printf '%s' "$3" | head -3)" ;;
    *) PASS=$((PASS+1)) ;;
  esac
}

# ---------------------------------------------------------------------------
# (a) The exact specimen from the incident. Not a convenient one: this is the
#     literal invocation that filed AEAB-10 with an empty body and wrong type.
# ---------------------------------------------------------------------------
out=$(run "cloud.amux.io is DOWN (502)" --type blocker --desc-file "$TMP/nope.md"); rc=$?
# --type IS accepted now, so the refusal must come from the unreadable --desc-file,
# NOT from the flag being unknown. Asserting which failure it is matters: a parser
# that refused *every* flag would also be wrong.
check_rc   "(a) specimen is refused, not silently accepted" 1 "$rc"
check_has  "(a) names the unreadable file"    "--desc-file" "$out"
check_lacks "(a) did not create a card"       "→ todo" "$out"

# ---------------------------------------------------------------------------
# (b) A genuinely unknown flag must be REFUSED and must say so.
# ---------------------------------------------------------------------------
out=$(run "PROBE title" --totally-bogus-flag xyz); rc=$?
check_rc   "(b) unknown flag refused"          1 "$rc"
check_has  "(b) names the offending flag"      "--totally-bogus-flag" "$out"
check_has  "(b) says it is unknown"            "unknown option" "$out"
check_has  "(b) names the escape"              "--stdin" "$out"
check_lacks "(b) did not create a card"        "→ todo" "$out"

# ---------------------------------------------------------------------------
# (c) --help prints help and files NOTHING. This is the case that polluted the
#     board, so it is asserted on both halves: help text present, no card.
# ---------------------------------------------------------------------------
out=$(run --help); rc=$?
check_rc   "(c) --help exits 0"                0 "$rc"
check_has  "(c) --help prints usage"           "Usage: amux board add" "$out"
check_has  "(c) --help documents --type"       "--type" "$out"
check_lacks "(c) --help created no card"       "→ todo" "$out"

# ---------------------------------------------------------------------------
# (d) THE CONTROL, and it is load-bearing. Everything above passes if the parser
#     simply rejects all input. A plain positional title must still be accepted
#     and must reach the network — here that means failing at CONNECT against the
#     closed port, which is a different failure from a parse refusal. Without
#     this case, `add` could be broken outright and (a)-(c) would stay green.
#     It has already earned that: it is the case that detected the AMUX_URL/AMUX_API
#     isolation bug described above, which was writing to a live board.
# ---------------------------------------------------------------------------
out=$(run "an ordinary title with no flags"); rc=$?
check_lacks "(d) plain title not treated as a bad flag" "unknown option" "$out"
check_lacks "(d) plain title did not print usage"       "Usage: amux board add" "$out"
# curl exit 7 = "failed to connect". Only reachable AFTER parsing succeeded and a
# POST was attempted, so it is a positive signal rather than mere absence of a
# refusal — which a silent early `return 0` would also have produced.
check_rc   "(d) plain title reached the network layer"  7 "$rc"

# ---------------------------------------------------------------------------
# (e) A multi-word positional title must not be split or partially eaten. The old
#     code did `title="$*"`, so this case passed before AND after — it is here to
#     prove the rewrite to a while-loop did not regress the ordinary path.
# ---------------------------------------------------------------------------
out=$(run three separate words); rc=$?
check_lacks "(e) multi-word title accepted" "unknown option" "$out"
check_lacks "(e) multi-word title no usage" "Usage: amux board add" "$out"

# ---------------------------------------------------------------------------
# (f) A title that legitimately starts with '--' is still expressible via --file.
#     Refusing '--*' would be too blunt if it left no escape, so the escape the
#     error message advertises has to actually work.
# ---------------------------------------------------------------------------
printf -- '--this is really the title--\n' > "$TMP/title.txt"
out=$(run --file "$TMP/title.txt"); rc=$?
check_lacks "(f) --file title not refused as a flag" "unknown option" "$out"
check_lacks "(f) --file title did not print usage"   "Usage: amux board add" "$out"

# ---------------------------------------------------------------------------
# (g) Missing values are refused rather than swallowing the next token.
# ---------------------------------------------------------------------------
out=$(run "t" --type); rc=$?
check_rc   "(g) --type with no value refused" 1 "$rc"
out=$(run "t" --desc); rc=$?
check_rc   "(g) --desc with no value refused" 1 "$rc"

# ---------------------------------------------------------------------------
# (h) retitle had the identical catch-all twenty lines away; fixed in the same
#     change, so it is covered here rather than left as a known hole.
# ---------------------------------------------------------------------------
out=$(HOME="$TMP" "$AMUX_BIN" board retitle AEAB-1 "a title" --totally-bogus-flag 2>&1); rc=$?
check_rc  "(h) retitle refuses an unknown flag"      1 "$rc"
check_has "(h) retitle names the offending flag"     "--totally-bogus-flag" "$out"

# ---------------------------------------------------------------------------
# (i) THE PAYLOAD. Everything above tests the PARSER; none of it proves the
#     parsed values reach the wire. That gap is the whole bug restated: the old
#     code parsed "successfully" too, and shipped the wrong JSON. So point the
#     CLI at a stub that records the request body, and assert on what a server
#     would actually receive — title, type and desc as separate fields.
#
#     Without this, `--type`/`--desc` could be accepted, ignored, and every case
#     above would stay green — which is precisely the shape of the original
#     defect (accepted, ignored, exit 0).
# ---------------------------------------------------------------------------
BODY="$TMP/body.json"
# Job control off around the stub so bash does not print "Terminated" at exit.
set +m
python3 - "$BODY" >/dev/null 2>&1 <<'PYSTUB' &
import json, sys, ssl, http.server
out = sys.argv[1]
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        open(out, "wb").write(self.rfile.read(n))
        b = json.dumps({"id": "STUB-1", "status": "todo"}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)
    def do_GET(self):   # /api/board/contract, for the types list in usage
        # The REAL server publishes `types` at TOP LEVEL (board.rs, KNOWN_TYPES).
        # This stub used to serve {"fields": {"valid_types": [...]}} — a shape the
        # server has never served — which matched what the CLI helper read, so the
        # two agreed with each other and both disagreed with production. The test
        # was green the whole time `amux board add --help` printed "server
        # unreachable" against a healthy server. A fixture built to match the code
        # under test cannot fail on the defect; it certifies it.
        b = json.dumps({"types": ["code", "blocker", "chore"]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)
    def log_message(self, *a):  # keep the test output clean
        pass
srv = http.server.HTTPServer(("127.0.0.1", 8899), H)
srv.serve_forever()
PYSTUB
STUB_PID=$!
disown "$STUB_PID" 2>/dev/null || true
trap 'kill "$STUB_PID" 2>/dev/null; wait "$STUB_PID" 2>/dev/null; rm -rf "$TMP"' EXIT
# Wait for the port rather than sleeping a guessed interval.
for _ in $(seq 1 50); do
  if curl -s -o /dev/null --max-time 1 "http://127.0.0.1:8899/api/board/contract" 2>/dev/null; then break; fi
  sleep 0.1
done

printf 'a body written from a file\n' > "$TMP/desc.md"
rm -f "$BODY"
AMUX_API="http://127.0.0.1:8899" run "a real title" --type blocker --desc-file "$TMP/desc.md" >/dev/null 2>&1

if [ -s "$BODY" ]; then
  PASS=$((PASS+1))
  got=$(python3 -c 'import json,sys
b=json.load(open(sys.argv[1]))
print("title="+repr(b.get("title")))
print("type="+repr(b.get("type")))
print("desc="+repr(b.get("desc")))
print("status="+repr(b.get("status")))' "$BODY")
  check_has "(i) title reaches the wire, WITHOUT the flags in it" "title='a real title'" "$got"
  check_has "(i) --type reaches the wire as its own field"        "type='blocker'"      "$got"
  check_has "(i) --desc-file reaches the wire as its own field"   "a body written"      "$got"
  check_has "(i) status still defaults to todo"                   "status='todo'"       "$got"
  # The original defect, asserted directly: no flag text in the title.
  check_lacks "(i) title carries no '--type' text"    "--type"      "$got"
  check_lacks "(i) title carries no scratch path"     "/desc.md"    "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1])).get("title",""))' "$BODY")"
else
  FAIL=$((FAIL+1)); echo "FAIL: (i) stub server recorded no POST body — cannot verify the payload"
fi

# ---------------------------------------------------------------------------
# (j) The types list in `--help`, and the THREE cells it can land in.
#
# Until 2026-08-21 nothing here read the types line at all, so the stub above
# could serve any shape it liked and the suite stayed green. It served
# {"fields": {"valid_types": [...]}} because that is what the CLI helper read —
# and the real server publishes `types` at TOP LEVEL, so on a healthy server
# `amux board add --help` printed "server unreachable" and sent the reader off
# on the AMUX-3046 stranded-port hunt. Fixture and code agreed with each other
# and both disagreed with production. Asserting the OUTPUT is what makes that
# impossible to repeat: a fixture nobody reads certifies nothing.
#
# Three cells, because the whole point of the fix is that they are distinct:
#   200 + real shape  -> the actual list
#   transport failure -> "unreachable", naming the http code
#   200 + no types    -> "contract shape changed", NOT "unreachable"
helptypes() { AMUX_API="$1" HOME="$TMP" "$AMUX_BIN" board add --help 2>&1 | grep "^types:"; }

got=$(helptypes "http://127.0.0.1:8899")
check_has   "(j) 200 + real contract shape lists the types" "code blocker chore" "$got"
check_lacks "(j) a healthy server is never called unreachable" "unreachable"     "$got"

got=$(helptypes "https://127.0.0.1:9")
check_has   "(j) transport failure says unreachable"        "unreachable"        "$got"
check_has   "(j) transport failure names the http code"     "http=000"           "$got"
check_lacks "(j) transport failure is not a shape complaint" "shape changed"     "$got"

# A 200 carrying neither spelling of the list.
python3 - >/dev/null 2>&1 <<'PYSTUB2' &
import json, http.server
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        b = json.dumps({"gates": {}}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)
    def log_message(self, *a):
        pass
http.server.HTTPServer(("127.0.0.1", 8902), H).serve_forever()
PYSTUB2
STUB2_PID=$!
disown "$STUB2_PID" 2>/dev/null || true
for _ in $(seq 1 50); do
  if curl -s -o /dev/null --max-time 1 "http://127.0.0.1:8902/api/board/contract" 2>/dev/null; then break; fi
  sleep 0.1
done
got=$(helptypes "http://127.0.0.1:8902")
kill "$STUB2_PID" 2>/dev/null; wait "$STUB2_PID" 2>/dev/null
check_has   "(j) 200 without a type list blames the CONTRACT" "shape changed"    "$got"
check_lacks "(j) 200 without a type list is not 'unreachable'" "unreachable"     "$got"

echo
echo "test-board-add-flags: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
