#!/usr/bin/env bash
# AF-243 — the RETIREMENT tool had no test, and it is the one that DELETES.
#
# scripts/frustrations-archive.py removes an entry from frustrations.md and writes it
# to frustrations-archive.md. That is the only sanctioned way an entry leaves the
# ledger, it is destructive to the source file, and it shipped with zero coverage —
# then gained a whole new code path (`--superseded`, the third disposition) on
# 2026-08-26, still with none.
#
# THE PROPERTY THAT MATTERS is not "the entry ended up in the archive". It is that the
# ledger loses EXACTLY the target entry and nothing else. A tool that took one extra
# entry, or truncated the tail, would satisfy every naive assertion — the target is in
# the archive, the target is gone from the ledger — while silently destroying a
# neighbour. So the cells below pin the SURVIVORS, by name, on both sides.
#
# THE CARD WRITE IS UNREACHABLE HERE, ON PURPOSE. `carry_to_card` is best-effort by
# design: the archive is what makes a move recoverable, so a failed card write must
# never block the move or leave an entry half-retired. Pointing it at a closed port is
# therefore not a limitation of this harness, it is the cell that proves the asymmetry
# holds — the entry must still move, and the failure must be REPORTED rather than
# swallowed.
#
# HOW THE ISOLATION IS ACHIEVED, and the first version got this WRONG. Setting
# AMUX_URL does NOT work: `_api()` in the tool runs `amux url` first and falls back to
# a hardcoded literal, and never reads AMUX_URL at all. That is CORRECT of the tool —
# CLAUDE.md mandates `$(amux url)` over `$AMUX_URL` precisely because a lane's env can
# carry a stale port (AMUX-3046) — so the fix belongs here, not there.
#
# The cost of getting it wrong was visible in the live request log the next morning:
# `404 PATCH /api/board/{id}` with the literal `X-2` twelve times, this harness's own
# fixture id, hitting the REAL board. Harmless only because X-1/X-2/X-3 do not exist.
# A fixture that reused a real card id would have mutated production, and cell (c)
# would have passed either way — it asserted the write was reported as failed, which
# a 404 from the live board satisfies just as well as a dead port.
#
# So the isolation is now STRUCTURAL: a stub `amux` earlier on PATH answers `url` with
# a closed port, which exercises the tool's REAL resolution path and yields a dead
# endpoint. Cell (z) asserts the stub is the one being resolved, because an isolation
# that silently stops working is exactly what happened last time.
#
# Exit 0 = all pass, 1 = a failure.
set -uo pipefail
cd "$(dirname "$0")/.."
# Overridable so a MUTANT runs through the same cells (ethos rule 7).
ARCHIVE_TOOL="${FRUSTRATIONS_ARCHIVE:-$(pwd)/scripts/frustrations-archive.py}"
PASS=0; FAIL=0
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
STUBBIN="$TMP/stubbin"
mkdir -p "$STUBBIN"
cat > "$STUBBIN/amux" <<'STUB'
#!/usr/bin/env bash
# Test stub: only `url` is needed, and it must answer a CLOSED port.
[ "${1:-}" = "url" ] && { echo "https://127.0.0.1:9"; exit 0; }
exit 0
STUB
chmod +x "$STUBBIN/amux"
export PATH="$STUBBIN:$PATH"

ok()   { PASS=$((PASS+1)); printf '  ok   — %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL — %s\n' "$1"; }
want() { [ "$2" = "$3" ] && ok "$1" || { bad "$1"; printf '         want %s got %s\n' "$3" "$2"; }; }
has()  { grep -qF "$2" "$3" && ok "$1" || bad "$1 (missing: $2)"; }
lacks(){ grep -qF "$2" "$3" && bad "$1 (present but must not be: $2)" || ok "$1"; }

