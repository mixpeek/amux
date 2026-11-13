// Use case 6: "Trace the $388 car rental payment from Tova" produces a
// one-page reconciliation from email and calendar.
import { test, expect } from '@playwright/test';
import { seed, deliveredSends, openHome, ask, reply, noJargon } from './lib';

test('a reconciliation of the car rental split', async ({ page, request }, info) => {
  await seed(request, info);
  const sends = await deliveredSends(request);
  await openHome(page);
  await ask(page, 'Trace the $388 car rental payment from Tova');
  await reply(page, /776\.40/);
  const text = await page.locator('.cv-post[data-from="helper"] .cv-post-body').last().innerText();
  expect(text).toMatch(/388/);
  expect(text, 'it names the trip from the calendar').toMatch(/vermont/i);
  await noJargon(page);
  expect(await deliveredSends(request), 'nothing was sent outside').toBe(sends);
});
