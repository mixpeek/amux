// Dictation settings, kept on the server (owner, 2026-11-17: "make it so that I can configure
// the microphone for dictation ... dictionaries and other stuff like the sources ... they
// should be saved on server", and "looking at past dictations" to improve it).
// The page lists microphones from a stand-in device list, saves through the real
// /api/dictation/settings, and dictates with the chosen one; Teach turns a word in a past
// dictation into a dictionary correction on the real server.
import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';

test.describe.configure({ mode: 'serial' });
// The settings are one server-wide value, and projects run at once against one server:
// a second project would change them under the first. One project covers the page.
test.beforeEach(({}, info) => { test.skip(info.project.name !== 'desktop', 'server-wide setting; covered by desktop'); });

const DEVICES = [
  { kind: 'audioinput', deviceId: 'default', label: 'Default - MacBook Pro Microphone', groupId: 'g0' },
  { kind: 'audioinput', deviceId: 'dev-builtin', label: 'MacBook Pro Microphone', groupId: 'g1' },
  { kind: 'audioinput', deviceId: 'dev-shure', label: 'Shure MV7', groupId: 'g2' },
];

async function settle(page: Page, opts: { native?: boolean } = {}): Promise<void> {
  await page.addInitScript(({ devices, native }) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    (window as any)._gum = [];
    (window as any)._asked = [];
    if (native) {
      (window as any).webkit = { messageHandlers: { amuxDictation: { postMessage: (m: any) => (window as any)._asked.push(m) } } };
    }
    Object.defineProperty(MediaDevices.prototype, 'enumerateDevices', { configurable: true, value: async () => devices });
    Object.defineProperty(MediaDevices.prototype, 'getUserMedia', { configurable: true, value: async (c: any) => {
      (window as any)._gum.push(c);
      const ctx = new AudioContext(); const osc = ctx.createOscillator(); const dest = ctx.createMediaStreamDestination();
      osc.connect(dest); osc.start(); return dest.stream;
    } });
  }, { devices: DEVICES, native: !!opts.native });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any)._dictSettingsSave === 'function');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => { await bar.locator('button').last().click(); });
}

async function openSettings(page: Page) {
  // The settings are server-wide, so they live in Settings > Dictation; a worker's
  // Dictation tab links there.
  await page.evaluate(() => { (window as any).openPeek('dict-worker'); (window as any).setPeekTab('dictation'); });
  await page.locator('#dict-open-settings').click();
  await expect(page.locator('#stab-dictation')).toBeVisible();
  await expect(page.locator('#dict-mic')).toBeEnabled();
}

const serverSettings = (page: Page) => page.evaluate(async () =>
  (await fetch('/api/dictation/settings', { headers: (window as any)._authHeaders() })).json());

async function reset(page: Page) {
  await page.evaluate(async () => {
    await fetch('/api/dictation/settings', { method: 'PUT', headers: (window as any)._authHeaders({ 'Content-Type': 'application/json' }),
      body: JSON.stringify({ mic: '', context: '', workers: true, contacts: false }) });
  });
}

test.afterEach(async ({ page }, info) => { if (info.project.name === 'desktop') await reset(page); });

test('the microphone, sources and context are saved on the server and shown back', async ({ page }) => {
  await settle(page);
  await openSettings(page);
  const mic = page.locator('#dict-mic');
  await expect(mic.locator('option')).toHaveText(['Default microphone', 'MacBook Pro Microphone', 'Shure MV7']);

  await mic.selectOption('Shure MV7');
  await expect.poll(async () => (await serverSettings(page)).settings.mic).toBe('Shure MV7');

  await page.locator('#dict-src-workers').uncheck();
  await page.locator('#dict-src-contacts').check();
  await page.locator('#dict-context').fill('I run Mixpeek and talk about TubeScience.');
  await page.locator('#dict-context-save').click();
  await expect.poll(async () => (await serverSettings(page)).settings).toEqual({
    mic: 'Shure MV7', context: 'I run Mixpeek and talk about TubeScience.', workers: false, contacts: true });

  // Another device: a fresh page reads the same settings from the server.
  await page.reload();
  await page.waitForFunction(() => typeof (window as any)._dictSettingsSave === 'function');
  await openSettings(page);
  await expect(page.locator('#dict-mic')).toHaveValue('Shure MV7');
  await expect(page.locator('#dict-src-workers')).not.toBeChecked();
  await expect(page.locator('#dict-src-contacts')).toBeChecked();
  await expect(page.locator('#dict-context')).toHaveValue('I run Mixpeek and talk about TubeScience.');
});

