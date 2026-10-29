// The Hotkeys tab (Ethan, 2026-10-26). Acceptance: typing ## in whatever input
// is active starts dictation for that input, and the transcript lands there.
// The microphone is a generated tone (no device in CI) and /api/dictate is
// answered by the test, because transcription needs a speech engine; the
// recorder, the trigger, the popup and the insertion are the real code.
import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';

// One server per project and one `hotkeys` pref on it: run in order.
test.describe.configure({ mode: 'serial' });

async function settle(page: Page, withMic = false): Promise<void> {
  if (withMic) {
    await page.addInitScript(() => {
      const fake = async () => {
        const ctx = new AudioContext();
        const osc = ctx.createOscillator();
        const dest = ctx.createMediaStreamDestination();
        osc.connect(dest);
        osc.start();
        return dest.stream;
      };
      // On the prototype: WebKit ignores an own-property override.
      Object.defineProperty(MediaDevices.prototype, 'getUserMedia', { configurable: true, value: fake });
    });
  }
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._hkEnter === 'function' && typeof (window as any).switchView === 'function');
  await page.evaluate(() => (window as any)._hkLoad());
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => {
    await bar.locator('button').last().click();
  });
}

test('## in any input starts dictation there, and the transcript lands in that input', async ({ page, browserName }) => {
  test.skip(!(await page.evaluate(() => 'MediaRecorder' in window)), 'this browser has no MediaRecorder');
  await settle(page, true);
  await page.evaluate("switchView('scheduler')");
  const input = page.locator('#sched-search');
  await input.click();
  await input.pressSequentially('note ##');
  // The trigger is removed and the recorder opens beside the input.
  await expect(input).toHaveValue('note ');
  const pop = page.locator('#dict-popup');
  await expect(pop).toHaveClass(/open/);
  await expect(pop).toHaveClass(/dict-floating/);
  await expect(pop).toBeVisible();
  if (browserName === 'webkit') {
    // Playwright's WebKit never delivers MediaRecorder data for a synthesized
    // WebAudio stream, so the round trip is proven on Chromium. Here: Cancel
    // leaves the input as it was and the recorder closes.
    await pop.getByRole('button', { name: 'Cancel' }).click();
    await expect(input).toHaveValue('note ');
  } else {
    await page.route('**/api/dictate?**', route => route.fulfill({ json: { text: 'hello world', id: 1 } }));
    await page.waitForTimeout(1500);
    await pop.getByRole('button', { name: 'Done' }).click();
    await expect(input).toHaveValue('note hello world', { timeout: 15000 });
  }
  await expect(pop).not.toHaveClass(/open/);
  // The popup went back to the composer it belongs to.
  expect(await page.evaluate(() => document.getElementById('dict-popup')!.parentElement!.closest('#peek-overlay, .peek-cmd-row, body > *') !== null)).toBe(true);
  expect(await page.evaluate(() => document.getElementById('dict-popup')!.parentElement === document.body)).toBe(false);
});

