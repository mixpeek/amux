// Scratchpad must not drop what was typed (Ethan, 2026-10-16: "scratchpad is
// dropping data"). A new note opens in the file editor with a manual Save;
// closing it (X) used to discard the text silently. The quick-capture box also
// lost typed text on a reload.
import { test, expect, Page } from './fixtures';
import { scratchpadDir } from './scratchpad-dir';
import { boot } from './lifecycle/evidence';

type Win = Window & { switchView: (v: string) => void; _AMUX_AUTH_TOKEN: string };
async function openScratchpad(page: Page) {
  await boot(page);
  await page.evaluate(() => (window as unknown as Win).switchView('scratchpad'));
  await expect(page.locator('#scratchpad-view')).toBeVisible();
}

test('Scratchpad keeps typed text: a note closed before Save is saved, and capture text survives a reload', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  await openScratchpad(page);
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as unknown as Win)._AMUX_AUTH_TOKEN)}` };
  const marker = `keep-me-${info.project.name}-${Date.now()}`;

  await page.evaluate(() => (window as any)._scratchpadNewNote());
  await expect(page.locator('#file-edit-ta')).toBeVisible();
  await page.locator('#file-edit-ta').fill(`a note\n${marker}\n`);
  // Close without pressing Save.
  await page.locator('#file-overlay .btn', { hasText: '✕' }).first().click();
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
  // Find the note by its content: a same-second name clash moves it to "-2".
  const found = async () => {
    const ls = await (await request.get('/api/ls?path=' + encodeURIComponent(await scratchpadDir(request, auth)) + '&hidden=0', { headers: auth })).json();
    for (const e of (ls.entries || []).filter((x: any) => String(x.name).endsWith('.md'))) {
      const r = await request.get('/api/file?path=' + encodeURIComponent((await scratchpadDir(request, auth)) + '/' + e.name), { headers: auth });
      if (r.ok() && String((await r.json()).content || '').includes(marker)) return 1;
    }
    return 0;
  };
  await expect.poll(found, { timeout: 15_000, message: 'the closed note was saved' }).toBe(1);

  // An empty new note closed straight away adds no file. A new note's path is
  // the shared `untitled.md`, which a co-tenant project (or another CI shard on
  // the same Scratchpad) can hold at the same moment, so reading the disk races
  // them (e1ffba833 went red on exactly that). The property is client logic:
  // closing an empty note sends no write for its path. Watch the requests.
  if (info.project.name === 'desktop') {
    await page.evaluate(() => (window as any)._scratchpadNewNote());
    await expect(page.locator('#file-edit-ta')).toBeVisible();
    const emptyPath = await page.evaluate(() => (0, eval)('_fileData').path as string);
    const writes: string[] = [];
    const onReq = (r: any) => {
      if (r.method() !== 'GET' && new URL(r.url()).pathname === '/api/file'
          && String(r.postData() || '').includes(JSON.stringify(emptyPath).slice(1, -1))) writes.push(r.method() + ' ' + r.url());
    };
    page.on('request', onReq);
    await page.locator('#file-overlay .btn', { hasText: '\u2715' }).first().click();
    await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
    await page.waitForTimeout(1000);
    page.off('request', onReq);
    expect(writes, 'an empty note is not saved to disk').toEqual([]);
  }

  // Quick capture: typed (not yet saved) text survives a reload.
  await page.locator('#sp-capture').fill(`draft ${marker}`);
  await page.reload();
  await openScratchpad(page);
  await expect(page.locator('#sp-capture')).toHaveValue(`draft ${marker}`);
});

// The tab reads its folder from /api/scratchpad/config. A note made the
// moment the tab opened went to the fallback folder while that read was in
// flight (#359 CI, twice). Hold the read for 2s and make the note at once:
// it must still land in the configured folder.
test('a note made while the folder is still being read lands in the configured folder', async ({ page, request }, info) => {
  test.setTimeout(60_000);
  test.skip(info.project.name !== 'desktop', 'client ordering; proven once');
  await page.route('**/api/scratchpad/config', async r => { await new Promise(res => setTimeout(res, 2000)); await r.continue(); });
  await boot(page);
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as unknown as Win)._AMUX_AUTH_TOKEN)}` };
  const marker = `race-${Date.now()}`;
  await page.evaluate(() => (window as unknown as Win).switchView('scratchpad'));
  await page.evaluate(() => (window as any)._scratchpadNewNote());
  await expect(page.locator('#file-edit-ta')).toBeVisible();
  const path = await page.evaluate(() => (0, eval)('_fileData').path as string);
  expect(path.startsWith((await scratchpadDir(request, auth)) + '/'), 'new note path ' + path).toBe(true);
  await page.locator('#file-edit-ta').fill(marker);
  await page.locator('#file-overlay .btn', { hasText: '✕' }).first().click();
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
  await page.unroute('**/api/scratchpad/config');
});

