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
