// Pasting an image into a scratchpad note (Ethan, 2026-11-08: "I should be able
// to paste images in scratchpad"). The image is saved beside the note and the
// note gets ![](name) at the cursor; the preview shows the picture. Covered for
// the note editor and for the quick-capture box while it holds typing (an empty
// box already saved a pasted image as its own file). Runs in every project.
import { test, expect, Page } from './fixtures';
import { scratchpadDir } from './scratchpad-dir';
import { boot } from './lifecycle/evidence';

type Win = Window & { switchView: (v: string) => void; _scratchpadNewNote: () => void; _AMUX_AUTH_TOKEN: string };
const token = (p: Page) => p.evaluate(() => (window as unknown as Win)._AMUX_AUTH_TOKEN);

async function openScratchpad(p: Page) {
  await boot(p);
  await p.evaluate(() => (window as unknown as Win).switchView('scratchpad'));
  await expect(p.locator('#scratchpad-view')).toBeVisible();
}

// A real 8x8 red PNG, pasted into `sel` the way a browser delivers a screenshot.
async function pasteImage(p: Page, sel: string) {
  await p.evaluate(async s => {
    const c = document.createElement('canvas'); c.width = 8; c.height = 8;
    const g = c.getContext('2d')!; g.fillStyle = '#e33'; g.fillRect(0, 0, 8, 8);
    const blob: Blob = await new Promise(r => c.toBlob(b => r(b!), 'image/png'));
    const dt = new DataTransfer();
    dt.items.add(new File([blob], 'image.png', { type: 'image/png' }));
    const t = document.querySelector(s) as HTMLTextAreaElement;
    t.focus();
    t.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
  }, sel);
}
const linkIn = (v: string) => (v.match(/!\[\]\((pasted-[^)]+\.png)\)/) || [])[1] || '';

test('an image pasted into a note editor is saved beside it and shows in the preview', async ({ page }) => {
  test.setTimeout(90_000);
  await openScratchpad(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  const dir = await scratchpadDir(page.request, auth);
  await page.evaluate(() => (window as unknown as Win)._scratchpadNewNote());
  const ta = page.locator('#file-edit-ta');
  await expect(ta).toBeVisible();
  const head = `Screenshot note ${Date.now()}`;
  await ta.fill(head + '\n');
  await ta.press('End');
  await pasteImage(page, '#file-edit-ta');
  await expect.poll(async () => linkIn(await ta.inputValue())).not.toBe('');
  const name = linkIn(await ta.inputValue());
  expect(await ta.inputValue(), 'the typed text stays, the link follows it').toContain(head);
  // The image is on disk beside the note, under the name the link uses.
  await expect.poll(async () => (await page.request.get('/api/file/raw?path=' + encodeURIComponent(dir + '/' + name), { headers: auth })).status(), { timeout: 15_000 }).toBe(200);
  await page.locator('#file-save-btn').click();
  await expect(page.locator('#file-save-btn')).toHaveText('Saved!');
  await page.locator('#file-tab-preview').click();
  const img = page.locator('#file-body img');
  await expect(img).toHaveCount(1);
  await expect.poll(() => img.evaluate((i: HTMLImageElement) => i.complete && i.naturalWidth)).toBe(8);
});

test('an image pasted into the capture box while typing goes into the note', async ({ page }) => {
  test.setTimeout(90_000);
  await openScratchpad(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  const dir = await scratchpadDir(page.request, auth);
  const box = page.locator('#sp-capture');
  const head = `capture with picture ${Date.now()}`;
  await box.fill(head + ' ');
  await pasteImage(page, '#sp-capture');
  await expect.poll(async () => linkIn(await box.inputValue())).not.toBe('');
  const name = linkIn(await box.inputValue());
  expect(await box.inputValue()).toContain(head);
  await expect.poll(async () => (await page.request.get('/api/file/raw?path=' + encodeURIComponent(dir + '/' + name), { headers: auth })).status(), { timeout: 15_000 }).toBe(200);
  // Saving keeps the link in the note.
  await box.press(process.platform === 'darwin' ? 'Meta+Enter' : 'Control+Enter');
  await expect(box).toHaveValue('');
  const noteName = async () => {
    const ls = await (await page.request.get('/api/ls?path=' + encodeURIComponent(dir) + '&hidden=0', { headers: auth })).json();
    return (ls.entries || []).map((e: any) => String(e.name)).find((n: string) => n.startsWith('capture-with-picture')) || '';
  };
  await expect.poll(noteName, { timeout: 15_000 }).not.toBe('');
  const note = await noteName();
  expect(note).toBeTruthy();
  const body = await (await page.request.get('/api/file?path=' + encodeURIComponent(dir + '/' + note), { headers: auth })).json();
  expect(String(body.content)).toContain('![](' + name + ')');
});

// AF-1261: a paste made before the scratchpad config answered went to the
// fallback folder: "not a directory" on CI, the wrong folder on an install
// with a configured one. The page is put back in its before-config state and
// the config answer is held, so the paste lands first; the image must still
// go to the configured folder.
test('an image pasted before the scratchpad folder is known still goes to that folder', async ({ page }) => {
  test.setTimeout(90_000);
  await openScratchpad(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  const dir = await scratchpadDir(page.request, auth);
  await page.route(/\/api\/scratchpad\/config(?:\?.*)?$/, async r => { await new Promise(res => setTimeout(res, 2500)); await r.continue(); });
  await page.evaluate(() => {
    const g = globalThis as any;
    try { localStorage.removeItem('amux_sp_root'); } catch (e) { console.warn('test: no localStorage', e); }
    g.eval("_spRootRead = null; _SP_ROOT = '~/sp-fallback-not-here'; _spPath = _SP_ROOT;");
  });
  const box = page.locator('#sp-capture');
  await box.fill('early paste ');
  await pasteImage(page, '#sp-capture');
  await expect.poll(async () => linkIn(await box.inputValue())).not.toBe('');
  const name = linkIn(await box.inputValue());
  await expect.poll(async () => (await page.request.get('/api/file/raw?path=' + encodeURIComponent(dir + '/' + name), { headers: auth })).status(), { timeout: 20_000 }).toBe(200);
});
