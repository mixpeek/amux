#!/usr/bin/env node
// END-TO-END OFFLINE: with the amux server DOWN, the dashboard still lists the
// saved workers, a worker still opens, and messages typed into it are kept;
// when the server returns they reach the agent ONE BY ONE, EXACTLY ONCE, IN
// ORDER. Ground truth is the fake agent's own log, not amux's receipts.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> [N=12] node e2e/chaos/offline-fleet.mjs
import path from 'node:path';
import { chromium } from 'playwright';
import { startAmux, waitFor } from './harness.mjs';

const N = Number(process.env.N || 12);
const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY });
let browser;
const shot = (page, n) => page.screenshot({ path: path.join(amux.root, n + '.png') }).catch(() => {});
try {
  const name = 'offline-lane';
  await amux.req('POST', '/api/sessions', { name, dir: amux.root });
  await waitFor('agent launch', () => amux.fakeLog().find(e => e.event === 'launch'), 30000);
  await waitFor('composer painted', () => {
    const pane = amux.tmux('capture-pane', '-p', '-t', `amux-${name}`);
    return pane.includes('❯') && pane.includes('bypass permissions');
  }, 20000);

  browser = await chromium.launch();
  const page = await (await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 } })).newPage();
  const pageErrors = [];
  page.on('pageerror', e => pageErrors.push(String(e)));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto(amux.base + '/', { waitUntil: 'domcontentloaded' });
  const card = page.locator(`.card[data-session="${name}"]`).first();
  await card.waitFor({ state: 'visible', timeout: 20000 });

  // Open the worker once while online, the way it gets saved on the device,
  // and put recognisable text on its terminal.
  await amux.req('POST', `/api/workers/${name}/send`, { text: 'seen online before the outage' }, 60000);
  await page.evaluate(n => openPeek(n), name);
  await waitFor('peek shows the online text', () => page.locator('#peek-body').innerText().then(t => t.includes('seen online')).catch(() => false), 20000, 300)
    .catch(() => null);
  await page.waitForTimeout(1500);   // let the frame be written to the device cache
  await page.evaluate(() => closePeek && closePeek());

  // ---- the server goes away ----
  await amux.down();
  await page.evaluate(() => typeof fetchSessions === 'function' && fetchSessions()).catch(() => {});
  await waitFor('client notices the outage', () => page.evaluate(() => !!_sessionLoadError || !online), 30000, 500);
  await page.evaluate(() => { try { render(); } catch (e) {} });
  await shot(page, '1-offline-list');
  check('offline: the saved worker is still listed', await card.isVisible());
  check('offline: no "cached workers are hidden" gate', await page.locator('.offline-cache-gate').count() === 0);
  check('offline: the list is marked as last-known', await page.locator('#cards.workers-stale, .cards.workers-stale').count() > 0);

  // ---- open the worker offline and type N messages ----
  await page.evaluate(n => openPeek(n), name);
  const input = page.locator('#peek-cmd-input');
  await input.waitFor({ state: 'visible', timeout: 10000 });
  check('offline: the worker opens', await input.isVisible());
  const offlineText = await waitFor('cached terminal', () => page.locator('#peek-body').innerText().then(t => t.includes('seen online') ? t : null), 10000, 300).catch(() => '');
  check('offline: the worker shows its saved terminal content', !!offlineText, { body: (await page.locator('#peek-body').innerText().catch(() => '')).slice(0, 300) });
  const sent = Array.from({ length: N }, (_, i) => `offline msg ${String(i + 1).padStart(3, '0')} ✓`);
  for (const text of sent) {
    await input.fill(text);
    // Enter adds a newline in this composer by design; Send is the button.
    await page.locator('#peek-overlay .send-split-main').click();
    await waitFor('composer cleared after send', () => input.inputValue().then(v => v === ''), 10000, 50);
  }
  await shot(page, '2-offline-typed');
  const queued = await page.evaluate(() => (JSON.parse(localStorage.getItem('amux_offline_queue') || '[]')).length);
  check('offline: every message is held in the local outbox', queued >= N, { queued, N });
  check('offline: nothing reached the agent yet', !amux.fakeLog().some(e => (e.text || '').includes('offline msg')));

  // ---- the server returns ----
  await amux.up();
  const got = await waitFor('all messages at the agent', () => {
    const texts = amux.fakeLog().filter(e => (e.text || '').includes('offline msg')).map(e => e.text);
    return texts.length >= N ? texts : null;
  }, 120000, 500).catch(() => amux.fakeLog().filter(e => (e.text || '').includes('offline msg')).map(e => e.text));
  await page.waitForTimeout(3000);  // let any duplicate arrive before we count
  const final = amux.fakeLog().filter(e => (e.text || '').includes('offline msg')).map(e => e.text);
  const order = final.map(t => sent.findIndex(s => t.includes(s)));
  check('online: every message arrived', sent.every(s => final.some(t => t.includes(s))), { missing: sent.filter(s => !final.some(t => t.includes(s))) });
  check('online: none arrived twice', final.length === new Set(order).size && !order.includes(-1), { final });
  check('online: they arrived in the order typed', order.every((v, i) => i === 0 || v > order[i - 1]), { order });
  const drained = await waitFor('outbox drained', () => page.evaluate(() => JSON.parse(localStorage.getItem('amux_offline_queue') || '[]').length === 0), 30000, 500).catch(() => false);
  check('online: the local outbox drained', drained);
  await shot(page, '3-online');
  check('no uncaught page errors', pageErrors.length === 0, pageErrors.slice(0, 5));
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally {
  if (browser) await browser.close().catch(() => {});
  await amux.stop();
}
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
