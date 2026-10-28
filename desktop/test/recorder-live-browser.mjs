// The browser half of recorder-live-e2e.py: the Record tab in Chromium, with
// a real speech file as the microphone (Chromium's fake capture plays the WAV
// through getUserMedia), against the harness's private amux.
//
//   node desktop/test/recorder-live-browser.mjs <url> <speech.wav> <seconds>
//
// Prints one JSON line: what the live panel showed, when, and the session id.
import { chromium } from 'playwright';

const [url, wav, secs] = process.argv.slice(2);
const browser = await chromium.launch({
  args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream', `--use-file-for-fake-audio-capture=${wav}%noloop`,
    '--ignore-certificate-errors', '--autoplay-policy=no-user-gesture-required'],
});
const out = { ok: false, first_line_s: null, lines: [], hotkey: null };
try {
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true, permissions: ['microphone'] });
  const page = await ctx.newPage();
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); });
  await page.goto(url + '/');
  await page.waitForFunction(() => typeof window._recorderToggle === 'function', null, { timeout: 30000 });
  await page.evaluate(() => window.switchView('record'));
  // Start with the hotkey, as a person would (Alt+Shift+R by default).
  await page.keyboard.press('Alt+Shift+R');
  await page.waitForFunction(() => !!window.eval('_recorderLive'), null, { timeout: 15000 });
  out.hotkey = 'started';
  const id = await page.evaluate(() => window.eval('_recorderLive').id);
  out.id = id;
  await page.locator('#recorder-notes').fill('Browser leg notes: pelican check. Send to @recorder-lead please.');
  const t0 = Date.now();
  while (Date.now() - t0 < Number(secs) * 1000) {
    const lines = await page.locator('#recorder-live-text .recorder-live-line').allTextContents();
    if (lines.length && out.first_line_s === null) out.first_line_s = Math.round((Date.now() - t0) / 100) / 10;
    out.lines = lines;
    await page.waitForTimeout(1000);
  }
  await page.keyboard.press('Alt+Shift+R');
  await page.waitForFunction(() => !window.eval('_recorderLive'), null, { timeout: 30000 });
  out.hotkey = 'started and stopped';
  // Give the page a moment to send the last seconds and the finish.
  await page.waitForTimeout(8000);
  out.ok = true;
} catch (e) {
  out.error = String(e && e.message || e);
} finally {
  await browser.close();
}
console.log(JSON.stringify(out));
