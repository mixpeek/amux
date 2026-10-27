// Record tab (AMUX-4625): tap to record, every second saved on the device as it
// arrives, synced once to the configured folder when amux is reachable.
//
// The microphone is a WebAudio tone installed before the app loads. Chromium's
// --use-fake-device-for-media-stream never resolved getUserMedia in headless
// Chromium 151 on macOS (probe: TIMEOUT after 6 s), and a device that exists on
// one host and hangs on another is a flaky test, so the tone is the input on
// every host. Everything after getUserMedia is the shipped code: MediaRecorder,
// the IndexedDB chunk store, sync, recovery. Chromium only: the WebKit build
// Playwright ships cannot be relied on for MediaRecorder, so ios-safari skips
// these by name rather than reporting a missing encoder as a product failure.
import { test, expect, Page } from './fixtures';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

declare const _recorderLive: { id: string } | null;
declare function _recorderChunks(id: string): Promise<unknown[]>;
declare function _recorderGet(id: string): Promise<Record<string, unknown> | undefined>;
declare function switchView(view: string): void;
declare function _uiComponentCheck(root: Element | null): { measured: boolean; n_considered: number; issues: string[] };

test.use({ launchOptions: { args: ['--autoplay-policy=no-user-gesture-required'] } });
test.skip(({ browserName }) => browserName !== 'chromium', 'MediaRecorder is only dependable in the Chromium build');
test.beforeEach(async ({ context }) => {
  await context.addInitScript(() => {
    navigator.mediaDevices.getUserMedia = async () => {
      const ctx = new AudioContext();
      await ctx.resume().catch(() => {});
      const tone = ctx.createOscillator();
      const out = ctx.createMediaStreamDestination();
      tone.connect(out);
      tone.start();
      return out.stream;
    };
  });
});

const AUDIO = /\.(m4a|mp4|webm|ogg|aac|wav)$/;

// Every test points the server at its own temp folder FIRST: the e2e server
// runs with the real $HOME, so the default folder would be a real one.
// The page's own fetch carries the owner token; a direct API call must pass it
// itself, the way card-details.spec.ts does, or the server answers 401.
async function useTempFolder(page: Page): Promise<string> {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-e2e-recordings-'));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const saved = await page.request.post('/api/recordings/config', {
    headers: { Authorization: `Bearer ${token}` },
    data: { dir },
  });
  expect(saved.status(), await saved.text()).toBe(200);
  return dir;
}

async function openRecord(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._recorderInit === 'function');
  await expect(page.locator('#tab-record')).toHaveCount(1);
  // The strip can overflow on a phone, so navigate the way the button does.
  await page.evaluate(() => switchView('record'));
  await expect(page.locator('#record-view')).toBeVisible();
}

async function liveChunks(page: Page): Promise<number> {
  return page.evaluate(async () => (_recorderLive ? (await _recorderChunks(_recorderLive.id)).length : 0));
}

async function record(page: Page, seconds: number) {
  await page.locator('#recorder-btn').click();
  await expect(page.locator('#recorder-btn')).toHaveAttribute('aria-pressed', 'true');
  // Chunks reach IndexedDB WHILE recording, before any stop.
  await expect.poll(() => liveChunks(page), { timeout: 10_000 }).toBeGreaterThanOrEqual(Math.max(1, seconds - 1));
  await page.locator('#recorder-btn').click();
  await expect(page.locator('#recorder-btn')).toHaveAttribute('aria-pressed', 'false');
}

function audioFilesFor(dir: string, id: string): string[] {
  return fs.readdirSync(dir).filter((f) => f.includes(id) && AUDIO.test(f));
}

