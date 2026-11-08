// The owner renames an app's address (owner, 2026-11-08: "I should be able to
// rename the id there to something I want"), for a public app and an app inside
// amux. The new name opens it, the old one still does by a redirect, a worker
// cannot rename, and a name another link has is refused.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';
import * as http from 'http';
import type { AddressInfo } from 'net';

test('the owner renames a public app and an app inside amux; old addresses still lead there', async ({ page, playwright }, testInfo) => {
  const app = http.createServer((req, res) => {
    res.writeHead(200, { 'Content-Type': 'text/html' });
    res.end(`<h1>renamed app</h1><p id="at">${(req.url || '').replace(/</g, '')}</p>`);
  });
  await new Promise<void>(r => app.listen(0, '127.0.0.1', () => r()));
  const port = (app.address() as AddressInfo).port;
  const stamp = `${testInfo.project.name}-${Date.now()}`;
  const anon = await playwright.request.newContext({ ignoreHTTPSErrors: true });
  const modalBtn = (name: string) => page.locator('#modal-btns').getByRole('button', { name });
  const rename = async (card: ReturnType<typeof page.locator>, name: string) => {
    await card.getByRole('button', { name: /More actions/ }).click();
    await page.locator('#pub-menu [data-act="rename"]').click();
    await page.locator('#modal-prompt-input').fill(name);
    await modalBtn('OK').click();
  };
  try {
    await page.goto('/');
    await page.waitForFunction(() => !!(window as any)._AMUX_AUTH_TOKEN, { timeout: 20000 });
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
    await page.goto('/?_token=' + encodeURIComponent(token));
    await page.waitForURL(/_fresh=1/, { timeout: 20000 });
    const base = new URL(page.url()).origin;
    await page.goto('/#view=apps');

    // 1. A public app: /pub/<new name>/ serves it, the old id redirects there.
    await page.getByRole('button', { name: 'Share an app' }).click();
    await page.locator('#pub-new-target').fill(String(port));
    await page.locator('#pub-new-title').fill(`pubren ${stamp}`);
    await modalBtn('Create link').click();
    const pub = page.locator('#pub-apps .pub-card', { hasText: `pubren ${stamp}` });
    const oldHref = (await pub.locator('.pub-url').getAttribute('href'))!;
    const oldId = /\/pub\/([0-9a-f]{32})\/$/.exec(oldHref)![1];
    // A worker (the bearer, no person) cannot rename it.
    const byWorker = await anon.patch(`${base}/api/page-links/${oldId}`, { headers: { Authorization: 'Bearer ' + token, 'Content-Type': 'application/json' }, data: { id: 'worker-pick' } });
    expect(byWorker.status()).toBe(403);
    const pubName = `demo-${stamp}`;
    await rename(pub, pubName);
    await expect(pub.locator('.pub-url')).toHaveAttribute('href', new RegExp(`/pub/${pubName}/$`));
    const fresh = await anon.get(`${base}/pub/${pubName}/deep?x=1`);
    expect(fresh.status()).toBe(200);
    expect(await fresh.text()).toContain('<p id="at">/deep?x=1</p>');
    const old = await anon.get(`${base}/pub/${oldId}/deep?x=1`, { maxRedirects: 0 });
    expect(old.status(), 'temporary, so a rename back cannot loop through a cached redirect').toBe(307);
    expect(old.headers()['location']).toBe(`/pub/${pubName}/deep?x=1`);

    // 2. An app inside amux: renamed, it opens from Apps, and its /app/<name>/
    // address on the apps origin answers while the old id redirects there.
    await page.getByRole('button', { name: 'Add app' }).click();
    await page.locator('#pub-new-target').fill(String(port));
    await page.locator('#pub-new-title').fill(`inren ${stamp}`);
    await modalBtn('Add').click();
    const inside = page.locator('#pub-internal .pub-card', { hasText: `inren ${stamp}` });
    const oldApp = /\/app\/([0-9a-f]{32})\//.exec((await inside.locator('.pub-card-meta').textContent())!)![1];
    const inName = `inside-${stamp}`;
    await rename(inside, inName);
    await expect(inside.locator('.pub-card-meta')).toContainText(`/app/${inName}/`);
    // Another link's name is taken.
    // Sent the way the dashboard sends it: the page's token and the owner's cookie.
    const clash = await page.evaluate(([from, to]) => (window as any)._pubFetch('/' + from, { method: 'PATCH', body: JSON.stringify({ id: to }) })
      .then(() => 'renamed', (e: Error) => e.message), [inName, pubName]);
    expect(clash).toBe('another link already has that name');
    await inside.click();
    await expect(page.frameLocator('.pub-app-frame').locator('h1')).toHaveText('renamed app');
    const appsOrigin = `${base.replace(/:\d+$/, '')}:${Number(new URL(base).port) + 1}`;
    const oldAppUrl = await anon.get(`${appsOrigin}/app/${oldApp}/x`, { maxRedirects: 0 });
    expect(oldAppUrl.status()).toBe(307);
    expect(oldAppUrl.headers()['location']).toBe(`/app/${inName}/x`);
    // A deep link to Apps by the new name opens it.
    await page.getByRole('button', { name: 'Back to all apps' }).click();
    await page.goto(`/#view=apps&open=${inName}`);
    await expect(page.frameLocator('.pub-app-frame').locator('h1')).toHaveText('renamed app');
  } finally {
    app.close();
    await cleanup('dispose the anonymous request context', () => anon.dispose(), testInfo);
  }
});
