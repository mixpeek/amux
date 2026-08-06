// Board "Ask Amux" status-request toast — regression test for AMUX-4.
//
// Ethan clicked Ask Amux three times and got "Could not reach Amux" every
// time. All three had actually been DELIVERED (the card log recorded them and
// the session received the prompts) — only the toast was wrong. apiCall()
// resolves to the raw Response, not parsed JSON, so reading `.delivered` off
// it was always undefined and the success branch was unreachable.
//
// Like tests/test_peek_parity.py, this loads the REAL function out of
// amux-server.py instead of replicating it. That is the whole point: a
// re-typed paraphrase of _askCardStatus would have passed against the broken
// client, because the bug was in the shipped text, not in the logic as
// imagined (ethos §7 — test the shipped code path, not a paraphrase).
//
// Run:  node tests/test_ask_card_status.mjs
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const SERVER = join(dirname(fileURLToPath(import.meta.url)), '..', 'amux-server.py');
const src = readFileSync(SERVER, 'utf8');

const m = src.match(/async function _askCardStatus\(id, sess\) \{[\s\S]*?\n\}/);
if (!m) {
  console.error('FAIL: could not extract _askCardStatus from amux-server.py '
    + '(renamed or reshaped? update this test rather than deleting it)');
  process.exit(1);
}
const fnSrc = m[0];

let toasts = [];

// Stubs mirroring the real client environment. apiCall returns the raw
// Response — that IS the contract, and the bug was forgetting it.
function makeApiCall(payload, { httpOk = true } = {}) {
  return async () => {
    if (!httpOk) return null;   // apiCall already toasted / queued offline
    return new Response(JSON.stringify(payload), {
      status: 200, headers: { 'Content-Type': 'application/json' },
    });
  };
}

async function check(label, payload, expectSubstr, opts) {
  toasts = [];
  // Evaluating source read from our own repo file, same trust model as the
  // ast/exec loading in the Python tests next door.
  const fn = new Function('API', 'apiCall', 'showToast',
    fnSrc + '; return _askCardStatus;')(
    'https://localhost:8822', makeApiCall(payload, opts), (t) => toasts.push(t));
  await fn('AMUX-4', 'Amux');
  const got = toasts[0] === undefined ? '(no toast)' : toasts[0];
  const pass = expectSubstr === null ? toasts.length === 0 : got.includes(expectSubstr);
  console.log(`${pass ? 'PASS' : 'FAIL'}  ${label}\n      toast: ${got}`);
  return pass;
}

const results = [];

// The exact case Ethan hit: the server delivered it, so the UI must say so.
results.push(await check('delivered:true -> success toast',
  { ok: true, delivered: true, session: 'Amux', message: 'asked Amux to post a status update' },
  'Asked Amux to post a status update'));

// The server's honest offline path must reach the human verbatim, not be
// flattened into a generic failure.
results.push(await check('delivered:false -> server reason shown',
  { ok: false, delivered: false, reason: "session 'Amux' is not running" },
  'is not running'));

// apiCall already showed 'Error: NNN' or queued the op — a second toast here
// would be a false claim about reachability.
results.push(await check('apiCall returned null -> no second toast',
  null, null, { httpOk: false }));

const failed = results.filter((r) => !r).length;
console.log(failed ? `\n${failed} test(s) FAILED` : '\nAll tests passed');
process.exit(failed ? 1 : 0);
