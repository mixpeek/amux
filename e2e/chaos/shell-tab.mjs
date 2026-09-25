#!/usr/bin/env node
// END-TO-END: a worker's Shell tab shows its shell schedule's output LIVE while
// it runs on the host, then the full output and the result.
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/shell-tab.mjs
import path from 'node:path';
import { chromium } from 'playwright';
import { startAmux, waitFor } from './harness.mjs';

const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
// Run Now needs the Rust scheduler out of shadow mode (the live server runs with it).
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY, env: { AMUX_RS_SCHEDULER: '1' } });
let browser;
try {
  await amux.req('POST', '/api/sessions', { name: 'ticker', dir: amux.root, start: false });
  const cmd = 'for i in 1 2 3 4 5 6; do echo "tick line $i"; sleep 1; done; echo "warning on stderr" >&2';
  const sc = await amux.req('POST', '/api/schedules', { title: 'Slow tick', session: 'ticker', kind: 'shell', command: cmd, schedule_expr: 'daily at 3am' });
  const id = sc.body.id || sc.body.schedule?.id;
  check('shell schedule created', !!id, sc.body);
  browser = await chromium.launch();
  const page = await (await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 } })).newPage();
  const errors = []; page.on('pageerror', e => errors.push(String(e)));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto(amux.base + '/', { waitUntil: 'domcontentloaded' });
  await page.waitForFunction(() => typeof openPeek === 'function');
  await page.evaluate(() => openPeek('ticker'));
  await page.evaluate(() => setPeekTab('shell'));
  const card = page.locator(`.peek-shell-card[data-id="${id}"]`);
  await card.waitFor({ timeout: 10000 });
  check('the Shell tab lists the worker\'s shell schedule', await card.isVisible());
  check('before any run it says nothing is recorded yet', /No output recorded yet/.test(await card.locator('.peek-shell-out').innerText()));
  await card.getByRole('button', { name: 'Run now' }).click();
  // Live: see partial output while the job is still running.
  const partial = await waitFor('partial live output', async () => {
    const t = await page.locator(`#shell-out-${id}`).innerText().catch(() => '');
    return /tick line [1-4]/.test(t) && !/tick line 6/.test(t) ? t : null;
  }, 15000, 250).catch(() => null);
  check('output streams in while the job runs (partial lines, not yet finished)', !!partial, partial);
  check('the status reads running meanwhile', /running/.test(await card.locator('.peek-shell-status').innerText().catch(() => '')));
  await page.screenshot({ path: path.join(amux.root, 'shell-1-live.png') });
  const done = await waitFor('run finished', async () => {
    const t = await page.locator(`.peek-shell-card[data-id="${id}"] .peek-shell-out`).innerText().catch(() => '');
    return /\[exit 0\]/.test(t) ? t : null;
  }, 30000, 500).catch(() => null);
  check('final output has every line, stderr, and the exit code', done && /tick line 6/.test(done) && /warning on stderr/.test(done) && /\[exit 0\]/.test(done), done);
  await page.waitForTimeout(1500);
  const st = await page.locator(`.peek-shell-card[data-id="${id}"] .peek-shell-status`).innerText().catch(() => '');
  check('status becomes ok when it finishes', /ok/.test(st), st);
  const opts = await page.locator(`.peek-shell-card[data-id="${id}"] select option`).count();
  check('the run is listed in the history picker', opts >= 1, opts);
  await page.screenshot({ path: path.join(amux.root, 'shell-2-done.png') });
  check('no uncaught page errors', errors.length === 0, errors.slice(0, 3));
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally { if (browser) await browser.close().catch(() => {}); await amux.stop(); }
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
