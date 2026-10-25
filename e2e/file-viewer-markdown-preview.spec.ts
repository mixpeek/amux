// The file viewer (shared by Files, Scratchpad, the peek's file split and
// MDAI) must reach the end of a long file at a zoomed-in level, and render
// markdown as a compact note rather than with a blank line between every block
// (Ethan, 2026-10-20: "when i zoom in slightly and scroll to the bottom i cant
// see"; "make the preview mode cleaner", with Obsidian as the goal).
import { test, expect, Page } from './fixtures';
import fs from 'node:fs';
import path from 'node:path';
import { scratchpadDir } from './scratchpad-dir';
import { cleanup } from './teardown';

const MD = ['# GTM', '', '## OSS', '', '- https://github.com/mixpeek/mvs-migrate', '- https://github.com/mixpeek/mixpeek-extractors', '',
  '## New Capabilities to Market', '', 'each one should have an integration partner', '',
  '- **Embedding Translation / Migration**', '  - Switch embedding models without re-embedding the full corpus.', '',
  ...Array.from({ length: 60 }, (_, i) => `- item ${i}`), '', '# Engineering', '', '## Roadmap', '', 'THE-LAST-LINE'].join('\n');

async function openNote(page: Page, request: any) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function' && typeof (window as any).zoomIn === 'function');
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)}` };
  const file = path.join(await scratchpadDir(request, auth), `fv-${Date.now()}-${Math.random().toString(36).slice(2, 7)}.md`);
  fs.writeFileSync(file, MD);
  return file;
}

test('zoomed in, the file viewer ends inside the window and its last line scrolls into view', async ({ page, request }) => {
  const file = await openNote(page, request);
  try {
    for (let i = 0; i < 3; i++) await page.evaluate(() => (window as any).zoomIn());
    expect(await page.evaluate(() => (0, eval)('_zoomLevel'))).toBe(130);
    await page.evaluate((f) => (window as any).openFilePreview(f), file);
    await expect(page.locator('#file-body')).toContainText('THE-LAST-LINE');
    const g = await page.evaluate(() => {
      const ov = document.getElementById('file-overlay')!.getBoundingClientRect();
      const b = document.getElementById('file-body')!;
      b.scrollTop = b.scrollHeight;
      return { ovBottom: ov.bottom, bodyBottom: b.getBoundingClientRect().bottom, innerHeight };
    });
    expect(g.ovBottom, JSON.stringify(g)).toBeLessThanOrEqual(g.innerHeight + 1);
    expect(g.bodyBottom, JSON.stringify(g)).toBeLessThanOrEqual(g.innerHeight + 1);
    await expect.poll(() => page.evaluate(() => {
      const last = [...document.querySelectorAll('#file-body *')].find(e => e.textContent?.trim() === 'THE-LAST-LINE');
      const b = document.getElementById('file-body')!.getBoundingClientRect();
      if (!last) return false;
      const r = last.getBoundingClientRect();
      return r.bottom <= Math.min(innerHeight, b.bottom) + 1 && r.top >= b.top;
    })).toBe(true);
  } finally { await cleanup('remove fixture note', () => fs.rmSync(file, { force: true })); }
});

test('markdown preview is compact: no blank line between blocks, no rule under headings, Raw keeps its lines', async ({ page, request }) => {
  const file = await openNote(page, request);
  try {
    await page.evaluate((f) => (window as any).openFilePreview(f), file);
    const body = page.locator('#file-body');
    await expect(body).toContainText('THE-LAST-LINE');
    const m = await page.evaluate(() => {
      const b = document.getElementById('file-body')!;
      const lis = [...b.querySelectorAll('li')].filter(l => /^item [0-9]+$/.test(l.textContent!.trim()));
      const h1 = b.querySelector('h1')!;
      const lh = parseFloat(getComputedStyle(lis[0]).lineHeight);
      return { ws: getComputedStyle(b).whiteSpace, step: lis[1].getBoundingClientRect().top - lis[0].getBoundingClientRect().top, lh,
        h1Border: getComputedStyle(h1).borderBottomStyle };
    });
    expect(m.ws).toBe('normal');
    // Consecutive items sit one line apart (Obsidian), not a line plus a blank line.
    expect(m.step, JSON.stringify(m)).toBeLessThan(m.lh * 1.5);
    expect(m.h1Border).toBe('none');
    // The Raw view still shows the file's own line breaks.
    await page.evaluate(() => (window as any).setFileViewMode('raw'));
    await expect.poll(() => page.evaluate(() => getComputedStyle(document.getElementById('file-body')!).whiteSpace)).toMatch(/pre/);
  } finally { await cleanup('remove fixture note', () => fs.rmSync(file, { force: true })); }
});
