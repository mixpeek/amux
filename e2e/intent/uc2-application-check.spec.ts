// Use case 2: "Remind me to fill in Ezra's school application" becomes an
// Idea that checks email for the confirmation and deadline emails and gives a
// done or missing verdict.
import { test, expect } from '@playwright/test';
import { seed, deliveredSends, openHome, ask, reply, noJargon, pane, until } from './lib';

test("an idea that checks whether Ezra's application is done", async ({ page, request }, info) => {
  await seed(request, info);
  const sends = await deliveredSends(request);
  await openHome(page);
  await ask(page, "Remind me to fill in Ezra's school application");
  await reply(page, /remind/i);
  await until(async () => (await (await request.get('/api/intent/ideas')).json()).ideas || [],
    (v) => v.some((i: { title: string }) => /application/i.test(i.title)));
  await pane(page, 'ideas');
  const card = page.locator('#home-ideas .home-card').filter({ hasText: /application/i }).first();
  await expect(card).toBeVisible();
  await noJargon(page);
  await card.getByRole('button', { name: 'Do it' }).click();
  await expect(page.locator('#home-chat')).toBeVisible();
  await reply(page, /verdict:\s*(done|missing)/i);
  await noJargon(page);
  expect(await deliveredSends(request), 'nothing was sent outside').toBe(sends);
});
