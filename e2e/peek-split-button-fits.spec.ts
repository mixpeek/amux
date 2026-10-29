// Ethan, 2026-10-19, screenshot of the worker toolbar: "Split" squeezed against
// its caret with no icon. The toolbar's flex 0 0 44px pinned the button at
// 44px and the icon shrank to zero width. On a phone the same button is the
// folder icon in an even toolbar cell, which must not change.
import { test, expect } from './fixtures';

const W = 'e2e-split-fit';

test('the Split button shows its icon, label and caret without squeezing', async ({ page }) => {
  await page.route('**/api/sessions', route => route.request().method() === 'GET'
    ? route.fulfill({ json: [{ name: W, dir: '/tmp/', provider: 'claude', status: 'idle', running: true, tags: ['e2e'], desc: '', pinned: false, isolated: false }] })
    : route.fallback());
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    // Split is the Mac app's (a new pane): stand in for its pane bridge so the
    // button shows, as it does there.
    (window as any).webkit = { messageHandlers: { amuxPane: { postMessage: () => {} } } };
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(n => (window as any).openPeek(n), W);
  const btn = page.locator('#peek-split-toggle');
  await expect(btn).toBeVisible();
  const m = await btn.evaluate(b => {
    const box = (sel: string) => {
      const e = b.querySelector(sel) as HTMLElement;
      const r = e.getBoundingClientRect();
      return { shown: getComputedStyle(e).display !== 'none', w: r.width, right: r.right, clipped: e.scrollWidth > e.clientWidth + 1 };
    };
    const r = b.getBoundingClientRect();
    return { vw: innerWidth, w: r.width, h: r.height, right: r.right,
      icon: box('.psp-ico-split'), label: box('.psp-btn-lbl'), caret: box('.psp-btn-caret'), folder: box('.psp-ico-folder'),
      findLeft: document.getElementById('peek-find-toggle')!.getBoundingClientRect().left };
  });
  expect(m.h).toBeGreaterThanOrEqual(44);
  if (m.vw > 600) {
    // In the Mac app Split opens a pane at once, so it has no menu and no caret.
    expect(m.icon.shown && m.label.shown).toBe(true);
    expect(m.caret.shown).toBe(false);
    expect(m.icon.w).toBeGreaterThanOrEqual(17);
    expect(m.label.clipped).toBe(false);
    expect(m.w).toBeGreaterThan(44);
  } else {
    expect(m.folder.shown).toBe(true);
    expect(m.label.shown).toBe(false);
    expect(m.folder.w).toBeGreaterThanOrEqual(17);
  }
  expect(m.right).toBeLessThanOrEqual(m.findLeft + 0.5);
  await page.locator('.peek-toolbar').screenshot({ path: test.info().outputPath('peek-toolbar.png') });
});
