// Dictation in the Mac app (owner, 2026-11-16: "Improve the dictation flow
// test it e2e and clean it up and keep iterating until its as clean as
// wisperflow"). The app recognizes on device and talks to the page through
// webkit.messageHandlers.amuxDictation (desktop/amux-dictation.swift); here a
// stand-in records what the page asks and answers as the app does. The real
// app, end to end with on-device speech, is desktop/test/dictation-e2e.py.
import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';

test.describe.configure({ mode: 'serial' });

async function settle(page: Page, opts: { mic?: boolean } = {}): Promise<void> {
  await page.addInitScript(({ mic }) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    (window as any)._asked = [];
    (window as any).webkit = { messageHandlers: { amuxDictation: { postMessage: (m: any) => (window as any)._asked.push(m) } } };
    if (mic) {
      (window as any)._micAsked = 0;
      Object.defineProperty(MediaDevices.prototype, 'getUserMedia', { configurable: true, value: async () => {
        (window as any)._micAsked++;
        const ctx = new AudioContext(); const osc = ctx.createOscillator(); const dest = ctx.createMediaStreamDestination();
        osc.connect(dest); osc.start(); return dest.stream;
      } });
    }
  }, { mic: !!opts.mic });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._dictNativeEvent === 'function' && typeof (window as any).switchView === 'function');
  await page.evaluate(() => (window as any)._hkLoad());
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => { await bar.locator('button').last().click(); });
}

const asked = (page: Page) => page.evaluate(() => (window as any)._asked.map((m: any) => m.op));
const app = (page: Page, ev: object) => page.evaluate(e => (window as any)._dictNativeEvent(e), ev);

async function box(page: Page, value: string, caret: number) {
  await page.evaluate(([v, c]) => {
    const t = document.createElement('textarea');
    t.id = 'dict-box';
    t.style.cssText = 'position:fixed;top:120px;left:40px;width:300px;height:80px;z-index:99999';
    document.body.appendChild(t);
    t.value = v as string; t.focus(); t.setSelectionRange(c as number, c as number);
  }, [value, caret]);
  return page.locator('#dict-box');
}

test('holding 🌐 dictates into the box you are in, and letting go puts the words at the caret', async ({ page }) => {
  await settle(page);
  const input = await box(page, 'Notes:  end', 7);
  let posted: any = null;
  await page.route('**/api/dictate', async route => {
    posted = route.request().postDataJSON();
    await route.fulfill({ json: { id: 7, text: 'ship the mixpeek-research plan', engine: 'apple', words: 4 } });
  });

  expect(await page.evaluate(() => (window as any)._hkNativeKey('Globe Hold', { wispr: false }))).toBe('ran');
  const pop = page.locator('#dict-popup');
  await expect(pop).toHaveClass(/open/);
  await expect(pop).toHaveClass(/dict-ptt/);
  await expect(pop).toHaveClass(/dict-floating/);
  await expect(pop.locator('.dict-popup-btns')).toBeHidden();          // letting go ends it
  await expect(page.locator('#dict-popup-status')).toBeHidden();       // no words while it listens
  expect(await asked(page)).toEqual(['start']);

  // The app's levels move the bars, newest on the right.
  for (const v of [0.2, 0.9, 0.5]) await app(page, { type: 'level', v });
  const scales = await page.$$eval('#dict-wave .dict-bar', bs => bs.map(b => (b as HTMLElement).style.transform));
  expect(scales.slice(-3)).toEqual(['scaleY(0.2)', 'scaleY(0.9)', 'scaleY(0.5)']);

  expect(await page.evaluate(() => (window as any)._hkNativeKey('Globe Release'))).toBe('released');
  expect(await asked(page)).toEqual(['start', 'stop']);
  await expect(pop).toHaveClass(/dict-busy/);

  await app(page, { type: 'done', text: 'ship the mix peak research plan', audio: 'AAAA', mime: 'audio/mp4', ms: 2100, finish_ms: 90 });
  // The server's names pass wins; the text goes in at the caret.
  await expect(input).toHaveValue('Notes: ship the mixpeek-research plan end');
  expect(await input.evaluate((t: HTMLTextAreaElement) => t.selectionStart)).toBe('Notes: ship the mixpeek-research plan'.length);
  await expect(pop).not.toHaveClass(/open/);
  expect(posted).toMatchObject({ text: 'ship the mix peak research plan', engine: 'apple', audio: 'AAAA', mime: 'audio/mp4', dur_ms: 2100 });

  // A release with no held take is nothing.
  expect(await page.evaluate(() => (window as any)._hkNativeKey('Globe Release'))).toBe('release_ignored');
});

test('the text still goes in when the server cannot be reached, and a selection is replaced', async ({ page }) => {
  await settle(page);
  const input = await box(page, 'say WRONG here', 4);
  await input.evaluate((t: HTMLTextAreaElement) => t.setSelectionRange(4, 9));
  await page.route('**/api/dictate', route => route.fulfill({ status: 500, json: { error: 'down' } }));
  await page.evaluate(() => (window as any)._hkNativeKey('Globe Hold', {}));
  // Each take is started (after the saved settings are read) before the app answers it.
  const starts = async () => (await asked(page)).filter(op => op === 'start').length;
  await expect.poll(starts).toBe(1);
  await page.evaluate(() => (window as any)._hkNativeKey('Globe Release'));
  await app(page, { type: 'done', text: 'right', ms: 900, finish_ms: 40 });
  await expect(input).toHaveValue('say right here');

  // Words dictated before a word get a space after them.
  await input.evaluate((t: HTMLTextAreaElement) => { t.value = 'world'; t.setSelectionRange(0, 0); });
  await page.evaluate(() => (window as any)._hkNativeKey('Globe Hold', {}));
  await expect.poll(starts).toBe(2);
  await page.evaluate(() => (window as any)._hkNativeKey('Globe Release'));
  await app(page, { type: 'done', text: 'Hello', ms: 900, finish_ms: 40 });
  await expect(input).toHaveValue('Hello world');
});

