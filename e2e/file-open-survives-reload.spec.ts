import { test, expect } from './fixtures';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { cleanup } from './teardown';

// Ethan, 2026-10-26: "when i close and re-open it doesnt open the file that
// was open". Only folders were ever written to the address, and a deep-linked
// file was overwritten by its folder as the listing behind it loaded, so the
// next reload of the page (a new dashboard version reloads every page, and the
// Mac app restores tabs by address) showed the folder instead of the file.

test('an open file is in the address, so a reload opens it again; closing it returns to the folder', async ({ page }) => {
  const dir = await mkdtemp(join(tmpdir(), 'amux-file-reload-'));
  const file = join(dir, 'plan.md');
  // The title shows the heading, not the name: the address must not use it.
  await writeFile(file, '# A heading that is not the file name\n\nbody text\n');
  const enc = (p: string) => '#path=' + p.split('/').map(encodeURIComponent).join('/');
  const decoded = async () => decodeURIComponent(await page.evaluate(() => location.hash));
  try {
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.goto('/' + enc(dir));
    await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
    await page.evaluate(p => (window as any).openFilePreview(p), file);
    await expect(page.locator('#file-overlay')).toHaveClass(/active/);
    await expect.poll(decoded).toBe('#path=' + file);

    await page.reload();
    await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 15000 });
    await expect(page.locator('#file-body')).toContainText('body text');
    // The listing that loads behind the file does not take the address back.
    await page.waitForTimeout(1500);
    expect(await decoded()).toBe('#path=' + file);

    await page.evaluate(() => (window as any).closeFilePreview());
    await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
    expect(await decoded()).toBe('#path=' + dir);
  } finally {
    await cleanup('remove the fixture folder', () => rm(dir, { recursive: true, force: true }));
  }
});

// Owner, 2026-10-29: "when i refresh an open md file it goes to the worker
// view". This used to leave #peek= in place; a file opened over a worker now
// takes the address, and closing it gives the worker's back.
test('a file opened over a worker takes the address, and closing it gives the worker link back', async ({ page }) => {
  const dir = await mkdtemp(join(tmpdir(), 'amux-file-reload-'));
  const file = join(dir, 'note.txt');
  await writeFile(file, 'hello\n');
  try {
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
    await page.evaluate(() => history.replaceState(null, '', location.pathname + '#peek=someone&tab=terminal'));
    await page.evaluate(p => (window as any).openFilePreview(p), file);
    await expect(page.locator('#file-overlay')).toHaveClass(/active/);
    expect(decodeURIComponent(await page.evaluate(() => location.hash))).toBe('#path=' + file);
    await page.evaluate(() => (window as any).closeFilePreview());
    expect(await page.evaluate(() => location.hash)).toBe('#peek=someone&tab=terminal');
    // A link the file did not open over is left alone, and is not put back later.
    await page.evaluate(p => (window as any).openFilePreview(p), file);
    await page.evaluate(() => history.replaceState(null, '', location.pathname + '#issue=AMUX-1'));
    await page.evaluate(() => (window as any).closeFilePreview());
    expect(await page.evaluate(() => location.hash)).toBe('#issue=AMUX-1');
    await page.evaluate(p => (window as any).openFilePreview(p), file);
    await page.evaluate(() => (window as any).closeFilePreview());
    expect(await page.evaluate(() => location.hash)).not.toContain('peek=someone');
  } finally {
    await cleanup('remove the fixture folder', () => rm(dir, { recursive: true, force: true }));
  }
});
