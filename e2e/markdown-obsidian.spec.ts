// The file viewer renders an Obsidian note as Obsidian does (Ethan,
// 2026-10-20: "the file view should be on par with obsidian", checked against
// a 410 KB course note): $ and $$ math with \$ prices left as text, callouts
// (folding ones too), and a note past the old 200,000-character cut arrives
// whole.
import { test, expect } from './fixtures';
import fs from 'node:fs';
import path from 'node:path';
import { scratchpadDir } from './scratchpad-dir';
import { cleanup } from './teardown';

test('math, callouts and a long note render the way Obsidian shows them', async ({ page, request }) => {
  test.setTimeout(120_000);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)}` };
  const file = path.join(await scratchpadDir(request, auth), `obs-${Date.now()}-${Math.random().toString(36).slice(2, 6)}.md`);
  const filler = Array.from({ length: 2600 }, (_, i) => `Filler paragraph ${i} keeps this note well past two hundred thousand characters of text.`).join('\n\n');
  fs.writeFileSync(file, [
    '# Note', '',
    'Inline $t \\approx L + s/B$ and prices \\$0.08 per GiB-month and \\$3.09 stay text.', '',
    '$$WA = \\frac{\\text{bytes written to NAND}}{\\text{bytes written by the host}}$$', '',
    '> [!example] Worked example: half-bandwidth size', '> On NVMe $s_{1/2} = L \\times B$.', '',
    '> [!tip]- Answer', '> HIDDEN-UNTIL-OPENED', '',
    '> A plain quote stays a quote.', '',
    filler, '', '## Last section', '', 'THE-VERY-END'].join('\n'));
  try {
    await page.evaluate((f) => (window as any).openFilePreview(f), file);
    const body = page.locator('#file-body');
    await expect(body).toContainText('THE-VERY-END', { timeout: 30_000 });   // nothing cut at 200 KB
    // Math: inline and display drawn by KaTeX; escaped prices are plain text.
    await expect(body.locator('.md-math .katex')).toHaveCount(3, { timeout: 30_000 });
    await expect(body.locator('.md-math-display .katex-display, .md-math-display .katex').first()).toBeVisible();
    await expect(body).toContainText('$0.08 per GiB-month');
    await expect(body).toContainText('$3.09 stay text');
    await expect(body.locator('.katex-error, .md-math-error')).toHaveCount(0);
    // Callouts: a coloured box with its title, a folding one closed, a plain quote untouched.
    const ex = body.locator('.md-callout-example');
    await expect(ex.locator('.md-callout-title')).toHaveText('Worked example: half-bandwidth size');
    await expect(ex).not.toContainText('[!example]');
    const fold = body.locator('details.md-callout-tip');
    await expect(fold).not.toHaveAttribute('open', /.*/);
    await expect(fold.getByText('HIDDEN-UNTIL-OPENED')).toBeHidden();
    await fold.locator('summary').click();
    await expect(fold.getByText('HIDDEN-UNTIL-OPENED')).toBeVisible();
    await expect(body.locator('blockquote')).toContainText('A plain quote stays a quote.');
  } finally { await cleanup('remove fixture note', () => fs.rmSync(file, { force: true })); }
});
