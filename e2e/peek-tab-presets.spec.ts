// The worker view's tab menu (Ethan, 2026-10-24, Mac app screenshot):
// 1. it hangs from the ⊞ that was clicked. The Mac app's WebKit reported the
//    sticky ⊞'s box far left of where it paints it, so the menu opened ~200px
//    left of the button. A click outside the reported box now places the menu.
// 2. it saves presets server side, like the homepage tab list (scope=peek).
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

async function openWorkerView(page: import('@playwright/test').Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function', { timeout: 20000 });
  await page.evaluate(() => (window as any).openPeek('e2e-probe'));
  await page.waitForSelector('#peek-overlay', { state: 'visible', timeout: 15000 });
}

test('the worker tab menu hangs from the ⊞ that was clicked, even when its box is misreported', async ({ page }, info) => {
  test.skip((page.viewportSize()?.width || 0) <= 600, 'phones get the bottom sheet, which is not anchored to the button');
  const beacons: any[] = [];
  await page.route('**/api/client-debug', async r => { beacons.push(r.request().postDataJSON()); await r.fulfill({ json: { ok: true } }); });
  await openWorkerView(page);
  // What the Mac app's WebKit did: the ⊞ reports a box 400px left of where it
  // is painted. Playwright clicks where it is painted.
  await page.evaluate(() => {
    const b = document.getElementById('peek-tab-customize')!;
    const real = Element.prototype.getBoundingClientRect;
    (b as any).getBoundingClientRect = function () { const r = real.call(this); return new DOMRect(r.x - 400, r.y, r.width, r.height); };
  });
  await page.locator('#peek-tab-customize').click();
  await expect(page.locator('#peek-tab-customizer-menu')).toBeVisible();
  await page.waitForTimeout(300);
  const g = await page.evaluate(() => {
    const real = Element.prototype.getBoundingClientRect;
    const btn = real.call(document.getElementById('peek-tab-customize')!);
    const m = document.getElementById('peek-tab-customizer-menu')!.getBoundingClientRect();
    return { btnRight: btn.right, btnBottom: btn.bottom, mRight: m.right, mTop: m.top, mW: m.width, vw: innerWidth };
  });
  const expectedRight = Math.min(g.btnRight, g.vw - 8);
  expect(Math.abs(g.mRight - expectedRight), JSON.stringify(g)).toBeLessThanOrEqual(2);
  expect(Math.abs(g.mTop - (g.btnBottom + 2)), JSON.stringify(g)).toBeLessThanOrEqual(2);
  await expect.poll(() => beacons.find(b => b.kind === 'tabcust-geo' && b.which === 'peek')?.verdict).toBe('anchor_rect_mismatch');
  await page.screenshot({ path: info.outputPath('peek-tab-menu-anchored.png') });
});

// Through the page: it carries the dashboard's auth, which a bare request does not.
const api = (page: import('@playwright/test').Page, path: string, method = 'GET') =>
  page.evaluate(async ([p, m]) => { const r = await fetch(p, { method: m }); return r.json(); }, [path, method] as const);

test('a worker tab preset is saved on the server, loads back, and stays out of the homepage list', async ({ page }, info) => {
  const name = `e2e-peek-${info.project.name}-${Date.now()}`;
  await openWorkerView(page);
  const before = await page.evaluate(() => (0, eval)('JSON.stringify([...peekHiddenTabs])'));
  try {
    const tab = await page.evaluate(() => (0, eval)('PEEK_TABS.map(t => t.id).find(id => !PEEK_REQUIRED_TABS.has(id))'));
    const open = async () => {
      if (!(await page.locator('#peek-tab-customizer-menu').isVisible())) await page.locator('#peek-tab-customize').click();
      await expect(page.locator('#peek-tab-customizer-menu .tab-preset-section')).toBeVisible();
    };
    const box = () => page.locator(`#peek-tab-customizer-menu label[data-ptab-id="${tab}"] input`);
    await open();
    await box().check();
    await box().uncheck();
    await expect(page.locator(`#peek-tab-${tab}`)).toBeHidden();

    // Save the current layout as a preset, through the shared prompt.
    await page.locator('#peek-tab-customizer-menu .tab-preset-section button', { hasText: '+ Save current' }).click();
    await page.locator('#modal-prompt-input').fill(name);
    await page.locator('#modal-prompt-input').press('Enter');
    await expect.poll(async () => (await api(page, '/api/layout-presets?scope=peek')).map((p: any) => p.name)).toContain(name);
    const saved = (await api(page, '/api/layout-presets?scope=peek')).find((p: any) => p.name === name);
    expect(saved.hidden).toContain(tab);
    const homepage = (await api(page, '/api/layout-presets')).map((p: any) => p.name);
    expect(homepage, 'a worker-view preset is not a homepage preset').not.toContain(name);

    // Change the layout, then load the preset back.
    await open();
    await box().check();
    await expect(page.locator(`#peek-tab-${tab}`)).toBeVisible();
    await page.locator('#peek-tab-customizer-menu .preset-list[data-scope="peek"] button', { hasText: name }).click();
    await expect(page.locator(`#peek-tab-${tab}`)).toBeHidden();
    await expect(box()).not.toBeChecked();
    await page.screenshot({ path: info.outputPath('peek-tab-preset.png') });
  } finally {
    await cleanup('delete the peek preset', () => api(page, '/api/layout-presets/' + encodeURIComponent(name) + '?scope=peek', 'DELETE'), info);
    // Put the worker-view layout back: it is saved server side for every spec.
    await cleanup('restore the worker-view tabs', () => page.evaluate(h => { (0, eval)('peekHiddenTabs = new Set(' + h + '); _savePeekTabPrefs(); _applyPeekTabVisibility()'); }, before), info);
  }
});
