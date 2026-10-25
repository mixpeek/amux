// File-list multi-select (Ethan, 2026-10-15): Finder/Drive gestures in the
// shared row builder, a bar of commands, and Delete acting on exactly the
// selection. Real files in a temp dir; the Files tab drives it.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';
import { mkdtemp, writeFile, mkdir, readdir, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

test('click, Cmd-click, Shift-click and Cmd+A select; Delete removes exactly the selection', async ({ page, browserName }, info) => {
  test.skip(test.info().project.name !== 'desktop', 'pointer gestures; touch is the long-press path');
  const dir = await mkdtemp(join(tmpdir(), 'amux-fsel-'));
  try {
    await mkdir(join(dir, 'sub'));
    for (const f of ['a', 'b', 'c', 'd', 'e']) await writeFile(join(dir, f + '.txt'), f);
    page.on('dialog', d => d.accept());
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any)._fselClick === 'function');
    await page.evaluate(() => (window as any).switchView('files'));
    await page.evaluate((d) => (window as any).loadFiles(d), dir);
    const row = (n: string) => page.locator(`#files-body [data-path$="/${n}"]`);
    await row('a.txt').waitFor();
    // _fsel is a top-level const in app.js, not a window property.
    const sel = () => page.evaluate(() => [...(globalThis as any).eval('_fsel').paths].map((p: string) => p.split('/').pop()).sort());
    const bar = page.locator('#fsel-bar');
    const mod = browserName === 'webkit' ? 'Meta' : 'ControlOrMeta';

    await row('a.txt').click();
    expect(await sel()).toEqual(['a.txt']);
    await expect(bar).toHaveCount(0);
    await row('c.txt').click({ modifiers: [mod as any] });
    await expect(bar.locator('.fsel-count')).toHaveText('2 selected');
    await row('e.txt').click({ modifiers: ['Shift'] });
    expect(await sel()).toEqual(['c.txt', 'd.txt', 'e.txt']);
    await page.keyboard.press('Escape');
    expect(await sel()).toEqual([]);
    await row('a.txt').click();
    await page.keyboard.press('ControlOrMeta+a');
    expect((await sel()).length).toBe(6);
    await page.keyboard.press('Escape');

    await row('b.txt').click();
    await row('d.txt').click({ modifiers: [mod as any] });
    await expect(bar.getByRole('button', { name: 'Delete' })).toBeVisible();
    await bar.getByRole('button', { name: 'Delete' }).click();
    await expect.poll(async () => (await readdir(dir)).sort()).toEqual(['a.txt', 'c.txt', 'e.txt', 'sub']);
    await expect(bar).toHaveCount(0);
  } finally {
    // Through cleanup(): a throwing finally would discard the test's own error.
    await cleanup('remove temp folder', () => rm(dir, { recursive: true, force: true }), info);
  }
});

// The standard checkbox/select-all affordance (Ethan, 2026-10-17: "add a
// checkbox/select all standard thing to the datatable ... re-usable for all
// file list views"). Runs on every project — a checkbox is the one gesture
// that must work on touch as well as pointer.
test('a row checkbox selects it and the header checkbox selects all', async ({ page }, info) => {
  const dir = await mkdtemp(join(tmpdir(), 'amux-fselcb-'));
  try {
    for (const f of ['a', 'b', 'c']) await writeFile(join(dir, f + '.txt'), f);
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any)._fselClick === 'function');
    await page.evaluate(() => (window as any).switchView('files'));
    await page.evaluate((d) => (window as any).loadFiles(d), dir);
    const row = (n: string) => page.locator(`#files-body [data-path$="/${n}"]`);
    await row('a.txt').waitFor();
    const sel = () => page.evaluate(() => [...(globalThis as any).eval('_fsel').paths].map((p: string) => p.split('/').pop()).sort());
    const bar = page.locator('#fsel-bar');

    // One row checkbox: that row selected, action bar shown even for a single item.
    await row('a.txt').locator('.fe-check').check();
    expect(await sel()).toEqual(['a.txt']);
    await expect(bar.locator('.fsel-count')).toHaveText('1 selected');
    await expect(row('a.txt').locator('.fe-check')).toBeChecked();

    // The header "select all" checks every row in this list.
    const headAll = page.locator('#fe-col-headers .fe-check-all');
    await headAll.check();
    expect((await sel()).length).toBe(3);
    await expect(row('b.txt').locator('.fe-check')).toBeChecked();
    await expect(headAll).toBeChecked();

    // Unchecking the header clears the whole selection.
    await headAll.uncheck();
    expect(await sel()).toEqual([]);
    await expect(bar).toHaveCount(0);
    await expect(row('a.txt').locator('.fe-check')).not.toBeChecked();
  } finally {
    await cleanup('remove temp folder', () => rm(dir, { recursive: true, force: true }), info);
  }
});

// The SAME component drives the Scratchpad note list (Ethan, 2026-10-17: "use
// the same component for both ... and scratchpad list"). Desktop only: the
// three projects share the one real ~/Vault/Scratchpad, so this scopes to its
// own uniquely named note and never select-alls across a co-tenant's notes.
test('the scratchpad note list shares the selection component', async ({ page, request }, info) => {
  test.skip(test.info().project.name !== 'desktop', 'shared real Scratchpad; one project drives it deterministically');
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._fselClick === 'function');
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)}` };
  // The scratchpad body is a first-class root of the shared selection engine.
  expect(await page.evaluate(() => (globalThis as any).eval('_FSEL_ROOTS').includes('#scratchpad-body'))).toBe(true);
  await page.evaluate(() => (window as any).switchView('scratchpad'));
  await expect(page.locator('#scratchpad-view')).toBeVisible();
  // Same header select-all as the file views.
  await expect(page.locator('#sp-col-headers .fe-check-all')).toHaveCount(1);
  // Create a uniquely named note via the fs API (deterministic), then reload.
  const marker = `fselsp-${Date.now()}`;
  // Await the configured folder first, as every app-side writer does
  // (_spLoadRoot): read too early, _spPath is still the ~/Vault/Scratchpad
  // fallback and the note lands in a folder the list never shows (CI, 8582c5714).
  await page.evaluate(() => (globalThis as any).eval('_spLoadRoot()'));
  const spDir = (await page.evaluate(() => (globalThis as any).eval('_spPath')) as string).replace(/\/$/, '');
  const vpath = spDir + '/' + marker + '.md';
  const put = await request.put('/api/file', { headers: { ...auth, 'Content-Type': 'application/json' },
    data: { path: vpath, content: marker } });
  expect(put.ok()).toBe(true);
  try {
    await page.evaluate(() => (window as any)._scratchpadLoad());
    const myRow = page.locator(`#scratchpad-body [data-path$="/${marker}.md"][data-type="file"]`);
    await myRow.waitFor({ timeout: 15000 });
    // Its checkbox selects exactly that note into the shared _fsel engine and
    // shows the same action bar the file views use.
    await myRow.locator('.fe-check').check();
    await expect(myRow.locator('.fe-check')).toBeChecked();
    const selected = await page.evaluate(() => [...(globalThis as any).eval('_fsel').paths]);
    expect(selected.some((p: string) => p.endsWith(`/${marker}.md`))).toBe(true);
    await expect(page.locator('#fsel-bar .fsel-count')).toContainText('selected');
  } finally {
    // Route cleanup through cleanup() so a teardown failure is reported, not
    // substituted for the test's real error (e2e-teardown-ratchet guard).
    await cleanup('delete scratchpad test note', () => request.delete('/api/fs/delete',
      { headers: { ...auth, 'Content-Type': 'application/json' }, data: { path: vpath } }), info);
  }
});
