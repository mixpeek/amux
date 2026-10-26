import { test, expect } from './fixtures';
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { cleanup } from './teardown';

// Ethan, 2026-10-26: "make sure we have a file editor for every file, i cannot
// edit html files". The viewer offered Edit only for markdown; every text file
// opens in the same editor now, and saves through the same PUT /api/file.

test('an HTML file and a code file are editable and saved to disk', async ({ page }) => {
  const dir = await mkdtemp(join(tmpdir(), 'amux-edit-any-'));
  try {
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
    for (const [name, before, after, shown] of [
      ['page.html', '<h1>Old title</h1>\n', '<h1>New title</h1>\n', 'New title'],
      ['tool.py', 'print("old")\n', 'print("new")\n', 'print("new")'],
    ]) {
      const file = join(dir, name);
      await writeFile(file, before);
      await page.evaluate(p => (window as any).openFilePreview(p), file);
      const edit = page.locator('#file-tab-edit');
      await expect(edit).toBeVisible();
      await expect(edit).toBeEnabled();
      await edit.click();
      await expect(page.locator('#file-edit-ta')).toHaveValue(before);
      await page.locator('#file-edit-ta').fill(after);
      await page.locator('#file-save-btn').click();
      await expect.poll(() => readFile(file, 'utf8')).toBe(after);
      // Back to what it renders as, showing the saved text.
      await page.locator('#file-tab-raw').click();
      await expect(page.locator('#file-body')).toContainText(shown);
      await page.evaluate(() => (window as any).closeFilePreview());
    }
  } finally {
    await cleanup('remove the fixture folder', () => rm(dir, { recursive: true, force: true }));
  }
});

test('Edit is not offered where a save would lose data', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._fileEditable === 'function');
  const editable = (d: object) => page.evaluate(d => (window as any)._fileEditable(d), d);
  expect(await editable({ content: '<p>x</p>', is_html: true })).toBe(true);
  expect(await editable({ content: 'a,b', is_csv: true })).toBe(true);
  // Shown cut at the size limit: saving would drop everything past it.
  expect(await editable({ content: 'x', truncated: true })).toBe(false);
  expect(await editable({ is_image: true })).toBe(false);
  expect(await editable({ content: '', is_binary: true })).toBe(false);
  expect(await editable({ content: 'x', readOnly: true })).toBe(false);
});
