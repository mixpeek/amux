// Right-click a file link in worker output to open it in the split view or a
// new window (Ethan, 2026-10-24, screenshot of the browser's own menu on a
// path in the Mac app). The link is rendered by the shipped pipeline
// (_peekHtml) and the menu is opened by a real contextmenu event.
import { test, expect, Page } from './fixtures';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const DIR = mkdtempSync(join(tmpdir(), 'amux-e2e-ctx-'));
const FILE = join(DIR, 'ctx-note.md');
writeFileSync(FILE, '# Context menu note\n\nopened from a right-click\n');
mkdirSync(join(DIR, 'sub'));
writeFileSync(join(DIR, 'sub', 'rel-note.md'), '# Relative note\n\nfound through the worker cwd\n');

async function openPeek(page: Page) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function', { timeout: 20000 });
  await page.evaluate(() => (window as any).openPeek('e2e-probe'));
  await page.waitForSelector('#peek-overlay', { state: 'visible', timeout: 15000 });
}

// Render and right-click in one evaluate: #peek-body repaints on a poll, but
// the menu lives on <body>, so it survives the next paint.
async function rightClickPath(page: Page, path: string, via: 'peek' | 'chat' = 'peek') {
  const n = await page.evaluate(([p, v]) => {
    const body = document.getElementById('peek-body')!;
    const w = window as any;
    // Chat links paths with _chatLinkify (only _linkifyPaths); peek output
    // goes through the full _peekHtml pipeline.
    body.innerHTML = v === 'chat' ? w._chatLinkify(w.esc('see ' + p + ' for details')) : w._peekHtml('see ' + p + ' for details\n');
    const link = body.querySelector('.file-link, .md-link') as HTMLElement | null;
    if (!link) return 0;
    const r = link.getBoundingClientRect();
    link.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.left + 4, clientY: r.top + 4 }));
    return 1;
  }, [path, via] as const);
  expect(n, 'the path rendered as a link').toBe(1);
  await expect(page.locator('#file-ctx-menu')).toBeVisible();
}
const items = (page: Page) => page.locator('#file-ctx-menu .card-menu-item').allTextContents();

test('right-click a file link: open it in a new window or beside the worker in the split view', async ({ page }, info) => {
  await openPeek(page);
  await rightClickPath(page, FILE);
  const labels = (await items(page)).map(s => s.trim().replace(/^\S+\s+/, ''));
  const phone = (info.project.use.viewport?.width || 1280) <= 600;
  expect(labels).toEqual(phone ? ['Open', 'Open in new window', 'Copy path']
    : ['Open', 'Open in split view', 'Open in new window', 'Copy path']);
  await page.screenshot({ path: info.outputPath('file-link-menu.png') });

  // Escape closes it without acting.
  await page.keyboard.press('Escape');
  await expect(page.locator('#file-ctx-menu')).toHaveCount(0);

  // New window: the file's own #path= link, opened from the click itself.
  await rightClickPath(page, FILE);
  const popupP = page.waitForEvent('popup');
  await page.locator('#file-ctx-menu .card-menu-item', { hasText: 'Open in new window' }).click();
  const popup = await popupP;
  expect(decodeURIComponent(new URL(popup.url()).hash)).toBe('#path=' + FILE);
  await expect(popup.locator('#file-overlay.active')).toBeVisible({ timeout: 15000 });
  await expect(popup.locator('#file-body')).toContainText('opened from a right-click');
  await popup.close();
  await expect(page.locator('#file-ctx-menu')).toHaveCount(0);

  // In the Chat tab a bare relative path is linked by _linkifyPaths alone,
  // which used to leave no data-path for the menu to read.
  await openPeek(page);
  await page.evaluate((cwd) => (0, eval)('peekSessionDir = ' + JSON.stringify(cwd)), DIR);
  await rightClickPath(page, 'sub/rel-note.md', 'chat');
  const relPop = page.waitForEvent('popup');
  await page.locator('#file-ctx-menu .card-menu-item', { hasText: 'Open in new window' }).click();
  const rp = await relPop;
  await expect(rp.locator('#file-body')).toContainText('found through the worker cwd', { timeout: 15000 });
  await rp.close();

  if (phone) return;
  // Split view: the first pane loads the Files view on this file, and peek stays up.
  // e2e-probe is not a live worker, so peek may have closed itself meanwhile.
  await openPeek(page);
  await rightClickPath(page, FILE);
  await page.locator('#file-ctx-menu .card-menu-item', { hasText: 'Open in split view' }).click();
  await expect(page.locator('#peek-split-wrap')).toHaveClass(/split-active/);
  const frame = page.locator('#psp-frame');
  await expect(frame).toHaveAttribute('src', /embed=files#path=/);
  expect(decodeURIComponent((await frame.getAttribute('src'))!.split('#path=')[1])).toBe(FILE);
  const pane = page.frameLocator('#psp-frame');
  await expect(pane.locator('#file-overlay.active')).toBeVisible({ timeout: 15000 });
  await expect(pane.locator('#file-body')).toContainText('opened from a right-click');
  await expect(page.locator('#peek-overlay')).toBeVisible();
  await page.screenshot({ path: info.outputPath('file-link-split.png') });
});
