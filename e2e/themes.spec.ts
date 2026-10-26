import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';
import { readFileSync } from 'node:fs';

// Ethan, 2026-10-26: one design system with a theme engine. "changing a theme
// updates the entire UI immediately; users can create, upload, preview,
// activate, and export themes". These run the real dashboard against the e2e
// server and look at what a person sees: computed colors on every surface.

test.use({ colorScheme: 'dark' });
// Several tests here load the whole app two or three times.
test.describe.configure({ timeout: 90_000 });
// A theme activated here is saved to the server so a new device gets it. The
// e2e server is shared by every spec in a project, so clear it after each test:
// left set, it booted settings.spec's next fresh page in Atom One Light (CI
// ce3589473, settings_theme_toggle on ios-safari).
test.afterEach(async ({ page }) => {
  await page.evaluate(() => Promise.all(['ui_theme', 'ui_themes'].map(key => fetch('/api/prefs', {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ key, value: '' }) }))))
    .catch(() => {});
});

const VIEWS = ['sessions', 'board', 'tasks', 'groups', 'calendar', 'scheduler', 'files', 'scratchpad',
  'mdai', 'pages', 'email', 'connectors', 'vault', 'logs', 'messages'];

async function boot(page: Page, hash = '#view=sessions') {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/' + hash);
  await page.waitForFunction(() => typeof (window as any).themeActivate === 'function' && !!document.body.dataset.theme);
}
// Settings > Device holds Appearance, where the Theme picker is.
async function openAppearance(page: Page) {
  await page.locator('#settings-btn').click();
  await page.locator('.settings-tab-btn[data-stab="device"]').click();
  await expect(page.locator('#theme-select')).toBeVisible();
}
const bodyBg = (page: Page) => page.evaluate(() => getComputedStyle(document.body).backgroundColor);
const active = (page: Page) => page.evaluate(() => document.body.dataset.theme);

// Every visible element's own colors, by position in the DOM, so two themes
// can be compared element by element.
async function colors(page: Page) {
  return page.evaluate(() => {
    const out: [number, string, string, string, string][] = [];
    document.querySelectorAll('body *').forEach((el, i) => {
      const r = el.getBoundingClientRect();
      if (r.width < 2 || r.height < 2 || r.bottom < 0 || r.top > innerHeight || r.right < 0 || r.left > innerWidth) return;
      const c = getComputedStyle(el);
      if (c.visibility === 'hidden' || +c.opacity === 0) return;
      // Content that brings its own colors: images, media, embedded pages,
      // and the calendar's events (each in its calendar's own color).
      if (el.closest('img, video, canvas, iframe, .fc-event, .theme-swatches, .ui-guide-swatch i, .cal-dot, [data-content-color]')) return;
      const text = [...el.childNodes].some(n => n.nodeType === 3 && n.textContent!.trim());
      const bw = parseFloat(c.borderTopWidth) + parseFloat(c.borderLeftWidth);
      const name = el.tagName.toLowerCase() + (el.id ? '#' + el.id : '') + (typeof el.className === 'string' && el.className ? '.' + el.className.trim().split(/\s+/).slice(0, 2).join('.') : '');
      const clear = (v: string) => v === 'rgba(0, 0, 0, 0)' || v === 'transparent' ? '' : v;
      out.push([i, text ? c.color : '', clear(c.backgroundColor), bw ? clear(c.borderTopColor) : '', name]);
    });
    return out;
  });
}

// A probe theme sets EVERY token to a value no built-in uses. Anything still
// showing its default-theme color under it is not reading the tokens.
async function probe(page: Page, on: boolean) {
  await page.evaluate(on => {
    const el = document.getElementById('amux-theme-vars')!;
    if (!on) { (window as any).themeActivate((window as any).themeActiveId() || 'amux-dark', { local: true }); return; }
    const tokens: [string, string][] = (0, eval)('THEME_TOKENS').map((t: string[], i: number) => [t[1], '#' + (0x123456 + i * 0x0a1b2c).toString(16).slice(-6)]);
    el.textContent = 'html body,html body.light{' + tokens.map(([p, v]) => p + ':' + v).join(';') + '}';
  }, on);
}