# A throwaway repo: scripts/ + a ledger with THREE entries, so "removed exactly one"
# is a claim with survivors on both sides of the target.
build() { # dir
  local d="$1"; mkdir -p "$d/scripts"
  cp "$ARCHIVE_TOOL" "$d/scripts/frustrations-archive.py"
  cat > "$d/frustrations.md" <<'LEDGER'
# amux frustrations

Header prose. The template below is indented so it cannot count itself.

```
  ## <title>
  AREA: <area>
```

---
## FIRST entry, must survive
AREA: cli
SEVERITY: annoys
STATUS: open
DATE: 2026-08-01
SESSION: lane-a
CARD: X-1
SYMPTOM: first symptom text
COST: first cost text

## TARGET entry, the one being retired
AREA: gates
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-02
SESSION: lane-b
CARD: X-2
SYMPTOM: target symptom text
COST: target cost text

## LAST entry, must survive
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-08-03
SESSION: lane-c
CARD: X-3
SYMPTOM: last symptom text
COST: last cost text
LEDGER
}

run() { # dir args...
  local d="$1"; shift
  # NO inline PATH here on purpose: the isolation must come from ONE place (the
  # exported PATH above) so cell (z) can actually fail when it breaks. The first
  # version set it both here and in (z), which made (z) a check on its own inline
  # prefix — it passed with the export deleted, which is the regression it exists
  # to catch.
  ( cd "$d" && python3 scripts/frustrations-archive.py "$@" ) \
    > "$d/out.txt" 2>&1
  echo $?
}

echo "AF-243 — frustrations retirement tool"

# ---- (a) --list names every entry and mutates nothing -----------------------
A="$TMP/a"; build "$A"
before=$(shasum -a 256 "$A/frustrations.md" | cut -d' ' -f1)
rc=$(run "$A" --list)
want "(a) --list exits 0" "$rc" 0
has  "(a) --list names the target" "TARGET entry" "$A/out.txt"
has  "(a) --list names a survivor" "FIRST entry" "$A/out.txt"
after=$(shasum -a 256 "$A/frustrations.md" | cut -d' ' -f1)
want "(a) --list does not touch the ledger" "$after" "$before"

# ---- (b) VALIDATED: exactly one entry moves ---------------------------------
B="$TMP/b"; build "$B"
LN=$(cd "$B" && python3 scripts/frustrations-archive.py --list | grep -F "TARGET entry" | awk '{print $1}' | tr -d 'L')
rc=$(run "$B" "$LN" lane-b --evidence-stdin <<< "the evidence line")
want "(b) exits 0" "$rc" 0
lacks "(b) the target LEFT the ledger"        "TARGET entry" "$B/frustrations.md"
has   "(b) the entry BEFORE it survived"      "FIRST entry"  "$B/frustrations.md"
has   "(b) the entry AFTER it survived"       "LAST entry"   "$B/frustrations.md"
has   "(b) the header survived"               "Header prose" "$B/frustrations.md"
has   "(b) it landed in the archive"          "TARGET entry" "$B/frustrations-archive.md"
has   "(b) stamped VALIDATED with the signer" "VALIDATED: lane-b | the evidence line" "$B/frustrations-archive.md"
has   "(b) the entry body came with it"       "target symptom text" "$B/frustrations-archive.md"
lacks "(b) survivors did NOT follow it"       "FIRST entry" "$B/frustrations-archive.md"

# ---- (c) the card write is unreachable, and that must not block the move ----
has  "(c) the unreachable card write is REPORTED, not swallowed" "NOT carried" "$B/out.txt"

