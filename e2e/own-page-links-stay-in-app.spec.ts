import { test, expect } from './fixtures';
import { cleanup } from './teardown';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

// A page on this amux that is not the dashboard never replaces it in an app
// window (the iOS app, an installed web app): there is no browser Back there.
// Doron, 2026-11-12: a worker's /pub/ link tapped in the terminal left the
// whole app on "Link not found" with no way back. In an app a /pub/ link now
// opens over the dashboard with a close bar, and a link still waiting for
// approval opens Apps with the reason. A browser tab keeps its new tab.
test('a /pub/ link opened in the app keeps the dashboard reachable', async ({ page, browser, playwright }, testInfo) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-own-page-'));
  fs.writeFileSync(path.join(dir, 'index.html'), '<h1>shared page</h1>');
  const tag = testInfo.project.name + '-' + Date.now();
  const made: string[] = [];
  // The app window: the iOS app's web view and a home-screen web app both
  // read as standalone to the page (app.js _inAppWindow).
  await page.addInitScript(() => Object.defineProperty(Navigator.prototype, 'standalone', { get: () => true }));
  try {
    await page.goto('/');
    await page.waitForFunction(() => !!(window as any)._AMUX_AUTH_TOKEN, { timeout: 20000 });
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
    await page.goto('/?_token=' + encodeURIComponent(token));
    await page.waitForURL(/_fresh=1/, { timeout: 20000 });
    await page.goto('/#view=sessions');
    await page.waitForFunction(() => typeof (window as any)._termLinkOpen === 'function');
    const base = new URL(page.url()).origin;

    // A live link, made by the owner, and one a worker asked for (bearer
    // only, no person), which waits for approval.
    const live = await page.evaluate(async ([d, t]) => (window as any)._pubFetch('', { method: 'POST',
      body: JSON.stringify({ kind: 'file', path: d, title: 'live ' + t }) }), [dir, tag]);
    made.push(live.id);
    expect(live.pending, 'an owner-made link is live').toBeFalsy();
    // No cookie: the owner's session would make this request a person's.
    const anon = await playwright.request.newContext({ ignoreHTTPSErrors: true });
    const asked = await anon.post(`${base}/api/page-links`, { headers: { Authorization: 'Bearer ' + token, 'Content-Type': 'application/json' },
      data: { kind: 'file', path: dir, title: 'asked ' + tag } });
    expect(asked.status()).toBe(201);
    const pending = await asked.json();
    await anon.dispose();
    made.push(pending.id);
    expect(pending.pending).toBe(true);
    const liveUrl = new URL(live.url || live.path, base).toString();
    const pendingUrl = new URL(pending.url || pending.path, base).toString();

    // 1. The terminal's link handler (what xterm's web-links addon calls on a
    // tap) with the pending link: Apps opens with the reason, not a dead page.
    await page.evaluate(u => (window as any)._termLinkOpen(new MouseEvent('click'), u), pendingUrl);
    // Apps is the 'pages' view inside app.js.
    await expect.poll(() => page.evaluate('activeView')).toBe('pages');
    await expect(page.locator('.toast').filter({ hasText: 'Approve it first' }).first()).toBeVisible();
    expect(new URL(page.url()).pathname, 'the dashboard was replaced').toBe('/');

    // 2. The same handler with the live link: it opens over the dashboard.
    await page.evaluate(u => (window as any)._termLinkOpen(new MouseEvent('click'), u), liveUrl);
    const overlay = page.locator('#own-page-overlay');
    await expect(overlay).toBeVisible();
    await expect(overlay.locator('iframe')).toHaveAttribute('src', liveUrl);
    await expect(page.frameLocator('#own-page-overlay iframe').locator('h1')).toHaveText('shared page');
    await overlay.getByRole('button', { name: 'Back to amux' }).click();
    await expect(overlay).toHaveCount(0);
    expect(new URL(page.url()).pathname).toBe('/');

    // 3. A link in a message or markdown (an anchor, target=_blank): same.
    await page.evaluate(u => {
      const a = document.createElement('a');
      a.href = u; a.target = '_blank'; a.id = 'msg-pub-link'; a.textContent = 'shared';
      a.style.cssText = 'position:fixed;top:120px;left:20px;z-index:99999;padding:8px;background:#fff';
      document.body.appendChild(a);
    }, liveUrl);
    const popups: string[] = [];
    page.context().on('page', p => popups.push(p.url()));
    await page.locator('#msg-pub-link').click();
    await expect(overlay).toBeVisible();
    await overlay.getByRole('button', { name: 'Back to amux' }).click();
    await expect(overlay).toHaveCount(0);
    expect(popups, 'the app opened a window of its own').toEqual([]);
    expect(new URL(page.url()).pathname).toBe('/');

    // 4. A browser tab is unchanged: the same anchor opens a new tab.
    const tab = await browser.newPage({ ignoreHTTPSErrors: true });
    try {
      await tab.goto(`${base}/?_token=${encodeURIComponent(token)}`);
      await tab.waitForURL(/_fresh=1/, { timeout: 20000 });
      await tab.waitForFunction(() => typeof (window as any)._ownPageLink === 'function');
      expect(await tab.evaluate(u => (window as any)._ownPageLink(u), liveUrl), 'a browser tab took the app path').toBeNull();
    } finally { await cleanup('close the browser tab', () => tab.close()); }
  } finally {
    await cleanup('remove the test links', () => Promise.all(made.map(id =>
      page.evaluate(i => (window as any)._pubFetch('/' + encodeURIComponent(i), { method: 'DELETE' }).catch(() => {}), id).catch(() => {}))));
    await cleanup('remove the test folder', () => fs.rmSync(dir, { recursive: true, force: true }));
  }
});