test('every built-in theme is offered in Settings and applies to the whole UI at once', async ({ page }) => {
  await boot(page);
  const ids = await page.evaluate(() => (window as any).themeAll().map((t: any) => t.name));
  for (const name of ['GitHub Dark', 'GitHub Light', 'One Dark Pro', 'Dracula', 'Atom One Light', 'Ayu Dark', 'Ayu Mirage', 'Ayu Light'])
    expect(ids).toContain(name);
  await openAppearance(page);
  const sel = page.locator('#theme-select');
  await expect(sel.locator('option')).toHaveCount(ids.length);
  await sel.selectOption('dracula');
  await expect.poll(() => bodyBg(page)).toBe('rgb(40, 42, 54)');
  await expect(page.locator('body')).not.toHaveClass(/light/);
  await sel.selectOption('atom-one-light');
  await expect.poll(() => bodyBg(page)).toBe('rgb(250, 250, 250)');
  await expect(page.locator('body')).toHaveClass(/light/);
  // Kept across a reload, painted before the app script runs (no flash).
  await page.addInitScript(() => document.addEventListener('DOMContentLoaded', () => {
    (window as any).__firstBg = getComputedStyle(document.body).backgroundColor;
  }));
  await page.reload();
  await page.waitForFunction(() => !!document.body.dataset.theme);
  expect(await page.evaluate(() => (window as any).__firstBg)).toBe('rgb(250, 250, 250)');
  expect(await active(page)).toBe('atom-one-light');
});

test('a theme reaches every surface: no element keeps its old color under a probe theme', async ({ page }) => {
  test.setTimeout(240_000);
  await boot(page);
  // Colors that animate would be read mid-change; the theme is what is tested.
  await page.addStyleTag({ content: '*,*::before,*::after{transition:none!important;animation:none!important}' });
  const stuck: string[] = [];
  let colored = 0;
  const surfaces: [string, () => Promise<void>][] = [
    ...VIEWS.map(v => [v, async () => { await page.evaluate(v => (window as any).switchView(v), v); }] as [string, () => Promise<void>]),
    ['settings', async () => { await page.evaluate(() => (window as any).switchView('sessions')); await openAppearance(page); }],
    ['themes', async () => { await page.evaluate(() => (window as any).openThemes()); }],
    ['style guide', async () => { await page.evaluate(() => (window as any).openStyleGuide()); }],
  ];
  for (const [name, open] of surfaces) {
    await page.keyboard.press('Escape').catch(() => {});
    await page.evaluate(() => { (window as any).closeThemes?.(); (window as any).closeStyleGuide?.(); (window as any).closeSettings?.(); });
    await open();
    await page.waitForTimeout(700);
    const before = await colors(page);
    await probe(page, true);
    const after = new Map((await colors(page)).map(x => [x[0], x]));
    await probe(page, false);
    for (const x of before) {
      const y = after.get(x[0]);
      if (!y) continue;
      for (const k of [1, 2, 3] as const) {
        if (!x[k]) continue;
        colored++;
        if (x[k] === y[k]) stuck.push(`${name}: ${x[4]} ${['', 'color', 'background', 'border'][k]} ${x[k]}`);
      }
    }
  }
  expect(colored).toBeGreaterThan(1500);   // the probe really ran over a full UI
  expect([...new Set(stuck)], `${stuck.length} of ${colored} colors ignored the theme`).toEqual([]);
});

