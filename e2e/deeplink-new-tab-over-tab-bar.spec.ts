import { test, expect } from './fixtures';
import fs from 'fs';
import os from 'os';
import path from 'path';

// Owner, 2026-11-22: middle-clicked a path in a reply; "It did open a new tab
// but it did not deep link into that JSONL file." A browser tab with no tab
// bar of its own starts from the bar last used (the localStorage snapshot),
// including which of its tabs was open. When that was a tab after the first,
// its iframe covered the page, and the file the link opened sat hidden under
// it. A tab opened on a link now shows the link: its first tab, the page.
let file = '';
test.beforeAll(() => {
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'amux-linktab-')));
  file = path.join(dir, 'transcript.jsonl');
  fs.writeFileSync(file, '{"type":"note","text":"opened from a link"}\n');
});
test.afterAll(() => { if (file) fs.rmSync(path.dirname(file), { recursive: true, force: true }); });

// The bar another window left: two tabs, the second one open.
async function lastUsedBarOnSecondTab(page: import('@playwright/test').Page) {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    if (sessionStorage.getItem('seeded')) return;   // a reload keeps what the page wrote
    sessionStorage.setItem('seeded', '1');
    localStorage.setItem('amux_chrome_tabs2', JSON.stringify([{ id: 1, name: '', hash: '' }, { id: 2, name: '', hash: '#view=board' }]));
    localStorage.setItem('amux_chrome_active2', '2');
  });
}

test('a new browser tab opened on a file link shows the file, not the tab last open', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
  await lastUsedBarOnSecondTab(page);
  await page.goto('/#path=' + file);
  await page.waitForFunction(() => typeof (window as any)._chromeSwitchTab === 'function');
  expect(await page.evaluate('_chromeTabs.map(t => t.id)'), 'the bar is still the one last used').toEqual([1, 2]);
  expect(await page.evaluate('_chromeActiveId'), 'the link opens on the page, the first tab').toBe(1);
  await expect(page.locator('#chrome-tab-frames'), 'no tab covers the page').not.toHaveClass(/\bactive\b/);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  await expect(page.locator('#file-overlay')).toBeVisible();
  await expect(page.locator('#file-title')).toContainText('transcript.jsonl');
  // The other tab is one click away, on its own address.
  await page.evaluate('_chromeSwitchTab(2)');
  await expect(page.locator('#chrome-tab-frames iframe[data-tab-id="2"]')).toHaveAttribute('src', '/?_tab=2#view=board');
});

test('a reload keeps the tab that was open, whatever the page address says', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
  await lastUsedBarOnSecondTab(page);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._chromeSwitchTab === 'function');
  expect(await page.evaluate('_chromeActiveId')).toBe(2);
  // The page under the tabs is on a file, so its address is a #path= link.
  await page.evaluate(f => { history.replaceState({}, '', '/#path=' + f); }, file);
  await page.reload();
  await page.waitForFunction(() => typeof (window as any)._chromeSwitchTab === 'function');
  expect(await page.evaluate('_chromeActiveId'), 'a reload is not a link: tab 2 stays open').toBe(2);
  await expect(page.locator('#chrome-tab-frames')).toHaveClass(/\bactive\b/);
});
