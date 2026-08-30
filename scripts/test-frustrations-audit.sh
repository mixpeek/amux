#!/usr/bin/env bash
# AEAB-19 — the frustrations.md gate must be able to FAIL, with no board reachable.
#
# It could not. Two independent defects in scripts/frustrations_audit.py, and each one
# alone was enough to make `checks` — the only required status check on main — green
# over a broken file:
#
#   1. the board-unreachable branch returned a bare `2`, discarding whether `problems`
#      was non-empty. CI never has a board (checks.yml says so in its own comment) and
#      treats 2 as a pass, so per-entry structural findings were printed and thrown away
#      on every push.
#   2. `structure_ok = structure_check(...)` was assigned and never read. The drift check
#      whose docstring records it stopping a live incident from being queued for DELETION
#      was advisory by accident.
#
# Both were live. Commit 18590ca8 landed an entry with no `## ` heading — the parser folds
# such a block into the preceding entry — and main went green over it. One defect hid the
# other, which is why this test asserts each failure mode separately rather than just
# "the audit exits non-zero on a bad file".
#
# EVERY case runs with the board pointed at a closed port, because that is CI's condition
# and the condition under which the gate was broken. A version of this test that ran
# against a live board would pass against the OLD code and prove nothing.
#
# The audit resolves frustrations.md as `Path(__file__).parent.parent/frustrations.md`, so
# each case builds a throwaway repo (scripts/ + frustrations.md) and copies the REAL
# script into it — the shipped decision path, not a paraphrase.
#
# Exit 0 = all pass, 1 = a failure. Wired into .github/workflows/checks.yml.
set -uo pipefail
cd "$(dirname "$0")/.."
# Overridable so a MUTANT can be run through the same cells (ethos rule 7: a
# check that cannot fail on the case it was written for is theatre).
AUDIT="${FRUSTRATIONS_AUDIT:-$(pwd)/scripts/frustrations_audit.py}"
REAL_FRUST="$(pwd)/frustrations.md"
PASS=0; FAIL=0
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

# Board unreachable for every case: port 9 is reserved/discard.
export AMUX_URL="https://127.0.0.1:9"
export AMUX_API="https://127.0.0.1:9"

# A conforming entry, used as the base that each defect case mutates. Kept in one place
# so a contract change breaks this file loudly instead of leaving stale fixtures.
good_entry() {
  cat <<'EOF'
## a well-formed entry
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-08-17
SESSION: test
CARD: none
SYMPTOM: something observable happened
COST: 1 minute
FIX: do the thing
EOF
}

# Build a throwaway repo whose frustrations.md is whatever arrives on stdin, run the
# REAL audit inside it, echo the exit code.
run_on() { # $1 = case dir name
  local d="$TMP/$1"
  mkdir -p "$d/scripts"
  cp "$AUDIT" "$d/scripts/frustrations_audit.py"
  cat > "$d/frustrations.md"
  ( cd "$d" && python3 scripts/frustrations_audit.py >"$d/out.txt" 2>&1; echo $? )
}

check_rc() { # label expected actual casedir
  if [ "$2" = "$3" ]; then PASS=$((PASS+1));
  else
    FAIL=$((FAIL+1)); echo "FAIL: $1 — expected exit $2, got $3"
    [ -f "$TMP/$4/out.txt" ] && sed 's/^/      /' "$TMP/$4/out.txt" | head -6
  fi
}
check_says() { # label needle casedir
  if grep -qF "$2" "$TMP/$3/out.txt" 2>/dev/null; then PASS=$((PASS+1));
  else FAIL=$((FAIL+1)); echo "FAIL: $1 — output did not mention '$2'"; fi
}

