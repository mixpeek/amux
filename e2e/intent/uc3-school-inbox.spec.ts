// Use case 3: "Go through my inbox and find things for my kids' school"
// produces the items, and the inferred goal "Never miss a school form" appears
// under Goals with a status line.
import { test, expect } from '@playwright/test';
import { seed, deliveredSends, openHome, ask, reply, noJargon, pane, until } from './lib';

test('school items now, and a standing goal for school forms', async ({ page, request }, info) => {
  await seed(request, info);
  const sends = await deliveredSends(request);
  await openHome(page);
  await ask(page, "Go through my inbox and find things for my kids' school");
  await reply(page, /permission slip/i);
  await noJargon(page);
  const goals = await until(async () => (await (await request.get('/api/intent/goals')).json()).goals || [],
    (v) => v.some((g: { title: string }) => /school form/i.test(g.title)));
  const goal = goals.find((g: { title: string }) => /school form/i.test(g.title));
  expect(goal.sources.length, 'the goal records what it came from').toBeGreaterThan(0);
  expect(goal.schedule_id, 'the goal is checked on a schedule').toBeTruthy();
  await pane(page, 'goals');
  const card = page.locator('.home-card').filter({ hasText: /never miss a school form/i }).first();
  await expect(card).toBeVisible();
  await expect(card.locator('.home-status-line')).not.toHaveText(/^\s*$/);
  await noJargon(page);
  expect(await deliveredSends(request), 'nothing was sent outside').toBe(sends);
});