test('themes change colors only: the layout of every view is identical under each', async ({ page }) => {
  await boot(page);
  // Dialogs animate open; measure where things land, not where they pass.
  await page.addStyleTag({ content: '*,*::before,*::after{transition:none!important;animation:none!important}' });
  // Surfaces no other test writes to (the e2e server is shared by parallel
  // workers, so the board and the fleet change underneath a measurement).
  const surfaces: [string, () => Promise<void>][] = [
    ['files', async () => { await page.evaluate(() => (window as any).switchView('files')); }],
    ['settings', async () => { await openAppearance(page); }],
    ['style guide', async () => { await page.evaluate(() => { (window as any).closeSettings?.(); (window as any).openStyleGuide(); }); }],
  ];
  for (const [v, open] of surfaces) {
    await open();
    await page.waitForTimeout(1500);
    // The two Settings rows that NAME the theme ("Light mode", the picker's
    // label) change text with it; that is content, not layout.
    const boxes = async () => page.evaluate(() => [...document.querySelectorAll('body *')].slice(0, 1500).map(e => {
      if (e.closest('.settings-row')?.querySelector('#theme-select, #theme-label')) return 'names-the-theme';
      const r = e.getBoundingClientRect(); return [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)].join(',');
    }));
    await page.evaluate(() => (window as any).themeActivate('amux-dark', { local: true }));
    const a = await boxes();
    await page.waitForTimeout(300);
    // Live content (a ticking "3s ago") moves on its own; measure it out.
    const again = await boxes();
    const live = new Set(a.map((b, i) => b !== again[i] ? i : -1).filter(i => i >= 0));
    const steady = (list: string[]) => list.filter((_, i) => !live.has(i));
    expect(live.size, `${v}: most of the page holds still`).toBeLessThan(a.length / 20);
    for (const id of ['dracula', 'atom-one-light', 'ayu-mirage']) {
      await page.evaluate(id => (window as any).themeActivate(id, { local: true }), id);
      let got = await boxes();
      if (JSON.stringify(steady(got)) !== JSON.stringify(steady(a))) {
        // A relative time ("1m ago") can tick between samples. Re-measure the
        // default theme once; a change the theme made shows up both times.
        await page.evaluate(() => (window as any).themeActivate('amux-dark', { local: true }));
        const base = await boxes();
        base.forEach((b, i) => { if (b !== a[i]) live.add(i); });
        await page.evaluate(id => (window as any).themeActivate(id, { local: true }), id);
        got = await boxes();
        expect(steady(got), `${v} under ${id}`).toEqual(steady(base));
      }
    }
  }
});

test('preview shows a theme everywhere and closing the dialog puts yours back', async ({ page }) => {
  await boot(page);
  await page.evaluate(() => (window as any).themeActivate('amux-dark'));
  const dark = await bodyBg(page);
  await openAppearance(page);
  await page.locator('#themes-btn').click();
  const item = page.locator('.theme-item[data-theme-id="ayu-light"]');
  await item.getByRole('button', { name: 'Preview' }).click();
  await expect.poll(() => bodyBg(page)).toBe('rgb(252, 252, 252)');
  await expect(item.getByRole('button', { name: 'End preview' })).toBeVisible();
  expect(await page.evaluate(() => localStorage.getItem('amux_theme'))).toBe('amux-dark');
  await page.locator('#theme-dialog .modal-close').click();
  await expect.poll(() => bodyBg(page)).toBe(dark);
  expect(await active(page)).toBe('amux-dark');
});

