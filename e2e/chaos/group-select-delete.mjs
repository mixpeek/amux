#!/usr/bin/env node
// END-TO-END: bulk delete per worker group, and checkboxes across groups.
// Two paused workers, two archived, one active. Check one paused row and the
// archived group's select-all: the bar reads "3 selected" and "Delete 3", and
// deleting removes exactly those three. Then the paused group's own menu
// offers "Delete 1" and removes the last paused worker. The active one stays.
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/group-select-delete.mjs
import path from 'node:path';
import { chromium } from 'playwright';
import { startAmux, waitFor } from './harness.mjs';

const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY });
let browser;
const list = async () => { const b = (await amux.req('GET', '/api/sessions')).body; return Array.isArray(b) ? b : []; };
const names = async () => (await list()).map(s => s.name).sort();
try {
  for (const n of ['gp-one', 'gp-two', 'ga-one', 'ga-two', 'gk-live'])
    await amux.req('POST', '/api/sessions', { name: n, dir: amux.root });
  for (const n of ['gp-one', 'gp-two']) {
    const r = await amux.req('POST', '/api/workers/' + n + '/pause');
    check('pause ' + n, r.status < 300, r.body);
  }

  browser = await chromium.launch({ channel: 'chrome' });
  const page = await (await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 }, serviceWorkers: 'block' })).newPage();
  const pageErrors = []; page.on('pageerror', e => pageErrors.push(String(e)));
  page.on('dialog', d => { pageErrors.push('native dialog: ' + d.message()); d.dismiss(); });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto(amux.base + '/', { waitUntil: 'domcontentloaded' });
  // Archiving is dashboard-only (UI token), so archive from the page.
  await page.locator('.card[data-session="gk-live"]').waitFor({ timeout: 20000 });
  for (const n of ['ga-one', 'ga-two']) {
    const token = await page.evaluate(() => window._AMUX_UI_TOKEN || '');
    const r = await amux.req('POST', '/api/sessions/' + n + '/archive', undefined, 30000, { 'X-Amux-UI-Token': token });
    check('archive ' + n, r.status < 300, r.body);
  }
  await waitFor('states', async () => {
    const l = await list();
    return l.filter(s => s.lifecycle === 'paused' && !s.archived).length === 2 && l.filter(s => s.archived).length === 2;
  }, 30000, 500).catch(() => null);
  check('two paused and two archived workers exist', (await list()).filter(s => s.archived).length === 2, (await list()).map(s => [s.name, s.lifecycle, s.archived, s.running]));
  await page.evaluate(() => fetchSessions());
  await page.locator('.paused-footer').waitFor({ timeout: 20000 });
  await page.locator('.archived-footer').waitFor({ timeout: 20000 });
  await page.locator('.paused-footer .worker-group-toggle').click();
  await page.locator('.archived-footer .worker-group-toggle').click();
  await page.locator('.paused-card[data-session="gp-one"] .wsel input').waitFor({ timeout: 10000 });

  await page.locator('.paused-footer .worker-group-menu summary').click();
  const pausedMenu = await page.locator('.paused-footer .worker-group-dropdown').innerText();
  check('the paused group menu offers "Delete 2"', /Delete 2/.test(pausedMenu), pausedMenu);
  await page.locator('.paused-footer .worker-group-menu summary').click();

  await page.locator('.paused-card[data-session="gp-one"] .wsel input').check();
  await page.locator('.archived-footer .wsel input').check();
  const bar = page.locator('#worker-selbar');
  await bar.waitFor({ timeout: 5000 });
  const barText = await bar.innerText();
  check('bar counts the selection across both groups', /3 selected/.test(barText), barText);
  check('bar offers Delete 3', /Delete 3/.test(barText), barText);
  check('select-all checked both archived rows', await page.locator('.archived-card .wsel input:checked').count() === 2);
  const bb = await bar.boundingBox();
  check('bar fits a 390px screen', bb && bb.x >= 0 && bb.x + bb.width <= 390, bb);
  check('bar lays its buttons out in a row, not a column', bb && bb.height <= 120, bb);
  const tap = await page.locator('.paused-card[data-session="gp-two"] .wsel').boundingBox();
  check('row checkbox is a 44px touch target on a phone', tap && tap.width >= 44 && tap.height >= 44, tap);
  await page.screenshot({ path: path.join(amux.root, '1-selected.png') });

  await bar.locator('button', { hasText: 'Delete 3' }).click();
  await page.locator('#modal-btns button', { hasText: 'Delete' }).click();
  await waitFor('three deleted', async () => (await list()).length === 2, 30000, 500).catch(() => null);
  check('exactly the three checked workers are deleted', JSON.stringify(await names()) === JSON.stringify(['gk-live', 'gp-two']), await names());
  await waitFor('bar gone', async () => await page.locator('#worker-selbar').count() === 0, 10000, 300).catch(() => null);
  check('the bar goes away once its workers are gone', await page.locator('#worker-selbar').count() === 0);

  await page.locator('.paused-footer .worker-group-menu summary').click();
  await page.locator('.paused-footer .worker-group-dropdown button', { hasText: 'Delete 1' }).click();
  await page.locator('#modal-btns button', { hasText: 'Delete' }).click();
  await waitFor('paused deleted', async () => (await list()).length === 1, 30000, 500).catch(() => null);
  check('the paused group menu deletes its remaining worker', JSON.stringify(await names()) === JSON.stringify(['gk-live']), await names());
  await page.screenshot({ path: path.join(amux.root, '2-after.png') });
  check('no uncaught page errors or native dialogs', pageErrors.length === 0, pageErrors.slice(0, 5));
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
