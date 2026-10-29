// An attachment previews before it is sent (Ethan, 2026-10-29: "when i attach
// a file i want to be able to click the icon and it shows a preview of it (in
// a modal thats closable)"). The chip's icon opens the shared dialog with the
// device's own copy; Escape and the Close button both close it. Upload
// endpoints are routed; the composer and the attachment code are real.
import {test, expect, Page} from './fixtures';

// A 1x1 PNG, so the image really decodes.
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==', 'base64');

async function setup(page: Page, diag: any[]) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [{name: 'pv-worker', running: true, status: 'idle', dir: '/tmp/pv'}]}));
  await page.route(/\/api\/sessions\/pv-worker\/peek\?/, r => r.fulfill({json: {name: 'pv-worker', live: 'Worker output', history: ''}}));
  await page.route(/\/api\/sessions\/pv-worker\/subagents$/, r => r.fulfill({json: {session: 'pv-worker', subagents: []}}));
  await page.route('**/api/client-debug', r => { try { diag.push(JSON.parse(r.request().postData() || '{}')); } catch {} return r.fulfill({json: {ok: true}}); });
  await page.route(/\/api\/upload\//, async r => {
    const url = r.request().url();
    if (url.endsWith('/start')) return r.fulfill({json: {id: 'pv1', chunks: JSON.parse(r.request().postData() || '{}').chunks}});
    if (url.includes('/finish')) return r.fulfill({json: {path: '/Users/x/.amux/uploads/file', url: '/api/uploads/file'}});
    if (/\/chunk\/\d+/.test(url)) return r.fulfill({json: {ok: true}});
    return r.fallback();
  });
  await page.goto('/', {waitUntil: 'domcontentloaded'});
  await page.waitForFunction("typeof openPeek === 'function' && typeof _attachPreviewOpen === 'function'");
  await page.evaluate(() => (window as any).openPeek('pv-worker'));
  await expect(page.locator('#peek-overlay')).toHaveCSS('opacity', '1');
}

test('clicking an attachment icon previews it in a dialog that closes', async ({page}) => {
  const diag: any[] = [];
  await setup(page, diag);
  await page.locator('#peek-file-input').setInputFiles({name: 'shot.png', mimeType: 'image/png', buffer: PNG});
  const thumb = page.locator('#peek-attach-bar .peek-attach-chip .chip-thumb').first();
  await expect(thumb).toBeVisible();

  await thumb.click();
  const dlg = page.locator('#attach-preview');
  await expect(dlg.locator('.modal-header h3')).toHaveText('shot.png');
  const img = dlg.locator('img.attach-preview-img');
  await expect(img).toBeVisible();
  await expect.poll(() => img.evaluate((el: HTMLImageElement) => el.naturalWidth)).toBe(1);
  await page.keyboard.press('Escape');
  await expect(dlg).toHaveCount(0);
  // The attachment is still there after a preview.
  await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(1);

  // A text file shows its text; the Close button closes it.
  await page.locator('#peek-file-input').setInputFiles({name: 'notes.md', mimeType: 'text/markdown', buffer: Buffer.from('# Reconciliation notes\n')});
  await page.locator('#peek-attach-bar .peek-attach-chip').nth(1).locator('.chip-thumb').click();
  await expect(dlg.locator('pre.attach-preview-text')).toContainText('Reconciliation notes');
  await dlg.locator('[data-close]').click();
  await expect(dlg).toHaveCount(0);
  expect(diag.filter(d => d.kind === 'attach-preview' && d.verdict === 'shown').map(d => d.file_kind)).toEqual(['image', 'text']);
});