test('the tab lists hotkeys and built-ins, refuses collisions, and saves across a reload', async ({ page }) => {
  await settle(page);
  await page.evaluate("switchView('hotkeys')");
  const view = page.locator('#hotkeys-view');
  await expect(view).toBeVisible();
  const dictRow = view.locator('.hk-row[data-hk="hk-dictate-text"]');
  await expect(dictRow).toContainText('Start dictation');
  await expect(dictRow).toContainText('##');
  await expect(view).toContainText('Built into amux');
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(overflow, 'the tab fits the screen').toBeLessThanOrEqual(1);
  // One row of controls above the list: every control's centre sits in the bar.
  const bar = await page.locator('.hk-bar').boundingBox();
  expect(bar!.height).toBeLessThanOrEqual(56);
  for (const sel of ['#hk-pills', '#hk-search', '#hk-add']) {
    const b = await page.locator(sel).boundingBox();
    expect(b!.y + b!.height / 2).toBeGreaterThan(bar!.y);
    expect(b!.y + b!.height / 2).toBeLessThan(bar!.y + bar!.height);
  }

  // A text trigger already in use is refused.
  await page.locator('#hk-add').click();
  const dlg = page.locator('#hk-dialog');
  await expect(dlg).toBeVisible();
  await dlg.locator('label.hk-type', { hasText: 'Text trigger' }).click();
  await dlg.locator('#hk-trigger').fill('##');
  await dlg.locator('#hk-save').click();
  await expect(dlg.locator('#hk-trigger')).toHaveAttribute('aria-invalid', 'true');
  await expect(dlg.locator('#hk-trigger-help')).toContainText('Already used by Start dictation');

  // A shortcut amux already binds is refused; ⌘K / Ctrl+K is search.
  await dlg.locator('label.hk-type', { hasText: 'Keyboard shortcut' }).click();
  await dlg.locator('#hk-record').click();
  await page.keyboard.press('ControlOrMeta+k');
  await expect(dlg.locator('#hk-trigger')).toHaveValue('Mod+K');
  await dlg.locator('#hk-save').click();
  await expect(dlg.locator('#hk-trigger-help')).toContainText('amux already uses this for Search');

  // A new text trigger for search saves.
  await dlg.locator('#hk-action').selectOption('search');
  await dlg.locator('label.hk-type', { hasText: 'Text trigger' }).click();
  await dlg.locator('#hk-trigger').fill('@@');
  await dlg.locator('#hk-save').click();
  await expect(dlg).toBeHidden();
  await expect(view.locator('.hk-row', { hasText: '@@' })).toContainText('Search');

  // Turning ## off is saved too.
  await dictRow.getByRole('switch').click();
  await expect(dictRow.getByRole('switch')).toHaveAttribute('aria-checked', 'false');

  await page.reload();
  await page.waitForFunction(() => typeof (window as any)._hkEnter === 'function');
  await page.evaluate(() => (window as any)._hkLoad());
  await page.evaluate("switchView('hotkeys')");
  await expect(page.locator('.hk-row', { hasText: '@@' })).toContainText('Search');
  await expect(page.locator('.hk-row[data-hk="hk-dictate-text"]').getByRole('switch')).toHaveAttribute('aria-checked', 'false');

  // Off means off: ## stays as typed. The new @@ opens search.
  await page.evaluate("switchView('scheduler')");
  const input = page.locator('#sched-search');
  await input.click();
  await input.pressSequentially('a##');
  await expect(input).toHaveValue('a##');
  await expect(page.locator('#dict-popup')).not.toHaveClass(/open/);
  await input.pressSequentially('@@');
  await expect(page.locator('#cmdk-overlay')).toHaveClass(/active/);
  await expect(input).toHaveValue('a##');

  // Put the pref back for the other tests on this server.
  await page.evaluate(() => fetch('/api/prefs', { method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ key: 'hotkeys', value: '' }) }));
});

test('a keyboard shortcut runs its action from anywhere, including while typing', async ({ page }, info) => {
  test.skip(info.project.name !== 'desktop', 'a hardware keyboard shortcut; phones have no Alt key');
  await settle(page);
  await page.evaluate("switchView('scheduler')");
  await page.locator('#sched-search').click();
  await page.keyboard.press('Alt+Shift+KeyN');
  await expect(page.locator('#board-edit-overlay')).toHaveClass(/active/);
});

// Owner, 2026-10-29: "i want to use the globe button on my keyboard as a hotkey
// too". A page never sees 🌐; the Mac app calls _hkNativeKey with "Globe" or
// "Globe+<key>", the same call its key monitor makes (amux-workspace.swift
// startGlobeKey). Recording takes it, it saves, it shows as 🌐, and it runs.
test('the Globe key records, saves and runs a hotkey, alone or with a key', async ({ page }) => {
  await settle(page);
  await page.evaluate("switchView('hotkeys')");
  const dlg = page.locator('#hk-dialog');
  for (const [combo, action, opened] of [['Globe+B', 'open_board', 'board'], ['Globe', 'open_messages', 'messages']] as const) {
    await page.evaluate("switchView('hotkeys')");
    await page.locator('#hk-add').click();
    await expect(dlg).toBeVisible();
    await dlg.locator('label.hk-type', { hasText: 'Keyboard shortcut' }).click();
    await dlg.locator('#hk-action').selectOption(action);
    await dlg.locator('#hk-record').click();
    expect(await page.evaluate(c => (window as any)._hkNativeKey(c), combo)).toBe('recorded');
    await expect(dlg.locator('#hk-trigger')).toHaveValue(combo);
    await dlg.locator('#hk-save').click();
    await expect(dlg).toBeHidden();
    await expect(page.locator('#hotkeys-view .hk-row', { hasText: '🌐' }).first()).toBeVisible();
    await page.evaluate("switchView('scheduler')");
    expect(await page.evaluate(c => (window as any)._hkNativeKey(c), combo)).toBe('ran');
    await expect.poll(() => page.evaluate(() => (globalThis as any).eval('activeView'))).toBe(opened);
  }
  // A Globe combination nobody bound is said, not silently dropped.
  expect(await page.evaluate(() => (window as any)._hkNativeKey('Globe+Q'))).toBe('unbound');
  await page.evaluate(() => fetch('/api/prefs', { method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ key: 'hotkeys', value: '' }) }));
});
