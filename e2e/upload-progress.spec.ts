// AMUX-5504 (Ethan, 2026-10-02): a 40 MB video from the iPhone app sat at
// "Uploading 0%". upload/start answered three times; no chunk ever reached the
// server. Each case below is one rule of the fix, driven through the real
// composer with routed upload endpoints (no real worker is involved).
import {test, expect, Page} from './fixtures';

const MB = 1024 * 1024;
const blob = (bytes: number) => ({name: 'clip.mov', mimeType: 'video/quicktime', buffer: Buffer.alloc(bytes, 7)});

async function setup(page: Page) {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.removeItem('amux_upload_bps');
  });
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [{name: 'up-worker', running: true, status: 'idle', dir: '/tmp/up'}]}));
  await page.route(/\/api\/sessions\/up-worker\/peek\?/, r => r.fulfill({json: {name: 'up-worker', live: 'Worker output', history: ''}}));
  await page.route(/\/api\/sessions\/up-worker\/subagents$/, r => r.fulfill({json: {session: 'up-worker', subagents: []}}));
  await page.goto('/', {waitUntil: 'domcontentloaded'});
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(() => (window as any).openPeek('up-worker'));
  await expect(page.locator('#peek-overlay')).toHaveCSS('opacity', '1');
}
const chip = (page: Page) => page.locator('#peek-attach-bar .peek-attach-chip').first();
type Log = {starts: {chunks: number}[], chunks: string[]};
async function routeUploads(page: Page, onChunk: (id: string, n: number, log: Log) => Promise<'ok' | 'hold'>) {
  const log: Log = {starts: [], chunks: []};
  await page.route(/\/api\/upload\//, async r => {
    const url = r.request().url();
    if (url.endsWith('/start')) {
      const body = JSON.parse(r.request().postData() || '{}');
      log.starts.push({chunks: body.chunks});
      return r.fulfill({json: {id: 'up' + log.starts.length, chunks: body.chunks}});
    }
    if (url.includes('/finish')) return r.fulfill({json: {path: '/uploads/clip.mov', url: '/api/uploads/clip.mov'}});
    const m = url.match(/\/api\/upload\/([^/]+)\/chunk\/(\d+)/);
    if (m) {
      log.chunks.push(m[1] + ':' + m[2]);
      if (await onChunk(m[1], Number(m[2]), log) === 'hold') return; // never answered: a stalled link
      return r.fulfill({json: {ok: true, chunk: Number(m[2])}});
    }
    return r.fallback();
  });
  return log;
}

test('progress is shown in bytes and moves chunk by chunk (1 MB pieces, not 5 MB)', async ({page}) => {
  let release!: () => void;
  const gate = new Promise<void>(r => release = r);
  const log = await routeUploads(page, async (_id, n) => { if (n === 1) await gate; return 'ok'; });
  await setup(page);
  await page.locator('#peek-file-input').setInputFiles(blob(3 * MB));
  // The server is told 3 chunks for 3 MB: the first upload starts at 1 MB pieces.
  await expect.poll(() => log.starts[0]?.chunks, {timeout: 30000}).toBe(3);
  // Chunk 1 is held: the chip already shows a third done, in bytes.
  await expect(chip(page)).toContainText(/Uploading 33% · 1\.0 \/ 3\.0 MB/);
  release();
  await expect(chip(page)).toContainText('✓');
  expect(log.chunks).toEqual(['up1:0', 'up1:1', 'up1:2']);
});

test('a first chunk that stalls restarts with smaller pieces instead of failing at 0%', async ({page}) => {
  const diag: any[] = [];
  await page.route('**/api/client-debug', r => { try { diag.push(JSON.parse(r.request().postData() || '{}')); } catch {} return r.fulfill({json: {ok: true}}); });
  const log = await routeUploads(page, async (id) => id === 'up1' ? 'hold' : 'ok');
  await setup(page);
  await page.clock.install();
  await page.locator('#peek-file-input').setInputFiles(blob(2 * MB));
  await expect.poll(() => log.chunks.length, {timeout: 30000}).toBe(1);
  await page.clock.fastForward(20001);   // no progress for the stall window
  await page.clock.fastForward(1001);    // retry backoff
  await expect(chip(page)).toContainText('✓', {timeout: 15000});
  // Second upload ID, twice the chunk count: 512 KB pieces.
  expect(log.starts.map(s => s.chunks)).toEqual([2, 4]);
  expect(diag.some(d => d.action === 'chunk-size-reduced' && d.stalled === true && d.chunkSize === 1024 * 1024)).toBe(true);
});