test('a recording is saved on the device while it records and syncs to the folder', async ({ page }) => {
  const dir = await useTempFolder(page);
  await openRecord(page);
  await record(page, 3);
  const row = page.locator('.recorder-item').first();
  await expect(row).toBeVisible();
  const id = (await row.getAttribute('data-rec-id'))!;
  expect(id).toMatch(/^r[a-z0-9]+$/);
  await expect(row).toHaveAttribute('data-sync-state', 'synced', { timeout: 30_000 });
  // The device copy is dropped only because the folder holds identical bytes.
  expect(await page.evaluate(async (rid) => (await _recorderChunks(rid)).length, id)).toBe(0);
  const meta = await page.evaluate(async (rid) => _recorderGet(rid), id);
  expect(String(meta!.upload_sha256)).toMatch(/^[0-9a-f]{64}$/);
  expect(audioFilesFor(dir, id)).toHaveLength(1);
});

test('a recording made offline stays on the device and syncs exactly once when amux is reachable', async ({ page, context }) => {
  const dir = await useTempFolder(page);
  await openRecord(page);
  await context.setOffline(true);
  await record(page, 3);
  const row = page.locator('.recorder-item').first();
  await expect(row).toHaveAttribute('data-sync-state', 'local');
  const id = (await row.getAttribute('data-rec-id'))!;
  expect(await page.evaluate(async (rid) => (await _recorderChunks(rid)).length, id)).toBeGreaterThan(0);
  expect(audioFilesFor(dir, id)).toHaveLength(0);
  await context.setOffline(false);
  await expect(row).toHaveAttribute('data-sync-state', 'synced', { timeout: 30_000 });
  // A later sync pass must not upload it again.
  await page.evaluate(() => (window as any)._recorderSync(true));
  expect(audioFilesFor(dir, id)).toHaveLength(1);
});

test('a reload mid-recording keeps what was captured and finishes it as a recovered recording', async ({ page }) => {
  await useTempFolder(page);
  await openRecord(page);
  await page.locator('#recorder-btn').click();
  await expect.poll(() => liveChunks(page), { timeout: 10_000 }).toBeGreaterThanOrEqual(2);
  const id = await page.evaluate(() => _recorderLive!.id);
  await page.reload();
  await page.waitForFunction(() => typeof (window as any)._recorderInit === 'function');
  await page.evaluate(() => switchView('record'));
  const row = page.locator(`.recorder-item[data-rec-id="${id}"]`);
  await expect(row).toContainText('recovered after a reload');
  const meta = await page.evaluate(async (rid) => _recorderGet(rid), id);
  expect(meta!.recovered).toBe(true);
  expect(Number(meta!.dur_ms)).toBeGreaterThan(0);
  await expect(row).toHaveAttribute('data-sync-state', 'synced', { timeout: 30_000 });
});

test('the Record tab meets the shared control contract', async ({ page }) => {
  await useTempFolder(page);
  await openRecord(page);
  await page.locator('.recorder-settings summary').click();
  const result = await page.evaluate(() => _uiComponentCheck(document.querySelector('#record-view')));
  expect(result.measured).toBe(true);
  expect(result.n_considered).toBeGreaterThan(0);
  expect(result.issues).toEqual([]);
  const btn = await page.locator('#recorder-btn').boundingBox();
  expect(btn!.width).toBeGreaterThanOrEqual(96);
  const width = page.viewportSize()!.width;
  const overflow = await page.locator('#record-view').evaluate((e) => e.scrollWidth - e.clientWidth);
  expect(overflow, `record view overflows a ${width}px screen`).toBeLessThanOrEqual(0);
});

// ── Streaming to a recording session (docs/design/recorder.md) ───────────────

function sidecarFor(dir: string, id: string): Record<string, any> {
  const f = fs.readdirSync(dir).find((n) => n.endsWith(`_${id}.json`));
  expect(f, `no sidecar for ${id} in ${dir}`).toBeTruthy();
  return JSON.parse(fs.readFileSync(path.join(dir, f!), 'utf8'));
}

