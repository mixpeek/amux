import { test, expect } from './fixtures';

// dev.to has a key and no test endpoint, so its Test answers "untested": it
// tried nothing. The card said "Last live test failed" and the drawer drew a
// red ✗ for it (Ethan's deploy check of 4a317af7f, 2026-10-29), while the
// server already listed the connector as Connected.
test('a connector with no test endpoint reads untested, not failed', async ({ page }) => {
  const untested = { ok: false, status: 'untested', measured: false,
    why_unmeasured: 'this connector has no test endpoint; message the bot and check GET /api/bridge' };
  await page.route(/\/api\/connectors$/, async route => {
    if (route.request().method() !== 'GET') return route.continue();
    // The page fetches this list from several places, and the search corpus
    // aborts its copy after 8 s (AbortSignal.timeout). Playwright disposes the
    // fetched response of an aborted request, so reading it threw "Response
    // has been disposed" inside this handler and failed the test on main
    // (731f90cd0, 559eede40) before any assertion ran. A request the page
    // abandoned has nobody to answer.
    let res, body;
    try {
      res = await route.fetch();
      body = await res.json();
    } catch (e) {
      if (/disposed|aborted|closed/i.test(String(e))) return;
      throw e;
    }
    const devto = (body.connectors || []).find((c: any) => c.id === 'devto');
    expect(devto, 'the built-in dev.to connector is listed').toBeTruthy();
    Object.assign(devto, { status: 'connected', detail: null, usable: true,
      env_keys: [{ name: 'DEVTO_API_KEY', set: true, masked: '****fixture' }],
      last_test: { ...untested, at: Date.now() / 1000 - 30 } });
    await route.fulfill({ response: res, json: body }).catch(e => {
      if (!/disposed|aborted|closed|already handled/i.test(String(e))) throw e;
    });
  });
  await page.route(/\/api\/connectors\/devto\/test$/, route => route.fulfill({ json: untested }));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/#view=connectors');

  const card = page.locator('.cx-card').filter({ hasText: 'dev.to' });
  await expect(card).toContainText('Connected');
  await expect(card).not.toContainText('failed');

  await card.click();
  const result = page.locator('.cx-drawer .cx-test');
  await expect(result).toContainText('No live test for this connector');
  await expect(result).not.toHaveClass(/bad/);

  // Pressing Test again gets the same answer, and the result line agrees.
  await page.locator('.cx-drawer').getByRole('button', { name: 'Test connection', exact: true }).click();
  await expect(page.locator('.cx-drawer .cx-test')).toContainText('No live test for this connector');
  await expect(page.locator('.cx-drawer .cx-test')).not.toHaveClass(/bad/);
  await expect(page.locator('.cx-card').filter({ hasText: 'dev.to' })).not.toContainText('failed');
});
