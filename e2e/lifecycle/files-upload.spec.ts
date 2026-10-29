import { test, expect } from '../fixtures';
import { boot, auth, checkpoint } from './evidence';
import { cleanup } from '../teardown';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

test('LC-FILES-UPLOAD: upload, preview, rename, download exact bytes, and delete from Files', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  const dir = await mkdtemp(path.join(os.tmpdir(), 'amux-lc-files-'));
  await boot(page);
  await auth(page);
  const filename = 'résumé fruit counts.csv', renamed = 'reviewed fruit counts.csv';
  const bytes = Buffer.from('fruit,count\napples,2\npears,4\n');
  try {
    // Files opens the saved folder (files_cwd). That pref is server-wide, and
    // four other specs set and restore it in parallel workers on this same
    // server, so writing it here raced them: the page opened another test's
    // folder, or "/" after one restored an empty value (main CI 40874475222).
    // This page alone reads this test's folder; nobody else's is touched.
    await page.route(u => u.pathname === '/api/prefs' && u.searchParams.get('key') === 'files_cwd',
      route => route.request().method() === 'GET' ? route.fulfill({ json: { key: 'files_cwd', value: dir } }) : route.continue());
    await page.reload();
    await page.locator('#tab-files').click();
    await expect(page.locator('#files-breadcrumb')).toContainText(path.basename(dir));
    const choose = page.waitForEvent('filechooser');
    const upload = page.getByTitle('Upload files into this folder', { exact: true });
    if (await upload.isVisible()) await upload.click();
    else {
      await page.locator('#files-overflow-btn').click();
      await page.locator('#files-overflow-menu').getByRole('button', { name: /Upload files/ }).click();
    }
    await (await choose).setFiles({ name: filename, mimeType: 'text/csv', buffer: bytes });
    const row = (name: string) => page.locator('#files-body .fe-row').filter({ hasText: name });
    await expect(row(filename)).toBeVisible();
    expect(await readFile(path.join(dir, filename))).toEqual(bytes);
    if (await page.evaluate(() => matchMedia('(hover: none), (pointer: coarse)').matches)) await row(filename).click();
    else await row(filename).dblclick();
    await expect(page.locator('#file-body')).toContainText('apples');
    await checkpoint(page, info, 'uploaded-csv-preview');
    await page.locator('[onclick="closeFilePreview()"]').click();
    await row(filename).getByTitle('Options', { exact: true }).click();
    page.once('dialog', dialog => dialog.accept(renamed));
    await page.locator('.explore-menu-popup').getByRole('button', { name: 'Rename', exact: true }).click();
    await expect(row(renamed)).toBeVisible();
    await expect(row(filename)).toHaveCount(0);
    await row(renamed).getByTitle('Options', { exact: true }).click();
    const downloading = page.waitForEvent('download');
    await page.locator('.explore-menu-popup').getByRole('button', { name: 'Download', exact: true }).click();
    const downloaded = await downloading;
    expect(downloaded.suggestedFilename()).toBe(renamed);
    expect(await readFile((await downloaded.path())!)).toEqual(bytes);
    await checkpoint(page, info, 'uploaded-csv-renamed');
    await row(renamed).getByTitle('Options', { exact: true }).click();
    page.once('dialog', dialog => dialog.accept());
    await page.locator('.explore-menu-popup').getByRole('button', { name: 'Delete file', exact: true }).click();
    await expect(row(renamed)).toHaveCount(0);
    await expect(readFile(path.join(dir, renamed))).rejects.toThrow(/ENOENT/);
  } finally {
    // AMUX-4737. A teardown can throw AFTER the body has already failed, and
    // a throwing `finally` DISCARDS the body's error. This spec is the recorded
    // specimen: AMUX-4643 was titled "request context closed", which is what
    // this POST raises once a 90s timeout has killed the context, while the
    // step that actually broke was line 35. The card sat two days wearing its
    // teardown's error.
    await cleanup('remove the temp dir', () => rm(dir, { recursive: true, force: true }), info);
  }
});