# The negative counterpart of check_says. It did NOT exist when the AF-172 cells
# were written, and calling a missing function under `set -uo pipefail` (no -e)
# prints "command not found" and CONTINUES, so those cells would have incremented
# neither PASS nor FAIL: two assertions that could not fail, in a file whose whole
# subject is assertions that could not fail. Caught by grepping for the helper
# instead of assuming it.
check_lacks() { # label needle casedir
  if grep -qF "$2" "$TMP/$3/out.txt" 2>/dev/null; then
    FAIL=$((FAIL+1)); echo "FAIL: $1 — output mentioned '$2' and should not have"
    sed 's/^/      /' "$TMP/$3/out.txt" | head -8
  else PASS=$((PASS+1)); fi
}

header() { printf '# frustrations\n\nblurb\n\n---\n\n'; }

# ---------------------------------------------------------------------------
# (a) THE CONTROL, and it comes first on purpose. A well-formed file must exit 2
#     — board unchecked, nothing structurally wrong. If this ever fails, every
#     "exits 1" assertion below is meaningless, because a script that exits 1 on
#     everything would satisfy them all. This is also what proves the fix did not
#     simply turn the gate into an unconditional failure.
# ---------------------------------------------------------------------------
rc=$( { header; good_entry; } | run_on control )
check_rc "(a) CONTROL: conforming file exits 2 (board unchecked, nothing wrong)" 2 "$rc" control

# ---------------------------------------------------------------------------
# (b) The regression that motivated all of this: a missing required field, board
#     unreachable. Was exit 2 (pass). Must be exit 1.
# ---------------------------------------------------------------------------
rc=$( { header; good_entry | grep -v '^SEVERITY:'; } | run_on missing_severity )
check_rc   "(b) missing SEVERITY fails even with no board" 1 "$rc" missing_severity
check_says "(b) names the missing field" "SEVERITY" missing_severity
check_says "(b) still reports the board was unreachable" "CANNOT REACH BOARD" missing_severity

# ---------------------------------------------------------------------------
# (c) A second required field, to prove (b) is not special-cased.
# ---------------------------------------------------------------------------
rc=$( { header; good_entry | grep -v '^SYMPTOM:'; } | run_on missing_symptom )
check_rc   "(c) missing SYMPTOM fails even with no board" 1 "$rc" missing_symptom
check_says "(c) names the missing field" "SYMPTOM" missing_symptom

# ---------------------------------------------------------------------------
# (d) THE 18590ca8 SPECIMEN, rebuilt from the incident rather than invented: an
#     entry appended with no `## ` heading. The parser folds it into the previous
#     entry, so no field is "missing" — the only signal is the DATE/STATUS count
#     disagreeing with the heading count, which is precisely the check whose
#     result was being discarded. A test built only from missing fields (b, c)
#     would pass while this half stayed broken.
# ---------------------------------------------------------------------------
rc=$( { header; good_entry; printf '\n---\nDATE: 2026-08-17\nSTATUS: open\nAREA: attribution\n'; } | run_on headless )
check_rc   "(d) a heading-less entry fails via the drift check" 1 "$rc" headless
check_says "(d) reports the drift, not a missing field" "STRUCTURE DRIFT" headless

# ---------------------------------------------------------------------------
# (e) THE REAL FILE must pass, or this PR turns main red the moment it lands.
#     Fixing a gate is a migration event; this is the assertion that says the
#     migration was actually completed rather than merely intended.
# ---------------------------------------------------------------------------
rc=$( run_on realfile < "$REAL_FRUST" )
check_rc "(e) the repo's own frustrations.md passes (main stays green)" 2 "$rc" realfile

echo

# ── AF-172: the AREA cluster rank ───────────────────────────────────────────
# frustrations.md's own header says a single frustration is a complaint and a
# cluster is an argument, and that the pattern is invisible unless the entries
# are counted. Nothing counted them until now. These cells pin WHAT is counted.
mk_entry() { # $1 area  $2 status  $3 title
  printf '## %s\nAREA: %s\nSEVERITY: slows\nSTATUS: %s\nDATE: 2026-08-17\nSESSION: test\nCARD: none\nSYMPTOM: s\nCOST: c\nFIX: f\n\n' "$3" "$1" "$2"
}

