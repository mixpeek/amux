#!/usr/bin/env bash
# The fleet-wide cross-group switch must reflect the persisted global env value,
# never a synthetic offline-outbox acknowledgement or an optimistic checkbox.
# Drives the shipped app.js functions and outbox predicate.
set -uo pipefail
cd "$(dirname "$0")/.."
APP=crates/amux-dashboard/static/app.js
command -v node >/dev/null 2>&1 || { echo "SKIP: node not installed"; exit 2; }

node <<'NODE'
const fs = require('fs');
const src = fs.readFileSync('crates/amux-dashboard/static/app.js', 'utf8');
function grabConst(name) {
  const m = src.match(new RegExp('^const ' + name + ' = .*?;$', 'm'));
  if (!m) throw new Error('cannot find const ' + name);
  return m[0];
}
function grabFn(name) {
  const i = src.indexOf('function ' + name + '(');
  if (i < 0) throw new Error('cannot find function ' + name);
  const start = src.slice(Math.max(0, i - 6), i) === 'async ' ? i - 6 : i;
  let j = src.indexOf('{', i), depth = 0, k = j;
  for (; k < src.length; k++) {
    if (src[k] === '{') depth++;
    else if (src[k] === '}' && --depth === 0) break;
  }
  return src.slice(start, k + 1);
}

globalThis.location = { origin: 'https://localhost:8824' };
globalThis.API = '';
const geval = eval;
geval(grabConst('_OUTBOX_SKIP').replace(/^const /, 'var '));
geval(grabConst('_OUTBOX_METHODS').replace(/^const /, 'var '));
geval(grabFn('_outboxQueueable'));
geval(grabFn('_isLocallyQueued'));
geval(grabFn('readCrossGroupDefault'));
geval(grabFn('toggleCrossGroupDefault'));

const failures = [];
const check = (name, value) => {
  if (value) console.log('ok   ' + name);
  else { console.log('FAIL ' + name); failures.push(name); }
};
const put = { method: 'PUT', body: '{"allow":"*"}' };
check('cross-group PUT is never queued',
  _outboxQueueable('/api/config/cross-group', put) === false);
check('absolute cross-group PUT is never queued',
  _outboxQueueable(location.origin + '/api/config/cross-group', put) === false);
check('ordinary config mutation remains queueable',
  _outboxQueueable('/api/sessions/probe/config', put) === true);

const checkbox = { checked: false };
const note = { textContent: '', style: {} };
globalThis.document = { getElementById: id =>
  id === 'crossgroup-default-checkbox' ? checkbox :
  id === 'crossgroup-default-note' ? note : null };
let toasts = [];
globalThis.showToast = s => toasts.push(String(s));
globalThis._authHeaders = extra => Object.assign({ 'X-Test-Auth': 'yes' }, extra || {});
const real = (body, status = 200) => new Response(JSON.stringify(body), {
  status, headers: { 'Content-Type': 'application/json' }
});

(async () => {
  let calls = [];
  globalThis.fetch = async (url, init = {}) => {
    calls.push([url, init]);
    return calls.length === 1 ? real({ ok: true, enabled: true, message: 'saved' })
                              : real({ enabled: true, gate_enforcing: true });
  };
  checkbox.checked = true;
  await toggleCrossGroupDefault(true);
  check('success is verified with PUT then GET', calls.length === 2);
  check('persisted success keeps the switch on', checkbox.checked === true);
  check('PUT uses the normal auth-header helper', calls[0][1].headers['X-Test-Auth'] === 'yes');

  calls = []; toasts = [];
  globalThis.fetch = async (url, init = {}) => {
    calls.push([url, init]);
    return calls.length === 1 ? real({ ok: true, enabled: true })
                              : real({ enabled: false, gate_enforcing: true });
  };
  checkbox.checked = true;
  await toggleCrossGroupDefault(true);
  check('read-back mismatch rolls the switch back', checkbox.checked === false);
  check('read-back mismatch is visible', toasts.some(t => t.includes('not persisted')));

  calls = []; toasts = [];
  globalThis.fetch = async () => {
    calls.push(1);
    return new Response(JSON.stringify({ ok: true, queued: true, offline: true }), {
      status: 202,
      headers: { 'Content-Type': 'application/json', 'X-Amux-Outbox': 'queued' }
    });
  };
  checkbox.checked = true;
  await toggleCrossGroupDefault(true);
  check('synthetic outbox success rolls the switch back', checkbox.checked === false);
  check('synthetic outbox success is not read back as persisted', calls.length === 1);

  calls = []; toasts = [];
  globalThis.fetch = async () => { calls.push(1); throw new Error('offline'); };
  checkbox.checked = true;
  await toggleCrossGroupDefault(true);
  check('transport failure rolls the switch back', checkbox.checked === false);
  check('transport failure is visible', toasts.some(t => t.includes('offline')));

  if (failures.length) process.exit(1);
})().catch(err => { console.error(err); process.exit(1); });
NODE
