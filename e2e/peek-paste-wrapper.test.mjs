// A pasted prompt's <pasted_content id="..."> wrapper is transport, not what
// the person typed: the peek drops both tags (Ethan, 2026-10-08, screenshot of
// "</pasted_content id=\"bf0b\">" under a /goal prompt).
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';
const app = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const i = app.indexOf('function _peekUnwrapPaste('); let d = 0, end = 0;
for (let k = app.indexOf('{', i); k < app.length; k++) { if (app[k] === '{') d++; else if (app[k] === '}' && --d === 0) { end = k + 1; break; } }
const consts = (app.slice(Math.max(0, i - 900), i).match(/^const _PEEK_PASTE_TAG = .*;$/gm) || []).join('\n');
const unwrap = new Function(consts + '\n' + app.slice(i, end) + '\nreturn _peekUnwrapPaste;')();
test('both tags are dropped and the first pasted line sits beside the glyph', () => {
  const out = unwrap([
    '<span style="color:#888">❯ &lt;pasted_content id=&quot;bf0b&quot;&gt;</span>',
    '  /goal Execute the plan',
    '  to completion.',
    '  &lt;/pasted_content id=&quot;bf0b&quot;&gt;',
  ]);
  assert.equal(out.join('\n').includes('pasted_content'), false);
  assert.equal(out.length, 2);
  assert.match(out[0], /^<span style="color:#888">❯ <\/span>\/goal Execute the plan$/);
});
test('a closing tag with no id is dropped too', () => {
  const out = unwrap(['❯ &lt;pasted_content&gt;', '  text', '  &lt;/pasted_content&gt;']);
  assert.deepEqual(out, ['❯ text']);
});
test('a prompt with no paste is returned untouched', () => {
  const block = ['❯ fix the peek', '  second line'];
  assert.equal(unwrap(block), block);
});
test('every rendered prompt block goes through it, and classification strips an id-bearing close', () => {
  assert.match(app, /_peekUnwrapPaste\(lines\.slice\(i, end\)\)/);
  assert.match(app, /<\\\/pasted_content\\b\[\^>\]\*>/);
});
