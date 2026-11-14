// Use case 8: a deadline item such as open enrollment appears under Tracking
// with the days left. A to-do she asks for is a card on her own board,
// listed in Goals, and Done closes it.
import { test, expect } from '@playwright/test';
import { seed, deliveredSends, openHome, ask, reply, noJargon, pane, until } from './lib';

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

test('a to-do is a card on her board, listed in Goals, and Done closes it', async ({ page, request }, info) => {
  test.skip(!!process.env.AMUX_INTENT_URL, 'the stand-in helper files the card; a live run is a persona run');
  await seed(request, info);
  const what = `renew the passport ${info.project.name}`;
  await openHome(page);
  await ask(page, `Add ${what} to my to-dos`);
  await reply(page, /added/i);
  const todos = await until(async () => (await (await request.get('/api/intent/goals')).json()).todos || [],
    (v) => v.some((t: { title: string }) => t.title.includes(what)));
  const todo = todos.find((t: { title: string }) => t.title.includes(what));
  expect(todo, 'listed with the goals').toBeTruthy();
  const card = await (await request.get('/api/board/' + encodeURIComponent(todo.id))).json();
  expect(card.session || '', 'on her own board, not a helper\'s').toBe('');
  expect(card.owner_type, 'hers, so nothing dispatches it').toBe('human');
  // Goals was its own tab; an old link to it lands in Chat, where it lives now.
  await page.locator('#tab-everything').click();
  await expect(page.locator('#home-view')).toBeHidden();
  await page.goto('/#view=goals');
  await expect(page.locator('#home-view'), 'a saved Goals link opens Chat').toBeVisible({ timeout: 20_000 });
  await openHome(page);
  await pane(page, 'goals');
  const row = page.locator('.home-card').filter({ hasText: what });
  await expect(row).toBeVisible();
  await noJargon(page);
  await row.getByRole('button', { name: 'Done' }).click();
  await expect(row).toHaveCount(0, { timeout: 20_000 });
  expect((await (await request.get('/api/board/' + encodeURIComponent(todo.id))).json()).status).toBe('done');
});
