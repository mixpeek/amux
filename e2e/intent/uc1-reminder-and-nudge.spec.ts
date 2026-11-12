// Use case 1: "Remind me to call the camp director about Rafi's waitlist"
// becomes a reminder AND an Idea offering to draft a nudge from the earlier
// camp email thread.
import { test, expect } from '@playwright/test';
import { seed, deliveredSends, openHome, ask, reply, noJargon, pane, until } from './lib';

test('a reminder, and an idea to draft a nudge from the camp thread', async ({ page, request }, info) => {
  await seed(request, info);
  const sends = await deliveredSends(request);
  await openHome(page);
  await noJargon(page);
  await ask(page, "Remind me to call the camp director about Rafi's waitlist");
  await reply(page, /remind/i);
  const schedules = await until(async () => (await (await request.get('/api/schedules')).json()),
    (v) => (Array.isArray(v) ? v : v.schedules || []).some((s: { title: string }) => /camp|rafi/i.test(s.title)));
  expect((Array.isArray(schedules) ? schedules : schedules.schedules || []).some((s: { title: string }) => /camp|rafi/i.test(s.title)),
    'the reminder is a schedule').toBeTruthy();
  await noJargon(page);
  const ideas = await until(async () => (await (await request.get('/api/intent/ideas')).json()).ideas || [],
    (v) => v.some((i: { title: string }) => /camp kaylie|nudge/i.test(i.title)));
  const idea = ideas.find((i: { title: string }) => /camp kaylie|nudge/i.test(i.title));
  expect(idea, 'an idea to nudge the camp').toBeTruthy();
  expect(idea.sources.some((s: { kind: string }) => s.kind === 'email'), 'the idea records the camp email it came from').toBeTruthy();
  expect(idea.sources.some((s: { kind: string }) => s.kind === 'outcome'), 'and the request').toBeTruthy();
  await pane(page, 'ideas');
  const card = page.locator('.home-card').filter({ hasText: /camp kaylie|nudge/i }).first();
  await expect(card).toBeVisible();
  await expect(card).toContainText(/draft/i);
  await expect(card.locator('.home-card-from')).toContainText(/email/i);
  await noJargon(page);
  expect(await deliveredSends(request), 'nothing was sent outside').toBe(sends);
});
