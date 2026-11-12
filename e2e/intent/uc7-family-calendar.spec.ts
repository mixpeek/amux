// Use case 7: "Stay ahead of the family calendar" produces a goal whose status
// line names the next event and what is ready for it.
import { test, expect } from '@playwright/test';
import { seed, deliveredSends, openHome, ask, noJargon, pane, until } from './lib';

test('a calendar goal whose status names the next event', async ({ page, request }, info) => {
  await seed(request, info);
  const sends = await deliveredSends(request);
  const events = await (await request.get('/api/cal-events')).json();
  const titles = (Array.isArray(events) ? events : events.events || []).map((e: { title: string }) => e.title);
  await openHome(page);
  await ask(page, 'Stay ahead of the family calendar');
  const goals = await until(async () => (await (await request.get('/api/intent/goals')).json()).goals || [],
    (v) => v.some((g: { title: string; status_line: string }) => /calendar/i.test(g.title) && titles.some((t: string) => g.status_line.includes(t))));
  const goal = goals.find((g: { title: string }) => /calendar/i.test(g.title));
  expect(titles.some((t: string) => goal.status_line.includes(t)), `status names an event: ${goal.status_line}`).toBeTruthy();
  await pane(page, 'goals');
  const card = page.locator('.home-card').filter({ hasText: /family calendar/i }).first();
  await expect(card.locator('.home-status-line')).toContainText(/next/i);
  await noJargon(page);
  expect(await deliveredSends(request), 'nothing was sent outside').toBe(sends);
});
