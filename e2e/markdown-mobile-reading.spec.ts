// A long markdown note reads on a phone (Ethan, 2026-10-20, engine.md
// screenshot: table cells wrapped mid-word, "Acqui|re", no scrollbar to show
// progress, a framed body wasting the edges; Obsidian as the reference).
// Tables keep whole words and scroll sideways, the viewer draws its own
// scroll thumb with a percentage, and a wide diagram opens full screen at a
// readable scale.
import { test, expect } from './fixtures';
import fs from 'node:fs';
import path from 'node:path';
import { scratchpadDir } from './scratchpad-dir';
import { cleanup } from './teardown';

const row = (i: number) => `| ${i} | Acquire | Reach | impressions, views per asset | Buffer, Search Console, GA4, YouTube | Buffer, GSC | GSC 28d: 2,400 clicks, 23,272 impressions; 57,400 AI-feature impressions across the cited docs |`;
const NOTE = ['# Funnel', '', '| # | Stage | Step | Metric | Event or signal | Lands in | Latest |', '|---|---|---|---|---|---|---|',
  ...Array.from({ length: 12 }, (_, i) => row(i + 1)), '',
  '```mermaid', 'flowchart TD',
  ...Array.from({ length: 10 }, (_, i) => `    A${i}["Channel number ${i} with a long label"] --> S[Signup]`),
  '    S --> F[First value] --> P[Paid]', '```', '',
  ...Array.from({ length: 80 }, (_, i) => `Paragraph ${i} of a long note that needs a sense of progress.`), '', 'END-OF-NOTE'].join('\n');

test('a markdown note reads on a phone: whole words in tables, a scroll thumb, a zoomable diagram', async ({ page, request }) => {
  test.setTimeout(90_000);
  const vw = page.viewportSize()?.width || 0;
  test.skip(vw > 600, 'phone layout only');
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)}` };
  const file = path.join(await scratchpadDir(request, auth), `mobile-read-${Date.now()}-${Math.random().toString(36).slice(2, 7)}.md`);
  fs.writeFileSync(file, NOTE);
  try {
    await page.evaluate((f) => (window as any).openFilePreview(f), file);
    const body = page.locator('#file-body');
    await expect(body).toContainText('END-OF-NOTE');

    // The body uses the full width: no frame inset around the note.
    const box = await body.boundingBox();
    expect(box!.x).toBeLessThanOrEqual(1);
    expect(box!.width).toBeGreaterThanOrEqual(vw - 1);

    // No hyphen-free word in a table cell is split across two lines, and the
    // table scrolls sideways inside its frame rather than widening the page.
    await expect.poll(() => page.evaluate(() => document.querySelector('#file-body table')?.hasAttribute('data-fit'))).toBe(true);
    const t = await page.evaluate(() => {
      const split: string[] = [];
      for (const td of document.querySelectorAll('#file-body td, #file-body th')) {
        const w = document.createTreeWalker(td, NodeFilter.SHOW_TEXT); let n: Node | null;
        while ((n = w.nextNode())) {
          const s = (n as Text).data, re = /[^\s-]+/g; let m: RegExpExecArray | null;
          while ((m = re.exec(s))) {
            const r = document.createRange(); r.setStart(n, m.index); r.setEnd(n, m.index + m[0].length);
            if (new Set([...r.getClientRects()].map(x => Math.round(x.top))).size > 1) split.push(m[0]);
          }
        }
      }
      const wrap = document.querySelector('#file-body .table-scroll')!;
      return { split, scrolls: wrap.scrollWidth > wrap.clientWidth + 1, pageOverflow: document.documentElement.scrollWidth > innerWidth + 1 };
    });
    expect(t.split, 'words split mid-word').toEqual([]);
    expect(t.scrolls).toBe(true);
    expect(t.pageOverflow).toBe(false);

    // A scroll thumb shows where you are, and follows the scroll.
    const thumb = page.locator('#file-overlay .file-scroll-thumb');
    await expect(page.locator('#file-overlay .file-scroll-rail')).toHaveClass(/\bon\b/);
    const y0 = (await thumb.boundingBox())!.y;
    await page.evaluate(() => { const b = document.getElementById('file-body')!; b.scrollTop = (b.scrollHeight - b.clientHeight) / 2; });
    await expect.poll(async () => (await thumb.boundingBox())!.y).toBeGreaterThan(y0 + 50);
    await expect(page.locator('#file-overlay .file-scroll-pct')).toHaveText(/^(4[5-9]|5[0-5])%$/);

    // Dragging the thumb to the bottom reaches the end of the note.
    const tb = (await thumb.boundingBox())!;
    await page.mouse.move(tb.x + tb.width / 2, tb.y + 5);
    await page.mouse.down();
    await page.mouse.move(tb.x + tb.width / 2, tb.y + 2000, { steps: 6 });
    await page.mouse.up();
    await expect.poll(() => page.evaluate(() => { const b = document.getElementById('file-body')!; return b.scrollHeight - b.clientHeight - b.scrollTop; })).toBeLessThan(2);

    // The diagram opens full screen, zoomed to readable labels, and closes.
    const fig = body.locator('.md-mermaid');
    await expect(fig.locator('svg')).toHaveCount(1, { timeout: 30_000 });
    await fig.scrollIntoViewIfNeeded();
    await fig.click();
    const viewer = page.locator('#md-diagram-viewer');
    await expect(viewer).toBeVisible();
    await expect(viewer.locator('svg')).toContainText('Channel number 0');
    const inlineW = (await fig.locator('svg').boundingBox())!.width;
    const viewerW = (await viewer.locator('.md-dv-img').boundingBox())!.width;
    expect(viewerW, 'the viewer draws the diagram larger than the inline column').toBeGreaterThan(inlineW * 1.2);
    await viewer.getByRole('button', { name: 'Close diagram' }).click();
    await expect(viewer).toHaveCount(0);
  } finally { await cleanup('remove fixture note', () => fs.rmSync(file, { force: true })); }
});
