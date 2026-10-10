// File paths in a chat reply open like peek's (Ethan, 2026-10-07, AMUX-5670):
// the same linkifier, including `~/` paths inside inline code.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source = fs.readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const fn = name => {
  const i = source.indexOf('function ' + name + '('); let d = 0;
  for (let k = source.indexOf('{', i); k < source.length; k++) { if (source[k] === '{') d++; else if (source[k] === '}' && --d === 0) return source.slice(i, k + 1); }
};
const ctx = vm.createContext({});
vm.runInContext("var peekSessionDir = '/Users/ethan/Dev/amux';"
  + "function esc(s){return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/\"/g,'&quot;');}"
  + "function escJs(s){return String(s).replace(/\\\\/g,'\\\\\\\\').replace(/'/g,\"\\\\'\");}", ctx);
for (const n of ['_resolveOutputPath', '_linkifyPaths', '_chatLinkify']) vm.runInContext(fn(n), ctx);
const opens = h => [...h.matchAll(/_openPathFromOutput\('([^']+)'\)/g)].map(m => m[1]);

test('a ~/ path in inline code is a link, and is not joined onto the cwd', () => {
  const h = ctx._chatLinkify('found <code>~/.amux/server.env</code> with 51 lines');
  assert.deepEqual(opens(h), ['~/.amux/server.env']);
  assert.equal(ctx._resolveOutputPath('~/.amux/server.env'), '~/.amux/server.env');
});
test('an absolute path in inline code is a link', () => {
  assert.deepEqual(opens(ctx._chatLinkify('<code>/Users/ethan/.amux/server.env</code>')), ['/Users/ethan/.amux/server.env']);
});
test('the four chat render sites go through the linkifier', () => {
  const chat = source.slice(source.indexOf('function _chatBubble('), source.indexOf('function _chatRender('))
    + source.slice(source.indexOf('function _chatRender('), source.indexOf('function _chatRender(') + 12000);
  assert.equal((chat.match(/_chatMarkdown\(/g) || []).length, 4);
  assert.equal((chat.match(/[^(]renderMarkdown\(/g) || []).length, 0, 'no chat site renders markdown without it');
});


test('Chat uses the worker cwd for Markdown files and binds delegated viewer clicks', () => {
  const markdown = fn('_chatMarkdown');
  assert.match(markdown, /peekSessionDir/);
  assert.match(markdown, /renderMarkdown\(text, base\)/);
  const render = source.slice(source.indexOf('function _chatRender('), source.indexOf('function _chatRender(') + 400);
  assert.match(render, /_bindMdFileLinks\(body\)/);
});
