// A dictation clip is kept as its bytes, and a clip that can no longer be read
// says so instead of failing every Retry (owner's iPhone, 2026-11-18: a 104 s
// clip read "Sending… Load failed" on every Retry and no request ever reached
// the server). On WebKit a Blob kept in IndexedDB is a file reference that can
// stop being readable; fetch then rejects with "Load failed" before sending.
import { test, expect, Page } from './fixtures';

const W = 'zz-rec-bytes';

async function boot(page: Page, hold = true) {
  const sent: number[] = [];
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: W, dir: '/tmp/rec-bytes', running: true, status: 'idle', flags: '', steering: [] }] }));
  page.on('request', r => { if (/\/api\/dictate\?/.test(r.url())) sent.push((r.postDataBuffer() || Buffer.alloc(0)).length); });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => (globalThis as any).eval('typeof _idb') === 'object'
    && (globalThis as any).eval('typeof _dictOutboxAdd') === 'function');
  // Hold the app's own background send pass, which skips while a pass is
  // marked running. Otherwise it can send a test's clip first, and the test's
  // send then re-runs a finished clip (kept so a transcript can be re-run):
  // two uploads on iOS Safari, from the race rather than the app. Retry
  // waits for a running pass, so a test of Retry boots without the hold.
  if (hold) await page.evaluate(() => (globalThis as any).eval('_dictFlushing = Date.now()'));
  return sent;
}

test('a clip is stored as its bytes and sends them', async ({ page }) => {
  const sent = await boot(page);
  await page.route(/\/api\/dictate\?.*/, r => r.fulfill({ json: { text: 'from the stored bytes' } }));
  const id = await page.evaluate(async () => {
    const g = globalThis as any;
    return g.eval('_dictOutboxAdd')(new Blob([new Uint8Array(3000).fill(7)], { type: 'audio/webm' }), 'audio/webm', 3000);
  });
  expect(id, 'the clip was saved on this device (WebKit included)').toBeTruthy();
  const stored = await page.evaluate(async () => {
    const q = (await (globalThis as any).eval('_idb').get('dict_pending')) || [];
    return q.map((x: any) => ({ hasBytes: !!x.bytes && x.bytes.byteLength, hasBlob: !!x.blob }));
  });
  expect(stored, 'kept as bytes, not as a Blob').toEqual([{ hasBytes: 3000, hasBlob: false }]);
  const text = await page.evaluate((i) => (globalThis as any).eval('_dictTrySend')(i, false), id);
  expect(text).toBe('from the stored bytes');
  expect(sent, 'the request carried the clip').toEqual([3000]);
});

test('two senders reaching one clip at once upload it once', async ({ page }) => {
  const sent = await boot(page);
  await page.route(/\/api\/dictate\?.*/, async r => {
    await new Promise(res => setTimeout(res, 300));
    await r.fulfill({ json: { text: 'sent once' } });
  });
  const texts = await page.evaluate(async () => {
    const g = globalThis as any;
    const id = await g.eval('_dictOutboxAdd')(new Blob([new Uint8Array(1200).fill(3)], { type: 'audio/webm' }), 'audio/webm', 1200);
    const send = g.eval('_dictTrySend');
    return Promise.all([send(id, false), send(id, true)]);
  });
  expect(texts, 'both callers get the one result').toEqual(['sent once', 'sent once']);
  expect(sent.filter(n => n === 1200), 'one upload').toHaveLength(1);
});

test('a clip that can no longer be read says so once, sends nothing, and offers only Delete', async ({ page }) => {
  const sent = await boot(page, false);
  await page.evaluate(async (w) => {
    const g = globalThis as any;
    await g.eval('_idb').set('dict_pending', [{ id: 'clip-gone', bytes: new Uint8Array(2400).buffer, mime: 'audio/webm',
      ms: 104000, ts: Date.now(), session: w, state: 'failed', attempts: 3, last: Date.now(), error: 'Load failed', text: '' }]);
    // The stored copy is unreadable, as on the phone.
    Blob.prototype.arrayBuffer = () => Promise.reject(Object.assign(new Error('The object can not be found here.'), { name: 'NotFoundError' }));
  }, W);
  await page.evaluate(() => (globalThis as any).eval('_dictRetry')('clip-gone'));
  const row = await page.evaluate(async () => ((await (globalThis as any).eval('_idb').get('dict_pending')) || [])[0]);
  expect(row.state).toBe('unreadable');
  expect(row.error).toContain('can no longer be read');
  expect(sent, 'no request is made for audio that cannot be read').toEqual([]);
  const html = await page.evaluate(async () => (globalThis as any).eval('_dictOutboxHTML')(await (globalThis as any).eval('_dictOutbox')()));
  expect(html).toContain('Can no longer be read');
  expect(html).not.toContain('_dictRetry(');
  expect(html).toContain('_dictOutboxDel(');
  const auto = await page.evaluate(async () => ((await (globalThis as any).eval('_dictSyncClips')(true)) || []).length);
  expect(auto, 'the automatic pass leaves it alone').toBe(0);
  // The connection modal offers no Send again for it either (land review F2).
  await page.evaluate(() => (globalThis as any).eval('_dictPendingBadge')());
  const rows = await page.evaluate(() => (globalThis as any).eval('_dictPendingRows'));
  expect(rows.map((r: any) => r.state)).toEqual(['unreadable']);
  expect(rows.some((r: any) => 'bytes' in r || 'blob' in r), 'the rows carry no audio').toBe(false);
  await page.locator('#conn-status').click();
  const clip = page.locator('#conn-hist-modal .conn-clip-row');
  await expect(clip).toContainText('can no longer be read');
  await expect(clip.getByRole('button', { name: 'Send again' })).toHaveCount(0);
});
