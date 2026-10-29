// The Apps tab's three kinds (static_files/public_links.rs): an app inside
// amux for the owner, the app shared publicly, and a public folder. The real
// server and a real loopback app; everything is made and removed in the UI,
// and the public links are opened with no amux credentials.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';
import * as fs from 'fs';
import * as http from 'http';
import * as os from 'os';
import * as path from 'path';
import type { AddressInfo } from 'net';

test('Apps provisions an app and a file inside amux, a public app and a public folder', async ({ page, playwright }, testInfo) => {
  const app = http.createServer((req, res) => {
    // Like mtp-demo's sign-in page: an app that forbids framing still shows in Apps.
    res.writeHead(200, { 'Content-Type': 'text/html', 'X-Frame-Options': 'SAMEORIGIN' });
    res.end(`<h1>shared app</h1><p id="cookie">${(req.headers.cookie || '').replace(/</g, '')}</p>`);
  });
  await new Promise<void>(r => app.listen(0, '127.0.0.1', () => r()));
  const port = (app.address() as AddressInfo).port;
  // A visible folder under HOME: amux never publishes hidden paths.
  const dir = fs.mkdtempSync(path.join(os.homedir(), 'pagesfiles-'));
  fs.writeFileSync(path.join(dir, 'report.txt'), 'quarterly report');
  fs.writeFileSync(path.join(dir, '.secret'), 'never public');
  // A deck in its own folder, beside its build script (sandbox rothco, 2026-10-26).
  const deckDir = fs.mkdtempSync(path.join(os.homedir(), 'pagesdeck-'));
  fs.writeFileSync(path.join(deckDir, 'deck.html'), '<h1>the deck</h1>');
  fs.writeFileSync(path.join(deckDir, 'build_deck.py'), 'print(1)');
  fs.writeFileSync(path.join(deckDir, 'notes.txt'), 'plain notes\n');
  const tag = testInfo.project.name;
  const modalBtn = (name: string) => page.locator('#modal-btns').getByRole('button', { name });
  const base = () => new URL(page.url()).origin;
  const anon = await playwright.request.newContext({ ignoreHTTPSErrors: true });
  let sharedDir = '';
  try {
    // A frame cannot send the bearer token: sign this browser in with the
    // owner cookie.
    await page.goto('/');
    await page.waitForFunction(() => !!(window as any)._AMUX_AUTH_TOKEN, { timeout: 20000 });
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
    await page.goto('/?_token=' + encodeURIComponent(token));
    await page.waitForURL(/_fresh=1/, { timeout: 20000 });
    await page.goto('/#view=apps');
    for (const h of ['Inside amux', 'Public apps', 'Public files']) await expect(page.getByRole('heading', { name: h })).toBeVisible();

    // 1. Inside amux: listed, and opens in the amux UI.
    await page.getByRole('button', { name: 'Add app' }).click();
    await page.locator('#pub-new-target').fill(String(port));
    await page.locator('#pub-new-title').fill(`inside ${tag}`);
    await modalBtn('Add').click();
    const inside = page.locator('#pub-internal .pub-card', { hasText: `inside ${tag}` });
    await expect(inside).toContainText(/\/app\/[0-9a-f]{32}\//);
    await inside.click();
    await expect(page.locator('#pages-open')).toBeVisible();
    await expect(page.frameLocator('.pub-app-frame').locator('h1')).toHaveText('shared app');
    // It runs on the apps origin (its own port), never the dashboard's.
    const frameSrc = await page.locator('.pub-app-frame').getAttribute('src');
    const frameUrl = new URL(frameSrc!);
    expect(frameUrl.pathname).toMatch(/^\/app\/[0-9a-f]{32}\/$/);
    expect(frameUrl.origin).not.toBe(base());
    // The one-time link was spent opening the frame; it opens nothing again.
    const spent = await anon.get(frameSrc!, { maxRedirects: 0 });
    expect(spent.status()).toBe(403);
    await expect(page.locator('#pages-page-actions')).toBeHidden();
    await page.getByRole('button', { name: 'Back to all apps' }).click();
    // A worker's localhost link lands on that Apps entry; /proxy/ is gone. The
    // older #view=pages address (in links workers already sent) still works.
    await page.goto(`/#view=pages&add-internal=${port}`);
    await expect(page.frameLocator('.pub-app-frame').locator('h1')).toHaveText('shared app');
    const gone = await anon.get(`${base()}/proxy/${port}/`);
    expect(await gone.text()).not.toContain('shared app');
    await page.getByRole('button', { name: 'Back to all apps' }).click();

    // 1b. A file inside amux, added by path: shown in the amux UI with no
    // server started, and only that file.
    await page.getByRole('button', { name: 'Add app' }).click();
    await page.locator('#pub-new-target').fill(path.join(deckDir, 'deck.html'));
    await page.locator('#pub-new-title').fill(`deck ${tag}`);
    await modalBtn('Add').click();
    const deck = page.locator('#pub-internal .pub-card', { hasText: `deck ${tag}` });
    await expect(deck).toContainText('deck.html');
    // Previews load as their card scrolls into view (a phone shows one at a time).
    await deck.scrollIntoViewIfNeeded();
    await expect(deck.frameLocator('.pub-preview iframe').locator('h1')).toHaveText('the deck');
    await deck.click();
    await expect(page.frameLocator('.pub-app-frame').locator('h1')).toHaveText('the deck');
    await page.getByRole('button', { name: 'Back to all apps' }).click();

    // A plain-text file shows on the frame's white backing under a dark system
    // theme: Chrome drew it in light text on white, a blank page (sandbox,
    // 2026-10-27), until the frames were made light. Playwright's dark emulation
    // overrides every frame's scheme, so this pins the frame's own scheme, which
    // is what real Chrome follows (checked live on the sandbox). The dashboard
    // must be dark for it to mean anything: a light one is inherited as light.
    await page.evaluate(() => (window as any).themeActivate('amux-dark'));
    await page.getByRole('button', { name: 'Add app' }).click();
    await page.locator('#pub-new-target').fill(path.join(deckDir, 'notes.txt'));
    await page.locator('#pub-new-title').fill(`notes ${tag}`);
    await modalBtn('Add').click();
    const notesCard = page.locator('#pub-internal .pub-card', { hasText: `notes ${tag}` });
    await notesCard.scrollIntoViewIfNeeded();
    await expect(notesCard.locator('.pub-preview iframe')).toHaveCSS('color-scheme', 'light');
    await notesCard.click();
    await expect(page.frameLocator('.pub-app-frame').locator('body')).toContainText('plain notes');
    await expect(page.locator('.pub-app-frame')).toHaveCSS('color-scheme', 'light');
    await page.getByRole('button', { name: 'Back to all apps' }).click();
    await page.evaluate(() => (window as any).themeActivate('amux-light'));

    // 2. A public app: anyone, no amux credential reaches it, and its card
    // shows a live preview.
    await page.getByRole('button', { name: 'Share an app' }).click();
    await page.locator('#pub-new-target').fill(String(port));
    await page.locator('#pub-new-title').fill(`public ${tag}`);
    await modalBtn('Create link').click();
    const pub = page.locator('#pub-apps .pub-card', { hasText: `public ${tag}` });
    const appHref = await pub.locator('.pub-url').getAttribute('href');
    expect(appHref).toMatch(/\/pub\/[0-9a-f]{32}\/$/);
    const appUrl = new URL(appHref!, base()).toString();
    const opened = await anon.get(appUrl, { headers: { Cookie: 'amux_member=m; appsid=1' } });
    expect(opened.status()).toBe(200);
    const body = await opened.text();
    expect(body).toContain('<h1>shared app</h1>');
    expect(body).not.toContain('amux_member');
    expect(opened.headers()['content-security-policy']).toContain('sandbox');
    await expect(pub.locator('.pub-preview iframe')).toHaveAttribute('src', /\/pub\/[0-9a-f]{32}\/$/);
    await expect(pub.locator('.pub-preview iframe')).toHaveAttribute('sandbox', 'allow-scripts');

    // An app that is not running shows a placeholder instead of a preview.
    await page.getByRole('button', { name: 'Add app' }).click();
    await page.locator('#pub-new-target').fill('65002');
    await page.locator('#pub-new-title').fill(`stopped ${tag}`);
    await modalBtn('Add').click();
    const stopped = page.locator('#pub-internal .pub-card', { hasText: `stopped ${tag}` });
    await expect(stopped.locator('.pub-preview')).toContainText('Not running');
    await expect(stopped.locator('.pub-preview iframe')).toHaveCount(0);

    // 3. A public folder added by path: its files, never its hidden ones.
    await page.getByRole('button', { name: 'Share a file' }).click();
    await page.locator('#pub-new-target').fill(dir);
    await page.locator('#pub-new-title').fill(`files ${tag}`);
    await modalBtn('Create link').click();
    const files = page.locator('#pub-files .pub-file', { hasText: `files ${tag}` });
    const folderUrl = new URL((await files.locator('.pub-url').getAttribute('href'))!, base()).toString();
    const listing = await (await anon.get(folderUrl)).text();
    expect(listing).toContain('report.txt');
    expect(listing).not.toContain('.secret');
    expect(await (await anon.get(folderUrl + 'report.txt')).text()).toBe('quarterly report');
    expect((await anon.get(folderUrl + '.secret')).status()).toBe(404);
    await expect(files).toContainText('1 file');

    // 4. An upload: a new ~/Shared/<name> folder, live at once, its file opens
    // signed out once it arrives.
    const upName = `upload-${tag}-${Date.now()}`;
    await page.getByRole('button', { name: 'Share a file' }).click();
    await page.locator('#pub-share-files').setInputFiles({ name: `${upName}.txt`, mimeType: 'text/plain', buffer: Buffer.from('uploaded and shared') });
    await expect(page.locator('#pub-share-picked')).toContainText('1 file ready to share');
    await modalBtn('Create link').click();
    const uploaded = page.locator('#pub-files .pub-file', { hasText: upName });
    await expect(uploaded).toBeVisible({ timeout: 15000 });
    sharedDir = (await uploaded.locator('.ui-help').last().textContent())!.split(' · ')[0].trim();
    expect(sharedDir).toMatch(/\/Shared\/upload-/);
    const upUrl = new URL((await uploaded.locator('.pub-url').getAttribute('href'))!, base()).toString();
    await expect.poll(async () => (await anon.get(upUrl + `${upName}.txt`)).status(), { timeout: 30000 }).toBe(200);
    expect(await (await anon.get(upUrl + `${upName}.txt`)).text()).toBe('uploaded and shared');

    // 5. A link a worker asks for (the bearer alone, no person) waits for
    // approval in Apps and serves nobody until approved here.
    const asked = await anon.post(`${base()}/api/page-links`, { headers: { Authorization: 'Bearer ' + token, 'Content-Type': 'application/json' },
      data: { kind: 'file', path: dir, title: `asked ${tag}` } });
    expect(asked.status()).toBe(201);
    const askedLink = await asked.json();
    expect(askedLink.pending).toBe(true);
    expect((await anon.get(new URL(askedLink.path, base()).toString())).status()).toBe(404);
    await page.goto('/#view=apps');
    const pending = page.locator('#pub-files .pub-file', { hasText: `asked ${tag}` });
    await expect(pending).toContainText('Waiting for your approval');
    await pending.getByRole('button', { name: 'Approve' }).click();
    await modalBtn('Approve').click();
    await expect(pending).not.toContainText('Waiting for your approval');
    expect((await anon.get(new URL(askedLink.path, base()).toString())).status()).toBe(200);

    // Search filters pages, apps and files.
    await page.locator('#pages-search').fill(`files ${tag}`);
    await expect(files).toBeVisible();
    await expect(pub).toBeHidden();
    await page.locator('#pages-search').fill('');
    await expect(pub).toBeVisible();

    // One row of controls per entry on a phone; cards fit the screen.
    // The list redraws itself while uploads land (2, 6 and 15 s refreshes), so
    // each poll reads the box once: a second read can fall between two renders
    // and find none (ios-safari CI, 2026-10-26).
    const vw = page.viewportSize()!.width;
    for (const row of [pub, files]) {
      await expect.poll(async () => {
        const w = (await row.boundingBox())?.width ?? -1;
        return w > 0 && w <= vw ? 'fits' : `width ${w} in a ${vw}px viewport`;
      }).toBe('fits');
    }

    // Turned off from the card's menu (where it lives on a phone), gone for everyone.
    const viaMenu = async (row: typeof pub, item: string) => {
      await row.locator('.pub-more').click();
      await page.locator('#pub-menu').getByRole('menuitem', { name: item }).click();
    };
    await viaMenu(pub, 'Turn off');
    await modalBtn('Turn off').click();
    await expect(pub).toHaveCount(0);
    expect((await anon.get(appUrl)).status()).toBe(404);
    for (const [row, url] of [[files, folderUrl], [uploaded, upUrl], [pending, new URL(askedLink.path, base()).toString()]] as const) {
      await row.getByRole('button', { name: 'Turn off' }).click();
      await modalBtn('Turn off').click();
      await expect(row).toHaveCount(0);
      expect((await anon.get(url)).status()).toBe(404);
    }
    for (const row of [inside, stopped]) {
      await viaMenu(row, 'Remove from Apps');
      await modalBtn('Remove').click();
      await expect(row).toHaveCount(0);
    }
  } finally {
    await cleanup('dispose the anonymous request context', () => anon.dispose());
    app.close();
    fs.rmSync(dir, { recursive: true, force: true });
    fs.rmSync(deckDir, { recursive: true, force: true });
    if (sharedDir && sharedDir.includes('/Shared/upload-')) fs.rmSync(sharedDir, { recursive: true, force: true });
  }
});

// Ethan, 2026-10-28: "Not approved: only the owner, signed in to the dashboard,
// can approve an Apps link" on his own click. A browser on this Mac gets the
// owner bearer in the page and never the owner-session cookie approval takes,
// unless it went through ?_token= first (the test above does; the owner does
// not). Approve now signs in once with the page's token and retries.
test('the owner approves a pending link from a plain page load, with no sign-in step', async ({ page, playwright }, testInfo) => {
  const dir = fs.mkdtempSync(path.join(os.homedir(), 'pagesapprove-'));
  fs.writeFileSync(path.join(dir, 'a.txt'), 'approve me\n');
  const anon = await playwright.request.newContext({ ignoreHTTPSErrors: true });
  try {
    await page.goto('/');
    await page.waitForFunction(() => !!(window as any)._AMUX_AUTH_TOKEN, { timeout: 20000 });
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
    const base = new URL(page.url()).origin;
    const title = `plain approve ${testInfo.project.name}`;
    const asked = await anon.post(`${base}/api/page-links`, { headers: { Authorization: 'Bearer ' + token, 'Content-Type': 'application/json' },
      data: { kind: 'file', path: dir, title } });
    expect(asked.status()).toBe(201);
    const link = await asked.json();
    expect(link.pending).toBe(true);
    // The bearer alone (a worker's kind of caller) is still refused.
    const self = await anon.post(`${base}/api/page-links/${link.id}/approve`, { headers: { Authorization: 'Bearer ' + token } });
    expect(self.status()).toBe(403);
    await page.goto('/#view=apps');
    const pending = page.locator('#pub-files .pub-file', { hasText: title });
    await expect(pending).toContainText('Waiting for your approval');
    await pending.getByRole('button', { name: 'Approve' }).click();
    await page.locator('#modal-btns').getByRole('button', { name: 'Approve' }).click();
    await expect(pending).not.toContainText('Waiting for your approval', { timeout: 15000 });
    expect((await anon.get(new URL(link.path, base).toString())).status()).toBe(200);
  } finally {
    await cleanup('dispose the anonymous request context', () => anon.dispose(), testInfo);
    await cleanup('remove the approved folder', () => fs.rmSync(dir, { recursive: true, force: true }), testInfo);
  }
});
