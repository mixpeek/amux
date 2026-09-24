// The create-worker modal lets the owner put the new worker in a group.
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';
const app = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const html = readFileSync(new URL('../crates/amux-dashboard/static/index.html', import.meta.url), 'utf8');
const slice = (name) => { const i = app.indexOf('function ' + name + '('); let d = 0;
  for (let k = app.indexOf('{', i); k < app.length; k++) { if (app[k] === '{') d++; else if (app[k] === '}' && --d === 0) return app.slice(i, k + 1); } };
test('the modal has a group field with suggestions', () => {
  assert.match(html, /id="create-group"[^>]*list="create-group-list"/);
  assert.match(html, /<datalist id="create-group-list">/);
});
test('typed groups become tags: trimmed, lowercased, deduplicated', () => {
  const f = new Function(slice('_createGroupsFrom') + '\nreturn _createGroupsFrom;')();
  assert.deepEqual(f(' Backend, api,backend ,, '), ['backend', 'api']);
  assert.deepEqual(f(''), []);
});
test('submitCreate sends them as tags', () => {
  assert.match(slice('submitCreate'), /if \(_groups\.length\) createBody\.tags = _groups;/);
});