test('dictating in a browser opens the chosen microphone, and says so when it is not connected', async ({ page }) => {
  await settle(page);
  await page.evaluate(() => (window as any)._dictSettingsSave({ mic: 'Shure MV7' }));
  await page.evaluate(() => (window as any)._dictStartRec());
  await expect.poll(() => page.evaluate(() => (window as any)._gum.length)).toBe(1);
  const first = await page.evaluate(() => (window as any)._gum[0]);
  expect(first.audio.deviceId).toEqual({ exact: 'dev-shure' });
  await page.evaluate(() => (window as any)._dictClosePopup(true));
  await expect.poll(() => page.evaluate('_dictRecording')).toBe(false);

  // A saved microphone this device does not have: the default, with a note.
  await page.evaluate(() => (window as any)._dictSettingsSave({ mic: 'Rode NT-USB' }));
  await page.evaluate(() => { const w = window as any; if (w._dictStream) { w._dictStream.getTracks().forEach((t: any) => t.stop()); w._dictStream = null; } });
  await page.evaluate(() => (window as any)._dictStartRec());
  await expect.poll(() => page.evaluate(() => (window as any)._gum.length)).toBe(2);
  const second = await page.evaluate(() => (window as any)._gum[1]);
  expect(second.audio.deviceId).toBeUndefined();
  await expect(page.locator('.toast, #toast').filter({ hasText: 'Rode NT-USB is not connected here' }).first()).toBeVisible();

  await openSettings(page);
  await expect(page.locator('#dict-mic option:checked')).toHaveText('Rode NT-USB (not connected here)');
});

test('the Mac app is asked for the chosen microphone, and worker names stay out when switched off', async ({ page }) => {
  await settle(page, { native: true });
  await page.evaluate(() => (window as any)._dictSettingsSave({ mic: 'Shure MV7', workers: false }));
  await page.evaluate(() => (window as any)._dictStartRec());
  await expect.poll(() => page.evaluate(() => (window as any)._asked.length)).toBeGreaterThan(0);
  const start = await page.evaluate(() => (window as any)._asked.find((m: any) => m.op === 'start'));
  expect(start.input).toBe('Shure MV7');
  const names = await page.evaluate(() => ((window as any).sessions || []).map((s: any) => s.name));
  for (const n of names) expect(start.hints).not.toContain(n);
  // The app could not open it and fell back: the person is told.
  await page.evaluate(() => (window as any)._dictNativeEvent({ type: 'notice', why: 'Shure MV7 is not connected; using the default microphone.' }));
  await expect(page.locator('.toast, #toast').filter({ hasText: 'Shure MV7 is not connected' }).first()).toBeVisible();
  await page.evaluate(() => (window as any)._dictNativeEvent({ type: 'error', why: 'cancelled by test' }));
});

test('Teach turns a word in a past dictation into a dictionary correction on the server', async ({ page }) => {
  await settle(page);
  await page.route('**/api/dictation/history*', route => route.fulfill({ json: {
    items: [{ id: 901, ts: Date.now(), text: 'ask tube science for the report', raw_text: 'ask tube science for the report', words: 6 }],
    total_words: 6 } }));
  await page.evaluate(() => { (window as any).openPeek('dict-worker'); (window as any).setPeekTab('dictation'); });
  const row = page.locator('#dict-row-901');
  await expect(row).toContainText('tube science');

  // Select the misheard words in the row, then Teach.
  await page.evaluate(() => {
    const t = document.querySelector('#dict-row-901 .dict-text')!.firstChild!;
    const at = t.textContent!.indexOf('tube science');
    const r = document.createRange(); r.setStart(t, at); r.setEnd(t, at + 'tube science'.length);
    const sel = window.getSelection()!; sel.removeAllRanges(); sel.addRange(r);
  });
  await row.locator('button[title^="Teach"]').click();
  await expect(page.locator('#dictm-mis')).toHaveValue('tube science');
  await page.locator('#dictm-cor').fill('TubeScience');
  await page.getByRole('button', { name: 'Add word' }).click();

  const words = await page.evaluate(async () =>
    (await fetch('/api/dictation/dict', { headers: (window as any)._authHeaders() })).json());
  const added = words.find((w: any) => w.word === 'tube science');
  expect(added?.correct).toBe('TubeScience');
  await page.evaluate(async (id) => {
    await fetch('/api/dictation/dict/' + id, { method: 'DELETE', headers: (window as any)._authHeaders() });
  }, added.id);
});