test('create: edit a color with live preview, validation blocks a bad one, save and use', async ({ page }) => {
  await boot(page);
  await page.evaluate(() => (window as any).openThemes());
  await page.getByRole('button', { name: 'New theme' }).click();
  await page.locator('#theme-edit-name').fill('E2E Midnight');
  const hex = (k: string) => page.locator(`.theme-hex[data-k="${k}"]`);
  await hex('bg').fill('#101820');
  await expect.poll(() => bodyBg(page)).toBe('rgb(16, 24, 32)');   // previewed while editing
  // Text nearly the background color: refused, nothing can be saved.
  await hex('text').fill('#151d25');
  await expect(page.locator('.theme-error').first()).toContainText('contrast');
  await expect(page.locator('#theme-edit-use')).toBeDisabled();
  await hex('text').fill('#e8eef4');
  await expect(page.locator('.theme-error')).toHaveCount(0);
  await page.locator('#theme-edit-use').click();
  await expect(page.locator('#theme-status')).toContainText('E2E Midnight');
  expect(await active(page)).toBe('custom-e2e-midnight');
  // It persists, on this device and on the server for the next one.
  await page.reload();
  await page.waitForFunction(() => document.body.dataset.theme === 'custom-e2e-midnight');
  expect(await bodyBg(page)).toBe('rgb(16, 24, 32)');
  await expect.poll(async () => {
    const pref = await page.evaluate(async () => (await (await fetch('/api/prefs?key=ui_themes')).json()).value);
    return pref ? JSON.parse(pref).map((t: any) => t.name) : [];
  }).toContain('E2E Midnight');
  // Delete it: the app falls back to the default.
  await page.evaluate(() => (window as any).openThemes());
  await page.locator('.theme-item[data-theme-id="custom-e2e-midnight"]').getByRole('button', { name: 'Delete' }).click();
  // The app's own confirmation dialog (showConfirm): its Delete button.
  await page.locator('button[onclick="_modalClose(true)"]', { hasText: 'Delete' }).click();
  await expect(page.locator('.theme-item[data-theme-id="custom-e2e-midnight"]')).toHaveCount(0);
  expect(await active(page)).toBe('amux-dark');
});

test('export a theme as a file, then import it back (and refuse a broken file)', async ({ page }, info) => {
  await boot(page);
  await page.evaluate(() => (window as any).openThemes());
  const [download] = await Promise.all([
    page.waitForEvent('download'),
    page.locator('.theme-item[data-theme-id="one-dark-pro"]').getByRole('button', { name: 'Export' }).click(),
  ]);
  const file = info.outputPath('one-dark-pro.amux-theme.json');
  await download.saveAs(file);
  const doc = JSON.parse(readFileSync(file, 'utf8'));
  expect(doc).toMatchObject({ schema: 'amux-theme', version: 1, name: 'One Dark Pro', base: 'dark' });
  expect(Object.keys(doc.colors).length).toBeGreaterThan(20);

  // A broken file is refused with the reason, and nothing changes.
  await page.locator('#theme-import-file').setInputFiles({ name: 'bad.json', mimeType: 'application/json',
    buffer: Buffer.from(JSON.stringify({ ...doc, version: 9 })) });
  await expect(page.locator('#theme-status')).toContainText('Not imported');
  await expect(page.locator('#theme-editor')).toBeHidden();

  // The good one opens in the editor, previewed, to be checked before saving.
  await page.locator('#theme-import-file').setInputFiles({ name: 'mine.json', mimeType: 'application/json',
    buffer: Buffer.from(JSON.stringify({ ...doc, name: 'My One Dark' })) });
  await expect(page.locator('#theme-editor')).toBeVisible();
  await expect(page.locator('#theme-edit-name')).toHaveValue('My One Dark');
  await expect.poll(() => bodyBg(page)).toBe('rgb(40, 44, 52)');
  await page.locator('#theme-edit-save').click();
  await expect(page.locator('.theme-item[data-theme-id="custom-my-one-dark"]')).toBeVisible();
});

test('other open pages (split panes, other Mac app tabs) follow a theme change', async ({ page, context }) => {
  await boot(page);
  const other = await context.newPage();
  await other.goto('/#view=board');
  await other.waitForFunction(() => !!document.body.dataset.theme);
  await page.evaluate(() => (window as any).themeActivate('ayu-dark'));
  await expect.poll(() => other.evaluate(() => document.body.dataset.theme)).toBe('ayu-dark');
  expect(await other.evaluate(() => getComputedStyle(document.body).backgroundColor)).toBe('rgb(13, 16, 23)');
});
