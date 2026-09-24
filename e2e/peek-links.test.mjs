// Links in worker output render clickable, including across syntax colours.
// Asked by tubescience-parity for Ethan, 2026-09-24.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source = fs.readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const slice = (a, b) => source.slice(source.indexOf(a), source.indexOf(b, source.indexOf(a)));
const ctx = vm.createContext({ location: { origin: 'https://h', port: '8824' }, _osc8Resolve: (h) => h });
vm.runInContext("var peekSessionDir = '/Users/ethan/Dev/mixpeek/customers/tubescience/';", ctx);
vm.runInContext(slice('function rewriteLocalhostUrls(', '// ═══════ PEEK MODE'), ctx);
{ const a = source.indexOf('const _LINK_TLDS'); const f = source.indexOf('function ansiToHtml(', a);
  vm.runInContext(source.slice(a, source.indexOf('\n}\n', f) + 3), ctx); }
const html = t => ctx.ansiToHtml(t);
const hrefs = h => [...h.matchAll(/href="([^"]+)"/g)].map(m => m[1]);
const paths = h => [...h.matchAll(/data-path="([^"]+)"/g)].map(m => m[1]);
const C = n => `\x1b[38;2;${n};1;1m`;

test('a URL split across colour codes is still one link target', () => {
  const h = html(`see ${C(1)}https://flawless-${C(2)}footage.ts.app/${C(3)}flow?x=1\x1b[0m now`);
  const hs = hrefs(h);
  assert.ok(hs.length >= 1 && hs.every(x => x === 'https://flawless-footage.ts.app/flow?x=1'), JSON.stringify(hs));
  assert.equal(h.replace(/<[^>]+>/g, ''), 'see https://flawless-footage.ts.app/flow?x=1 now');
});
test('a bare domain on a real TLD links; an email does not', () => {
  const h = html('Captured on production `flawless-footage.ts.app` as `ethan.mixpeek@tubescience.com`');
  assert.deepEqual(hrefs(h), ['https://flawless-footage.ts.app']);
});
test('repo-relative paths and file:line refs link', () => {
  const h = html('wrote customers/tubescience/parity/ff-capture/2026-09-24-FLAWLESS-FOOTAGE-FLOW-MAP.md and crates/amux-server/src/api/vault.rs:212 plus ./e2e/x.mjs');
  assert.deepEqual(paths(h), ['customers/tubescience/parity/ff-capture/2026-09-24-FLAWLESS-FOOTAGE-FLOW-MAP.md',
    'crates/amux-server/src/api/vault.rs:212', './e2e/x.mjs']);
});
test('a path coloured in pieces still links whole', () => {
  const h = html(`${C(1)}customers/${C(2)}tubescience/parity/${C(3)}MAP.md\x1b[0m`);
  const ps = paths(h);
  assert.ok(ps.length >= 1 && ps.every(p => p === 'customers/tubescience/parity/MAP.md'), JSON.stringify(ps));
});
test('an @-mention links the path without the @', () => {
  assert.deepEqual(paths(html('look at @/Users/ethan/.amux/uploads/e88f-image.png please')), ['/Users/ethan/.amux/uploads/e88f-image.png']);
});
test('the tail of a hard-wrapped path is not a link of its own', () => {
  assert.deepEqual(paths(html('saved to /private/tmp/claude-5\n01/scratch/out.txt')), []);
});
test('with no known worker directory, a relative path stays plain', () => {
  vm.runInContext("peekSessionDir = '';", ctx);
  assert.deepEqual(paths(html('see a/b/c.md')), []);
  vm.runInContext("peekSessionDir = '/Users/ethan/Dev/mixpeek/customers/tubescience/';", ctx);
});
test('absolute paths keep working', () => {
  assert.deepEqual(paths(html('open /Users/ethan/Dev/amux/README.md:3 please')), ['/Users/ethan/Dev/amux/README.md:3']);
});
test('no false links', () => {
  const h = html('app.js and items.json, and/or e.g. v1.2.3 then 3.14 or x/y without ext');
  assert.deepEqual(hrefs(h), []);
  assert.deepEqual(paths(h), []);
});
test('text and escaping are preserved', () => {
  const h = html('a <b> & https://x.com/?q=<1>');
  assert.equal(h.replace(/<[^>]+>/g, ''), 'a &lt;b&gt; &amp; https://x.com/?q=&lt;1&gt;'.replace('&lt;1&gt;', '&lt;1&gt;'));
});
