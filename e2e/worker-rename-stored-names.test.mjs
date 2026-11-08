// A renamed worker's old name, stored by the app, is rewritten to the new one
// (2026-11-06: a phone kept sending to `workflows` after it became `wexus`).
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';

const src = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const slice = (name) => {
  const i = src.indexOf('function ' + name + '(');
  assert.ok(i >= 0, name + ' not found');
  let d = 0;
  for (let k = src.indexOf('{', i); k < src.length; k++) {
    if (src[k] === '{') d++; else if (src[k] === '}' && --d === 0) return src.slice(i, k + 1);
  }
};
const rewrite = new Function(slice('_workerRenameRewrite') + '\nreturn _workerRenameRewrite;')();
const map = { workflows: 'wexus', 'mtp-demo': 'medical-talent-pros' };

test('a queued send to the old name is addressed to the new one', () => {
  const q = { id: 'a', url: '/api/sessions/workflows/send', state: 'blocked',
    options: { method: 'POST', body: JSON.stringify({ text: 'Draft the email Alan should send' }) } };
  const r = rewrite(q, map);
  assert.equal(r.changed, true);
  assert.equal(r.value.url, '/api/sessions/wexus/send');
  assert.equal(r.value.options.body, q.options.body, 'the message text is untouched');
});

test('the open worker, split panes and grid order follow', () => {
  assert.deepEqual(rewrite({ session: 'workflows', tab: 'messages' }, map).value, { session: 'wexus', tab: 'messages' });
  assert.deepEqual(rewrite(['mtp-demo', 'rothco'], map).value, ['medical-talent-pros', 'rothco']);
  assert.equal(rewrite('#peek=workflows&tab=chat', map).value, '#peek=wexus&tab=chat');
  assert.equal(rewrite('/api/workers/mtp-demo/peek?x=1', map).value, '/api/workers/medical-talent-pros/peek?x=1');
});

test('per-worker keys are renamed', () => {
  assert.deepEqual(rewrite({ workflows: { draft: 'hi' } }, map).value, { wexus: { draft: 'hi' } });
});

test('names that merely contain an old name are left alone', () => {
  const r = rewrite({ session: 'workflows-2', text: 'see workflows', url: '/api/sessions/workflows-2/send' }, map);
  assert.equal(r.changed, false);
  assert.deepEqual(r.value, { session: 'workflows-2', text: 'see workflows', url: '/api/sessions/workflows-2/send' });
});
