// Files read "1 recording queued (2.4 MB) - sending..." over a dictation clip
// nothing was sending (Ethan, 2026-10-30: "figure out what this is and fix on
// the files view"). A clip is marked `sending` before its upload, and a reload
// mid-upload left it there for good: never retried, never shown as stuck. The
// clips here are fakes in this browser's IndexedDB and /api/dictate is
// answered by the test, so no audio leaves the machine.
import { test, expect, Page } from './fixtures';

const W = 'zz-rec-a';

// Each test stubs only what it calls: the fixtures fail a stub nothing used.
async function boot(page: Page, { dictate = false, peek = false } = {}) {
  const dictated: string[] = [];
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: W, dir: '/tmp/rec-a', running: true, status: 'idle', flags: '', steering: [] }] }));
  if (peek) await page.route(/\/api\/sessions\/zz-rec-a\/peek\?.*/, r => r.fulfill({ json: { output: 'idle\n', history: '' } }));
  if (dictate) await page.route(/\/api\/dictate\?.*/, r => { dictated.push(r.request().url()); return r.fulfill({ json: { text: 'hello from the clip' } }); });
  // Observed, not stubbed, so a test that expects no send can say so.
  else page.on('request', r => { if (/\/api\/dictate\?/.test(r.url())) dictated.push(r.url()); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function'
    && (globalThis as any).eval('typeof _idb') === 'object'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === 'zz-rec-a'));
  return dictated;
}
async function seed(page: Page, clips: any[]) {
  await page.evaluate(async (cs) => {
    const g = globalThis as any;
    const withBlobs = cs.map((c: any) => ({ ...c, blob: new Blob([new Uint8Array(2400)], { type: 'audio/webm' }) }));
    await g.eval('_idb').set('dict_pending', withBlobs);
    await g.eval('_upqRenderBadge')();
  }, clips);
}
const badge = (page: Page) => page.locator('#files-cache-status .upq-badge');

// WEBKIT GAP, CAUSE FOUND (2026-10-31): Playwright's WebKit runs an ephemeral
// (private) context, and WebKit refuses to store a Blob in IndexedDB there:
// `put` fails with "UnknownError: Error preparing Blob/File data to be stored in
// object store" (probed directly). So no clip can be seeded on ios-safari. The
// Mac app and Safari keep a persistent store, where Blobs are stored, so this is
// the test environment, not the product. Chromium (desktop, mobile) runs these.
const webkitGap = (browserName: string) => test.fixme(browserName === 'webkit', 'ephemeral WebKit contexts cannot store a Blob in IndexedDB (see the note above)');

test('a clip left "sending" by a reload is sent again and leaves the queue', async ({ page, browserName }) => {
  webkitGap(browserName);
  const dictated = await boot(page, { dictate: true });
  // Marked sending ten minutes ago, the way a reload mid-upload leaves it.
  await seed(page, [{ id: 'clip-stale', session: W, state: 'sending', sending_at: Date.now() - 600000, ms: 3000, mime: 'audio/webm', attempts: 0 }]);
  await page.evaluate(() => (globalThis as any).eval('_dictFlushOffline')());
  await expect.poll(() => dictated.length, { message: 'the stuck clip was sent' }).toBe(1);
  const left = await page.evaluate(async () => ((await (globalThis as any).eval('_idb').get('dict_pending')) || [])
    .filter((x: any) => x.state !== 'done').length);
  expect(left).toBe(0);
});

test('a clip that is really sending is left alone', async ({ page }) => {
  const dictated = await boot(page);
  await seed(page, [{ id: 'clip-live', session: W, state: 'sending', sending_at: Date.now() - 5000, ms: 3000, mime: 'audio/webm', attempts: 0 }]);
  await page.evaluate(() => (globalThis as any).eval('_dictFlushOffline')());
  await page.waitForTimeout(500);
  expect(dictated.length, 'a send still inside its 120s window is not doubled').toBe(0);
});

test('a clip past its retries says so on Files and opens the Dictation tab', async ({ page, browserName }) => {
  webkitGap(browserName);
  await boot(page, { peek: true });
  await page.evaluate(() => (window as any).switchView?.('files'));
  await seed(page, [{ id: 'clip-held', session: W, state: 'failed', attempts: 6, last: Date.now(), error: 'quota', ms: 3000, mime: 'audio/webm' }]);
  await expect(badge(page)).toBeVisible();
  await expect(badge(page)).toContainText('1 recording queued');
  await expect(badge(page)).toContainText('1 not sent, open to retry');
  await expect(badge(page)).not.toContainText('sending');
  await badge(page).click();
  await expect(page.locator('#peek-tab-dictation')).toHaveClass(/active/);
  await expect.poll(() => page.evaluate(() => (globalThis as any).eval('peekSession'))).toBe(W);
});

