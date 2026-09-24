#!/usr/bin/env node
// END-TO-END: "All shown (N)" on the group pill row acts on exactly the workers
// the list is showing. Three running workers, two in group x, one in group y.
// Filter to x, stop all shown: the two x workers stop and y keeps running.
// Then delete all shown: a wrong typed count cancels, the right one deletes
// exactly the two x workers.
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/bulk-visible.mjs
import path from 'node:path';
import { chromium } from 'playwright';
import { startAmux, waitFor } from './harness.mjs';

const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY });
let browser;
const list = async () => { const b = (await amux.req('GET', '/api/sessions')).body; return Array.isArray(b) ? b : []; };
const running = async n => (await list()).find(s => s.name === n)?.running === true;
try {
  for (const [n, tag] of [['bx-one', 'x'], ['bx-two', 'x'], ['by-three', 'y']])
    await amux.req('POST', '/api/sessions', { name: n, dir: amux.root, tags: [tag] });
  await waitFor('three running', async () => (await Promise.all(['bx-one', 'bx-two', 'by-three'].map(running))).every(Boolean), 45000, 500);

  browser = await chromium.launch();
  const page = await (await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 } })).newPage();
  const pageErrors = []; page.on('pageerror', e => pageErrors.push(String(e)));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto(amux.base + '/', { waitUntil: 'domcontentloaded' });
  await page.locator('.card[data-session="by-three"]').waitFor({ state: 'visible', timeout: 20000 });
  const btn = page.locator('#bulk-visible-btn');
  await waitFor('button counts all 3', () => btn.textContent().then(t => t === 'All shown (3)'), 10000, 200);
  check('button is on the pill row and counts every shown worker', await btn.textContent() === 'All shown (3)');
  const box = await btn.boundingBox();
  check('button is on screen and tappable at 390px', box && box.x + box.width <= 390 && box.height >= 32, box);

  await page.locator('.tag-filter', { hasText: /^x$/ }).click();
  await waitFor('filtered count', () => btn.textContent().then(t => t === 'All shown (2)'), 10000, 200);
  check('filtering to group x makes it "All shown (2)"', await btn.textContent() === 'All shown (2)');
  await page.screenshot({ path: path.join(amux.root, '1-filtered.png') });

  await btn.click();
  await page.locator('#bulk-actions-overlay.open').waitFor({ timeout: 5000 });
  const names = await page.locator('.bulk-visible-names span').allTextContents();
  check('the sheet names exactly the shown workers', JSON.stringify(names.sort()) === JSON.stringify(['bx-one', 'bx-two']), names);
  await page.locator('.bulk-visible-actions button', { hasText: 'Stop' }).click();
  await page.locator('#modal-btns button', { hasText: 'Stop' }).click();
  await waitFor('x workers stopped', async () => !(await running('bx-one')) && !(await running('bx-two')), 45000, 500).catch(() => null);
  check('stop: both shown workers stopped', !(await running('bx-one')) && !(await running('bx-two')));
  check('stop: the hidden worker kept running', await running('by-three'));

  // Delete: a wrong typed count must cancel.
  await btn.click();
  await page.locator('.bulk-visible-actions button', { hasText: 'Delete' }).click();
  await page.locator('#modal-btns button', { hasText: 'Delete' }).click();
  await page.locator('#modal-prompt-input').fill('3');
  await page.locator('#modal-btns button', { hasText: 'OK' }).click();
  await page.waitForTimeout(1500);
  check('delete: a wrong typed count deletes nothing', (await list()).length === 3, (await list()).map(s => s.name));
  await btn.click();
  await page.locator('.bulk-visible-actions button', { hasText: 'Delete' }).click();
  await page.locator('#modal-btns button', { hasText: 'Delete' }).click();
  await page.locator('#modal-prompt-input').fill('2');
  await page.locator('#modal-btns button', { hasText: 'OK' }).click();
  await waitFor('deleted', async () => (await list()).length === 1, 30000, 500).catch(() => null);
  const left = (await list()).map(s => s.name);
  check('delete: exactly the two shown workers are gone', JSON.stringify(left) === JSON.stringify(['by-three']), left);
  await page.screenshot({ path: path.join(amux.root, '2-after.png') });
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