# (cl1) three OPEN in one AREA is an argument and must be named.
rc=$( { echo "# h"; echo; echo "---"; echo;
        mk_entry widgets open one; mk_entry widgets open two; mk_entry widgets open three; } | run_on cl1 )
check_says "cl1: 3 open in one AREA is reported as an argument" "widgets" cl1
check_says "cl1: and it prints the open/total split" "3 open /  3 total" cl1

# (cl2) THE DISCRIMINATOR. Three entries in one AREA, all FIXED, is a SOLVED
# argument and must NOT be listed. A rank on TOTAL rather than OPEN would list
# it, and would keep proposing rebuilds of subsystems already repaired — so this
# is the cell that fails if the wrong number ranks.
rc=$( { echo "# h"; echo; echo "---"; echo;
        mk_entry gadgets fixed one; mk_entry gadgets fixed two; mk_entry gadgets fixed three; } | run_on cl2 )
check_lacks "cl2: 3 FIXED in one AREA is a solved argument, not a live one" "gadgets" cl2

# (cl3) below threshold stays quiet, or the rank is an alarm that is always on.
rc=$( { echo "# h"; echo; echo "---"; echo;
        mk_entry sprockets open one; mk_entry sprockets open two; } | run_on cl3 )
check_lacks "cl3: 2 open is below threshold and is not reported" "sprockets" cl3
check_says  "cl3: and it says so rather than printing nothing" "no AREA has 3 open entries" cl3

[ "$FAIL" -eq 0 ] || exit 1


# ── AF-173: the --since delta ───────────────────────────────────────────────
# The delta needs no scheduler and no new storage because frustrations.md is in
# git. These cells pin the two properties that make it trustworthy: it reports
# real growth, and it REFUSES on a rev it cannot read rather than treating an
# unreadable rev as "no entries" — which would render every open cluster as pure
# growth, a confident wrong answer in the alarming direction.
dl="$TMP/delta"; mkdir -p "$dl/scripts"; cp "$AUDIT" "$dl/scripts/frustrations_audit.py"
( cd "$dl" && git init -q -b main . && git config user.email t@t && git config user.name t \
  && git config commit.gpgsign false ) >/dev/null 2>&1
{ echo "# h"; echo; echo "---"; echo; mk_entry widgets open one; } > "$dl/frustrations.md"
( cd "$dl" && git add -A && git commit -qm base ) >/dev/null 2>&1
BASE=$( cd "$dl" && git rev-parse HEAD )
{ echo "# h"; echo; echo "---"; echo; mk_entry widgets open one; mk_entry widgets open two; } > "$dl/frustrations.md"

( cd "$dl" && python3 scripts/frustrations_audit.py --since "$BASE" >out.txt 2>&1 )
# Matched on CONTENT, not on padding: the first version of this cell counted
# spaces against a %-16s field, got it one short, and failed on a correct
# implementation. An assertion coupled to column width breaks on any format tweak.
if grep -qE "widgets +1 -> +2 +\+1" "$dl/out.txt"; then
  PASS=$((PASS+1)); echo "  ok   — (dl1) --since reports real growth per AREA"
else
  FAIL=$((FAIL+1)); echo "FAIL: (dl1) --since did not report widgets 1 -> 2"; sed 's/^/      /' "$dl/out.txt" | head -8
fi

# (dl2) THE CONTROL. An unreadable rev must REFUSE, not compute against nothing.
( cd "$dl" && python3 scripts/frustrations_audit.py --since deadbeefdeadbeef >out2.txt 2>&1 )
if grep -qF "cannot read frustrations.md" "$dl/out2.txt" && ! grep -qF "0 ->  2" "$dl/out2.txt"; then
  PASS=$((PASS+1)); echo "  ok   — (dl2) an unreadable rev REFUSES instead of showing all-growth"
