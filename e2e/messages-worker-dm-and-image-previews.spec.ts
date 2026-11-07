// Ethan, 2026-11-06: "can you make it so workers can DM people directly it
// should show up in a 1:1 conversation. i shoudl also be able to open my own
// (ethan) also make sure any image links have an inline preview (like slack)",
// with a screenshot of an `@.../uploads/...-IMG_5708.tiff` post shown as a
// bare link. A worker's direct message is listed with people, your own entry
// opens, and an image link of any common format previews (a format the
// browser cannot draw becomes a chip that opens it, never a broken image).
import { test, expect, Page } from './fixtures';

const ME = { email: 'owner', key: 'person:owner', kind: 'person', name: 'owner', scope_level: 'global', scope_name: '' };
const DM = 'd:mtp|owner';
const LIST = { measured: true, n_considered: 1, me: ME,
  channels: [{ conv: 'g:amux', name: 'amux', unread: 0, mentions: 0, last_ts: 0 }],
  people: [{ key: 'person:owner', conv: 'p:owner|owner', email: 'owner', name: 'owner', me: true, online: true, unread: 0, mentions: 0 }],
  direct: [{ conv: DM, worker: 'mtp', unread: 1, mentions: 1, last_ts: Date.now() }],
  workers: [{ name: 'mtp', conv: 'w:mtp', groups: [], unread: 0, mentions: 0 }],
  totals: { unread: 1, mentions: 1 } };
const TIFF = '/home/amux/.amux/uploads/2c8a20109408-IMG_5708.tiff';
const WEB = 'https://images.example.com/shot.jpg';
const phone = (page: Page) => (page.viewportSize()?.width || 1000) <= 600;
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==', 'base64');

async function setup(page: Page, body: string, conv = 'g:amux', images = false) {
  await page.route(/\/api\/conversations(\?.*)?$/, r => r.fulfill({ json: LIST }));
  await page.route(/\/api\/conversations\/[^/]+\/posts(\?.*)?$/, r => r.fulfill({ json: {
    measured: true, n_considered: 1, people: [ME], workers: [], mentionable_workers: [], typing: [],
    posts: [{ id: 1, conv, author_key: 'worker:mtp', author_name: 'mtp', body, mentions: ['person:owner'], deliveries: [], ts: Date.now() }] } }));
  await page.route(/\/api\/conversations\/[^/]+\/(read|typing)$/, r => r.fulfill({ json: { ok: true } }));
  // Only a post with image links fetches images; a stub nothing requests fails the run (AF-47).
  if (images) {
    // A .tiff the browser cannot draw: served as bytes that are not an image.
    await page.route(/\/api\/file\/raw\?/, r => r.request().url().includes('.tiff')
      ? r.fulfill({ contentType: 'image/tiff', body: Buffer.from('II*\0not-a-real-tiff') })
      : r.fulfill({ contentType: 'image/png', body: PNG }));
    await page.route(WEB, r => r.fulfill({ contentType: 'image/png', body: PNG }));
  }
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_cv_sel'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvSelect === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
  await page.evaluate((c) => (0, eval)(`_cvSelect(${JSON.stringify(c)})`), conv);
}

test('a worker DM and your own conversation are in the people list and open', async ({ page }) => {
  await setup(page, 'the card is ready', DM);
  const dm = page.locator(`#cv-people .cv-item[data-cv="${DM}"]`);
  await expect(dm, 'the worker DM is listed with people').toHaveCount(1);
  await expect(dm).toContainText('mtp');
  await expect(page.locator('#cv-meta')).toContainText('Direct message with mtp');
  const mine = page.locator('#cv-people .cv-item[data-cv="p:owner|owner"]');
  await expect(mine, 'your own entry is a button, not dead text').toHaveCount(1);
  // On a phone the open conversation covers the list; go back to it first.
  if (phone(page)) await page.evaluate(() => (window as any)._cvBack());
  await mine.click();
  await expect(page.locator('#cv-meta')).toContainText('Your notes to yourself');
});

test('an image link of any format previews inline, and one the browser cannot draw is a chip', async ({ page }) => {
  await setup(page, '@' + TIFF + '\nand ' + WEB, 'g:amux', true);
  const body = page.locator('.cv-post-body').first();
  await expect(body).toBeVisible();
  const web = body.locator('img.cv-thumb[data-url="' + WEB + '"]');
  await expect(web, 'a web image link previews').toHaveCount(1);
  await expect.poll(() => web.evaluate((i: HTMLImageElement) => i.complete && i.naturalWidth > 0)).toBe(true);
  // The .tiff is detected (it used to show nothing); in Chromium it cannot be
  // drawn, so it becomes a chip that opens the file viewer.
  const chip = body.locator('.cv-thumb-chip');
  await expect(chip).toHaveCount(1);
  await expect(chip).toContainText('IMG_5708.tiff');
  await page.evaluate(() => { (window as any).__opened = []; (window as any).openFilePreview = (p: string) => (window as any).__opened.push(p); });
  await chip.click();
  await expect.poll(() => page.evaluate(() => (window as any).__opened)).toEqual([TIFF]);
});
