import {test, expect} from './fixtures';
import {mkdtemp, writeFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {cleanup} from './teardown';

// A worker texts the owner a link to a markdown note to answer in place
// (Ethan, 2026-10-26: the self worker's 14-day check-in). #edit=<path> opens
// that file in Edit; #path=<path> keeps opening it in Preview.
test('#edit= opens a markdown file in Edit, #path= in Preview', async ({page}) => {
  const dir = await mkdtemp(join(tmpdir(), 'amux-edit-link-'));
  const md = join(dir, 'check in 2026-10-26.md');
  const txt = join(dir, 'notes.txt');
  const png = join(dir, 'pixel.png');
  await writeFile(md, '# Check-in\n\nHow did the last two weeks go?\n');
  await writeFile(txt, 'plain text\n');
  await writeFile(png, Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==', 'base64'));
  const enc = (p: string) => p.replace(/%/g, '%25').replace(/#/g, '%23').replace(/\?/g, '%3F').replace(/ /g, '%20');
  try {
    // A markdown file opens in the markdown editor: its source for #edit=,
    // its preview for #path= (owner, 2026-10-29, the editor redesign).
    await page.goto('/#edit=' + enc(md));
    await expect(page.locator('#mdx')).toHaveAttribute('data-mode', /edit|split/, {timeout: 15000});
    await expect(page.locator('#file-save-btn')).toBeVisible();
    expect(await page.evaluate('_fileData && _fileData.path')).toBe(md);

    await page.goto('about:blank');
    await page.goto('/#path=' + enc(md));
    await expect(page.locator('#mdx')).toHaveAttribute('data-mode', 'preview', {timeout: 15000});
    await expect(page.locator('#mdx-src')).toBeHidden();

    // Every text file has the editor now (Ethan, 2026-10-26: "make sure we
    // have a file editor for every file"), plain text included.
    await page.goto('about:blank');
    await page.goto('/#edit=' + enc(txt));
    await expect.poll(() => page.evaluate('_fileData && _fileData.path'), {timeout: 15000}).toBe(txt);
    await expect(page.locator('#file-tab-edit')).toHaveClass(/active/);

    // A file with no editor (an image) opens as it would from #path=.
    await page.goto('about:blank');
    await page.goto('/#edit=' + enc(png));
    await expect.poll(() => page.evaluate('_fileData && _fileData.path'), {timeout: 15000}).toBe(png);
    await expect(page.locator('#file-tab-edit')).not.toHaveClass(/active/);
  } finally {
    await cleanup('remove the fixture folder', () => rm(dir, {recursive: true, force: true}));
  }
});
