#!/usr/bin/env bash
# An mdai run must never enter the offline outbox (AF-371).
#
# WHAT ETHAN SAW, as two separate reports that turned out to be one bug:
#   "mdai files are stuck at running"
#   a banner reading `Syncing 0/1 · POST /api/files/mdai/run` that never cleared
#
# THE MECHANISM. The window.fetch interceptor queues a failed mutation and hands
# the caller a synthetic 202 `{ok:true,queued:true}`. `_mdaiRun` checked only
# `!r.ok || d.error`; a 202 is ok and carries no error, so the SUCCESS branch took
# it, the panel reported a completed run, and no run had left the browser. The
# queued op then sat in the outbox as an unfinished sync.
#
# ORDINARY PATH, not an outage: the auto-builder restarts the server on every
# commit, so any run straddling a deploy hits this. A run is 82-127s.
#
# Two independent guards, and this file pins BOTH, because either alone leaves a
# real hole: the skip entry stops this url being queued, and the caller-side check
# stops any other path handing the panel a 202 it reads as output.
#
# Drives the SHIPPED functions extracted from app.js, never a retyped copy.
set -uo pipefail
cd "$(dirname "$0")/.."
APP=crates/amux-dashboard/static/app.js
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
command -v node >/dev/null 2>&1 || { echo "SKIP: node not installed"; exit 2; }

echo "== mdai outbox cells =="

out=$(node -e '
const fs = require("fs");
const src = fs.readFileSync("'"$APP"'", "utf8");
function grabConst(name) {
  const re = new RegExp("^const " + name + " = .*?;$", "m");
  const m = src.match(re);
  if (!m) throw new Error("cannot find const " + name);
  return m[0];
}
function grabFn(name) {
  const i = src.indexOf("function " + name + "(");
  if (i < 0) throw new Error("cannot find function " + name);
  let j = src.indexOf("{", i), depth = 0, k = j;
  for (; k < src.length; k++) {
    if (src[k] === "{") depth++;
    else if (src[k] === "}") { depth--; if (depth === 0) break; }
  }
  return src.slice(i, k + 1);
}
// Indirect eval so the shipped declarations land in GLOBAL scope; a plain
// eval() defines them in the enclosing function scope, where _outboxQueueable
// cannot see the consts it closes over.
// The shipped _outboxQueueable reads location.origin, so node needs one. Stubbed
// rather than stripped from the source: editing the function under test is how a
// test stops testing the thing that ships.
globalThis.location = { origin: "https://localhost:8824" };
const geval = eval;
geval(grabConst("_OUTBOX_SKIP").replace(/^const /, "var "));
geval(grabConst("_OUTBOX_METHODS").replace(/^const /, "var "));
geval(grabFn("_outboxQueueable"));
geval(grabFn("_isLocallyQueued"));

const fails = [];
const t = (n, c) => { if (!c) fails.push(n); };
const RUN = "/api/files/mdai/run";
const post = { method: "POST", body: "{}" };

// 1. THE BUG: an mdai run must be refused by the outbox.
t("an mdai run is not queueable", _outboxQueueable(RUN, post) === false);
t("the absolute-url form is refused too",
  _outboxQueueable("https://localhost:8824" + RUN, post) === false);

// 2. THE CONTROL, load-bearing: the outbox must still ACCEPT ordinary mutations.
//    A skip regex broadened until it matched everything would pass cell 1 while
//    silently disabling offline sync for the whole app.
t("an ordinary board POST is still queueable",
  _outboxQueueable("/api/board", post) === true);
t("an ordinary session send is still queueable",
  _outboxQueueable("/api/sessions/amux/send", post) === true);

// 3. NEIGHBOURING mdai ROUTES ARE UNAFFECTED. Only the RUN is expensive and
//    answer-now; history and connect are ordinary writes. A regex of
//    `files/mdai` would over-skip and this is what catches that.
t("mdai connect is still queueable",
  _outboxQueueable("/api/files/mdai/connect", post) === true);

// 4. THE CALLER-SIDE GUARD. A synthetic 202 must be recognised as NOT a run.
const queued = new Response(JSON.stringify({ ok: true, queued: true, offline: true }),
  { status: 202, headers: { "Content-Type": "application/json", "X-Amux-Outbox": "queued" } });
t("a synthetic 202 is recognised as locally queued", _isLocallyQueued(queued) === true);

// 5. AND ITS CONTROL: a REAL 200 from the server must not be mistaken for one,
//    or the panel would report every successful run as unreachable.
const real = new Response(JSON.stringify({ output: "# result" }),
  { status: 200, headers: { "Content-Type": "application/json" } });
t("a real server response is NOT read as locally queued", _isLocallyQueued(real) === false);

// 6. The run path must actually CONSULT that guard. Source-level, and labelled:
//    _mdaiRun is a long async function against live DOM and fetch, so executing
//    it here would test a harness rather than the shipped path.
const runFn = grabFn("_mdaiRun");
t("_mdaiRun consults _isLocallyQueued", /_isLocallyQueued\(\s*r\s*\)/.test(runFn));

if (fails.length) { console.log("FAILED:" + fails.join("|")); process.exit(1); }
console.log("ALLOK");
' 2>&1)

if [ "$out" = "ALLOK" ]; then
  for c in "an mdai run is not queueable" \
           "the absolute-url form is refused too" \
           "an ordinary board POST is still queueable" \
           "an ordinary session send is still queueable" \
           "mdai connect is still queueable" \
           "a synthetic 202 is recognised as locally queued" \
           "a real server response is NOT read as locally queued" \
           "_mdaiRun consults _isLocallyQueued"; do ok "$c"; done
else
  echo "$out" | sed 's/^/    /'; FAIL=1
fi

echo
echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
