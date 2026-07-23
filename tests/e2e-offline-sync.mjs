/**
 * E2E test: offline-first PWA sync (AMUX-1825, v0.9.179)
 *
 * Validates the three pillars of offline support against a LIVE local server:
 *   1. Shell — the service worker serves the dashboard from cache with the
 *      network cut (asserted via performance API: navigation transferSize 0).
 *   2. Outbox — API mutations issued offline are intercepted at the fetch
 *      boundary (synthetic 202 {queued:true}), persisted in localStorage
 *      across an offline reload, and carry a msg_id for idempotent replay.
 *   3. Sync — on reconnect the single page-side replayer applies queued ops
 *      server-side; transient failures are RETAINED (never silently lost)
 *      with their original msg_id; online mutations pass through untouched.
 *
 * Run: node tests/e2e-offline-sync.mjs   (server must be running on :8822)
 * Cleans up every board item it creates.
 */
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { chromium } = require('playwright');

const URL = process.env.AMUX_E2E_URL || 'https://localhost:8822/';
// A session that exists but cannot accept a send right now (archived/stopped)
// exercises the retention path; if the send settles instead, the test degrades
// gracefully and only asserts the queue reached a terminal state.
const SEND_TARGET = process.env.AMUX_E2E_SEND_TARGET || 'amux-helper';
const MARK = '[e2e-offline] outbox test ' + Date.now();

const fails = [];
const ok = (cond, label) => {
  console.log((cond ? '  ✓ ' : '  ✗ ') + label);
  if (!cond) fails.push(label);
};
const dismissOverlays = (page) => page.evaluate(() => {
  try { _wtDismiss && _wtDismiss(); } catch (e) {}
  document.getElementById('wt-overlay')?.remove();
}).catch(() => {});

const browser = await chromium.launch({ args: ['--ignore-certificate-errors'] });
const ctx = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 } });
const page = await ctx.newPage();