test('the Files toolbar has no Up button; the parent row does that', async ({ page }) => {
  await boot(page);
  await page.evaluate(() => (window as any).switchView?.('files'));
  await expect(page.locator('#files-view .fe-toolbar')).toBeAttached();
  await expect(page.locator('#files-view .fe-toolbar').getByRole('button', { name: /^Up$/ })).toHaveCount(0);
  expect(await page.evaluate(() => typeof (window as any)._filesUp)).toBe('undefined');
});

// Owner, 2026-10-31: the badge still read "sending" for good in the Mac app,
// whose WebKit storage reads time out. A pass waited on a read that never
// settled, so _dictFlushing stayed set and every later pass returned at once.
test('a storage read that never settles does not hold the next pass', async ({ page, browserName }) => {
  webkitGap(browserName);
  test.setTimeout(60_000);
  const dictated = await boot(page, { dictate: true });
  await seed(page, [{ id: 'clip-wait', session: W, state: 'failed', attempts: 0, last: 0, ms: 3000, mime: 'audio/webm' }]);
  await page.evaluate(() => {
    const idb = (globalThis as any).eval('_idb'), get = idb.get;
    let hung = false;
    idb.get = (k: string) => { if (k === 'dict_pending' && !hung) { hung = true; return new Promise(() => {}); } return get.call(idb, k); };
  });
  await page.evaluate(() => { (globalThis as any).eval('_dictFlushOffline')(); });
  await page.waitForTimeout(6000);   // past the 5s read bound
  await page.evaluate(() => (globalThis as any).eval('_dictFlushOffline')());
  await expect.poll(() => dictated.length, { message: 'the next pass ran and sent the clip' }).toBe(1);
});

test('changing one clip never drops the others, even when a read fails', async ({ page, browserName }) => {
  webkitGap(browserName);
  await boot(page);
  await seed(page, [
    { id: 'c1', session: W, state: 'failed', attempts: 1, last: Date.now(), ms: 1000, mime: 'audio/webm' },
    { id: 'c2', session: W, state: 'failed', attempts: 1, last: Date.now(), ms: 1000, mime: 'audio/webm' }]);
  const ids = await page.evaluate(async () => {
    const g = globalThis as any, idb = g.eval('_idb'), get = idb.get;
    idb.get = () => Promise.reject(new Error('read failed'));   // the old read-then-write path wrote [] back
    await g.eval('_dictOutboxPatch')('c1', { error: 'patched' });
    idb.get = get;
    return ((await idb.get('dict_pending')) || []).map((x: any) => x.id + ':' + (x.error || ''));
  });
  expect(ids).toEqual(['c1:patched', 'c2:']);
});

test('a clip waiting for its next try says so, not "sending"', async ({ page, browserName }) => {
  webkitGap(browserName);
  await boot(page);
  await page.evaluate(() => (window as any).switchView?.('files'));
  await seed(page, [{ id: 'clip-retry', session: W, state: 'failed', attempts: 2, last: Date.now(), error: 'transcriber busy', ms: 3000, mime: 'audio/webm' }]);
  await expect(badge(page)).toContainText('retrying soon (transcriber busy)');
  await expect(badge(page)).not.toContainText('sending');
});

// Land review F1/F2: the badge follows every outbox change on its own, with no
// Dictation tab opened and no repaint called by hand.
test('the Files badge follows the outbox as clips are added and patched', async ({ page, browserName }) => {
  webkitGap(browserName);
  await boot(page);
  await page.evaluate(() => (window as any).switchView?.('files'));
  await page.evaluate(() => (globalThis as any).eval('_dictFlushing = Date.now()'));   // hold the sender so the clip stays queued
  const id = await page.evaluate(() => (globalThis as any).eval('_dictOutboxAdd')(new Blob([new Uint8Array(2400)], { type: 'audio/webm' }), 'audio/webm', 3000));
  expect(id).toBeTruthy();
  await expect(badge(page)).toContainText('1 recording');
  await page.evaluate(i => (globalThis as any).eval('_dictOutboxPatch')(i, { state: 'failed', attempts: 1, last: Date.now(), error: 'transcriber busy' }), id);
  await expect(badge(page)).toContainText('retrying soon (transcriber busy)');
});
