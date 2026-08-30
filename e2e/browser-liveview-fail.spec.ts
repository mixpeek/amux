import { test, expect } from './fixtures';
// Both server states, so the panel is asserted to DISCRIMINATE, not merely render.
for (const [label, running] of [['not-running', false], ['wedged', true]] as [string, boolean][]) {
  test(`browser live-view failure panel: ${label}`, async ({ page }) => {
    await page.route('**/api/browser/status', r =>
      r.fulfill({ contentType: 'application/json', body: JSON.stringify({ running }) }));
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any)._bwViewportFail === 'function', { timeout: 20000 });
    await page.evaluate(() => (window as any).switchView('browser'));
    // top-level `let` = global LEXICAL binding, not a window property — assigning
    // window._bwWantFrame silently does nothing and the function early-returns.
    await page.evaluate(() => { (0, eval)('_bwWantFrame = true; _bwHasFrame = false;'); });
    await page.evaluate(() => (window as any)._bwViewportFail('WebSocket protocol error: Connection reset without closing handshake'));
    await page.waitForTimeout(700);
    const r = await page.evaluate(() => {
      const ph = document.getElementById('bw-placeholder')!;
      const primary = ph.querySelector('.bw-btn.primary') as HTMLElement | null;
      return { heading: (ph.querySelector('div div') as HTMLElement)?.textContent || '',
               primary: primary?.textContent?.trim() || '',
               primaryOnclick: primary?.getAttribute('onclick') || '' };
    });
    console.log(`[${label}] ` + JSON.stringify(r));
    if (running === false) {
      expect(r.heading).toMatch(/No browser is running/);
      expect(r.primaryOnclick, 'primary action must START, not screenshot').toContain('_bwGo');
    } else {
      expect(r.heading).toMatch(/Live view unavailable/);
      expect(r.primaryOnclick, 'primary action should retry the frame').toContain('_bwScreenshot');
    }
  });
}
