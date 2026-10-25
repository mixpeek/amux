// A first visit has no service worker in control. When the new worker claims the page it
// fires controllerchange, and reloading then threw away a new amux Cloud customer's
// "Sign in with Claude" click about 5 s into their first load (2026-10-19). Only a worker
// that REPLACES one already in control means the page runs stale code.
import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source = fs.readFileSync('crates/amux-dashboard/static/app.js', 'utf8');
const start = source.lastIndexOf("if ('serviceWorker' in navigator) {", source.indexOf('let _swHadController'));
const code = source.slice(start, source.indexOf('  // A PWA kept in the foreground never re-checks', start)) + '\n}';

async function load(controller) {
  const handlers = {}, reloads = [];
  const ctx = vm.createContext({
    navigator: { onLine: false, serviceWorker: { controller,
      register: async () => ({ active: null }),
      addEventListener: (ev, fn) => { handlers[ev] = fn; } } },
    location: { reload: () => reloads.push(1) },
    document: { getElementById: () => null }, sessionStorage: { getItem: () => null, setItem() {} },
    localStorage: { setItem() {} }, console: { info() {}, warn() {} }, fetch: async () => ({ text: async () => '' }),
  });
  vm.runInContext(code, ctx);
  await new Promise(r => setTimeout(r, 0));
  return { fire: () => handlers.controllerchange(), reloads };
}

test('the first worker taking control of a first visit does not reload the page', async () => {
  const page = await load(null);
  page.fire();
  assert.equal(page.reloads.length, 0);
  page.fire();                      // a later update on the same page still reloads
  assert.equal(page.reloads.length, 1);
});

test('a worker replacing one already in control reloads onto the new code', async () => {
  const page = await load({ state: 'activated' });
  page.fire();
  assert.equal(page.reloads.length, 1);
});
