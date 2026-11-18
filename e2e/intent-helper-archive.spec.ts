// Archiving the worker Intent sends unmatched requests to asks first (owner,
// 2026-11-18: "intent should not be making new workers"; tracing that found the
// standing helper `personal` archived since 11-17, so Intent dropped 9 goal
// check-ins and 45 of its schedules sat paused). The server refuses a bare
// archive of the helper with code intent_helper; the dashboard shows why and
// sends confirm_helper only on "Archive anyway". The route is answered by the
// test, so no worker is archived.
import { test, expect } from './fixtures';

test('archiving the standing helper says what breaks and asks first', async ({ page }) => {
  const posted: any[] = [];
  await page.route(/\/api\/sessions\/personal\/archive$/, async route => {
    const body = JSON.parse(route.request().postData() || '{}');
    posted.push(body);
    if (!body.confirm_helper) {
      return route.fulfill({ status: 409, json: { code: 'intent_helper',
        error: 'personal is where Intent sends requests no other worker fits. Archived, those requests are not delivered and its schedules pause.' } });
    }
    return route.fulfill({ json: { ok: true, message: 'archived' } });
  });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).archiveSession === 'function');

  // Cancel: one refused request, nothing more is sent.
  const first = page.evaluate(() => (window as any).archiveSession('personal'));
  const dialog = page.locator('#modal-backdrop.open').filter({ hasText: 'where Intent sends requests' });
  await expect(dialog).toBeVisible();
  await dialog.getByRole('button', { name: 'Cancel' }).click();
  await first;
  expect(posted).toEqual([{}]);

  // Archive anyway: the second request carries the confirmation.
  const second = page.evaluate(() => (window as any).archiveSession('personal'));
  await expect(dialog).toBeVisible();
  await dialog.getByRole('button', { name: 'Archive anyway' }).click();
  await second;
  expect(posted).toEqual([{}, {}, { confirm_helper: true }]);
  await expect(page.locator('.toast, #toast').filter({ hasText: 'personal archived' }).first()).toBeVisible();
});
