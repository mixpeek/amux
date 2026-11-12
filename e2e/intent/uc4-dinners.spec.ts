// Use case 4: "Plan dinners this week" produces a plan, and an inferred goal
// for meal planning appears.
import { test, expect } from '@playwright/test';
import { seed, deliveredSends, openHome, ask, reply, noJargon, pane, until } from './lib';

test('a dinner plan, and a meal-planning goal', async ({ page, request }, info) => {
  await seed(request, info);
  const sends = await deliveredSends(request);
  await openHome(page);
  await ask(page, 'Plan dinners this week');
  await reply(page, /mon/i);
  await noJargon(page);
  await until(async () => (await (await request.get('/api/intent/goals')).json()).goals || [],
    (v) => v.some((g: { title: string }) => /dinner|meal/i.test(g.title)));
  await pane(page, 'goals');
  await expect(page.locator('.home-card').filter({ hasText: /dinner|meal/i }).first()).toBeVisible();
  await noJargon(page);
  expect(await deliveredSends(request), 'nothing was sent outside').toBe(sends);
});