test('seconds reach amux while recording, so stopping leaves only the last few to send', async ({ page }) => {
  const dir = await useTempFolder(page);
  await openRecord(page);
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  await page.locator('#recorder-btn').click();
  await expect.poll(() => liveChunks(page), { timeout: 10_000 }).toBeGreaterThanOrEqual(2);
  const id = await page.evaluate(() => _recorderLive!.id);
  // Before any stop, the server already holds seconds of this recording.
  await expect.poll(async () => {
    const r = await page.request.get(`/api/recordings/sessions/${id}`, { headers: { Authorization: `Bearer ${token}` } });
    return r.ok() ? Number((await r.json()).tracks?.mic?.received || 0) : 0;
  }, { timeout: 20_000 }).toBeGreaterThanOrEqual(2);
  await page.locator('#recorder-btn').click();
  const row = page.locator(`.recorder-item[data-rec-id="${id}"]`);
  await expect(row).toHaveAttribute('data-sync-state', 'synced', { timeout: 30_000 });
  const sc = sidecarFor(dir, id);
  expect(sc.session?.chunk_kind).toBe('bytes');
  expect(sc.partial).toBe(false);
  expect(sc.session.chunks).toBeGreaterThanOrEqual(2);
  // The device copy went only because the assembled file matches it.
  expect(await page.evaluate(async (rid) => (await _recorderChunks(rid)).length, id)).toBe(0);
  expect(fs.existsSync(path.join(dir, '.sessions', id)), 'the session is cleaned up once filed').toBe(false);
});

test('a second the device lost is filed as a declared gap, never closed up silently', async ({ page, context }) => {
  const dir = await useTempFolder(page);
  await openRecord(page);
  await context.setOffline(true);
  await record(page, 4);
  const row = page.locator('.recorder-item').first();
  const id = (await row.getAttribute('data-rec-id'))!;
  // Lose second 1 on the device, the way a failed IndexedDB write would.
  await page.evaluate((rid) => new Promise<void>((res, rej) => {
    const open = indexedDB.open('amux-recorder');
    open.onsuccess = () => {
      const tx = open.result.transaction('chunks', 'readwrite');
      tx.objectStore('chunks').delete([rid, 1]);
      tx.oncomplete = () => res();
      tx.onerror = () => rej(tx.error);
    };
    open.onerror = () => rej(open.error);
  }), id);
  await context.setOffline(false);
  await page.evaluate(() => (window as any)._recorderSync(true));
  await expect(row).toHaveAttribute('data-sync-state', 'synced', { timeout: 30_000 });
  const sc = sidecarFor(dir, id);
  expect(sc.partial).toBe(true);
  expect(sc.session.lost_chunks).toBe(1);
  expect(sc.session.gaps.map((g: any) => g.why)).toContain('lost_chunk');
});

test('a second window never takes over a recording another window is still making', async ({ page, context }) => {
  await useTempFolder(page);
  await openRecord(page);
  await page.locator('#recorder-btn').click();
  await expect.poll(() => liveChunks(page), { timeout: 10_000 }).toBeGreaterThanOrEqual(2);
  const id = await page.evaluate(() => _recorderLive!.id);
  const other = await context.newPage();
  await openRecord(other);
  await other.evaluate(() => (window as any)._recorderSync(true));
  // Still recording in the first window, untouched by the second's sync.
  expect((await other.evaluate(async (rid) => _recorderGet(rid), id))!.state).toBe('recording');
  await expect.poll(() => liveChunks(page), { timeout: 10_000 }).toBeGreaterThanOrEqual(4);
  // The first window closes mid-recording: once its lease lapses, the second adopts it.
  await page.close();
  await expect.poll(async () => {
    await other.evaluate(() => (window as any)._recorderSync(true));
    return (await other.evaluate(async (rid) => _recorderGet(rid), id))!.state;
  }, { timeout: 45_000, intervals: [3000] }).toBe('synced');
  expect((await other.evaluate(async (rid) => _recorderGet(rid), id))!.recovered).toBe(true);
});
