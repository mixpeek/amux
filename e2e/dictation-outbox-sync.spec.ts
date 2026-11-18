// Queued dictations go through the same flow as queued messages (owner,
// 2026-11-17: "Queued / pending / etc dictations should show up in the same flow
// as for messages and retry similarly"): the connection pill counts them, its
// modal lists them with Send again, Retry now sends them, and the outbox's own
// sync pass delivers them with a row in its checklist. The clips are fakes in
// this browser's IndexedDB and /api/dictate is answered by the test.
import { test, expect, Page } from './fixtures';

const W = 'zz-rec-sync';

async function boot(page: Page, { stub = true } = {}) {
  const dictated: string[] = [];
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: W, dir: '/tmp/rec-sync', running: true, status: 'idle', flags: '', steering: [] }] }));
  // The fixtures fail a stub nothing used, so a test that expects no send observes instead.
  if (stub) await page.route(/\/api\/dictate\?.*/, r => { dictated.push(r.request().url()); return r.fulfill({ json: { text: 'hello from the clip' } }); });
  else page.on('request', r => { if (/\/api\/dictate\?/.test(r.url())) dictated.push(r.url()); });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => (globalThis as any).eval('typeof _idb') === 'object'
    && (globalThis as any).eval('typeof _dictSyncClips') === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === 'zz-rec-sync'));
  return dictated;
}
async function seed(page: Page, clips: any[]) {
  await page.evaluate(async (cs) => {
    const g = globalThis as any;
    await g.eval('_idb').set('dict_pending', cs.map((c: any) => ({ ...c, blob: new Blob([new Uint8Array(2400)], { type: 'audio/webm' }) })));
    await g.eval('_dictPendingBadge')();
  }, clips);
}
const left = (page: Page) => page.evaluate(async () =>
  ((await (globalThis as any).eval('_idb').get('dict_pending')) || []).filter((x: any) => x.state !== 'done').length);

// Same WebKit limit as files-pending-recordings.spec.ts: an ephemeral WebKit
// context cannot store a Blob in IndexedDB, so no clip can be seeded there.
const webkitGap = (browserName: string) => test.fixme(browserName === 'webkit', 'ephemeral WebKit contexts cannot store a Blob in IndexedDB');

test('a failed clip is counted on the pill, listed in its modal, and Retry now sends it', async ({ page, browserName }) => {
  webkitGap(browserName);
  const dictated = await boot(page);
  // Past its automatic retries: only a person sends it now, as with a failed message.
  await seed(page, [{ id: 'clip-held', session: W, state: 'failed', attempts: 6, last: Date.now(), error: 'quota exceeded', ms: 4000, mime: 'audio/webm' }]);
  const pill = page.locator('#conn-status');
  await expect(pill).toHaveText('1 pending');
  await pill.click();
  const row = page.locator('#conn-hist-modal .conn-clip-row');
  await expect(row).toContainText('Transcribe recording (4s, ' + W + ')');
  await expect(row).toContainText('quota exceeded');
  await expect(row.getByRole('button', { name: 'Send again' })).toBeVisible();
  await page.locator('#conn-retry-now').click();
  await expect.poll(() => dictated.length, { message: 'Retry now sent the held clip' }).toBe(1);
  await expect.poll(() => left(page)).toBe(0);
  await expect(pill).not.toHaveText(/pending/);
});

test('the outbox pass delivers a waiting clip and lists it in the sync checklist', async ({ page, browserName }) => {
  webkitGap(browserName);
  const dictated = await boot(page);
  await seed(page, [{ id: 'clip-wait', session: W, state: 'failed', attempts: 1, last: 0, error: 'network', ms: 2000, mime: 'audio/webm' }]);
  await page.evaluate(() => (globalThis as any).eval('runSyncBanner')(true));
  await expect.poll(() => dictated.length).toBe(1);
  const items = await page.evaluate(() => (globalThis as any).eval('_syncChecklist').map((i: any) => [i.key, i.status]));
  expect(items).toContainEqual(['clip:clip-wait', 'done']);
  await expect.poll(() => left(page)).toBe(0);
});

test('an automatic pass keeps a clip\'s backoff, and a clip mid-send is not sent twice', async ({ page, browserName }) => {
  webkitGap(browserName);
  const dictated = await boot(page, { stub: false });
  await seed(page, [
    { id: 'clip-backoff', session: W, state: 'failed', attempts: 3, last: Date.now(), error: 'network', ms: 1000, mime: 'audio/webm' },
    { id: 'clip-live', session: W, state: 'sending', sending_at: Date.now() - 5000, ms: 1000, mime: 'audio/webm', attempts: 0 }]);
  await page.evaluate(() => (globalThis as any).eval('runSyncBanner')(true));
  await page.waitForTimeout(800);
  expect(dictated.length, 'nothing is due yet').toBe(0);
  await expect(page.locator('#conn-status')).toHaveText('2 pending');
});
