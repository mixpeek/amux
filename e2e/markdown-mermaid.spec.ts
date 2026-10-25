// A ```mermaid block in a markdown file draws as a diagram in the file
// viewer, rather than showing its source (Ethan, 2026-10-20: a "Course map"
// flowchart, "this should render"). A diagram that does not parse keeps its
// source and says why.
import { test, expect } from './fixtures';
import fs from 'node:fs';
import path from 'node:path';
import { scratchpadDir } from './scratchpad-dir';
import { cleanup } from './teardown';

const COURSE = ['## Course map', '', '```mermaid', 'flowchart LR',
  '    P1["Part 1<br/>The storage hierarchy<br/>L1-L3"] --> P2["Part 2<br/>Data structures on disk<br/>L4-L6"]',
  '    P2 --> P3["Part 3<br/>Storing representations<br/>L7-L10"]',
  '    P1 --> P4["Part 4<br/>Storage for inference and training<br/>L11-L13"]',
  '    P1 --> P5["Part 5<br/>Systems and economics<br/>L14-L17"]',
  '    P3 --> P5', '    P4 --> P5', '```', '', '```mermaid', 'flowchart LR', '    A[[[ this is not valid', '```', '', 'END-OF-NOTE'].join('\n');

test('a mermaid block in a markdown file draws as a diagram; a broken one keeps its source', async ({ page, request }) => {
  test.setTimeout(90_000);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)}` };
  const file = path.join(await scratchpadDir(request, auth), `mermaid-${Date.now()}-${Math.random().toString(36).slice(2, 7)}.md`);
  fs.writeFileSync(file, COURSE);
  try {
    await page.evaluate((f) => (window as any).openFilePreview(f), file);
    const body = page.locator('#file-body');
    await expect(body).toContainText('END-OF-NOTE');
    const fig = body.locator('.md-mermaid svg');
    await expect(fig).toHaveCount(1, { timeout: 30_000 });
    await expect(fig).toBeVisible();
    // The nodes are drawn with their labels, line breaks included.
    for (const label of ['Part 1', 'The storage hierarchy', 'Systems and economics', 'L14-L17']) await expect(fig).toContainText(label);
    await expect(fig).not.toContainText('<br');
    // The good block's source is gone; the broken one keeps its source and a reason.
    await expect(body.locator('code.language-mermaid')).toHaveCount(1);
    await expect(body.locator('code.language-mermaid')).toContainText('this is not valid');
    await expect(body.locator('.md-mermaid-error')).toContainText('Diagram not drawn');
    // It fits the column.
    const fits = await page.evaluate(() => {
      const s = document.querySelector('#file-body .md-mermaid svg')!.getBoundingClientRect();
      const b = document.getElementById('file-body')!.getBoundingClientRect();
      return s.width <= b.width + 1;
    });
    expect(fits).toBe(true);
  } finally { await cleanup('remove fixture note', () => fs.rmSync(file, { force: true })); }
});