try {
  // ── 1. Online load: SW installs and caches the shell ───────────────────────
  await page.goto(URL, { waitUntil: 'load' });
  await page.waitForTimeout(4000);   // let any SW-update reload settle
  await dismissOverlays(page);
  await page.waitForFunction(() => navigator.serviceWorker && navigator.serviceWorker.controller,
    null, { timeout: 20000 }).catch(() => {});
  await page.reload({ waitUntil: 'load' });   // ensure SW-controlled even on first visit
  ok(await page.evaluate(() => !!navigator.serviceWorker.controller), 'service worker controls the page');
  ok(await page.evaluate(async () => {
    const keys = await caches.keys();
    const c = await caches.open(keys.find(k => k.startsWith('amux-')) || '');
    return !!(await c.match('/'));
  }), 'app shell (/) present in SW cache');

  // ── 2. Offline reload: shell served from cache ─────────────────────────────
  await ctx.setOffline(true);
  let offlineLoaded = false, fromCache = false;
  try {
    await page.reload({ waitUntil: 'load', timeout: 25000 });
    offlineLoaded = await page.evaluate(() =>
      document.querySelectorAll('.session-card, #session-list, .tab-bar, header').length > 0);
    fromCache = await page.evaluate(() => {
      const n = performance.getEntriesByType('navigation')[0];
      return n ? n.transferSize === 0 : false;
    });
  } catch (e) { console.log('  offline reload threw:', e.message); }
  ok(offlineLoaded, 'dashboard shell renders while OFFLINE');
  ok(fromCache, 'offline navigation transferred 0 bytes (SW cache)');
  await dismissOverlays(page);

  // ── 3. Offline commands queue locally ──────────────────────────────────────
  await page.evaluate(() => window.dispatchEvent(new Event('offline')));
  await page.waitForTimeout(300);
  const rawResult = await page.evaluate(async (mark) => {
    const r = await fetch('/api/board', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ title: mark, session: 'amux', status: 'todo' }),
    });
    return { status: r.status, body: await r.json() };
  }, MARK);
  ok(rawResult.status === 202 && rawResult.body.queued === true,
     'raw fetch mutation intercepted → 202 queued (outbox interceptor)');
  await page.evaluate(async ([target, mark]) => { await doSend(target, mark + ' (send)'); },
    [SEND_TARGET, MARK]);
  await page.waitForTimeout(500);
  const q1 = await page.evaluate(() => JSON.parse(localStorage.getItem('amux_offline_queue') || '[]'));
  ok(q1.length >= 2, `both commands in localStorage outbox (${q1.length} ops)`);
  const sendOp = q1.find(o => o.url.includes('/send'));
  const origMsgId = sendOp && JSON.parse(sendOp.options.body).msg_id;
  ok(!!origMsgId, 'queued send carries a msg_id (idempotent replay)');

  // ── 4. Queue survives an offline reload ────────────────────────────────────
  await page.reload({ waitUntil: 'load', timeout: 25000 }).catch(() => {});
  ok(await page.evaluate(() => JSON.parse(localStorage.getItem('amux_offline_queue') || '[]').length) >= 2,
     'outbox persisted across offline reload');
  await dismissOverlays(page);

  // ── 5. Reconnect: replayer applies ops server-side ─────────────────────────
  await ctx.setOffline(false);
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  // The automatic flush fires on the offline→online TRANSITION; in emulation
  // the app may never have observed itself offline (background polls are
  // timing-dependent), so also invoke the user-visible "sync now" path to
  // make the replay deterministic.
  await page.waitForTimeout(1000);
  await page.evaluate(() => {
    const q = JSON.parse(localStorage.getItem('amux_offline_queue') || '[]');
    if (q.length && typeof runSyncBanner === 'function') { online = true; runSyncBanner(); }
  });
  let boardHas = false;
  for (let i = 0; i < 20 && !boardHas; i++) {
    boardHas = await page.evaluate(async (mark) => {
      const items = await (await fetch('/api/board')).json();
      return items.some(it => it.title === mark);
    }, MARK);
    if (!boardHas) await page.waitForTimeout(1000);
  }
  ok(boardHas, 'queued board command APPLIED server-side after sync');
  await page.waitForTimeout(2000);
  const after = await page.evaluate(() => JSON.parse(localStorage.getItem('amux_offline_queue') || '[]'));
  const retained = after.find(o => o.url.includes('/send'));
  ok(after.length === 0 || (after.length === 1 && !!retained),
     `queue reached terminal state: ${retained ? 'send RETAINED for retry (target unavailable)' : 'fully drained'}`);
  if (retained) {
    ok(JSON.parse(retained.options.body).msg_id === origMsgId,
       'retained send keeps its original msg_id (exactly-once on eventual replay)');
  }
  await page.evaluate(() => { offlineQueue = []; saveQueue(); });   // clear intentional leftover

  // ── 6. Passive when connected: online mutation passes straight through ─────
  const onlineResult = await page.evaluate(async (mark) => {
    const r = await fetch('/api/board', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ title: mark + ' (online)', session: 'amux', status: 'todo' }),
    });
    const d = await r.json();
    return { status: r.status, id: d.id, queued: !!d.queued,
             qlen: JSON.parse(localStorage.getItem('amux_offline_queue') || '[]').length };
  }, MARK);
  ok(onlineResult.status >= 200 && onlineResult.status < 300 && onlineResult.status !== 202
     && !!onlineResult.id && !onlineResult.queued && onlineResult.qlen === 0,
     'online mutation passes through untouched (real 2xx + real id, nothing queued)');

  // ── Cleanup: delete every board item this test created ─────────────────────
  // Sweep-and-verify: list staleness can hide just-created items and a DELETE
  // can fail silently, so keep sweeping until the board shows zero matches.
  const leftover = await page.evaluate(async ([mark, knownId]) => {
    const prefix = mark.split(' test ')[0];
    if (knownId) await fetch('/api/board/' + knownId, { method: 'DELETE' }).catch(() => {});
    for (let round = 0; round < 6; round++) {
      const items = await (await fetch('/api/board')).json();
      const mine = items.filter(i => (i.title || '').startsWith(prefix));
      if (!mine.length) return 0;
      for (const it of mine) await fetch('/api/board/' + it.id, { method: 'DELETE' }).catch(() => {});
      await new Promise(r => setTimeout(r, 1000));
    }
    const items = await (await fetch('/api/board')).json();
    return items.filter(i => (i.title || '').startsWith(prefix)).length;
  }, [MARK, onlineResult.id]);
  ok(leftover === 0, 'all test board items cleaned up');
} finally {
  await browser.close();
}

console.log(fails.length ? 'RESULT: FAIL — ' + fails.join(' | ') : 'RESULT: PASS');
process.exit(fails.length ? 1 : 0);
