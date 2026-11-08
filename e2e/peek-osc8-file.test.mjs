// A file:// link a worker prints (OSC-8) opens in amux's file viewer, never as
// a file:// URL handed to the browser or to Finder (owner, 2026-11-08: the Mac
// app showed "The application can't be opened. -50" for /home/amux).
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source = fs.readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const ctx = vm.createContext({ rewriteLocalhostUrls: (s) => s, URL });
const a = source.indexOf('function _osc8Resolve(');
vm.runInContext(source.slice(a, source.indexOf('\n}\n', a) + 3), ctx);
const park = (url, label) => [`0${label}`, [url]];
const resolve = (url, label) => ctx._osc8Resolve(...park(url, label));

test('a local file:// link becomes a file-viewer link to its path', () => {
  const h = resolve('file:///home/amux', '/home/amux');
  assert.ok(!/href=/.test(h), h);
  assert.match(h, /class="file-link" data-path="\/home\/amux"/);
  assert.match(h, /onclick="_openLinkedPath\(this,event\)"/);
  assert.equal(h.replace(/<[^>]+>/g, ''), '/home/amux');
});
test('a markdown file opens in the markdown viewer, and an encoded path is decoded', () => {
  assert.match(resolve('file://localhost/Users/e/My%20Notes/a.md', 'a.md'), /class="md-link" data-path="\/Users\/e\/My Notes\/a.md"/);
});
test('a label that crosses colours gets one link per run', () => {
  const h = ctx._osc8Resolve('0<span a>src/</span><span b>x.rs</span>', ['file:///r/src/x.rs']);
  assert.equal((h.match(/data-path="\/r\/src\/x.rs"/g) || []).length, 2, h);
});
test('a remote-host file URL stays plain; http links are unchanged', () => {
  assert.equal(resolve('file://otherbox/etc/x', 'x'), 'x');
  assert.match(resolve('https://ex.com/a', 'site'), /<a href="https:\/\/ex.com\/a"/);
});
