// The Messages composer is the last row of a conversation (Ethan, 2026-10-30:
// "get rid of this row at the bottom"). A hint row under the box ("@person to
// notify a teammate ...") wrapped to two lines on a narrow window and to four
// beside an open thread. The placeholder already says what @ does.
import { test, expect } from './fixtures';

test('nothing sits under the Messages composer, with or without a thread open', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction("typeof switchView === 'function' && typeof _cvSelect === 'function'");
  const root = await page.evaluate(async () => {
    const r = await fetch(API + '/api/conversations/' + encodeURIComponent('g:general') + '/posts', {
      method: 'POST', headers: _authHeaders({ 'Content-Type': 'application/json' }), body: JSON.stringify({ body: 'thread root ' + Date.now() }) });
    return (await r.json()).id as number;
  });
  await page.evaluate("switchView('messages')");
  await page.evaluate("_cvSelect('g:general')");
  await expect(page.locator('#cv-compose')).toBeVisible();
  await expect(page.locator('#cv-hint')).toHaveCount(0);

  const gapUnder = async (compose: string, pane: string) => {
    const c = (await page.locator(compose).boundingBox())!;
    const p = (await page.locator(pane).boundingBox())!;
    return p.y + p.height - (c.y + c.height);
  };
  expect(await gapUnder('#cv-compose', '#cv-pane'), 'the composer is the bottom row').toBeLessThanOrEqual(1);

  // Beside an open thread the pane is narrow; still nothing under the box.
  await page.evaluate(id => (window as any)._cvOpenThread(id), root);
  await expect(page.locator('#cv-thread')).toBeVisible();
  if (await page.locator('#cv-pane').isVisible()) {
    expect(await gapUnder('#cv-compose', '#cv-pane')).toBeLessThanOrEqual(1);
  }
  await page.evaluate("_cvCloseThread()");
  await expect(page.locator('#cv-compose')).toBeVisible();
  expect(await gapUnder('#cv-compose', '#cv-pane')).toBeLessThanOrEqual(1);
});
