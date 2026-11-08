// A folder of HTML behaves like a small website inside amux (owner, 2026-10-29,
// the GTM demos): opened as an app, a link the app opens in a new tab stays in
// the viewer, with Back; opened from the Files tab, the page's sibling CSS
// loads and its links work, with no app added. Real server, real loopback app.
import { test, expect } from './fixtures';
import * as fs from 'fs';
import * as http from 'http';
import * as os from 'os';
import * as path from 'path';
import type { AddressInfo } from 'net';

function makeSite(): string {
  const dir = fs.mkdtempSync(path.join(os.homedir(), 'viewer-site-'));
  fs.mkdirSync(path.join(dir, 'landing'));
  fs.mkdirSync(path.join(dir, 'design'));
  fs.writeFileSync(path.join(dir, 'index.html'), '<!doctype html><html><head><title>hub</title><link rel="stylesheet" href="design/site.css"></head>'
    + '<body><h1>The hub</h1><a id="tab" href="landing/core.html" target="_blank">Landing</a></body></html>');
  fs.writeFileSync(path.join(dir, 'landing', 'core.html'), '<!doctype html><html><head><title>core</title><link rel="stylesheet" href="../design/site.css"></head>'
    + '<body><h1>Core page</h1><a id="home" href="../index.html">Home</a></body></html>');
  fs.writeFileSync(path.join(dir, 'design', 'site.css'), 'h1{color:rgb(1, 2, 3)}');
  return dir;
}

async function signIn(page: import('@playwright/test').Page) {
  await page.goto('/');
  await page.waitForFunction(() => !!(window as any)._AMUX_AUTH_TOKEN, { timeout: 20000 });
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  await page.goto('/?_token=' + encodeURIComponent(token));
  await page.waitForURL(/_fresh=1/, { timeout: 20000 });
}

test('an app inside amux keeps its links in the viewer, with Back', async ({ page, context }, testInfo) => {
  const dir = makeSite();
  const app = http.createServer((req, res) => {
    const rel = decodeURIComponent((req.url || '/').split('?')[0]);
    const file = path.join(dir, rel.endsWith('/') ? rel + 'index.html' : rel);
    if (!file.startsWith(dir) || !fs.existsSync(file)) { res.writeHead(404); res.end(); return; }
    res.writeHead(200, { 'Content-Type': file.endsWith('.css') ? 'text/css' : 'text/html' });
    res.end(fs.readFileSync(file));
  });
  await new Promise<void>(r => app.listen(0, '127.0.0.1', () => r()));
  const port = (app.address() as AddressInfo).port;
  let popups = 0;
  context.on('page', () => { popups++; });
  try {
    await signIn(page);
    const tag = `viewer ${testInfo.project.name}`;
    await page.goto('/#view=apps');
    await page.getByRole('button', { name: 'Add app' }).click();
    await page.locator('#pub-new-target').fill(String(port));
    await page.locator('#pub-new-title').fill(tag);
    await page.locator('#modal-btns').getByRole('button', { name: 'Add' }).click();
    await page.locator('#pub-internal .pub-card', { hasText: tag }).click();
    const frame = page.frameLocator('.pub-app-frame');
    await expect(frame.locator('h1')).toHaveText('The hub', { timeout: 20000 });
    await expect(page.locator('#pages-app-back')).toBeHidden();
    // target=_blank to a page of the same app: it opens here, not in a tab.
    await frame.locator('#tab').click();
    await expect(frame.locator('h1')).toHaveText('Core page');
    await expect(frame.locator('h1')).toHaveCSS('color', 'rgb(1, 2, 3)');
    await expect(page.locator('#pages-status')).toHaveText('/landing/core.html');
    expect(popups).toBe(0);
    await page.locator('#pages-app-back').click();
    await expect(frame.locator('h1')).toHaveText('The hub');
    await expect(page.locator('#pages-app-back')).toBeHidden();
  } finally {
    app.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('the Files tab shows an HTML file as its folder\'s website', async ({ page }) => {
  const dir = makeSite();
  try {
    await signIn(page);
    await page.evaluate(f => (window as any).openFilePreview(f), path.join(dir, 'landing', 'core.html'));
    const frame = page.frameLocator('.file-html-preview iframe');
    await expect(frame.locator('h1')).toHaveText('Core page', { timeout: 20000 });
    // The sibling stylesheet one folder up loaded.
    await expect(frame.locator('h1')).toHaveCSS('color', 'rgb(1, 2, 3)');
    const src = await page.locator('.file-html-preview iframe').getAttribute('src');
    expect(new URL(src!).pathname).toMatch(/^\/app\/[0-9a-f]{32}\/landing\/core\.html$/);
    expect(new URL(src!).origin).not.toBe(new URL(page.url()).origin);
    // Still sandboxed: the page cannot reach the dashboard or the owner's apps.
    await expect(page.locator('.file-html-preview iframe')).toHaveAttribute('sandbox', /allow-scripts/);
    expect(await page.locator('.file-html-preview iframe').getAttribute('sandbox')).not.toContain('allow-same-origin');
    await frame.locator('#home').click();
    await expect(frame.locator('h1')).toHaveText('The hub');
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