test('a stall after acknowledged chunks resumes at the next chunk, never from 0', async ({page}) => {
  let stalls = 0;
  const log = await routeUploads(page, async (_id, n) => (n === 2 && stalls++ === 0) ? 'hold' : 'ok');
  await setup(page);
  await page.clock.install();
  await page.locator('#peek-file-input').setInputFiles(blob(4 * MB));
  await expect.poll(() => log.chunks.length, {timeout: 30000}).toBe(3);
  await page.clock.fastForward(20001);
  await page.clock.fastForward(1001);
  await expect(chip(page)).toContainText('✓', {timeout: 15000});
  expect(log.starts.length).toBe(1);                       // same upload ID
  expect(log.chunks).toEqual(['up1:0', 'up1:1', 'up1:2', 'up1:2', 'up1:3']);  // 0 and 1 never re-sent
});

test('a failed local save still uploads from the original file and says so', async ({page}) => {
  const diag: any[] = [];
  await page.route('**/api/client-debug', r => { try { diag.push(JSON.parse(r.request().postData() || '{}')); } catch {} return r.fulfill({json: {ok: true}}); });
  const log = await routeUploads(page, async () => 'ok');
  await setup(page);
  await page.evaluate(() => { (0, eval)('_idb').putUpload = () => Promise.reject(new Error('QuotaExceededError')); });
  await page.locator('#peek-file-input').setInputFiles(blob(2 * MB));
  await expect(chip(page)).toContainText('✓');
  expect(log.chunks).toEqual(['up1:0', 'up1:1']);
  expect(diag.some(d => d.action === 'cache-fallback' && d.noLocalCopy === true && d.reason === 'local_save_failed')).toBe(true);
});

// The server binds an interaction id to one method+path (409 on reuse). A
// restart under a new upload id must not reuse the old chunk ids: live on
// 2026-10-02 the phone's retry got 409 "Interaction could not be recorded".
test('a restarted upload gives every chunk an interaction id that names the new upload', async ({page}) => {
  const ids: string[] = [];
  let first = true;
  await page.route(/\/api\/upload\//, async r => {
    const url = r.request().url();
    if (url.endsWith('/start')) return r.fulfill({json: {id: first ? 'gone1' : 'fresh2'}});
    if (url.includes('/finish')) return r.fulfill({json: {path: '/uploads/clip.mov', url: '/api/uploads/clip.mov'}});
    ids.push(r.request().headers()['x-amux-interaction-id'] || '');
    if (first) { first = false; return r.fulfill({status: 404, json: {error: 'unknown upload'}}); }
    return r.fulfill({json: {ok: true}});
  });
  await setup(page);
  await page.locator('#peek-file-input').setInputFiles(blob(1 * MB));
  await expect(chip(page)).toContainText('✓', {timeout: 30000});
  expect(ids.length).toBe(2);
  expect(ids[0]).toContain('_gone1_chunk_0');
  expect(ids[1]).toContain('_fresh2_chunk_0');
  expect(new Set(ids).size).toBe(2);
});

// Finish is bound to its path the same way. Live 2026-10-22 on the Mac app: a
// finish answered 404, the restart under a new upload id reused "_finish_1",
// and every retry got 409 while the message carrying it sat unsent.
test('a restarted upload gives finish an interaction id that names the new upload', async ({page}) => {
  const finishes: string[] = [];
  let starts = 0;
  await page.route(/\/api\/upload\//, async r => {
    const url = r.request().url();
    if (url.endsWith('/start')) return r.fulfill({json: {id: ++starts === 1 ? 'gone1' : 'fresh2'}});
    if (url.includes('/finish')) {
      finishes.push(r.request().headers()['x-amux-interaction-id'] || '');
      if (finishes.length === 1) return r.fulfill({status: 404, json: {error: 'unknown upload'}});
      return r.fulfill({json: {path: '/uploads/clip.mov', url: '/api/uploads/clip.mov'}});
    }
    return r.fulfill({json: {ok: true}});
  });
  await setup(page);
  await page.locator('#peek-file-input').setInputFiles(blob(1 * MB));
  await expect(chip(page)).toContainText('✓', {timeout: 30000});
  expect(finishes.length).toBe(2);
  expect(finishes[0]).toContain('_gone1_finish_');
  expect(finishes[1]).toContain('_fresh2_finish_');
});
