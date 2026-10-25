// A long file stays scrollable to its last line at a zoomed-in level on
// WebKit (Ethan, 2026-10-19, the Mac app: "cant scroll to the bottom without
// zooming out"). The dashboard zooms WebKit through the root font size, so
// the file viewer must still end inside the window.
import { test, expect } from './fixtures';
import fs from 'node:fs';
import path from 'node:path';
import { scratchpadDir } from './scratchpad-dir';

test('a zoomed-in file viewer ends inside the window and reaches its last line', async ({ page, request }, info) => {
  test.skip(info.project.name !== 'ios-safari', 'WebKit only (the engine the Mac app uses)');
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)}` };
  const dir = await scratchpadDir(request, auth);
  const file = path.join(dir, `zoom-${Date.now()}.md`);
  fs.writeFileSync(file, Array.from({ length: 200 }, (_, i) => `Line ${i + 1} of the long note.`).join('\n\n') + '\n\nTHE-LAST-LINE\n');
  for (let i = 0; i < 3; i++) await page.evaluate(() => (window as any).zoomIn());
  expect(await page.evaluate(() => (0, eval)('_zoomLevel'))).toBeGreaterThan(100);
  await page.evaluate((f) => (window as any).openFilePreview(f), file);
  await expect(page.locator('#file-body')).toContainText('THE-LAST-LINE');
  const geo = await page.evaluate(() => {
    const ov = document.getElementById('file-overlay')!.getBoundingClientRect();
    const body = document.getElementById('file-body')!;
    body.scrollTop = body.scrollHeight;
    return { ovBottom: ov.bottom, innerHeight };
  });
  expect(geo.ovBottom, JSON.stringify(geo)).toBeLessThanOrEqual(geo.innerHeight + 1);
  await expect.poll(() => page.evaluate(() => {
    const last = [...document.querySelectorAll('#file-body *')].find(e => e.textContent?.trim() === 'THE-LAST-LINE');
    return last ? last.getBoundingClientRect().bottom <= innerHeight : false;
  })).toBe(true);
  fs.rmSync(file, { force: true });
});
