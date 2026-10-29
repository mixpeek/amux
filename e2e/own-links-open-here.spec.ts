import {test, expect} from './fixtures';
import {cleanup} from './teardown';
import {mkdtemp, writeFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';

// A link to this amux opens in this amux (Ethan, 2026-10-28: "it should
// recognize links when links are local and use them within the app rather
// than opening a new browser/window"). A worker's /f/<path> link to a file
// with spaces in its name, and a /#view= link, open in place; a modifier
// click and a link to another site keep opening a new tab.
test('a link to this amux opens here, not in a new tab', async ({page, context}) => {
  const dir = await mkdtemp(join(tmpdir(), 'amux-own-link-'));
  const file = join(dir, 'Coeur Wine Co - AR Reconciliation.md');
  await writeFile(file, '# Reconciliation\n');
  const newTabs: string[] = [];
  context.on('page', p => newTabs.push(p.url()));
  try {
    await page.goto('/#view=sessions');
    await page.waitForFunction(() => typeof (window as any)._openOwnLink === 'function');
    const addLink = (href: string, id: string) => page.evaluate(([h, i]) => {
      const a = document.createElement('a');
      a.href = h; a.target = '_blank'; a.id = i; a.textContent = i;
      a.style.cssText = 'position:fixed;top:120px;left:20px;z-index:99999;padding:8px;background:#fff';
      document.body.appendChild(a);
    }, [href, id]);
    const origin = new URL(page.url()).origin;
    const fileLink = origin + '/f' + file.split('/').map(encodeURIComponent).join('/');

    await addLink(fileLink, 'own-file');
    await page.locator('#own-file').click();
    await expect.poll(() => page.evaluate('_fileData && _fileData.path'), {timeout: 15000}).toBe(file);
    await expect(page.locator('#file-overlay')).toHaveClass(/active/);
    await page.evaluate(() => document.getElementById('own-file')!.remove());
    await page.evaluate(() => (window as any).closeFilePreview());

    await addLink(origin + '/#view=board', 'own-view');
    await page.locator('#own-view').click();
    await expect.poll(() => page.evaluate('activeView')).toBe('board');
    expect(newTabs, 'a link to this amux opened a new tab').toEqual([]);

    // A link to another site is untouched: it still opens a new tab, and this
    // page stays where it was.
    const before = page.url();
    await addLink('https://example.invalid/x', 'other-site');
    const popup = page.waitForEvent('popup');
    await page.locator('#other-site').click();
    expect(await popup, 'an outside link opens a new tab').toBeTruthy();
    expect(page.url()).toBe(before);
  } finally {
    await cleanup('remove the temp folder', () => rm(dir, {recursive: true, force: true}));
  }
});
