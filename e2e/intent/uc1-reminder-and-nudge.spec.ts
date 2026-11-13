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
  // The conversation shows the reminder it set, and only that one: a reminder
  // the same helper sets later, for something else, stays out of it.
  await expect(page.locator('.home-reminder').filter({ hasText: /camp/i })).toBeVisible({ timeout: 30_000 });
  if (!process.env.AMUX_INTENT_URL) {
    const outcomes = (await (await request.get('/api/intent/outcomes')).json()).outcomes || [];
    const worker = outcomes.find((o: { title: string }) => /camp/i.test(o.title))?.worker;
    expect(worker, 'the camp request has a helper').toBeTruthy();
    await page.waitForTimeout(16_000);
    const when = new Date(Date.now() + 3 * 86400000).toISOString().slice(0, 16);
    expect((await request.post('/api/schedules', { data: { title: 'Reminder: renew the car registration', session: worker,
      kind: 'tmux', once: when, command: 'Post the reminder' } })).ok(), 'an unrelated reminder by the same helper').toBeTruthy();
    await until(async () => (await (await request.get('/api/intent/reminders')).json()).reminders || [],
      (v) => v.some((x: { title: string }) => /registration/i.test(x.title)), 30_000);
    // A repeating reminder (amux wake --every) is a reminder too; amux's own
    // shell checks are not.
    expect((await request.post('/api/schedules', { data: { title: 'Reminder: put the bins out', session: worker,
      schedule_expr: 'weekly on Sunday at 18:00', command: 'Post the reminder' } })).ok(), 'a repeating reminder').toBeTruthy();
    expect((await request.post('/api/schedules', { data: { title: 'check: goal inputs', session: worker, kind: 'shell',
      schedule_expr: 'every 2h', command: 'true' } })).ok(), 'a check').toBeTruthy();
    const rem = (await (await request.get('/api/intent/reminders')).json()).reminders || [];
    const bins = rem.find((x: { title: string }) => /bins/i.test(x.title));
    expect(bins?.every, 'listed, with how often it repeats').toBeTruthy();
    expect(rem.some((x: { title: string }) => /goal inputs/i.test(x.title)), 'a shell check is not a reminder').toBeFalsy();
    // A fresh render of the conversation, so the check reads current data.
    const id = outcomes.find((o: { title: string }) => /camp/i.test(o.title))?.id;
    await openHome(page);
    await page.locator(`.home-row[data-outcome="${id}"]`).click();
    await expect(page.locator('.home-reminder').filter({ hasText: /camp/i })).toBeVisible({ timeout: 30_000 });
    await expect(page.locator('.home-reminder').filter({ hasText: /registration/i })).toHaveCount(0);
  }
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
  // Doing the idea gives a draft Dana sends from her own mail; amux sends nothing.
  await card.getByRole('button', { name: 'Do it' }).click();
  const draft = page.locator('.home-draft').first();
  await expect(draft).toBeVisible({ timeout: 60_000 });
  await expect(draft).toContainText(/not sent/i);
  await expect(draft.getByRole('link', { name: 'Open in Mail' })).toHaveAttribute('href', /^mailto:marcy%40campkaylie\.test\?subject=Re/);
  await noJargon(page);
  expect(await deliveredSends(request), 'nothing was sent outside').toBe(sends);
});
