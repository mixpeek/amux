#!/usr/bin/env bash
# Cells for the boot divergence check (AMUX-3965).
#
# WHY THIS EXISTS. `start-all` increments `started` when `cmd_start --detach`
# returns 0, and `tmux new-session -d` returns immediately. So `started` counts
# SPAWNS THAT RETURNED, not workers that came up. fleet-boot.sh independently
# reads /api/sessions and logs what is actually running. On the boot of
# 2026-08-30 those two disagreed by six -- `55 started, 0 failed` beside a
# verdict naming 7 workers down, 6 of them in start-all's own started list --
# and the boot exited 0. Both numbers were already in this script; nothing
# compared them.
#
# Runs the SHIPPED fleet-boot.sh, with AMUX_BIN and AMUX_FLEET_BOOT_BASE pointed
# at stubs. No real worker is ever started by this file.
set -uo pipefail
cd "$(dirname "$0")/.."
BOOT="${FLEET_BOOT_SH:-$(pwd)/scripts/fleet-boot.sh}"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
no(){ FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"; [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null' EXIT

# A stub server for /health and /api/sessions. The sessions payload is read from
# a file at REQUEST time, so a cell can change it between runs.
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
cat > "$TMP/srv.py" <<PY
import http.server, json, os
SESS = os.environ["SESS_FILE"]
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            b = b'{"status":"ok"}'
        elif self.path == "/api/sessions":
            b = open(SESS, "rb").read()
        else:
            self.send_response(404); self.end_headers(); return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", $PORT), H).serve_forever()
PY
SESS_FILE="$TMP/sessions.json" python3 "$TMP/srv.py" >"$TMP/srv.err" 2>&1 & SRV=$!
for _ in $(seq 1 50); do
  curl -s "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break; sleep 0.1
done

# sessions payload: $1 = how many of 8 are running
sessions(){
  python3 - "$1" > "$TMP/sessions.json" <<'PY'
import json, sys
n = int(sys.argv[1])
out = [{"name": "w%d" % i, "archived": False, "running": i < n} for i in range(8)]
json.dump(out, open("/dev/stdout", "w"))
PY
}

# stub amux: answers `url` and `start-all`. The summary text is whatever the
# cell wrote to $TMP/summary.
cat > "$TMP/amux" <<EOS
#!/usr/bin/env bash
case "\$1" in
  url)       echo "http://127.0.0.1:$PORT" ;;
  start-all) cat "$TMP/summary"; exit 0 ;;
  *)         exit 0 ;;
esac
EOS
chmod +x "$TMP/amux"

boot(){ # boot <summary line> <running count> -> rc; log lands in $TMP/boot.log
  printf '%s\n' "$1" > "$TMP/summary"
  sessions "$2"
  : > "$TMP/boot.log"
  AMUX_BIN="$TMP/amux" \
  AMUX_FLEET_BOOT_BASE="http://127.0.0.1:$PORT" \
  AMUX_FLEET_BOOT_LOG="$TMP/boot.log" \
  AMUX_FLEET_BOOT_HEALTH_TIMEOUT=10 \
  AMUX_START_ALL_STAGGER=0 \
    bash "$BOOT" >/dev/null 2>&1
  echo $?
}

# DID THE MEASUREMENT RUN? Every negative cell below needs this, and none of them
# needed it until CI proved otherwise.
#
# On a Linux runner `mktemp -t <name-with-no-Xs>` fails, fleet-boot's $health_tmp
# came out empty, the health probe could never succeed, and the whole verdict
# block was skipped. The result: every POSITIVE cell failed and every NEGATIVE
# cell PASSED — because "no DIVERGENCE line" is trivially true when nothing was
# computed. 5 passed / 4 failed, and the 5 were vacuous.
#
# That is ethos rule 4 in the check written to enforce it: an assertion that can
# read empty must publish whether the measurement ran. So a negative is only
# believed when the verdict line proves fleet-boot got that far.
verdict_ran(){ grep -q 'verdict: .*non-archived workers running' "$TMP/boot.log"; }

echo "fleet-boot divergence cells (AMUX-3965)"

# A. THE 2026-08-30 SHAPE. Claims 7 up (5 started + 2 already), 3 actually run.
rc="$(boot 'start-all: 5 started · 2 already running · 0 failed · 66 archived (skipped)' 3)"
# PRECONDITION. If the stub server never answered, fleet-boot skips its verdict
# block entirely and every negative cell below passes on an empty log. Fail here,
# loudly, with the server's own stderr, rather than reporting vacuous greens.
if ! verdict_ran; then
  no "PRECONDITION: fleet-boot never reached its verdict block" \
     "the stub server was unreachable, so nothing below was measured. srv stderr: $(head -3 "$TMP/srv.err" 2>/dev/null || echo '(none)') | boot.log tail: $(tail -2 "$TMP/boot.log" 2>/dev/null)"
  printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
  exit 1