# ---- (d) --superseded stamps differently ------------------------------------
D="$TMP/d"; build "$D"
LN=$(cd "$D" && python3 scripts/frustrations-archive.py --list | grep -F "TARGET entry" | awk '{print $1}' | tr -d 'L')
rc=$(run "$D" "$LN" lane-b --superseded --evidence-stdin <<< "the mechanism was wrong")
want  "(d) exits 0" "$rc" 0
has   "(d) stamped SUPERSEDED" "SUPERSEDED: lane-b | the mechanism was wrong" "$D/frustrations-archive.md"
# ANCHORED to line start on purpose. A whole-file grep for "VALIDATED:" matches the
# archive HEADER, which explains the VALIDATED: line in prose — so the first version of
# this cell went red against correct code. A stamp is a line, and only a line.
n=$(grep -c '^VALIDATED:' "$D/frustrations-archive.md" || true)
want "(d) NO entry is stamped VALIDATED — the whole point" "$n" 0
has   "(d) says so on stdout"  "SUPERSEDED (entry was WRONG)" "$D/out.txt"
lacks "(d) the target still left the ledger" "TARGET entry" "$D/frustrations.md"
has   "(d) survivors intact"   "LAST entry" "$D/frustrations.md"

# ---- (e) a bad line refuses and changes nothing -----------------------------
E="$TMP/e"; build "$E"
before=$(shasum -a 256 "$E/frustrations.md" | cut -d' ' -f1)
rc=$(run "$E" 999999 lane-b --evidence-stdin <<< "x")
want "(e) a line with no entry exits 1" "$rc" 1
after=$(shasum -a 256 "$E/frustrations.md" | cut -d' ' -f1)
want "(e) and the ledger is untouched" "$after" "$before"
[ -n "$before" ] && ok "(e) the hash is non-empty, so that comparison could have failed" \
                 || bad "(e) hash was empty — the check could not fail"

# ---- (y) A HYPHENATED FIELD MUST NOT BLANK THE FIELD ABOVE IT (AF-264) -------
# The field terminator was `[A-Z_]+:`, so a field NAME containing a hyphen was
# not seen as the start of the next field — and because the body pattern is
# non-greedy and needs that lookahead to stop, the match failed outright and the
# PRECEDING field came back empty. An entry with `CARD:` followed by
# `NOTE-CARD:` reported "no CARD field" and was archived with its symptom never
# reaching the card, which is the exact AF-38 guarantee AF-239 exists to keep.
#
# The failure lands one field UPSTREAM of its cause, so the cell seeds the
# hyphenated field and asserts on the one ABOVE it.
Y="$TMP/y"; build "$Y"
# sed, not an embedded python heredoc: the first version of this fixture was
# written by a python script whose OWN string ate the \n escapes, so the
# heredoc it emitted was a syntax error and applied nothing. The three cells
# below then passed for the wrong reason — with no hyphenated field present,
# CARD parses fine. The fixture guard is what caught it, which is why it is
# here rather than assumed.
sed -i.bak 's|^CARD: X-2$|CARD: X-2\
NOTE-CARD: repointed, and this line used to blank CARD above it|' "$Y/frustrations.md"
grep -q '^NOTE-CARD:' "$Y/frustrations.md" && ok "(y) fixture: the hyphenated field is present" \
                                           || bad "(y) fixture did not apply — the cell proves nothing"
LN=$(cd "$Y" && python3 scripts/frustrations-archive.py --list | grep -F "TARGET entry" | awk '{print $1}' | tr -d 'L')
rc=$(run "$Y" "$LN" lane-b --evidence-stdin <<< "hyphen check")
want "(y) exits 0" "$rc" 0
# The tool cannot reach a card here (dead port), but it must have RESOLVED the
# id — "no CARD field" is the bug's signature and must not appear.
lacks "(y) CARD: is still parsed with a hyphenated field beneath it" "no CARD field" "$Y/out.txt"
has   "(y) and the card id it resolved is named" "X-2" "$Y/out.txt"

# ---- (z) THE ISOLATION ITSELF -----------------------------------------------
# Without this, a stub that stops being found leaves every cell above green while
# the tool writes to the REAL board again — which is precisely how the X-2 rows
# reached production. Assert what the tool would actually resolve.
resolved=$(amux url 2>/dev/null)
want "(z) the tool resolves amux-url to the CLOSED port, not the live server" \
     "$resolved" "https://127.0.0.1:9"

printf '\n  %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
