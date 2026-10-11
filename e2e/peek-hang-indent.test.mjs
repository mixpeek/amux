// Wrapped peek lines hang under their text like the terminal, not at the edge.
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';
const app = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const i = app.indexOf('function _hangIndent('); let d = 0, end = 0;
for (let k = app.indexOf('{', i); k < app.length; k++) { if (app[k] === '{') d++; else if (app[k] === '}' && --d === 0) { end = k + 1; break; } }
// The function reads module-level constants declared just above it; carry
// them along, or every call throws (this file failed silently from 892e1cae:
// Playwright loads it but does not count node:test results).
const consts = (app.slice(Math.max(0, i - 400), i).match(/^const _[A-Z_]+ = .*;$/gm) || []).join('\n');
const hang = new Function(consts + '\n' + app.slice(i, end) + '\nreturn _hangIndent;')();
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
test('a reply block\'s first and last lines hang inside their markup', () => {
  const open = '<div class="ptc"><div class="ptc-body">  - first reply line';
  assert.match(hang(open), /<div class="ptc-body"><span class="pk-hang" style="--h:4ch">  - first reply line<\/span>$/);
  assert.match(hang('  - last reply line</div></div>'), /^<span class="pk-hang" style="--h:4ch">  - last reply line<\/span><\/div><\/div>$/);
});
test('the peek pipeline applies it last', () => {
  // The pipelines are lists now (PEEK_PIPELINES); every full pipeline is built
  // from _PEEK_FULL, which must end with scroll-box-blocks then hang-indent.
  assert.match(app, /const _PEEK_FULL = \[[^\]]*'scroll-box-blocks', 'hang-indent'\];/);
  assert.match(app, /const _PEEK_FULL_NO_CALLS = _PEEK_FULL\.filter\(/);
});
