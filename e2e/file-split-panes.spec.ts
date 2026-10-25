// "Open in split view" on several files puts each in a pane of its own, and the
// same right-click menu works on Files rows and board artifacts (Ethan,
// 2026-10-24: "finish all", after file-link-context-menu.spec.ts covered links
// in worker output and markdown).
import { test, expect, Page } from './fixtures';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const DIR = mkdtempSync(join(tmpdir(), 'amux-e2e-split-panes-'));
const FILES = ['one.md', 'two.txt', 'three.txt', 'four.txt'].map((f) => {
  const p = join(DIR, f);
  writeFileSync(p, `${f}\nSPLIT_PANES_PROOF ${f}\n`);
  return p;
});

async function openPeek(page: Page) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function', { timeout: 20000 });
  await page.evaluate(() => (window as any).openPeek('e2e-probe'));
  await page.waitForSelector('#peek-overlay', { state: 'visible', timeout: 15000 });
}

// Render the path through the shipped output pipeline and right-click it.
async function splitFile(page: Page, path: string) {
  await page.evaluate((p) => {
    const body = document.getElementById('peek-body')!;
    body.innerHTML = (window as any)._peekHtml('see ' + p + ' for details\n');
    const link = body.querySelector('.file-link, .md-link') as HTMLElement;
    const r = link.getBoundingClientRect();
    link.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.left + 4, clientY: r.top + 4 }));
  }, path);
  await page.locator('#file-ctx-menu .card-menu-item', { hasText: 'Open in split view' }).click();
}

const views = (page: Page) => page.locator('#psp-panes .psp-pane').evaluateAll((els) => els.map((e) => (e as HTMLElement).dataset.view));

test('several files open side by side in the split, one pane each', async ({ page }, info) => {
  test.skip((info.project.use.viewport?.width || 1280) <= 600, 'a phone has no split view');
  await openPeek(page);
  await splitFile(page, FILES[0]);
  await expect(page.locator('#peek-split-wrap')).toHaveClass(/split-active/);
  await expect.poll(() => views(page)).toEqual(['file:' + FILES[0]]);
  await expect(page.frameLocator('#psp-frame').locator('#file-body')).toContainText('SPLIT_PANES_PROOF one.md', { timeout: 15000 });

  await splitFile(page, FILES[1]);
  await splitFile(page, FILES[2]);
  await expect.poll(() => views(page)).toEqual(FILES.slice(0, 3).map((f) => 'file:' + f));
  // Each pane shows its own file, and its dropdown names it.
  const frames = page.locator('#psp-panes .psp-frame');
  await expect(frames).toHaveCount(3);
  for (let i = 0; i < 3; i++) {
    await expect(page.locator('#psp-panes .psp-pane').nth(i).locator('.psp-frame').contentFrame().locator('#file-body'))
      .toContainText('SPLIT_PANES_PROOF ' + FILES[i].split('/').pop(), { timeout: 15000 });
  }
  await expect(page.locator('#psp-panes .psp-pane').nth(1).locator('.psp-pane-view option:checked')).toHaveText('two.txt');

  // The same file again adds nothing; a fourth takes the last pane's place.
  await splitFile(page, FILES[1]);
  await expect.poll(() => views(page)).toEqual(FILES.slice(0, 3).map((f) => 'file:' + f));
  await splitFile(page, FILES[3]);
  await expect.poll(() => views(page)).toEqual([FILES[0], FILES[1], FILES[3]].map((f) => 'file:' + f));
  // The replaced pane shows the new file, not what its page was on before.
  await expect(page.locator('#psp-panes .psp-pane').nth(2).locator('.psp-frame').contentFrame().locator('#file-body'))
    .toContainText('SPLIT_PANES_PROOF four.txt', { timeout: 15000 });

  // A re-render keeps them.
  await page.evaluate(() => (window as any)._peekSplitRenderPanes());
  await expect.poll(() => views(page)).toEqual([FILES[0], FILES[1], FILES[3]].map((f) => 'file:' + f));
  await page.screenshot({ path: info.outputPath('three-file-panes.png') });

  // Closing a pane leaves the others.
  await page.locator('#psp-panes .psp-pane').nth(1).locator('.psp-pane-close').click();
  await expect.poll(() => views(page)).toEqual([FILES[0], FILES[3]].map((f) => 'file:' + f));
});

test('Files rows and board artifacts take the same right-click menu', async ({ page }, info) => {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function', { timeout: 20000 });
  await page.evaluate((d) => { (0, eval)('_filesPath = ' + JSON.stringify(d)); (window as any).switchView('files'); }, DIR);
  const row = page.locator('.fe-row[data-type="file"]', { hasText: 'two.txt' }).first();
  await expect(row).toBeVisible({ timeout: 15000 });
  await row.click({ button: 'right' });
  const labels = async () => (await page.locator('#file-ctx-menu .card-menu-item').allTextContents()).map((s) => s.trim().replace(/^\S+\s+/, ''));
  // No worker open, so no split to offer.
  expect(await labels()).toEqual(['Open', 'Open in new window', 'Copy path']);
  await page.screenshot({ path: info.outputPath('files-row-menu.png') });
  const popupP = page.waitForEvent('popup');
  await page.locator('#file-ctx-menu .card-menu-item', { hasText: 'Open in new window' }).click();
  const popup = await popupP;
  expect(decodeURIComponent(new URL(popup.url()).hash)).toBe('#path=' + FILES[1]);
  await expect(popup.locator('#file-body')).toContainText('SPLIT_PANES_PROOF two.txt', { timeout: 15000 });
  await popup.close();
  await page.keyboard.press('Escape');

  // A board artifact naming a file: same menu, absolute path as given.
  await page.evaluate((p) => {
    const host = document.createElement('div');
    host.id = 'e2e-artifact';
    host.style.cssText = 'position:fixed;left:40px;top:200px;z-index:5000;background:#fff;padding:8px';
    host.innerHTML = (window as any)._bdArtifactRef({ ref: p }, {});
    document.body.appendChild(host);
  }, FILES[2]);
  const art = page.locator('#e2e-artifact .board-artifact-file');
  await expect(art).toHaveAttribute('data-path', FILES[2]);
  await art.click({ button: 'right' });
  await page.locator('#file-ctx-menu .card-menu-item', { hasText: 'Copy path' }).waitFor();
  expect(await labels()).toEqual(['Open', 'Open in new window', 'Copy path']);
});
