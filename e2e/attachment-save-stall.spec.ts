// MSG-92727 (Ethan, 2026-10-22): a screenshot sat at "Saving locally…" and the
// message could not be sent. The local save (IndexedDB) never completed, which
// WebKit does after its storage process restarts, and nothing bounded it. A save
// with no progress for 8s is now abandoned: the attachment uploads from memory,
// the chip says "no local copy", and a beacon names the stall. Upload endpoints
// are routed; the composer, the attachment code and the send are real.
import {test, expect, Page} from './fixtures';

async function setup(page: Page, sends: string[], diag: any[]) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [{name: 'up-worker', running: true, status: 'idle', dir: '/tmp/up'}]}));
  await page.route(/\/api\/sessions\/up-worker\/peek\?/, r => r.fulfill({json: {name: 'up-worker', live: 'Worker output', history: ''}}));
  await page.route(/\/api\/sessions\/up-worker\/subagents$/, r => r.fulfill({json: {session: 'up-worker', subagents: []}}));
  await page.route(/\/api\/sessions\/up-worker\/send/, r => { sends.push(r.request().postData() || ''); return r.fulfill({json: {ok: true, delivered: true, msg_id: 'm1'}}); });
  await page.route('**/api/client-debug', r => { try { diag.push(JSON.parse(r.request().postData() || '{}')); } catch {} return r.fulfill({json: {ok: true}}); });
  await page.route(/\/api\/upload\//, async r => {
    const url = r.request().url();
    if (url.endsWith('/start')) return r.fulfill({json: {id: 'up1', chunks: JSON.parse(r.request().postData() || '{}').chunks}});
    if (url.includes('/finish')) return r.fulfill({json: {path: '/Users/x/.amux/uploads/shot.png', url: '/api/uploads/shot.png'}});
    if (/\/chunk\/\d+/.test(url)) return r.fulfill({json: {ok: true}});
    return r.fallback();
  });
  await page.goto('/', {waitUntil: 'domcontentloaded'});
  await page.waitForFunction("typeof openPeek === 'function' && typeof _idb === 'object'");
  await page.evaluate(() => (window as any).openPeek('up-worker'));
  await expect(page.locator('#peek-overlay')).toHaveCSS('opacity', '1');
}

const png = {name: 'shot.png', mimeType: 'image/png', buffer: Buffer.alloc(200 * 1024, 1)};
const chip = (page: Page) => page.locator('#peek-attach-bar .peek-attach-chip').first();

test('a local save that never completes does not hold the attachment or the send', async ({page}) => {
  const sends: string[] = [], diag: any[] = [];
  await setup(page, sends, diag);
  // The storage request never completes, as WebKit's does after its storage process restarts.
  await page.evaluate('_idb.putUpload = () => new Promise(() => {})');
  await page.clock.install();
  await page.locator('#peek-file-input').setInputFiles(png);
  await expect(chip(page)).toContainText('Saving locally');
  await page.clock.fastForward(9000);
  await expect(chip(page)).toContainText('✓', {timeout: 15000});
  expect(diag.some(d => d.kind === 'attachment-upload' && d.action === 'attachment-save-stalled' && d.reason === 'local_save_stalled' && d.noLocalCopy)).toBe(true);
  await page.locator('#peek-cmd-input').fill('still buggy');
  await page.locator('#peek-overlay .send-split-main').click();
  await expect.poll(() => sends.join('\n'), {timeout: 15000}).toContain('/Users/x/.amux/uploads/shot.png');
});

test('control: a local save that completes stores the file and sends', async ({page}) => {
  const sends: string[] = [], diag: any[] = [];
  await setup(page, sends, diag);
  await page.locator('#peek-file-input').setInputFiles(png);
  await expect(chip(page)).toContainText('✓', {timeout: 15000});
  expect(diag.some(d => d.action === 'attachment-save-stalled')).toBe(false);
  await page.locator('#peek-cmd-input').fill('ok');
  await page.locator('#peek-overlay .send-split-main').click();
  await expect.poll(() => sends.join('\n'), {timeout: 15000}).toContain('/Users/x/.amux/uploads/shot.png');
});
