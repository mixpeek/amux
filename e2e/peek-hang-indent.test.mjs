// Wrapped peek lines hang under their text like the terminal, not at the edge.
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';
const app = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const i = app.indexOf('function _hangIndent('); let d = 0, end = 0;
for (let k = app.indexOf('{', i); k < app.length; k++) { if (app[k] === '{') d++; else if (app[k] === '}' && --d === 0) { end = k + 1; break; } }
const hang = new Function(app.slice(i, end) + '\nreturn _hangIndent;')();
test('a list item hangs by its marker width', () => {
  const out = hang('  - <b>Group field:</b> the New worker dialog has an optional field');
  assert.match(out, /^<span class="pk-hang" style="--h:4ch">  - <b>Group field:<\/b>/);
});
test('Claude bullets and tool results hang too', () => {
  assert.match(hang('● When you create a worker'), /--h:2ch/);
  assert.match(hang('  ⎿  Wrote 939 lines'), /--h:5ch/);
});
test('plain lines, blank lines and box blocks are untouched', () => {
  assert.equal(hang('plain text'), 'plain text');
  assert.equal(hang('   '), '   ');
  const box = '<div class="peek-box">│ a │\n  - inside│</div>';
  assert.equal(hang(box), box);
});
test('the peek pipeline applies it last', () => {
  assert.match(app, /return _hangIndent\(wrapBoxBlocks\(/);
});
