// Scratchpad must not drop what was typed (Ethan, 2026-10-16: "scratchpad is
// dropping data"). A new note opens in the file editor with a manual Save;
// closing it (X) used to discard the text silently. The quick-capture box also
// lost typed text on a reload.
import { test, expect, Page } from './fixtures';
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
    const ls = await (await request.get('/api/ls?path=' + encodeURIComponent('~/Vault/Scratchpad') + '&hidden=0', { headers: auth })).json();
    for (const e of (ls.entries || []).filter((x: any) => String(x.name).endsWith('.md'))) {
      const r = await request.get('/api/file?path=' + encodeURIComponent('~/Vault/Scratchpad/' + e.name), { headers: auth });
      if (r.ok() && String((await r.json()).content || '').includes(marker)) return 1;
    }
    return 0;
  };
  await expect.poll(found, { timeout: 15_000, message: 'the closed note was saved' }).toBe(1);

  // An empty new note closed straight away adds no file (path-based, so it is
  // robust when other specs write to the shared Scratchpad in parallel).
  await page.evaluate(() => (window as any)._scratchpadNewNote());
  await expect(page.locator('#file-edit-ta')).toBeVisible();
  const emptyPath = await page.evaluate(() => (0, eval)('_fileData').path as string);
  await page.locator('#file-overlay .btn', { hasText: '\u2715' }).first().click();
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
  await page.waitForTimeout(1000);
  expect((await request.get('/api/file?path=' + encodeURIComponent(emptyPath), { headers: auth })).ok(),
    'an empty note is not saved to disk').toBe(false);

  // Quick capture: typed (not yet saved) text survives a reload.
  await page.locator('#sp-capture').fill(`draft ${marker}`);
  await page.reload();
  await openScratchpad(page);
  await expect(page.locator('#sp-capture')).toHaveValue(`draft ${marker}`);
});
