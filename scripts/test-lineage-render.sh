#!/usr/bin/env bash
# AMUX-2393 — the Lineage tab must not upgrade a weak answer.
#
# `GET /api/why/task/<id>` is a careful instrument: it cites a table and row for
# every line, reports every source it consulted INCLUDING the ones that returned
# nothing (with the predicate they ran), and refuses to narrate when the evidence
# does not support a story. All of that care is destroyable by its renderer.
# An explainer's failure mode is confident narration from whatever it happened to
# find, and a printer is exactly where that gets reintroduced after the API went
# to the trouble of avoiding it.
#
# So this pins the four properties whose loss would be INVISIBLE — the panel would
# still look right, and would say more than it knows:
#   1. every `gaps` entry is rendered (drop one and a hole reads as a full story)
#   2. zero-row sources stay visible (a zero from a probe that COULD have matched
#      and a zero from one that never could look identical otherwise)
#   3. every source shows its predicate
#   4. the untimed badge appears iff some line is not timestamped (issues.log is
#      HH:MM only, so those lines are placed in source order — rendering them like
#      timestamped ones invents a chronology)
#
# It drives the SHIPPED `_bdLineageHtml` extracted from app.js, never a copy: a
# copy would pass forever while the real one rotted, which is the failure this
# file exists to prevent, one level up.
#
# Exit 0 = pass, 1 = failure, 2 = skipped (needs a live server for a real payload
# — a synthetic fixture cannot tell you the renderer matches what the endpoint
# actually emits, which is half the point).
set -uo pipefail
cd "$(dirname "$0")/.."

API="${AMUX_API:-https://localhost:8824}"
APP=crates/amux-dashboard/static/app.js
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

command -v node >/dev/null 2>&1 || { echo "SKIP: node not installed"; exit 2; }
if ! curl -sk -m 5 -o /dev/null "$API/health" 2>/dev/null; then
  echo "SKIP: $API/health unreachable — this test needs a real /api/why payload."
  echo "      Skipping is deliberate: a synthetic fixture would not prove the"
  echo "      renderer matches what the endpoint actually emits."
  exit 2
fi

# A card that exists and has a trail. Any card works; pick the newest so this
# does not rot against a hardcoded id that someone eventually archives.
CARD=$(curl -sk -m 10 "$API/api/board?limit=50" | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(''); sys.exit(0)
rows=d if isinstance(d,list) else (d.get('items') or [])
print(rows[0]['id'] if rows else '')" 2>/dev/null)
[ -n "$CARD" ] || { echo "SKIP: no board cards to trace"; exit 2; }

curl -sk -m 20 "$API/api/why/task/$CARD" > "$TMP/why.json" 2>/dev/null
python3 -c "import json,sys; json.load(open('$TMP/why.json'))" 2>/dev/null \
  || { echo "SKIP: /api/why/task/$CARD did not return JSON"; exit 2; }

# EXTRACT the shipped functions by brace-matching. If either moves or is renamed,
# this fails loudly rather than testing a stale copy.
python3 - "$APP" "$TMP/lin.js" <<'PY' || exit 1
import re, sys
src = open(sys.argv[1]).read()
def extract(name):
    m = re.search(r'^function ' + name + r'\(', src, re.M)
    if not m:
        sys.exit(f"FAIL — {name} not found in {sys.argv[1]}; this test is now blind")
    i = src.index('{', m.start()); depth = 0
    for j in range(i, len(src)):
        if src[j] == '{': depth += 1
        elif src[j] == '}':
            depth -= 1
            if depth == 0: return src[m.start():j+1]
    sys.exit(f"FAIL — unbalanced braces extracting {name}")
# Minimal DOM shim for `esc` ONLY, reproducing textContent->innerHTML exactly
# (escape & < > and nothing else, as the browser does). The function under test
# is the shipped source, unmodified.
shim = """
const document = { createElement: () => ({
  set textContent(v){ this._t = String(v); },
  get innerHTML(){ return this._t.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;'); }
}) };
"""
open(sys.argv[2], 'w').write(shim + extract('esc') + "\n" + extract('_bdLineageHtml'))
PY

cat > "$TMP/run.mjs" <<'EOF'
import fs from 'fs';
const dir = process.argv[2];
const src = fs.readFileSync(dir + '/lin.js', 'utf8');
const d = JSON.parse(fs.readFileSync(dir + '/why.json', 'utf8'));
const fn = new Function(src + '; return _bdLineageHtml;')();
const h = fn(d, 'CARD');
let pass = 0, fail = 0;
const ok = (c, m) => { c ? (pass++, console.log('  ok   — ' + m)) : (fail++, console.log('  FAIL — ' + m)); };

ok(h.includes('bd-lin-verdict'), 'renders a verdict block');
ok(!d.verdict || h.includes(d.verdict), 'names the actual verdict (' + d.verdict + ')');

const gaps = d.gaps || [];
if (gaps.length) {
  ok(gaps.every(g => h.includes(String(g).slice(0, 40))),
     'renders EVERY gap (' + gaps.length + ') — dropping one hides a hole');
} else {
  console.log('  --   payload has no gaps; that assertion is not exercised on this card');
}

const zero = (d.sources || []).filter(s => !s.rows);
if (zero.length) {
  ok(zero.every(s => h.includes(s.table)),
     'zero-row sources still rendered (' + zero.length + ') — a zero is evidence, not absence');
} else {
  console.log('  --   payload has no zero-row sources; that assertion is not exercised');
}
ok((d.sources || []).every(s => !s.query || h.includes(String(s.query).slice(0, 20))),
   'every source shows the predicate it ran');

// A source NOTE is the endpoint saying the row count alone would mislead: a
// reaped journal whose floor postdates the card, events that record THAT
// something changed but not into what, or the receipt that a trail really is
// complete. The panel rendered `query` and dropped `note` for its first two
// commits — which is the same defect one layer up that the note exists to
// prevent, and it looked fine because the numbers were all correct.
const noted = (d.sources || []).filter(s => s.note);
if (noted.length) {
  ok(noted.every(s => h.includes(String(s.note).slice(0, 40))),
     'every source NOTE is rendered (' + noted.length + ') — a caveat the API attached and the panel drops is a number with no caveat');
} else {
  console.log('  --   no source on this card carries a note; that assertion is not exercised');
}

// Truncation must be visible: silent capping reads as complete coverage.
const capped = (d.sources || []).filter(s => s.rows_total > s.rows);
if (capped.length) ok(h.includes('capped'), 'a capped source says so (' + capped.length + ')');

const untimed = (d.timeline || []).some(t => t.ordering && t.ordering !== 'timestamped');
ok(untimed === h.includes('bd-lin-untimed'),
   'untimed badge presence matches the data (' + untimed + ') — otherwise it invents a chronology');
ok(h.includes('AMUX-3607') && h.includes('authz:'),
   'states WHICH actions carry an authorisation trail and which do not');
ok(!/<script|onerror=/i.test(h), 'payload text cannot inject markup');

console.log('\n' + pass + ' passed, ' + fail + ' failed');
process.exit(fail ? 1 : 0);
EOF

echo "Lineage renderer, against a REAL payload for $CARD:"
node "$TMP/run.mjs" "$TMP"
