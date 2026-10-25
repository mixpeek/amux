// A right-click menu opens where the pointer is (Ethan, 2026-10-24, Mac app
// screenshot: the file-link menu opened far below and right of the click).
// The terminal output carries its own CSS zoom (its text size), and WebKit
// scales a click's coordinates by it. Run on desktop (Chromium) and ios-safari
// (WebKit, the Mac app's engine): a real right-click, at several terminal zooms.
import { test, expect, Page } from './fixtures';

async function openPeek(page: Page) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function', { timeout: 20000 });
  await page.evaluate(() => (window as any).openPeek('e2e-probe'));
  await page.waitForSelector('#peek-overlay', { state: 'visible', timeout: 15000 });
}

for (const zoom of [1, 0.7, 1.3]) {
  test(`the file-link menu opens at the pointer with the terminal at ${zoom * 100}%`, async ({ page }, info) => {
    await openPeek(page);
    await page.evaluate((z) => {
      const body = document.getElementById('peek-body')!;
      body.innerHTML = '\n\n\n\n' + (window as any)._peekHtml('notes: see /tmp/e2e-menu-at-pointer/plan.md for details\n');
      body.style.zoom = String(z);
    }, zoom);
    const link = page.locator('#peek-body .md-link, #peek-body .file-link').first();
    await expect(link).toBeVisible();
    // The first line of the link: a wrapped link's box also covers the gap between lines.
    const box = await link.evaluate((el) => { const r = el.getClientRects()[0]; return { x: r.left, y: r.top, height: r.height }; });
    const px = box.x + 10, py = box.y + box.height / 2;
    await page.mouse.click(px, py, { button: 'right' });
    const menu = page.locator('#file-ctx-menu');
    await expect(menu).toBeVisible();
    const m = (await menu.boundingBox())!;
    await page.screenshot({ path: info.outputPath(`menu-at-${zoom * 100}.png`) });
    // At the click: its corner within a line of the pointer, not hundreds of pixels off.
    expect(Math.abs(m.x - px), `menu x ${m.x} vs pointer ${px}`).toBeLessThan(40);
    expect(m.y - py, `menu y ${m.y} vs pointer ${py}`).toBeGreaterThan(-30);
    expect(m.y - py, `menu y ${m.y} vs pointer ${py}`).toBeLessThan(40);
  });
}

// Real WebKit (Safari, the Mac app) reports a click inside a CSS-zoomed
// element in that element's zoomed units; headless WebKit does not, so the
// quirk is reproduced here directly: the same right-click, coordinates divided
// by the terminal's zoom. The menu must still open at the link.
for (const zoom of [0.6, 0.8]) {
  test(`a pointer reported in the terminal's zoomed units (${zoom * 100}%) still opens the menu at the link`, async ({ page }, info) => {
    await openPeek(page);
    const r = await page.evaluate((z) => {
      const body = document.getElementById('peek-body')!;
      body.innerHTML = '\n\n\n\n\n\n' + (window as any)._peekHtml('see /tmp/e2e-menu-at-pointer/plan.md\n');
      body.style.zoom = String(z);
      const link = body.querySelector('.md-link, .file-link') as HTMLElement;
      const b = link.getBoundingClientRect();
      const px = b.left + 10, py = b.top + b.height / 2;
      link.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: px / z, clientY: py / z }));
      return { px, py, bottom: b.bottom };
    }, zoom);
    const menu = page.locator('#file-ctx-menu');
    await expect(menu).toBeVisible();
    const m = (await menu.boundingBox())!;
    await page.screenshot({ path: info.outputPath(`menu-zoomed-units-${zoom * 100}.png`) });
    expect(Math.abs(m.x - r.px), `menu x ${m.x} vs link ${r.px}`).toBeLessThan(40);
    expect(Math.abs(m.y - r.bottom), `menu y ${m.y} vs link bottom ${r.bottom}`).toBeLessThan(40);
  });
}
