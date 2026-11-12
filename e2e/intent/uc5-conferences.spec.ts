// Use case 5: "Rank my parent-teacher conference bookings before registration
// opens" produces a ranked order from the kids' grades and due work.
import { test, expect } from '@playwright/test';
import { seed, deliveredSends, openHome, ask, reply, noJargon } from './lib';

test('a ranked booking order from grades and missing work', async ({ page, request }, info) => {
  await seed(request, info);
  const sends = await deliveredSends(request);
  await openHome(page);
  await ask(page, 'Rank my parent-teacher conference bookings before registration opens');
  await reply(page, /1\.?\s*\**\s*Mr\.? Brandt|math/i);
  const text = await page.locator('.home-msg.theirs .home-bubble').last().innerText();
  expect(text.search(/brandt|math/i), 'math (the weakest grade) ranks first').toBeLessThan(text.search(/hale|social studies/i));
  await noJargon(page);
  expect(await deliveredSends(request), 'nothing was sent outside').toBe(sends);
});
