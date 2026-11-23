// Cmd-K's history against the real server (owner, 2026-11-23: rank by where
// you actually go, and keep it on the server so the phone and the Mac share
// it). One browser opens a worker from the list and picks a result for a
// query; a second browser, with nothing in its own storage, ranks the same
// way. command-palette-ranking.spec.ts fakes this endpoint to test ranking
// rules; this spec is the endpoint and the sharing.
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';

const R = Math.random().toString(36).slice(2, 7);
const A = 'zz-hist-alpha-' + R, B = 'zz-hist-beta-' + R;
const W = [A, B].map(name => ({ name, dir: '/tmp/' + name, running: true, status: 'idle', flags: '', desc: 'palette history test', tags: [] }));

async function boot(page: Page) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: W }));
  await page.route(/\/api\/sessions\/zz-hist-[^/]+\/peek\?.*/, r => r.fulfill({ json: { output: 'idle\n', history: '' } }));
  await page.goto('/');
  await page.waitForFunction(n => typeof (window as any).openCmdk === 'function'
    && (0, eval)('sessions').some((s: any) => s.name === n), A);
}

const firstRow = (page: Page) => page.locator('#cmdk-list .cmdk-row').first();

test('a pick and a visit made in one browser rank the same in another', async ({ page, browser, baseURL }, info) => {
  test.skip(info.project.name !== 'desktop', 'one server, one history: the desktop run proves it');
  test.setTimeout(90_000);
  await boot(page);
  // Before anything is learned, the first worker by name leads for this query.
  await page.evaluate(() => (window as any).openCmdk());
  await page.locator('#cmdk-input').fill('zz-hist-');
  await expect(firstRow(page)).toContainText(A);
  // Choose the second one.
  await page.locator('#cmdk-list .cmdk-row', { hasText: B }).click();
  await expect(page.locator('#peek-overlay')).toHaveAttribute('data-session', B);
  await expect.poll(async () => {
    const h = await page.evaluate(() => fetch('/api/palette/history', { headers: (window as any)._authHeaders() }).then(r => r.json()));
    return { visit: h.visits.some((v: any) => v.kind === 'worker' && v.id === B), pick: h.picks.some((p: any) => p.query === 'zz-hist-' && p.id === B) };
  }).toEqual({ visit: true, pick: true });

  // A second browser: its own storage, nothing local. It reads the server's.
  const ctx = await browser.newContext({ baseURL, ignoreHTTPSErrors: true, viewport: { width: 375, height: 700 },
    storageState: { cookies: [], origins: [{ origin: baseURL!, localStorage: [{ name: 'amux_walkthrough_done', value: '1' }, { name: 'amux_cloud_offer', value: '{"dismissed":1}' }] }] } });
  try {
    const phone = await ctx.newPage();
    await boot(phone);
    expect(await phone.evaluate(() => localStorage.getItem('amux-cmdk-usage'))).toBeNull();
    await phone.evaluate(() => (window as any).openCmdk());
    await phone.locator('#cmdk-input').fill('zz-hist-');
    await expect(firstRow(phone), 'the pick made on the other browser').toContainText(B);
    // And with nothing typed, Recent has the worker the other browser opened.
    await phone.locator('#cmdk-input').fill('');
    await expect(phone.locator('#cmdk-list .cmdk-row', { hasText: B }).first()).toBeVisible();
    await expect(phone.locator('#cmdk-list .cmdk-section').first()).toHaveText('Recent');
  } finally {
    await cleanup('close the second browser context', () => ctx.close());
  }
});
