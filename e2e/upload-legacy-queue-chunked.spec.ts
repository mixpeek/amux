// A file queued offline before the IDB chunk store existed (the `upload_queue`
// key, the whole file as one Blob) is delivered on reconnect through the
// chunked /api/upload, the one upload path (primitive integrity audit
// 2026-11-26, P1). Before, that replay was the dashboard's last caller of the
// single multipart POST to /api/fs/upload.
import {test, expect} from './fixtures';
import {cleanup} from './teardown';
import {mkdtemp, readFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
declare const _idb: any;

test('a pre-upgrade queued file uploads through the chunked path on reconnect', async ({page, context}, info) => {
  const dir = await mkdtemp(path.join(tmpdir(), 'amux-legacy-queue-'));
  const seen: string[] = [];
  page.on('request', r => { const u = new URL(r.url()); if (/^\/api\/(upload|fs\/upload)/.test(u.pathname)) seen.push(r.method() + ' ' + u.pathname); });
  try {
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any)._upqList === 'function');
    // The row shape the old queue stored: bytes as an ArrayBuffer under upload_queue.
    await page.evaluate(async (d) => {
      const bytes = new TextEncoder().encode('legacy queued bytes\n').buffer;
      await _idb.update('upload_queue', () => [{id: 'legacy-row-1', dir: d, name: 'queued note.txt', mime: 'text/plain', bytes, ts: Date.now()}]);
    }, dir);
    await page.reload();
    await expect.poll(() => page.evaluate(async () => (await (window as any)._upqList()).length)).toBe(1);
    // A real reconnect: the browser goes offline and comes back, which fires
    // the page's `online` handler and drains the queues.
    await context.setOffline(true);
    await context.setOffline(false);
    const dest = path.join(dir, 'queued note.txt');
    await expect.poll(async () => (await readFile(dest, 'utf8').catch(() => '')), {timeout: 30000}).toBe('legacy queued bytes\n');
    await expect.poll(() => page.evaluate(async () => (await (window as any)._upqList()).length)).toBe(0);
    expect(seen.filter(s => s.endsWith('/api/fs/upload')), JSON.stringify(seen)).toEqual([]);
    expect(seen.some(s => s === 'POST /api/upload/start'), JSON.stringify(seen)).toBe(true);
    expect(seen.some(s => /^PUT \/api\/upload\/[^/]+\/chunk\/0$/.test(s)), JSON.stringify(seen)).toBe(true);
    expect(seen.some(s => /^POST \/api\/upload\/[^/]+\/finish$/.test(s)), JSON.stringify(seen)).toBe(true);
    await page.screenshot({path: info.outputPath('legacy-queue-delivered.png')});
  } finally {
    await cleanup('remove temp folder', () => rm(dir, {recursive: true, force: true}));
  }
});
