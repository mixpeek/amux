// Teleprompter selfie recording (AA-39): the front camera runs behind the
// script, the take records while the text scrolls, and in the iOS app the
// bytes go to Photos through the amuxMedia bridge.
//
// The camera is a canvas stream plus a WebAudio tone, installed before the app
// loads, for the reason recorder.spec.ts gives: Chromium's fake media device
// does not resolve getUserMedia headless on every host. The amuxMedia bridge is
// a stand-in for the native handler with the same reply contract (each
// postMessage returns a promise), so the page's chunking and reassembly are
// the shipped code. Chromium only, like recorder.spec.ts.
import { test, expect, Page } from './fixtures';

declare let _fileData: { path: string; content: string } | null;
declare const _tp: { running: boolean };
declare function _filesOpenTeleprompter(): void;

test.use({ launchOptions: { args: ['--autoplay-policy=no-user-gesture-required'] } });
test.skip(({ browserName }) => browserName !== 'chromium', 'MediaRecorder is only dependable in the Chromium build');

test.beforeEach(async ({ context }) => {
  await context.addInitScript(() => {
    const w = window as any;
    w.__tpStreams = [];
    navigator.mediaDevices.getUserMedia = async () => {
      const canvas = document.createElement('canvas');
      canvas.width = 320; canvas.height = 240;
      const g = canvas.getContext('2d')!;
      let n = 0;
      setInterval(() => { g.fillStyle = `hsl(${(n += 7) % 360},80%,50%)`; g.fillRect(0, 0, 320, 240); }, 33);
      const video = (canvas as any).captureStream(30) as MediaStream;
      const ctx = new AudioContext();
      await ctx.resume().catch(() => {});
      const tone = ctx.createOscillator();
      const out = ctx.createMediaStreamDestination();
      tone.connect(out); tone.start();
      const stream = new MediaStream([...video.getVideoTracks(), ...out.stream.getAudioTracks()]);
      w.__tpStreams.push(stream);
      return stream;
    };
    // Stand-in for MediaBridge.swift: same ops, same promise replies.
    const files: Record<string, string[]> = {};
    w.__tpBridge = { saved: [] as { name: string; bytes: number; head: string }[], refuseSave: false };
    w.webkit = { messageHandlers: { amuxMedia: { postMessage: async (m: any) => {
      if (m.op === 'begin') { const id = 'take-' + Object.keys(files).length; files[id] = []; return id; }
      if (m.op === 'chunk') { files[m.id].push(m.data); return atob(m.data).length; }
      if (m.op === 'abort') { delete files[m.id]; return true; }
      if (m.op === 'save') {
        if (w.__tpBridge.refuseSave) throw new Error('Photos access is off for amux');
        const bin = files[m.id].map((s) => atob(s)).join('');
        const head = Array.from(bin.slice(0, 8), (c) => c.charCodeAt(0).toString(16).padStart(2, '0')).join('');
        w.__tpBridge.saved.push({ name: m.id, bytes: bin.length, head });
        delete files[m.id];
        return true;
      }
      throw new Error('Unknown op ' + m.op);
    } } } };
  });
});

async function openTeleprompter(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._tpRecordToggle === 'function');
  await page.evaluate(() => {
    _fileData = { path: 'scripts/intro.md', content: '# Intro\n\nHello, this is the take.\n\n' + 'More lines.\n\n'.repeat(40) } as any;
    _filesOpenTeleprompter();
  });
  await expect(page.locator('#teleprompter-overlay')).toBeVisible();
}

const saved = (page: Page) => page.evaluate(() => (window as any).__tpBridge.saved as { bytes: number; head: string }[]);

test('a take records over the running script and its bytes reach Photos', async ({ page }) => {
  await openTeleprompter(page);
  const rec = page.locator('#tp-rec-btn');
  await rec.click();
  await expect(rec).toContainText('Stop');
  await expect(page.locator('#tp-camera')).toBeVisible();
  await expect(page.locator('#tp-rec-badge')).toBeVisible();
  expect(await page.evaluate(() => _tp.running)).toBe(true);
  await expect(page.locator('#tp-rec-time')).toHaveText(/0:0[2-9]/, { timeout: 10_000 });

  await rec.click();
  await expect.poll(async () => (await saved(page)).length, { timeout: 15_000 }).toBe(1);
  const [take] = await saved(page);
  expect(take.bytes).toBeGreaterThan(1000);
  // The bytes are a real container, reassembled in order from the slices:
  // WebM opens with the EBML magic 1a45dfa3, MP4 with an ftyp box (66747970
  // at offset 4). Which one depends on the Chromium build's encoders.
  expect(take.head.startsWith('1a45dfa3') || take.head.slice(8) === '66747970', take.head).toBe(true);
  await expect(rec).toContainText('Rec');
  await expect(page.locator('#tp-rec-badge')).toBeHidden();
  await expect(page.locator('#tp-save-btn')).toBeHidden();
  expect(await page.evaluate(() => _tp.running)).toBe(false);
  // The camera stays on for the next take until it is turned off or closed.
  await expect(page.locator('#tp-camera')).toBeVisible();
});

test('a take Photos refuses is kept and saves on retry', async ({ page }) => {
  await openTeleprompter(page);
  await page.evaluate(() => { (window as any).__tpBridge.refuseSave = true; });
  const rec = page.locator('#tp-rec-btn');
  await rec.click();
  await expect(page.locator('#tp-rec-time')).toHaveText(/0:0[1-9]/, { timeout: 10_000 });
  await rec.click();
  const retry = page.locator('#tp-save-btn');
  await expect(retry).toBeVisible({ timeout: 15_000 });
  expect(await saved(page)).toHaveLength(0);

  await page.evaluate(() => { (window as any).__tpBridge.refuseSave = false; });
  await retry.click();
  await expect.poll(async () => (await saved(page)).length, { timeout: 15_000 }).toBe(1);
  await expect(retry).toBeHidden();
});

test('closing mid-take saves the take and releases the camera', async ({ page }) => {
  await openTeleprompter(page);
  await page.locator('#tp-rec-btn').click();
  await expect(page.locator('#tp-rec-time')).toHaveText(/0:0[1-9]/, { timeout: 10_000 });
  await page.evaluate(() => (window as any)._tpClose());
  await expect(page.locator('#teleprompter-overlay')).toBeHidden();
  await expect.poll(async () => (await saved(page)).length, { timeout: 15_000 }).toBe(1);
  const live = await page.evaluate(() =>
    ((window as any).__tpStreams as MediaStream[]).flatMap((s) => s.getTracks()).filter((t) => t.readyState === 'live').length);
  expect(live).toBe(0);
});

test('the camera preview toggles without recording', async ({ page }) => {
  await openTeleprompter(page);
  const cam = page.locator('#tp-cam-btn');
  await cam.click();
  await expect(page.locator('#tp-camera')).toBeVisible();
  await expect(page.locator('#tp-rec-badge')).toBeHidden();
  await cam.click();
  await expect(page.locator('#tp-camera')).toBeHidden();
  expect(await saved(page)).toHaveLength(0);
});
