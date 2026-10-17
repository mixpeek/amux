// Draw's toolbars on a phone (Ethan, 2026-10-16: "on mobile the draw toolbars
// should be positioned better"). On the iPhone PWA, Excalidraw padded its shape
// toolbar by the status-bar inset although amux's header already sits under the
// status bar, so the toolbar dropped about 47px and its right end slid under the
// lock/hand column, and amux's snap column floated over the middle of the
// drawing. A simulator reports no safe-area inset, so this sets the inset the
// way an iPhone does (--sat on :root, where Excalidraw declares it).
import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';
import { boot } from './lifecycle/evidence';

type Win = Window & { _draw: any; switchView: (v: string) => void };
type Box = { x: number; y: number; width: number; height: number };

async function openDraw(page: Page) {
  await boot(page);
  await page.waitForFunction(() => typeof (window as unknown as Win).switchView === 'function');
  await page.evaluate(() => (window as unknown as Win).switchView('draw'));
  await expect(page.locator('#draw-view')).toBeVisible();
  await page.waitForFunction(() => { const d = (window as unknown as Win)._draw; return !!(d && d.api && !d.opening); }, null, { timeout: 30_000 });
  await expect(page.locator('#draw-host .excalidraw')).toBeVisible();
}
const box = async (page: Page, sel: string): Promise<Box> => {
  const b = await page.locator(sel).first().boundingBox();
  if (!b) throw new Error(`no box for ${sel}`);
  return b;
};

test('Draw on a phone: the shape toolbar sits under the header and the side columns clear it', async ({ page }, info) => {
  test.skip(info.project.name === 'desktop', 'phone layout');
  await openDraw(page);
  // An iPhone PWA's status-bar inset, declared where Excalidraw declares it.
  await page.evaluate(() => document.documentElement.style.setProperty('--sat', '47px'));
  await page.waitForTimeout(300);
  const host = await box(page, '#draw-canvas-wrap');
  const bar = await box(page, '#draw-host .App-toolbar--mobile');
  const misc = await box(page, '#draw-host .mobile-misc-tools-container');
  const snap = await box(page, '#draw-snapbar');
  expect(await page.locator('#draw-host .excalidraw').evaluate(e => getComputedStyle(e).getPropertyValue('--sat').trim()),
    'the status-bar inset is not applied a second time under amux\'s header').toBe('0px');
  expect(bar.y - host.y, 'shape toolbar hugs the top of the canvas').toBeLessThan(30);
  expect(misc.y, 'the lock/hand column starts below the toolbar').toBeGreaterThanOrEqual(bar.y + bar.height);
  expect(snap.y, 'the snap column starts below the toolbar').toBeGreaterThanOrEqual(bar.y + bar.height);
  expect(Math.abs(snap.y - misc.y), 'the two side columns are top-aligned').toBeLessThan(16);
  const bottom = await box(page, '#draw-host .App-bottom-bar .App-toolbar');
  expect(snap.y + snap.height, 'the snap column clears the bottom bar').toBeLessThanOrEqual(bottom.y);
  const vw = page.viewportSize()!.width;
  expect(bar.x + bar.width).toBeLessThanOrEqual(vw);
  await page.screenshot({ path: info.outputPath('draw-mobile.png') });
});
