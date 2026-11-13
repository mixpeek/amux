// Use case 9: a follow-up in an existing outcome goes back to the same
// worker, and "Change" moves a misrouted request to another worker.
import { test, expect } from '@playwright/test';
import { seed, deliveredSends, openHome, ask, reply, noJargon, outcomeLike, until } from './lib';

test('a follow-up stays with its helper, and Change moves a misrouted request', async ({ page, request }, info) => {
  await seed(request, info);
  const sends = await deliveredSends(request);
  await openHome(page);
  // Make sure both helpers exist, so one request can land on the wrong one.
  await ask(page, 'Plan dinners this week');
  await reply(page, /mon/i);
  await ask(page, 'What should Rafi bring to soccer practice?');
  await expect(page.locator('.home-msg.theirs .home-bubble').first()).toBeVisible();
  const first = await outcomeLike(request, /soccer|rafi/i);
  await page.locator('#home-input').fill('And what time does it start?');
  await page.locator('#home-send').click();
  await expect(page.locator('.home-msg.mine')).toHaveCount(2);
  const after = await until(async () => (await (await request.get('/api/intent/outcomes/' + first.id)).json()),
    (v) => (v.thread || []).filter((p: { author_key: string }) => p.author_key.startsWith('worker:')).length >= 2);
  expect(after.worker, 'the follow-up went to the same helper').toBe(first.worker);
  expect(after.decision.action).not.toBe('continue');
  // A request the router sends to the wrong helper, moved with Change.
  await ask(page, 'Does the school bus run on snow days?');
  await expect(page.locator('.home-receipt')).toContainText(/meal planner/i);
  await page.locator('.home-receipt').getByRole('button', { name: 'Change' }).click();
  await page.locator('label.home-choice').filter({ hasText: 'Family helper' }).click();
  await page.getByRole('button', { name: 'Move it' }).click();
  await expect(page.locator('.home-receipt')).toContainText(/family helper/i);
  await reply(page, /bus/i);
  const stats = await (await request.get('/api/intent/stats')).json();
  expect(stats.measured).toBe(true);
  expect(stats.overridden).toBeGreaterThanOrEqual(1);
  await noJargon(page);
  expect(await deliveredSends(request), 'nothing was sent outside').toBe(sends);
});
