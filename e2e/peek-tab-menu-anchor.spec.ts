import { test, expect, devices } from '@playwright/test';
// Ethan's screenshot is a WIDE viewport (menu was right-pinned, button far left),
// so assert the anchor on a tablet-width viewport too, not just the phone.
for (const vp of [{name:'phone', w:375, h:812}, {name:'tablet', w:834, h:1112}]) {
  test(`peek customizer anchors under its button @${vp.name}`, async ({ browser }) => {
    const ctx = await browser.newContext({ ...devices['iPhone 11 Pro'],
      viewport: { width: vp.w, height: vp.h }, serviceWorkers: 'block' });
    // Menu placement is an existing-install scenario; this independent context
    // must initialize that prerequisite too, rather than clicking through onboarding.
    await ctx.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    const page = await ctx.newPage();
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any).openPeek === 'function', { timeout: 20000 });
    await page.evaluate(() => (window as any).openPeek('e2e-probe'));
    await page.waitForSelector('#peek-overlay', { state: 'visible', timeout: 15000 });
    await page.locator('#peek-tab-customize').click();
    // The presets list loads after the menu opens and widens it; the anchor tick
    // then re-places it. Read the SETTLED placement (nightly ios-safari,
    // 2026-10-30: read at a fixed 400ms, w 234 on Linux fonts, left still from the
    // narrower box). If re-placement stops, this times out and still fails.
    const measure = () => page.evaluate(() => {
      const m = document.getElementById('peek-tab-customizer-menu')!;
      const b = m.getBoundingClientRect();
      const btn = document.getElementById('peek-tab-customize')!.getBoundingClientRect();
      return { mLeft: Math.round(b.left), mRight: Math.round(b.right), w: Math.round(b.width),
               btnLeft: Math.round(btn.left), btnRight: Math.round(btn.right), vw: innerWidth,
               gap: Math.round(b.left - btn.left) };
    });
    const want = (r: any) => Math.max(8, Math.min(r.btnRight - r.w, r.vw - r.w - 8));
    await expect.poll(async () => { const r = await measure(); return Math.abs(r.mLeft - want(r)); },
      { timeout: 5000 }).toBeLessThanOrEqual(1);
    const r = await measure();
    console.log(`[${vp.name}] ` + JSON.stringify(r));
    expect(r.mRight, 'right edge off screen').toBeLessThanOrEqual(r.vw);
    expect(r.mLeft, 'left edge off screen').toBeGreaterThanOrEqual(0);
    // The real invariant: HANGS FROM THE BUTTON WHEN THERE IS ROOM, and is
    // otherwise pushed only as far as needed to stay on screen. Since d2ae1058
    // (zoom fix) it is RIGHT-aligned under the button, the way the worker-list
    // customizer hangs from its ⊞; clamping is the feature, not a miss.
    const ideal = Math.min(r.btnRight - r.w, r.vw - r.w - 8);
    const expected = Math.max(8, ideal);
    expect(Math.abs(r.mLeft - expected), `menu left ${r.mLeft} != expected ${expected} (btn ${r.btnLeft}-${r.btnRight}, w ${r.w}, vw ${r.vw})`).toBeLessThanOrEqual(1);
    await page.screenshot({ path: test.info().outputPath(`tab-menu-${vp.name}.png`) });
    await ctx.close();
  });
}