else
  FAIL=$((FAIL+1)); echo "FAIL: (dl2) unreadable rev did not refuse"; sed 's/^/      /' "$dl/out2.txt" | head -8
fi


# ---------------------------------------------------------------------------
# (s) THE STRANDED CELLS (AF-229). Everything above runs with the board at a dead
#     port, which is CI's condition — and it means the CARD-resolution half has
#     never been exercised by any test at all. So these cells stand up a fixture
#     board on localhost and point the audit at it. That keeps the new check
#     runnable in CI, where the REAL board is unreachable and always will be.
#
#     What is being pinned: "not on this board" was one output for two states, and
#     they need opposite handling. An unresolved id whose author is a live lane is
#     drainable — ask them. An unresolved id in a prefix namespace this board has
#     never held, by an author absent from the fleet, can never be signed off,
#     because the deletion protocol requires the originating session.
#
#     The discriminator is the PREFIX, not author liveness, and that distinction is
#     load-bearing rather than fussy: amux-rust is not live either, yet AR-114
#     answers HTTP 200 on this board. Judging on liveness alone called six drainable
#     AR-* entries permanently stranded on the first run of this feature. Cell (s3)
#     is that specimen, rebuilt.
# ---------------------------------------------------------------------------
fixture_board() { # $1 = dir, $2 = "sessions_ok" | "sessions_500"
  cat > "$1/fixture_server.py" <<PYEOF
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
MODE = sys.argv[1]
BOARD = [{"id": "KNOWN-1", "title": "a known card", "status": "done"},
         {"id": "KNOWN-2", "title": "another known card", "status": "done"}]
SESSIONS = [{"name": "live-lane"}, {"name": "other-live"}]
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        if self.path.startswith("/api/sessions"):
            if MODE == "sessions_500":
                self.send_response(500); self.end_headers(); return
            # An EMPTY list is a different failure from a dead endpoint and a far
            # more dangerous one: the server answers, so nothing looks broken, and
            # every author is trivially "not in the fleet". Cell (s7).
            body = json.dumps([] if MODE == "sessions_empty" else SESSIONS).encode()
        elif self.path.startswith("/api/board"):
            body = json.dumps(BOARD).encode()
        else:
            self.send_response(404); self.end_headers(); return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
srv = HTTPServer(("127.0.0.1", 0), H)
print(srv.server_port, flush=True)
srv.serve_forever()
PYEOF
  python3 "$1/fixture_server.py" "$2" > "$1/port.txt" 2>/dev/null &
  echo $! > "$1/pid.txt"
  for _ in $(seq 1 50); do
    [ -s "$1/port.txt" ] && break
    perl -e 'select(undef,undef,undef,0.1)' 2>/dev/null || true
  done
  cat "$1/port.txt"
}

# Same shape as run_on, but against the fixture board instead of a dead port.
run_on_board() { # $1 = case dir, $2 = sessions mode
  local d="$TMP/$1"; mkdir -p "$d/scripts"
  cp "$AUDIT" "$d/scripts/frustrations_audit.py"
  cat > "$d/frustrations.md"
  local port; port=$(fixture_board "$d" "$2")
  if [ -z "$port" ]; then echo "NOPORT"; return; fi
  ( cd "$d" && AMUX_URL="http://127.0.0.1:$port" AMUX_API="http://127.0.0.1:$port" \
      python3 scripts/frustrations_audit.py >"$d/out.txt" 2>&1; echo $? )
  kill "$(cat "$d/pid.txt")" 2>/dev/null || true
}

entry_with() { # $1 = card field, $2 = session field
  printf '## an entry\nAREA: cli\nSEVERITY: slows\nSTATUS: open\nDATE: 2026-08-25\nSESSION: %s\nCARD: %s\nSYMPTOM: s\nCOST: 1 minute\nFIX: f\n\n' "$2" "$1"
}

