// The Record tab's live feedback (owner, 2026-10-31: "put the audio wave thing
// to indicate sound recording. like a quick feedback thing ... i just did a
// recording and nothing happened"). The phone's recording had worked: the
// server filed it and transcribed it live. But the iOS app pushed its status
// only when a 10 s segment finished, so the page's clock and meter sat still.
//
// The app's recorder is a fake bridge here (window.webkit.messageHandlers
// .amuxRecorder) that answers start and stop once and then, like the shipped
// iOS app, pushes nothing until asked. The page's own recorder uses the
// WebAudio tone recorder.spec.ts uses as its microphone.
import { test, expect, Page } from './fixtures';

declare function switchView(view: string): void;

// The tone microphone needs audio to start without a gesture (the page recorder test).
test.use({ launchOptions: { args: ['--autoplay-policy=no-user-gesture-required'] } });

async function openRecord(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._recorderInit === 'function');
  await page.evaluate(() => switchView('record'));
  await expect(page.locator('#record-view')).toBeVisible();
}

function beacons(page: Page) {
  const seen: any[] = [];
  page.on('request', r => {
    if (!r.url().includes('/api/client-debug')) return;
    try { const b = r.postDataJSON(); if (b && b.kind === 'recorder') seen.push(b); } catch (e) {}
  });
  return seen;
}

// A fake app recorder. `answer: false` makes it never reply, as an app that dropped the message.
async function fakeApp(page: Page, answer = true) {
  await page.addInitScript((answer) => {
    const w = window as any;
    w.__ops = [];
    let st: any = { available: true, state: 'idle', spooled: 0 };
    const push = () => w._nativeRecorderStatus && w._nativeRecorderStatus(JSON.parse(JSON.stringify(st)));
    const handler = {
      postMessage(m: any) {
        w.__ops.push(m.op);
        if (!answer) return;
        if (m.op === 'start') st = { available: true, state: 'recording', id: 'nfake1', started_at: Date.now(), elapsed_ms: 0, tracks: ['mic'], segments: 0, pending_upload: 0, gaps: 0, levels: { mic: 0.1 }, spooled: 0 };
        if (m.op === 'stop') st = { available: true, state: 'idle', spooled: 0 };
        if (st.state === 'recording') { st.elapsed_ms = Date.now() - st.started_at; st.levels = { mic: 0.05 + Math.random() * 0.15 }; }
        setTimeout(push, 10);
      },
    };
    Object.defineProperty(w, 'webkit', { value: { messageHandlers: { amuxRecorder: handler } }, configurable: true });
  }, answer);
}

const statusOps = (page: Page) => page.evaluate(() => (window as any).__ops.filter((o: string) => o === 'status').length);
const bars = (page: Page) => page.evaluate(() => (window as any).eval('_recWave.levels.length'));
const inked = (page: Page) => page.evaluate(() => {
  const c = document.getElementById('recorder-wave') as HTMLCanvasElement;
  const d = c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data;
  let n = 0; for (let i = 3; i < d.length; i += 4) if (d[i]) n++;
  return n;
});

test('the app recorder: a waveform and a running clock while it records, with no pushes from the app', async ({ page }) => {
  const seen = beacons(page);
  await fakeApp(page);
  await openRecord(page);
  await page.locator('#recorder-btn').click();
  await expect(page.locator('#recorder-btn')).toHaveAttribute('aria-pressed', 'true');
  // The app said nothing after the start; the page asks, about 4 times a second.
  await expect.poll(() => statusOps(page), { timeout: 6000 }).toBeGreaterThanOrEqual(4);
  await expect.poll(() => bars(page)).toBeGreaterThanOrEqual(6);
  expect(await inked(page)).toBeGreaterThan(50);
  await expect(page.locator('#recorder-elapsed')).not.toHaveText('0:00', { timeout: 4000 });
  await page.screenshot({ path: test.info().outputPath('recorder-wave-native.png') });
  // The server hears that it started (the page asked the app for its state at load: idle).
  await expect.poll(() => seen.some(b => b.verdict === 'native_state' && b.from === 'idle' && b.to === 'recording' && b.id === 'nfake1')).toBe(true);
  // Hidden: the asking stops, nothing to draw for.
  await page.evaluate(() => { Object.defineProperty(document, 'hidden', { value: true, configurable: true }); document.dispatchEvent(new Event('visibilitychange')); });
  await page.waitForTimeout(400);
  const before = await statusOps(page);
  await page.waitForTimeout(1200);
  expect(await statusOps(page)).toBeLessThanOrEqual(before + 1);
  await page.evaluate(() => { Object.defineProperty(document, 'hidden', { value: false, configurable: true }); document.dispatchEvent(new Event('visibilitychange')); });
  await page.locator('#rec-stop').click();
  await expect(page.locator('#recorder-btn')).toHaveAttribute('aria-pressed', 'false');
  await expect.poll(() => bars(page)).toBe(0);
  const after = await statusOps(page);
  await page.waitForTimeout(800);
  expect(await statusOps(page)).toBe(after);
  await expect.poll(() => seen.some(b => b.verdict === 'native_state' && b.from === 'recording' && b.to === 'idle')).toBe(true);
});

test('an app that never answers the start is said out loud and reaches the server log', async ({ page }) => {
  const seen = beacons(page);
  await fakeApp(page, false);
  await openRecord(page);
  await page.locator('#recorder-btn').click();
  await expect(page.locator('#recorder-status')).toContainText('did not answer', { timeout: 7000 });
  await expect.poll(() => seen.some(b => b.verdict === 'native_no_status' && b.op === 'start')).toBe(true);
});

test.describe('the page recorder', () => {
  test.skip(({ browserName }) => browserName !== 'chromium', 'MediaRecorder is only dependable in the Chromium build');
  test('draws the waveform from the microphone and clears it on stop', async ({ page, context }) => {
    await context.addInitScript(() => {
      navigator.mediaDevices.getUserMedia = async () => {
        const ctx = new AudioContext();
        await ctx.resume().catch(() => {});
        const tone = ctx.createOscillator();
        const out = ctx.createMediaStreamDestination();
        tone.connect(out); tone.start();
        return out.stream;
      };
    });
    const seen = beacons(page);
    await openRecord(page);
    await page.locator('#recorder-btn').click();
    await expect(page.locator('#recorder-btn')).toHaveAttribute('aria-pressed', 'true');
    await expect.poll(() => bars(page)).toBeGreaterThanOrEqual(10);
    expect(await inked(page)).toBeGreaterThan(50);
    await expect(page.locator('#recorder-wave')).not.toHaveClass(/quiet/);
    // At most one level per 80 ms: about 12 a second, not one per frame.
    const n0 = await bars(page); await page.waitForTimeout(400);
    expect(await page.evaluate(() => (window as any).eval('_recWave.levels.length'))).toBeLessThanOrEqual(Math.min(56, n0 + 8));
    await page.screenshot({ path: test.info().outputPath('recorder-wave-page.png') });
    await page.locator('#recorder-btn').click();
    await expect(page.locator('#recorder-btn')).toHaveAttribute('aria-pressed', 'false');
    await expect.poll(() => bars(page)).toBe(0);
    expect(seen.some(b => b.verdict === 'started')).toBe(true);
    await expect.poll(() => seen.some(b => b.verdict === 'stopped')).toBe(true);
  });
});
