// A new amux Cloud server's first visit (MSG-91014, 2026-10-21): "Get started" (connect a
// model) and the walkthrough both open for an empty server, and the window covered the
// walkthrough's Skip. The walkthrough now waits until the window is closed.
import { test, expect } from './fixtures';

test('a new Cloud server shows Get started first, then the walkthrough', { annotation: { type: 'walkthrough' } }, async ({ page }) => {
  // A Cloud owner with no model connected, on a server with no workers yet.
  await page.route('**/api/identity', async r => {
    const res = await r.fetch();
    const d = await res.json();
    await r.fulfill({ response: res, json: { ...d, is_cloud: true, has_any_model: false, has_api_key: false } });
  });
  await page.route('**/api/sessions', async r => {
    if (r.request().method() !== 'GET') return r.fallback();
    await r.fulfill({ json: [] });
  });
  await page.addInitScript(() => { try { localStorage.removeItem('amux_walkthrough_done'); } catch (_) {} });
  await page.goto('/');

  const gs = page.locator('#apikey-setup-modal');
  await expect(gs).toBeVisible({ timeout: 15000 });
  // Long past the walkthrough's 1.5 s start: it is waiting, not drawn underneath.
  await page.waitForTimeout(4000);
  await expect(page.locator('#wt-overlay.open')).toHaveCount(0);

  await gs.locator('button[title="Close"]').click();
  await expect(gs).toBeHidden();
  const skip = page.locator('.wt-skip');
  await expect(skip).toBeVisible({ timeout: 10000 });
  await skip.click();   // reachable: nothing covers it
  await expect(page.locator('#wt-overlay.open')).toHaveCount(0);
});
