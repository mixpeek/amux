// Obsidian wikilinks link (Ethan, 2026-10-20: a lesson table of
// [[#Lesson 1 ...|1. ...]] showed as text, "these should link"): a heading link
// scrolls to its heading, a note link opens the note (at its heading).
import { test, expect } from './fixtures';
import fs from 'node:fs';
import path from 'node:path';
import { scratchpadDir } from './scratchpad-dir';
import { cleanup } from './teardown';

test('wikilinks: [[#Heading|label]] in a table scrolls to the heading, [[Note#Heading]] opens the note there', async ({ page, request }) => {
  test.setTimeout(90_000);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)}` };
  const dir = path.join(await scratchpadDir(request, auth), `wl-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`);
  fs.mkdirSync(dir, { recursive: true });
  const filler = Array.from({ length: 80 }, (_, i) => `Paragraph ${i} of filler so the headings are far apart.`).join('\n\n');
  fs.writeFileSync(path.join(dir, 'course.md'), [
    '# Course', '', '| Part | Lesson |', '|---|---|',
    '| 1. Storage | [[#Lesson 1 The memory and storage hierarchy\\|1. The memory and storage hierarchy]] |',
    '| 1. Storage | [[#Lesson 2 Inside the SSD\\|2. Inside the SSD]] |', '',
    'See [[glossary#Write amplification]] and [[glossary|the glossary]].', '', filler, '',
    '## Lesson 1 The memory and storage hierarchy', '', 'LESSON-ONE-BODY', '', filler, '',
    '## Lesson 2 Inside the SSD', '', 'LESSON-TWO-BODY', ''].join('\n'));
  fs.writeFileSync(path.join(dir, 'glossary.md'), ['# Glossary', '', filler, '', '## Write amplification', '', 'WA-BODY', '', filler].join('\n'));
  try {
    await page.evaluate((f) => (window as any).openFilePreview(f), path.join(dir, 'course.md'));
    const body = page.locator('#file-body');
    await expect(body).toContainText('LESSON-TWO-BODY');
    await expect(body).not.toContainText('[[');
    const lesson2 = body.locator('table a.md-anchor-link', { hasText: '2. Inside the SSD' });
    await expect(lesson2).toBeVisible();
    const inView = (id: string) => page.evaluate((i) => {
      const h = document.getElementById(i)!; const b = document.getElementById('file-body')!.getBoundingClientRect(); const r = h.getBoundingClientRect();
      return r.top >= b.top - 2 && r.top < b.bottom - 20;
    }, id);
    expect(await inView('lesson-2-inside-the-ssd')).toBe(false);
    await lesson2.click();
    await expect.poll(() => inView('lesson-2-inside-the-ssd')).toBe(true);
    await expect(page.locator('#file-overlay')).toHaveClass(/active/);

    // A note link opens the other note and lands on its heading.
    await body.locator('a.md-file-link', { hasText: 'glossary > Write amplification' }).click();
    await expect(body).toContainText('WA-BODY');
    await expect(page.locator('#file-title')).toHaveText('glossary.md');
    await expect.poll(() => inView('write-amplification')).toBe(true);
  } finally { await cleanup('remove fixture note', () => fs.rmSync(dir, { recursive: true, force: true })); }
});
