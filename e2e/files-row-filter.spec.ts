// Owner, 2026-10-31, on the Files view's top rows:
//  - "i thought we were getting rid of this Up button" (it went 2026-10-30;
//    pinned again here).
//  - "make it so the vertical .... (More) is just the vertical dot dot dot".
//  - "have a filter that stays selected locally ... on the left of the row:
//    all, pinned, recent, modified, shared. (dropdown) and based on whats
//    selected it should show the list all on the same row".
import { mkdtemp, mkdir, writeFile, utimes, realpath, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';

async function openFiles(page: Page, dir: string) {
  await page.waitForFunction("typeof switchView === 'function' && typeof loadFiles === 'function'");
  await page.evaluate("switchView('files')");
  await page.evaluate(d => (window as any).loadFiles(d), dir);
  await expect(page.locator('#files-body .fe-row').first()).toBeVisible();
}
const chips = (page: Page) => page.locator('#files-bookmarks-list .files-bm-chip');
const sources = (page: Page) => chips(page).evaluateAll(els => els.map(e => (e as HTMLElement).dataset.source));
const titles = (page: Page) => chips(page).evaluateAll(els => els.map(e => e.textContent || ''));

test('the Files row: no Up, a bare ⋮, and a remembered All/Pinned/Recent/Modified/Shared filter on one row', async ({ page }, info) => {
  test.setTimeout(90_000);
  const dir = await realpath(await mkdtemp(join(tmpdir(), 'amux-files-row-')));
  try {
    await mkdir(join(dir, 'pinned-dir'));
    const now = Date.now() / 1000;
    await writeFile(join(dir, 'old.txt'), 'old');
    await utimes(join(dir, 'old.txt'), now - 7200, now - 7200);
    await writeFile(join(dir, 'new.txt'), 'new');
    await utimes(join(dir, 'new.txt'), now - 60, now - 60);

    // Pinned and shared come from the server; stub only their reads so the
    // test owns its fixtures and leaves the server's prefs alone.
    const pins = [{ label: 'pinned-dir', path: join(dir, 'pinned-dir'), type: 'dir' }];
    await page.route(/\/api\/prefs\?key=files_bookmarks$/, r => r.request().method() === 'GET'
      ? r.fulfill({ json: { value: JSON.stringify(pins) } }) : r.fallback());
    await page.route(/\/api\/page-links$/, r => r.request().method() === 'GET' ? r.fulfill({ json: {
      links: [
        { id: 'a', kind: 'file', file: join(dir, 'elsewhere', 'report.txt'), files: 1, title: '', pending: false, path: '/pub/a/', url: 'https://example.test/pub/a/' },
        { id: 'b', kind: 'file', file: join(dir, 'waiting.txt'), files: 1, title: '', pending: true, path: '/pub/b/' },
        { id: 'c', kind: 'app', port: 9, file: '', title: 'an app', pending: false, path: '/pub/c/' },
      ], measured: true, n_considered: 3 } }) : r.fallback());
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.goto('/');
    await page.evaluate(p => {
      localStorage.removeItem('amux_files_row_filter');
      localStorage.setItem('amux_files_recent', JSON.stringify([{ path: p, type: 'file', at: Date.now() }]));
    }, join(dir, 'old.txt'));
    await openFiles(page, dir);
    await page.evaluate("_filesLoadBookmarks()");

    // 1. No Up button.
    const toolbar = page.locator('#files-view .fe-toolbar');
    await expect(toolbar.getByRole('button', { name: /^Up$/ })).toHaveCount(0);
    // 2. More is only the dots, still named for a screen reader, still opens.
    const more = page.locator('#files-overflow-btn');
    await expect(more).toHaveAttribute('aria-label', 'More');
    expect((await more.innerText()).trim()).toBe('');
    await more.click();
    await expect(page.locator('#files-overflow-menu')).toHaveClass(/open/);
    await page.evaluate("_filesOverflowClose()");

    // 3. The filter: left of the row, All by default.
    const sel = page.locator('#files-row-filter');
    await expect(sel).toHaveValue('all');
    const opts = await sel.locator('option').allTextContents();
    expect(opts).toEqual(['All', 'Pinned', 'Recent', 'Modified', 'Shared']);
    const selBox = (await sel.boundingBox())!;
    const listBox = (await page.locator('#files-bookmarks-list').boundingBox())!;
    expect(selBox.x, 'the filter sits left of the chips').toBeLessThan(listBox.x);

    await sel.selectOption('pinned');
    await expect.poll(() => sources(page)).toEqual(['pinned']);
    expect((await titles(page))[0]).toContain('pinned-dir');

    await sel.selectOption('recent');
    await expect.poll(() => titles(page)).toEqual(expect.arrayContaining([expect.stringContaining(dir.split('/').pop()!), expect.stringContaining('old.txt')]));
    expect(new Set(await sources(page))).toEqual(new Set(['recent']));

    await sel.selectOption('modified');
    await expect.poll(() => sources(page)).toEqual(['modified', 'modified']);
    const mod = await titles(page);
    expect(mod.findIndex(t => t.includes('new.txt')), 'newest first').toBeLessThan(mod.findIndex(t => t.includes('old.txt')));

    await sel.selectOption('shared');
    await expect.poll(() => titles(page)).toEqual([expect.stringContaining('report.txt')]);
    expect(await sources(page)).toEqual(['shared']);

    await sel.selectOption('all');
    await expect.poll(async () => new Set(await sources(page))).toEqual(new Set(['pinned', 'recent', 'modified', 'shared']));
    const paths = await chips(page).evaluateAll(els => els.map(e => (e as HTMLElement).dataset.path || e.getAttribute('title')));
    expect(paths.length, 'each path once').toBe(new Set(paths).size);
    await page.screenshot({ path: info.outputPath('files-row-all.png') });

    // One row: every chip and the filter sit in the row's single line.
    const row = (await page.locator('#files-bookmarks').boundingBox())!;
    for (const b of [selBox, ...(await chips(page).evaluateAll(els => els.map(e => { const r = e.getBoundingClientRect(); return { y: r.y, height: r.height }; })))]) {
      const mid = b.y + b.height / 2;
      expect(mid).toBeGreaterThanOrEqual(row.y);
      expect(mid).toBeLessThanOrEqual(row.y + row.height);
    }
    expect(await page.locator('#files-bookmarks-list').evaluate(e => getComputedStyle(e).flexWrap)).toBe('nowrap');
    if (page.viewportSize()!.width <= 600) {
      expect(selBox.height, 'the filter is a 44px touch target').toBeGreaterThanOrEqual(44);
      expect(row.height, 'the row is one line').toBeLessThanOrEqual(64);
    }

    // A chip opens what it names.
    await sel.selectOption('modified');
    await chips(page).filter({ hasText: 'new.txt' }).locator('span').first().click();
    await expect(page.locator('#file-overlay')).toHaveClass(/active/);
    await page.evaluate("typeof closeFilePreview === 'function' && closeFilePreview()");

    // 4. The choice survives a reload, on this device only.
    expect(await page.evaluate(() => localStorage.getItem('amux_files_row_filter'))).toBe('modified');
    await page.reload();
    await openFiles(page, dir);
    await expect(page.locator('#files-row-filter')).toHaveValue('modified');
    await expect.poll(() => sources(page)).toEqual(['modified', 'modified']);
  } finally {
    await cleanup('remove the temp folder', () => rm(dir, { recursive: true, force: true }), info);
  }
});
