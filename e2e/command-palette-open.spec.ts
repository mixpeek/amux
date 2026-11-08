// Cmd-K says which workers are already open (owner, 2026-11-07: "for each
// worker in the list if its already open it says it"). The worker on this
// page reads "Open here"; one open in another tab or pane of the Mac app
// reads "Open", from the app's answer to the page's `places` question (the
// app's message handler is stood in for here, as the browser has none).
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';

async function boot(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openCmdk === 'function' && typeof (window as any).fetchSessions === 'function');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  return { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
}

test('Cmd-K marks the worker open here and the ones open in other tabs', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  const auth = await boot(page);
  const stamp = `${info.project.name}-${Date.now()}`;
  const [here, other, closed] = ['here', 'other', 'closed'].map(k => `cmdkopen-${k}-${stamp}`);
  for (const name of [here, other, closed]) {
    expect((await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp', start: false } })).status()).toBe(201);
  }
  try {
    await expect.poll(() => page.evaluate(async (names) => {
      await (window as any).fetchSessions();
      return names.every((n: string) => (0, eval)('sessions').some((s: any) => s.name === n));
    }, [here, other, closed]), { timeout: 30_000 }).toBe(true);

    // The Mac app answers which places its other tabs and panes show.
    await page.evaluate((o) => {
      (window as any).webkit = { messageHandlers: { amuxPane: { postMessage: (m: any) => {
        (window as any)._askedPlaces = m;
        if (m.op === 'places') setTimeout(() => (window as any)._cmdkPlacesAck(m.token, ['peek=' + o, 'view=board']), 50);
      } } } };
    }, other);
    await page.evaluate((n) => (window as any).openPeek(n), here);
    await expect(page.locator('#peek-overlay')).toHaveClass(/active/);

    await page.evaluate(() => (window as any).openCmdk());
    await page.locator('#cmdk-input').fill(`cmdkopen-`);
    const row = (n: string) => page.locator('#cmdk-list .cmdk-row', { hasText: n });
    await expect(row(here).locator('.cmdk-open')).toHaveText('Open here');
    await expect(row(other).locator('.cmdk-open')).toHaveText('Open');
    await expect(row(closed)).toBeVisible();
    await expect(row(closed).locator('.cmdk-open')).toHaveCount(0);
    expect(await page.evaluate(() => (window as any)._askedPlaces?.op)).toBe('places');

    // A tab open elsewhere is marked too; an answer to an older question is not used.
    await page.locator('#cmdk-input').fill('board');
    await expect(page.locator('#cmdk-list .cmdk-row', { hasText: 'Board' }).first().locator('.cmdk-open')).toHaveText('Open');
    await page.evaluate((c) => (window as any)._cmdkPlacesAck('stale', ['peek=' + c]), closed);
    await page.locator('#cmdk-input').fill('cmdkopen-');
    await expect(row(closed).locator('.cmdk-open')).toHaveCount(0);

    // The badge stays inside the row on a phone.
    const box = await row(other).boundingBox();
    const badge = await row(other).locator('.cmdk-open').boundingBox();
    expect(badge!.x + badge!.width).toBeLessThanOrEqual(box!.x + box!.width + 1);
  } finally {
    for (const name of [here, other, closed]) {
      await cleanup('delete fixture worker', () => request.post(`/api/sessions/${encodeURIComponent(name)}/delete`, { headers: auth }));
    }
  }
});
