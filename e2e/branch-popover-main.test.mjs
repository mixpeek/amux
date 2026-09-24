// The branch popover must never tell a worker on main that it is "Not on main".
// Regression: hasBranch was `sessionBranch || ...`, so a recorded branch of
// "main" (a truthy string) took the isolated-branch arm.
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';

const src = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const slice = (name) => {
  const i = src.indexOf('function ' + name + '(');
  assert.ok(i >= 0, name + ' not found');
  let d = 0, j = src.indexOf('{', i);
  for (let k = j; k < src.length; k++) { if (src[k] === '{') d++; else if (src[k] === '}' && --d === 0) return src.slice(i, k + 1); }
};

function render(sessBranch, gitBranch) {
  let appended = null;
  const doc = { querySelectorAll: () => [], addEventListener() {}, removeEventListener() {}, body: { appendChild: (p) => { appended = p; } }, documentElement: { clientWidth: 400 },
    createElement: () => ({ style: {}, set innerHTML(v) { this.html = v; }, get innerHTML() { return this.html; } }) };
  const fn = new Function('document', 'gitInfo', 'sessions', 'esc', '_cssRect', 'window',
    slice('_isBranchMain') + '\n' + slice('showBranchPopover') + '\nreturn showBranchPopover;')(
    doc, { w: { branch: gitBranch } }, [{ name: 'w', branch: sessBranch }], (s) => String(s),
    () => ({ left: 0, top: 0, bottom: 0, right: 0, width: 0, height: 0 }), { innerWidth: 400, innerHeight: 800 });
  try { fn('w', { stopPropagation() {}, target: {} }); } catch (_) { /* positioning after render is not under test */ }
  return appended ? appended.html : '';
}

test('recorded branch main offers a worker branch, never "Not on main"', () => {
  const html = render('main', 'main');
  assert.doesNotMatch(html, /Not on main/);
  assert.match(html, /Create worker branch/);
});

test('a real worker branch still reports isolation', () => {
  const html = render('session/w', 'session/w');
  assert.match(html, /Not on main/);
});
