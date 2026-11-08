import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

// A WebKit storage read can hang for hours in the Mac app. The terminal's
// failed-fetch path awaited its saved copy through _idb.get, so a worker's
// peek request never settled and every later poll reused it: the worker
// opened to an empty terminal until the app was reloaded (2026-11-07).
// These run the SHIPPED _idb module against an IndexedDB whose reads never
// answer.

const source = fs.readFileSync('crates/amux-dashboard/static/app.js', 'utf8');
const start = source.indexOf('const _idb = (() => {');
const end = source.indexOf('\n})();\n', start) + 6;
assert.ok(start > 0 && end > start, '_idb module markers must exist in app.js');
const code = source.slice(start, end) + '\nglobalThis._idb = _idb;';

function hungDb() {
  const beacons = [];
  let opens = 0;
  const db = {
    objectStoreNames: { contains: () => true },
    close() {},
    // Every request is accepted and never answered, like the hung storage process.
    transaction: () => ({ objectStore: () => ({ get: () => ({}), getAll: () => ({}), put: () => ({}) }) }),
  };
  const ctx = vm.createContext({
    console, Promise, Map, Set, JSON, String, Date, Math, setTimeout, clearTimeout,
    window: {}, document: { visibilityState: 'visible', hidden: false, addEventListener() {} },
    addEventListener() {}, _uploadStorageError() {}, CHUNK_SIZE: 1,
    fetch: async (url, opts) => { beacons.push(JSON.parse(opts.body)); return { ok: true }; },
    indexedDB: { open: () => { opens++; const req = {}; setTimeout(() => { req.result = db; req.onsuccess(); }, 0); return req; } },
  });
  vm.runInContext(code, ctx);
  return { idb: ctx._idb, beacons, opens: () => opens };
}

test('a cache read settles as absent when storage never answers, and resets the connection', async () => {
  const { idb, beacons, opens } = hungDb();
  const t0 = Date.now();
  const v = await Promise.race([idb.get('peek_amux-admin'), new Promise(r => setTimeout(() => r('hung'), 6000))]);
  const waited = Date.now() - t0;
  assert.equal(v, null);
  assert.ok(waited >= 2900 && waited < 4500, 'settles at the deadline, waited ' + waited + 'ms');
  const reset = beacons.find(b => b.reason === 'cache_read_timeout');
  assert.ok(reset, 'the timeout is beaconed: ' + JSON.stringify(beacons));
  assert.equal(reset.verdict, 'idb_connection_reset');
  assert.equal(reset.had_connection, true);
  // The next read opens a fresh connection rather than reusing the hung one.
  const again = idb.get('chat_amux-admin');
  await new Promise(r => setTimeout(r, 20));
  assert.equal(opens(), 2);
  assert.equal(await Promise.race([again, new Promise(r => setTimeout(() => r('hung'), 6000))]), null);
});

test('a pending-work read keeps waiting: it is never read as empty', async () => {
  const { idb, beacons } = hungDb();
  const sentinel = Symbol('still waiting');
  const v = await Promise.race([idb.get('offline_queue'), new Promise(r => setTimeout(() => r(sentinel), 3500))]);
  assert.equal(v, sentinel, 'offline_queue must not resolve as absent on a timeout');
  assert.equal(beacons.filter(b => b.reason === 'cache_read_timeout').length, 0);
});