fi
ok "precondition: the stub server answered and fleet-boot reached its verdict"
grep -q '^.*DIVERGENCE: start-all claimed 7 up, 3 are actually running' "$TMP/boot.log" \
  && ok "a boot that claims more up than are running says so" \
  || no "the divergence must be named in the log" "$(grep -i diverg "$TMP/boot.log" || echo '(no DIVERGENCE line at all)')"
[ "$rc" != "0" ] \
  && ok "and the boot exits non-zero (rc=$rc), so its status carries what its log knows" \
  || no "a divergent boot must not exit 0" "rc=$rc"

# B. CONTROL, and it is the cell that matters: agreement must stay silent and
#    green, or A passes by the check firing on every boot ever.
rc="$(boot 'start-all: 5 started · 2 already running · 0 failed · 66 archived (skipped)' 7)"
if verdict_ran && ! grep -q 'DIVERGENCE' "$TMP/boot.log"; then
  ok "counts that agree produce no divergence line"
elif ! verdict_ran; then
  no "counts that agree produce no divergence line" "VACUOUS: fleet-boot never reached its verdict, so this cell proves nothing"
else
  no "a boot whose counts agree must NOT report a divergence" "$(grep -i diverg "$TMP/boot.log")"
fi
[ "$rc" = "0" ] \
  && ok "and an agreeing boot still exits 0 (the check did not just break boot)" \
  || no "an agreeing boot must exit 0" "rc=$rc"

# C. MORE RUNNING THAN CLAIMED is not a divergence. A worker that came up on its
#    own between the two reads is fine; only a SHORTFALL is a defect.
rc="$(boot 'start-all: 1 started · 0 already running · 0 failed · 66 archived (skipped)' 5)"
if verdict_ran && ! grep -q 'DIVERGENCE' "$TMP/boot.log"; then
  ok "a surplus of running workers is not reported as a shortfall"
elif ! verdict_ran; then
  no "a surplus of running workers is not reported as a shortfall" "VACUOUS: no verdict was computed"
else
  no "more running than claimed must not be flagged" "$(grep -i diverg "$TMP/boot.log")"
fi

# D. THE UNMEASURED CASE (ethos rule 4). If the claimed count cannot be parsed,
#    the comparison did not run -- and that must not read as agreement. This is
#    the same shape as AMUX-3696, where a silent skip looked like "no findings".
rc="$(boot 'start-all: something went sideways and printed no counts' 3)"
grep -q 'DIVERGENCE UNMEASURED' "$TMP/boot.log" \
  && ok "an unparseable summary reports UNMEASURED, not silence" \
  || no "a comparison that could not run must say so" "$(tail -3 "$TMP/boot.log")"

# F. A REAL ZERO IS NOT A FAILED PARSE. The fix for D tracks whether the TOKEN
#    matched, not whether the value was non-zero -- so a boot that legitimately
#    started nothing must still COMPARE, not report UNMEASURED. Without this
#    cell, "print nothing unless seen" could have been written as "print nothing
#    if the total is 0" and D would still pass.
rc="$(boot 'start-all: 0 started · 0 already running · 0 failed · 66 archived (skipped)' 0)"
if verdict_ran && ! grep -q 'DIVERGENCE UNMEASURED' "$TMP/boot.log"; then
  ok "a real zero is measured, not reported as UNMEASURED"
elif ! verdict_ran; then
  no "a real zero is measured, not reported as UNMEASURED" "VACUOUS: no verdict was computed"
else
  no "a genuine '0 started' must not read as an unparseable summary" "$(grep -i diverg "$TMP/boot.log")"
fi
if verdict_ran && ! grep -q 'DIVERGENCE:' "$TMP/boot.log"; then
  ok "0 claimed and 0 running compares clean"
elif ! verdict_ran; then
  no "0 claimed and 0 running compares clean" "VACUOUS: no verdict was computed"
else
  no "0 claimed against 0 running is agreement, not a shortfall" "$(grep -i diverg "$TMP/boot.log")"
fi

# E. CONTROL: none of this cost the verdict line that already worked.
grep -q 'verdict: .*non-archived workers running' "$TMP/boot.log" \
  && ok "the pre-existing verdict line still prints" \
  || no "the verdict line must survive" "$(tail -3 "$TMP/boot.log")"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
