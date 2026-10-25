// The dashboard's model sign-in pastes the code into the CLI's own login in a server
// terminal. Claude Code reads one write holding the code and its Enter as a paste and
// never submits it (amux Cloud e2e, 2026-10-19), so the Enter must be a write of its own.
import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source = fs.readFileSync('crates/amux-dashboard/static/app.js', 'utf8');
const slice = (from, to) => source.slice(source.indexOf(from), source.indexOf(to));
const code = slice('async function _mlInput(', 'async function _mlPollOutput(') +
  slice('async function modelLoginSubmitCode(', '// Leaving the page ends a sign-in');

test('the pasted code and its Enter reach the terminal as separate writes', async () => {
  const writes = [];
  const els = { 'model-login-paste': { value: '  abc123#state456  ' }, 'model-login-err': { textContent: '' },
                'model-login-status': { textContent: '' } };
  const ctx = vm.createContext({
    API: '', btoa, unescape, encodeURIComponent, setTimeout,
    _modelLogin: { id: 'term1' },
    _mlEl: id => els[id],
    fetch: async (url, opts) => { writes.push({ url, data: Buffer.from(JSON.parse(opts.body).data, 'base64').toString() }); return { ok: true }; },
  });
  vm.runInContext(code, ctx);
  await ctx.modelLoginSubmitCode();
  assert.deepEqual(writes.map(w => w.data), ['abc123#state456', '\r']);
  assert.ok(writes.every(w => w.url === '/api/terminal/term1/input'));
  assert.equal(els['model-login-paste'].value, '', 'box cleared');
  assert.equal(els['model-login-status'].textContent, 'Checking…');
});
