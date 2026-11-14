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
    // Set by the same helper inside this turn, but for ANOTHER conversation
    // (its prompt names that one): it stays out of this one.
    const soon = new Date(Date.now() + 2 * 86400000).toISOString().slice(0, 16);
    expect((await request.post('/api/schedules', { data: { title: 'Reminder: Hudson Prep statement', session: worker,
      kind: 'tmux', once: soon, command: "Post the reminder here with amux post 'o:oelsewhere1'" } })).ok()).toBeTruthy();
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
    // The same reminder set twice is listed once.
    expect((await request.post('/api/schedules', { data: { title: 'Reminder: put the bins out', session: worker,
      schedule_expr: 'weekly on Sunday at 18:00', command: 'Post the reminder' } })).ok(), 'the same again').toBeTruthy();
    const remBody = await (await request.get('/api/intent/reminders')).json();
    const rem = remBody.reminders || [];
    expect(rem.filter((x: { title: string }) => /bins/i.test(x.title)).length, 'one row per reminder').toBe(1);
    expect(remBody.folded, 'and the repeat is counted').toBeGreaterThan(0);
    const bins = rem.find((x: { title: string }) => /bins/i.test(x.title));
    expect(bins?.every, 'listed, with how often it repeats').toBeTruthy();
    expect(rem.some((x: { title: string }) => /goal inputs/i.test(x.title)), 'a shell check is not a reminder').toBeFalsy();
    // A fresh render of the conversation, so the check reads current data.
    const id = outcomes.find((o: { title: string }) => /camp/i.test(o.title))?.id;
    // A reload with a saved view and no remembered choice: the server's answer
    // alone has to bring simple mode back (her helpers do not count as a fleet).
    await page.evaluate(() => localStorage.removeItem('amux_home_choice'));
    await openHome(page);
    // The list shows the newest five; an older request is behind "Show all".
    const row = page.locator(`.home-row[data-outcome="${id}"]`);
    await expect(page.locator('#home-chat .home-row').first()).toBeVisible({ timeout: 20_000 });
    if (!(await row.isVisible())) await page.locator('#home-chat .home-link', { hasText: /show all/i }).click();
    await row.click();
    await expect(page.locator('.home-reminder').filter({ hasText: /camp/i })).toBeVisible({ timeout: 30_000 });
    await expect(page.locator('.home-reminder').filter({ hasText: /registration/i })).toHaveCount(0);
    await expect(page.locator('.home-reminder').filter({ hasText: /hudson/i }), 'another conversation\'s reminder').toHaveCount(0);
  }
  await noJargon(page);
  const ideas = await until(async () => (await (await request.get('/api/intent/ideas')).json()).ideas || [],
    (v) => v.some((i: { title: string }) => /camp kaylie|nudge/i.test(i.title)));
  const idea = ideas.find((i: { title: string }) => /camp kaylie|nudge/i.test(i.title));
  expect(idea, 'an idea to nudge the camp').toBeTruthy();
  expect(idea.sources.some((s: { kind: string }) => s.kind === 'email'), 'the idea records the camp email it came from').toBeTruthy();
  expect(idea.sources.some((s: { kind: string }) => s.kind === 'outcome'), 'and the request').toBeTruthy();
  await pane(page, 'ideas');
  const card = page.locator('#home-ideas .home-card').filter({ hasText: /camp kaylie|nudge/i }).first();
  await expect(card).toBeVisible();
  await expect(card).toContainText(/draft/i);
  await expect(card.locator('.home-card-from')).toContainText(/email/i);
  await noJargon(page);
  // An idea is a board card, tagged idea, kept off any helper's queue.
  const board = await (await request.get('/api/board?all=1&slim=0')).json();
  const ideaCard = (Array.isArray(board) ? board : []).find((c: { id: string }) => c.id === idea.id);
  expect(ideaCard, 'the idea is a board card').toBeTruthy();
  expect(ideaCard.status).toBe('backlog');
  expect(ideaCard.owner_type, 'owned by her, so nothing dispatches it').toBe('human');
  // Doing the idea: her helper writes the email through the email API, which
  // holds it for her OK. It shows in the conversation with Send and Discard.
  await card.getByRole('button', { name: 'Do it' }).click();
  const draft = page.locator('.home-draft').first();
  await expect(draft).toBeVisible({ timeout: 60_000 });
  await expect(draft).toContainText(/waiting for your OK/i);
  await expect(draft).toContainText('marcy@campkaylie.test');
  await expect(draft.getByRole('button', { name: 'Send email' })).toBeVisible();
  const heldNow = (await (await request.get('/api/email/approvals')).json()).pending || [];
  const mine = heldNow.find((p: { preview?: { subject?: string } }) => /Rafi Levin/.test(p.preview?.subject || ''));
  expect(mine?.preview?.conv, 'the held draft names the conversation it was written in').toMatch(/^o:o/);
  const held = (await (await request.get('/api/email/approvals')).json()).pending || [];
  expect(held.some((p: { preview?: { subject?: string } }) => /Rafi Levin/.test(p.preview?.subject || '')), 'held by the email API').toBeTruthy();
  // The same helper's draft for ANOTHER conversation (a goal's check) stays out
  // of this one: held drafts name their conversation.
  const doer = (await (await request.get('/api/intent/outcomes')).json()).outcomes.find((o: { title: string }) => /camp|nudge/i.test(o.title))?.worker;
  await request.post('/api/email/send', { headers: { 'X-Amux-Session': doer },
    data: { to: 'ortiz@maple.test', subject: "Rafi's reading logs", body: 'Hi Ms. Ortiz', conv: 'o:oelsewhere2' } });
  await page.waitForTimeout(5_000);
  await expect(page.locator('.home-draft').filter({ hasText: 'reading logs' })).toHaveCount(0);
  await noJargon(page);
  // Discard: the draft goes, and nothing was sent.
  await draft.getByRole('button', { name: 'Discard' }).click();
  await expect(page.locator('.home-draft')).toHaveCount(0, { timeout: 20_000 });
  const after = (await (await request.get('/api/email/approvals')).json()).pending || [];
  expect(after.some((p: { preview?: { subject?: string } }) => /Rafi Levin/.test(p.preview?.subject || '')), 'discarded').toBeFalsy();
  expect(await deliveredSends(request), 'nothing was sent outside').toBe(sends);
});