# (s1) IT FIRES: unknown prefix + author not in the fleet.
rc=$( { header; entry_with "ZZZ-1" "ghost-lane"; } | run_on_board stranded_fires sessions_ok )
check_says "(s1) unknown prefix + absent author reports STRANDED" "STRANDED" stranded_fires
check_says "(s1) names the author who cannot be asked" "ghost-lane" stranded_fires

# (s2) MUST NOT FIRE: same unresolved id, but the author IS live here.
rc=$( { header; entry_with "ZZZ-1" "live-lane"; } | run_on_board author_live sessions_ok )
check_lacks "(s2) an unresolved id whose author is live is NOT stranded" "STRANDED" author_live
check_says  "(s2) routes it to the live author instead" "IS live here" author_live

# (s3) MUST NOT FIRE — THE AR-* SPECIMEN. Author absent from the fleet, but the
#      prefix IS one this board holds, so the card is reachable and the entry drains
#      whenever that lane runs. Liveness alone would call this stranded.
rc=$( { header; entry_with "KNOWN-99" "ghost-lane"; } | run_on_board prefix_known sessions_ok )
check_lacks "(s3) a KNOWN prefix with a missing id is NOT stranded" "STRANDED" prefix_known

# (s4) MUST NOT FIRE: sessions unreachable means the question was never asked, and
#      absence of a STRANDED block must not read as "none stranded" (ethos rule 7 —
#      a passing check and an absent check look identical).
rc=$( { header; entry_with "ZZZ-1" "ghost-lane"; } | run_on_board no_sessions sessions_500 )
check_lacks "(s4) unreachable sessions claims nothing" "STRANDED" no_sessions
check_says  "(s4) says the question was not asked" "sessions unreachable" no_sessions

# (s6) THE ROLL-UP, pinned separately from the per-entry line. Deleting the
#      `stranded.append` leaves every advisory line intact and silently removes the
#      summary — and cells (s1)-(s5) all stayed green under exactly that mutation.
#      The COUNT is the product here ("three entries sharing an AREA is an argument"),
#      so it needs its own assertion rather than riding on the prose above it.
rc=$( { header; entry_with "ZZZ-1" "ghost-lane"; entry_with "ZZZ-2" "ghost-lane"; } | run_on_board rollup sessions_ok )
check_says "(s6) the summary reports how many entries are stranded" "STRANDED  2 entr" rollup
check_says "(s6) and groups them by the session that must sign them off" "ghost-lane" rollup

# (s7) THE EMPTY-SESSION-LIST CONTROL. fetch_sessions returns None on failure so
#      callers can say "unknown" — but a server that answers with `[]` takes the
#      SUCCESS path, and then every author is absent from the fleet and every entry
#      is branded permanently unactionable. A confident wrong answer in the alarming
#      direction, from a probe that looks healthy. `return names or None` is what
#      prevents it; changing it to `return names or set()` passed every other cell.
rc=$( { header; entry_with "ZZZ-1" "ghost-lane"; } | run_on_board sessions_empty sessions_empty )
check_lacks "(s7) an EMPTY session list claims nothing rather than stranding everything" "STRANDED" sessions_empty
check_says  "(s7) and says the question was not answered" "sessions unreachable" sessions_empty

# (s5) THE MULTI-ID REGRESSION. Real CARD fields read "AR-114, AR-115, AR-116" and
#      "AF-69 (investigation) + AMUX-3221 (the FIX)". board.get() on the whole string
#      matched none of them, so every multi-id entry had ALWAYS reported unresolved —
#      invisible while the branch said something mild, and instantly wrong once it
#      said something specific. Both ids here resolve; nothing may be reported.
rc=$( { header; entry_with "KNOWN-1, KNOWN-2" "live-lane"; } | run_on_board multi_id sessions_ok )
check_lacks "(s5) a multi-id CARD that fully resolves reports nothing" "not on this board" multi_id
check_lacks "(s5) and is not counted as partially missing" "ids resolve" multi_id


echo "test-frustrations-audit: $PASS passed, $FAIL failed"