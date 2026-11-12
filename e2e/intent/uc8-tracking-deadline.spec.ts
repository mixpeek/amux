// Use case 8: a deadline item such as open enrollment appears under Tracking
// with the days left.
import { test, expect } from '@playwright/test';
import { seed, deliveredSends, openHome, noJargon, pane, until } from './lib';

test('open enrollment under Tracking with the days left', async ({ page, request }, info) => {
  await seed(request, info);
  const sends = await deliveredSends(request);
  await until(async () => {
    const g = (await (await request.get('/api/intent/goals')).json()).goals || [];
    if (!g.some((x: { tier: string }) => x.tier === 'tracking')) await request.post('/api/intent/ideas/refresh', { data: {} });
    return g;
  }, (v) => v.some((g: { tier: string; title: string }) => g.tier === 'tracking' && /enrollment/i.test(g.title)));
  await openHome(page);
  await pane(page, 'goals');
  await expect(page.locator('.home-pane-title').filter({ hasText: 'Tracking' })).toBeVisible();
  const card = page.locator('.home-card').filter({ hasText: /open enrollment/i }).first();
  await expect(card).toContainText(/\d+ days left|due today|1 day left/);
  await noJargon(page);
  expect(await deliveredSends(request), 'nothing was sent outside').toBe(sends);
});
