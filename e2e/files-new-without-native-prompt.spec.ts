// Ethan, 2026-10-18, in the iOS app: "the folder name popup when I click new
// folder or new file doesnt appear." The app's WKWebView had no JavaScript
// panel handlers, so WebKit skipped window.prompt() and it returned null with
// no box. New folder / New file now ask through the dashboard's own dialog.
// This spec stands in for that web view by making prompt() return null.
import { mkdtemp, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

test('New folder and New file ask in the page when the native prompt is skipped', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  const dir = await mkdtemp(join(tmpdir(), 'amux-files-new-'));
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    (window as any).__nativePrompts = 0;
    window.prompt = () => { (window as any).__nativePrompts++; return null; };
  });
  await page.goto('/');
  const headers = await page.evaluate(() => ({ Authorization: 'Bearer ' + (window as any)._AMUX_AUTH_TOKEN }));
  const prior = await (await request.get('/api/prefs?key=files_cwd', { headers })).json();
  try {
    expect((await request.post('/api/prefs', { headers, data: { key: 'files_cwd', value: dir } })).ok()).toBeTruthy();
    await page.reload();
    await page.waitForFunction(() => typeof (window as any).loadFiles === 'function');
    await page.locator('#tab-files').click();
    await expect(page.locator('#files-body')).toBeVisible();
    await page.evaluate(p => (window as any).loadFiles(p), dir);

    // New folder, from the More menu, the path the report took.
    await page.locator('#files-overflow-btn').click();
    await page.locator('#files-overflow-menu .fe-tb-oitem', { hasText: 'New folder' }).click();
    const input = page.locator('#modal-prompt-input');
    await expect(input).toBeVisible();
    await expect(input).toBeFocused();
    await input.fill('from-the-app');
    await input.press('Enter');
    await expect(page.locator('#files-body .fe-row').filter({ hasText: 'from-the-app' })).toBeVisible();
    expect((await stat(join(dir, 'from-the-app'))).isDirectory()).toBe(true);

    // New file opens the editor on the typed name.
    await page.locator('#files-overflow-btn').click();
    await page.locator('#files-overflow-menu .fe-tb-oitem', { hasText: 'New file' }).click();
    await expect(input).toBeVisible();
    await input.fill('notes.txt');
    await input.press('Enter');
    await expect(page.locator('#file-overlay')).toHaveClass(/active/);
    await expect(page.locator('#file-title')).toHaveText('notes.txt (new)');

    expect(await page.evaluate(() => (window as any).__nativePrompts)).toBe(0);
  } finally {
    await cleanup('restore files_cwd', () => request.post('/api/prefs', { headers, data: { key: 'files_cwd', value: prior.value || '' } }), info);
    await cleanup('remove temp folder', () => rm(dir, { recursive: true, force: true }), info);
  }
});
