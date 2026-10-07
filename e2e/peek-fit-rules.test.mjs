// A border rule on a collapsible block's first or last line keeps the block's
// markup. _fitRules used to replace the whole line, dropping the block's
// closing </div></div>, so every later block nested inside a collapsed tool
// call and the peek looked frozen (Ethan, 2026-10-07, amux-helper).
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';
const app = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const fn = name => {
  const i = app.indexOf('function ' + name + '('); let d = 0;
  for (let k = app.indexOf('{', i); k < app.length; k++) { if (app[k] === '{') d++; else if (app[k] === '}' && --d === 0) return app.slice(i, k + 1); }
};
const warned = [];
const fit = new Function('esc', '_peekMarkupWarn',
  "const _PTC_BODY_OPEN = '<div class=\"ptc-body\">';\n" + fn('_fitRules') + '\nreturn _fitRules;'
)(s => String(s), (r, n) => warned.push([r, n]));
const RULE = '─'.repeat(60);
const divs = s => [(s.match(/<div\b/g) || []).length, (s.match(/<\/div>/g) || []).length];
test("a rule on a block's last line keeps the block's closing tags", () => {
  const html = '<div class="ptc collapsed"><div class="ptc-head">⏺ Bash(x)</div><div class="ptc-body">  ⎿  out\n'
    + '<span style="color:grey">     ' + RULE + '</span></div></div>\nlater reply';
  const out = fit(html);
  assert.deepEqual(divs(out), divs(html));
  assert.match(out, /<span class="peek-rule"><\/span><\/div><\/div>\nlater reply$/);
});
test("a rule on a block's first body line keeps the block's opening tags", () => {
  const html = '<div class="ptc"><div class="ptc-head">⏺ Bash(x)</div><div class="ptc-body">' + RULE + ' gs12-model ──</div></div>';
  const out = fit(html);
  assert.deepEqual(divs(out), divs(html));
  assert.match(out, /<div class="ptc-body"><span class="peek-rule"><span class="peek-rule-tag">gs12-model<\/span><\/span><\/div><\/div>$/);
});
test('a plain rule line is still fitted, and nothing is reported', () => {
  assert.equal(fit(RULE), '<span class="peek-rule"></span>');
  assert.equal(fit('plain'), 'plain');
  assert.deepEqual(warned, []);
});
