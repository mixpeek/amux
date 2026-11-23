// AF-1361. On 2026-11-23 the owner's Mac app sent /api/dictate 24 clips in 4 h
// that the transcriber refused ("Unable to process input audio: audio data is
// empty or corrupted"). Each was 3.6-6.6 KB and stamped as up to 24.8 h long,
// each reached the server twice in the same second, and the same clips came
// back from worker after worker. A take left in the live store was recovered by
// every window on every load, each window sent the same clip, and a refusal was
// retried as if the network had failed.
import { test, expect, Page, allowUnusedRoute } from './fixtures';

const W = 'zz-dict-refused';

async function boot(page: Page) {
  const sent: number[] = [];
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: W, dir: '/tmp/dict-refused', running: true, status: 'idle', flags: '', steering: [] }] }));
  page.on('request', r => { if (/\/api\/dictate\?/.test(r.url())) sent.push((r.postDataBuffer() || Buffer.alloc(0)).length); });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => (globalThis as any).eval('typeof _idb') === 'object'
    && (globalThis as any).eval('typeof _dictRecoverLive') === 'function');
  // Hold the background send pass so only the test sends (see dictation-clip-bytes).
  await page.evaluate(() => (globalThis as any).eval('_dictFlushing = Date.now()'));
  return sent;
}

const outbox = (page: Page) => page.evaluate(async () =>
  ((await (globalThis as any).eval('_idb').get('dict_pending')) || []).map(({ bytes, ...r }: any) => r));

test('a clip the transcriber refuses is sent once, kept, and never retried', async ({ page }) => {
  const sent = await boot(page);
  await page.route(/\/api\/dictate\?.*/, r => r.fulfill({ status: 422, json: {
    error: 'gemini 400: Unable to process input audio: audio data is empty or corrupted.', code: 'audio_unreadable' } }));
  const id = await page.evaluate(() => (globalThis as any).eval('_dictOutboxAdd')(
    new Blob([new Uint8Array(6635).fill(5)], { type: 'audio/webm' }), 'audio/webm', 4000));
  expect(await page.evaluate(i => (globalThis as any).eval('_dictTrySend')(i, true), id)).toBeNull();
  const [row] = await outbox(page);
  expect(row.state).toBe('refused');
  expect(row.error).toContain('no audio it could read');
  expect(await page.evaluate(async () => ((await (globalThis as any).eval('_dictSyncClips')(true)) || []).length),
    'nothing sends it again on its own').toBe(0);
  await page.evaluate(() => (globalThis as any).eval('_dictRetryAll')());
  expect(sent, 'one request, and Try all again sends nothing more').toEqual([6635]);
  const html = await page.evaluate(async () => (globalThis as any).eval('_dictOutboxHTML')(await (globalThis as any).eval('_dictOutbox')()));
  expect(html).toContain('No audio the transcriber can read');
  expect(html).not.toContain('_dictRetry(');
  expect(html).toContain('_dictPlay(');
  expect(html).toContain('_dictOutboxDel(');
});

test('a clip another window is sending is left to it', async ({ page }) => {
  const sent = await boot(page);
  const dictate = /\/api\/dictate\?.*/;
  await page.route(dictate, r => r.fulfill({ json: { text: 'should not be sent' } }));
  allowUnusedRoute(page, dictate);   // the point: the other window's clip is never sent from here
  await page.evaluate(async () => {
    await (globalThis as any).eval('_idb').set('dict_pending', [{ id: 'clip-other', bytes: new Uint8Array(3000).buffer,
      mime: 'audio/webm', ms: 3000, ts: Date.now(), session: '', state: 'sending', sending_at: Date.now(),
      attempts: 0, last: 0, error: '', text: '' }]);
  });
  expect(await page.evaluate(() => (globalThis as any).eval('_dictTrySend')('clip-other', true))).toBeNull();
  expect(sent).toEqual([]);
  expect((await outbox(page))[0].state).toBe('sending');
});

test('a take left in the live store is recovered once, with its own length', async ({ page }) => {
  await boot(page);   // the send pass is held, so recovery sends nothing
  const started = await page.evaluate(async (w) => {
    const g = globalThis as any;
    const started = Date.now() - 89_000_000;   // begun a day ago, as on the Mac app
    g.__live = { take: 7, started, last: started + 4000, mime: 'audio/webm', session: w,
      // Bytes, not a Blob: an ephemeral WebKit context will not keep a Blob in IndexedDB.
      chunks: [new Uint8Array(6635).fill(9)] };
    localStorage.removeItem('amux_dict_recovered');
    await g.eval('_idb').set('dict_live', g.__live);
    await g.eval('_dictRecoverLive')();
    // A second window, or the next load after a clear that did not land.
    await g.eval('_idb').set('dict_live', g.__live);
    await g.eval('_dictRecoverLive')();
    // Even where this device's note of it is gone, the outbox has it.
    localStorage.removeItem('amux_dict_recovered');
    await g.eval('_idb').set('dict_live', g.__live);
    await g.eval('_dictRecoverLive')();
    return started;
  }, W);
  const rows = (await outbox(page)).filter((x: any) => x.live_started === started);
  expect(rows, 'one clip for one take').toHaveLength(1);
  expect(rows[0].ms, 'the take was 4 s long, not a day').toBe(4000);
  expect(rows[0].session).toBe(W);
});

test('a chunk whose take has no record yet is dropped unless it is the take\'s first', async ({ page, browserName }) => {
  // The recorder's chunks are Blobs, and an ephemeral WebKit context refuses to
  // keep a Blob in IndexedDB, so the save cannot be observed there.
  test.skip(browserName === 'webkit', 'Playwright WebKit cannot store a Blob in IndexedDB');
  await boot(page);
  const stored = await page.evaluate(async () => {
    const g = globalThis as any;
    const idb = g.eval('_idb');
    await idb.set('dict_live', null);
    const take = g.eval('++_dictTake');
    const chunk = () => new Blob([new Uint8Array(2000).fill(1)], { type: 'audio/webm' });
    await g.eval('_dictLiveSave')(chunk(), take, false);   // a tail after its take was cleared
    const afterTail = await idb.get('dict_live');
    await g.eval('_dictLiveSave')(chunk(), take, true);
    await g.eval('_dictLiveSave')(chunk(), take, false);
    const live = await idb.get('dict_live');
    return { afterTail, take: live && live.take === take, chunks: live && live.chunks.length, hasLast: !!(live && live.last) };
  });
  expect(stored.afterTail, 'a headerless tail starts no record').toBeNull();
  expect(stored).toMatchObject({ take: true, chunks: 2, hasLast: true });
});