test('Esc throws a take away, and the app is told', async ({ page }) => {
  await settle(page);
  const input = await box(page, 'x', 1);
  await page.evaluate(() => (window as any)._hkNativeKey('Globe Globe', {}));
  const pop = page.locator('#dict-popup');
  await expect(pop).toHaveClass(/open/);
  await expect(pop).not.toHaveClass(/dict-ptt/);
  await expect(pop.getByRole('button', { name: 'Done' })).toBeVisible();   // hands-free: ✕ and ✓
  await page.keyboard.press('Escape');
  await expect(pop).not.toHaveClass(/open/);
  expect(await asked(page)).toEqual(['start', 'cancel']);
  await expect(input).toHaveValue('x');
});

test('a take that fails says why in the pill, then the pill goes', async ({ page }) => {
  await settle(page);
  await box(page, '', 0);
  await page.evaluate(() => (window as any)._hkNativeKey('Globe Hold', {}));
  // The app answers a take it was asked to start: the page reads the saved
  // dictation settings first, so an error sent before that start was
  // overwritten by it and the pill never closed.
  await expect.poll(() => asked(page)).toContain('start');
  await app(page, { type: 'error', why: 'The microphone is off for amux.' });
  await expect(page.locator('#dict-popup-status')).toHaveText('The microphone is off for amux.');
  await expect(page.locator('#dict-popup')).not.toHaveClass(/open/, { timeout: 6000 });
});

test('with Wispr Flow running, holding 🌐 is left to it and amux says so once', async ({ page }) => {
  await settle(page);
  await box(page, '', 0);
  for (const _ of [1, 2]) expect(await page.evaluate(() => (window as any)._hkNativeKey('Globe Hold', { wispr: true }))).toBe('left_to_wispr');
  expect(await asked(page)).toEqual([]);
  await expect(page.locator('.toast', { hasText: 'Wispr Flow' })).toHaveCount(1);
  // 🌐🌐 still dictates with amux.
  expect(await page.evaluate(() => (window as any)._hkNativeKey('Globe Globe', { wispr: true }))).toBe('ran');
  await expect.poll(() => asked(page)).toEqual(['start']);
  await page.evaluate(() => (window as any)._dictClosePopup(true));
});

test('when the app cannot dictate on device, the page records the take itself', async ({ page }) => {
  test.skip(!(await page.evaluate(() => 'MediaRecorder' in window)), 'this browser has no MediaRecorder');
  await settle(page, { mic: true });
  await box(page, '', 0);
  await page.evaluate(() => (window as any)._hkNativeKey('Globe Globe', {}));
  await app(page, { type: 'unavailable', why: 'the on-device speech model is downloading' });
  await expect.poll(() => page.evaluate(() => (window as any)._micAsked)).toBe(1);
  await expect.poll(() => page.evaluate(() => (globalThis as any).eval('_dictRecording && !_dictNative'))).toBe(true);
  // And the next take does not ask the app again straight away.
  await page.evaluate(() => (window as any)._dictClosePopup(true));
  await page.evaluate(() => (window as any)._hkNativeKey('Globe Globe', {}));
  expect(await asked(page)).toEqual(['start']);
  await page.evaluate(() => (window as any)._dictClosePopup(true));
});

// Owner, 2026-11-17: "when I press the button that was used to invoke the
// dictation, that should close the dictation". The app sends a double tap as
// "Globe" then "Globe Globe": the tail of a stopping double tap starts nothing.
test('🌐 again finishes a take, a single tap included, and does not start another', async ({ page }) => {
  await settle(page);
  await box(page, 'x', 1);
  expect(await page.evaluate(() => (window as any)._hkNativeKey('Globe Globe', {}))).toBe('ran');
  expect(await asked(page)).toEqual(['start']);
  expect(await page.evaluate(() => (window as any)._hkNativeKey('Globe', {}))).toBe('stopped');
  expect(await page.evaluate(() => (window as any)._hkNativeKey('Globe Globe', {}))).toBe('stop_tap_tail');
  expect(await asked(page)).toEqual(['start', 'stop']);
});

test('a start the app refuses as busy clears the pill and says why', async ({ page }) => {
  await settle(page);
  await box(page, '', 0);
  await page.evaluate(() => (window as any)._hkNativeKey('Globe Globe', {}));
  await app(page, { type: 'busy' });
  await expect(page.locator('#dict-popup-status')).toHaveText('Dictation is already running in another tab');
  expect(await page.evaluate(() => (globalThis as any).eval('_dictRecording'))).toBe(false);
});

// Owner, 2026-11-17: "There should also only be one dictation running per amux
// client." Two pages of one browser: starting in the second stops the first.
test('starting a take in another tab stops the take running here', async ({ page, context }) => {
  await settle(page);
  await box(page, 'x', 1);
  await page.evaluate(() => (window as any)._hkNativeKey('Globe Globe', {}));
  expect(await asked(page)).toEqual(['start']);
  const other = await context.newPage();
  await settle(other);
  await box(other, 'y', 1);
  await other.evaluate(() => (window as any)._hkNativeKey('Globe Globe', {}));
  await expect.poll(() => asked(page)).toEqual(['start', 'stop']);
  expect(await asked(other)).toEqual(['start']);
  await other.close();
});
