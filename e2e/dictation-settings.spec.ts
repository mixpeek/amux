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

async function settle(page: Page, opts: { native?: boolean; hiddenUntilAllowed?: boolean } = {}): Promise<void> {
  await page.addInitScript(({ devices, native, hidden }) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    (window as any)._gum = [];
    (window as any)._asked = [];
    if (native) {
      (window as any).webkit = { messageHandlers: { amuxDictation: { postMessage: (m: any) => (window as any)._asked.push(m) } } };
    }
    // WebKit and Chrome hide names until the page has opened a microphone.
    Object.defineProperty(MediaDevices.prototype, 'enumerateDevices', { configurable: true, value: async () =>
      hidden && !(window as any)._gum.length ? devices.map((d: any) => ({ ...d, label: '' })) : devices });
    Object.defineProperty(MediaDevices.prototype, 'getUserMedia', { configurable: true, value: async (c: any) => {
      (window as any)._gum.push(c);
      const ctx = new AudioContext(); const osc = ctx.createOscillator(); const dest = ctx.createMediaStreamDestination();
      osc.connect(dest); osc.start(); return dest.stream;
    } });
  }, { devices: DEVICES, native: !!opts.native, hidden: !!opts.hiddenUntilAllowed });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any)._dictSettingsSave === 'function');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => { await bar.locator('button').last().click(); });
}

async function openSettings(page: Page) {
  // The settings are server-wide, so they live in the Dictation tab; a worker's
  // Dictation tab links there.
  await page.evaluate(() => { (window as any).openPeek('dict-worker'); (window as any).setPeekTab('dictation'); });
  await page.locator('#dict-open-settings').click();
  await expect(page.locator('#dictation-view')).toBeVisible();
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

// Owner, 2026-11-20: "dictation mic settings should be client specific not
// server saved". The microphone is this device's; context and sources stay on
// the server for every device.
test('the microphone is saved on this device, sources and context on the server', async ({ page }) => {
  await settle(page);
  await openSettings(page);
  const mic = page.locator('#dict-mic');
  await expect(mic.locator('option')).toHaveText(['Default microphone', 'MacBook Pro Microphone', 'Shure MV7']);

  await mic.selectOption('Shure MV7');
  await expect.poll(() => page.evaluate(() => localStorage.getItem('amux_dict_mic'))).toBe('Shure MV7');

  await page.locator('#dict-src-workers').uncheck();
  await page.locator('#dict-src-contacts').check();
  await page.locator('#dict-context').fill('I run Mixpeek and talk about TubeScience.');
  await page.locator('#dict-context-save').click();
  await expect.poll(async () => (await serverSettings(page)).settings).toEqual({
    mic: '', context: 'I run Mixpeek and talk about TubeScience.', workers: false, contacts: true });

  // This device after a reload: still its own microphone.
  await page.reload();
  await page.waitForFunction(() => typeof (window as any)._dictSettingsSave === 'function');
  await openSettings(page);
  await expect(page.locator('#dict-mic')).toHaveValue('Shure MV7');

  // Another device (nothing in its storage): its default microphone, and the
  // same context and sources from the server.
  await page.evaluate(() => localStorage.removeItem('amux_dict_mic'));
  await page.reload();
  await page.waitForFunction(() => typeof (window as any)._dictSettingsSave === 'function');
  await openSettings(page);
  await expect(page.locator('#dict-mic')).toHaveValue('');
  await expect(page.locator('#dict-src-workers')).not.toBeChecked();
  await expect(page.locator('#dict-src-contacts')).toBeChecked();
  await expect(page.locator('#dict-context')).toHaveValue('I run Mixpeek and talk about TubeScience.');
});

test('dictating in a browser opens the chosen microphone, and says so when it is not connected', async ({ page }) => {
  await settle(page);
  await page.evaluate(() => (window as any)._dictSetMic('Shure MV7'));
  await page.evaluate(() => (window as any)._dictStartRec());
  await expect.poll(() => page.evaluate(() => (window as any)._gum.length)).toBe(1);
  const first = await page.evaluate(() => (window as any)._gum[0]);
  expect(first.audio.deviceId).toEqual({ exact: 'dev-shure' });
  await page.evaluate(() => (window as any)._dictClosePopup(true));
  await expect.poll(() => page.evaluate('_dictRecording')).toBe(false);

  // A saved microphone this device does not have: the default, with a note.
  await page.evaluate(() => (window as any)._dictSetMic('Rode NT-USB'));
  await page.evaluate(() => { const w = window as any; if (w._dictStream) { w._dictStream.getTracks().forEach((t: any) => t.stop()); w._dictStream = null; } });
  await page.evaluate(() => (window as any)._dictStartRec());
  await expect.poll(() => page.evaluate(() => (window as any)._gum.length)).toBe(2);
  const second = await page.evaluate(() => (window as any)._gum[1]);
  expect(second.audio.deviceId).toBeUndefined();
  await expect(page.locator('.toast, #toast').filter({ hasText: 'Rode NT-USB is not connected here' }).first()).toBeVisible();

  await openSettings(page);
  await expect(page.locator('#dict-mic option:checked')).toHaveText('Rode NT-USB (not connected here)');
});

// Owner, 2026-11-25: "ATR2500x-USB Microphone is not connected here" on every
// take in the Mac app, with the microphone connected. Before the page has opened
// a microphone it sees no names: the chosen one is opened once they show, with
// no note, and a Chrome-style label with its USB id is the same microphone.
test('a chosen microphone whose name is hidden until allowed is opened, not reported missing', async ({ page }) => {
  await settle(page, { hiddenUntilAllowed: true });
  await page.evaluate(() => (window as any)._dictSetMic('Shure MV7'));
  await page.evaluate(() => (window as any)._dictStartRec());
  await expect.poll(() => page.evaluate(() => (window as any)._gum.length)).toBe(2);
  const [first, second] = await page.evaluate(() => (window as any)._gum);
  expect(first.audio.deviceId).toBeUndefined();
  expect(second.audio.deviceId).toEqual({ exact: 'dev-shure' });
  await expect(page.locator('.toast, #toast').filter({ hasText: 'is not connected here' })).toHaveCount(0);
  expect(await page.evaluate(() => (window as any)._dictMicSame('Shure MV7 (0909:001a)', 'Shure MV7'))).toBe(true);
  expect(await page.evaluate(() => (window as any)._dictMicSame('MacBook Pro Microphone', 'Shure MV7'))).toBe(false);
  await page.evaluate(() => (window as any)._dictClosePopup(true));
});

test('the Mac app is asked for the chosen microphone, and worker names stay out when switched off', async ({ page }) => {
  await settle(page, { native: true });
  // Wait for the save: its "saved" toast landing after the notice below would
  // replace the notice's text in the one toast element.
  await page.evaluate(() => { (window as any)._dictSetMic('Shure MV7'); return (window as any)._dictSettingsSave({ workers: false }); });
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

  // The click starts the save; read the server until it has landed.
  const dictWord = () => page.evaluate(async () =>
    ((await (await fetch('/api/dictation/dict', { headers: (window as any)._authHeaders() })).json()) as any[])
      .find((w: any) => w.word === 'tube science'));
  await expect.poll(async () => (await dictWord())?.correct).toBe('TubeScience');
  const added = await dictWord();
  await page.evaluate(async (id) => {
    await fetch('/api/dictation/dict/' + id, { method: 'DELETE', headers: (window as any)._authHeaders() });
  }, added.id);
});
