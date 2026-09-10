import { test, expect } from '../fixtures';
import { boot, auth, deleteOwnedWorkers, getSessionsResilient } from './evidence';

test('LC-COORD-UNGROUPED: workers without groups retain peer and task awareness', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  await boot(page);
  const headers = await auth(page);
  const suffix = `${info.project.name}-${Date.now()}`;
  const grouped = `lc-grouped-${suffix}`, ungrouped = `lc-ungrouped-${suffix}`;
  try {
    for (const name of [grouped, ungrouped]) {
      const made = await request.post('/api/sessions', { headers,
        data: { name, dir: '/tmp', tags: name === grouped ? [`lc-team-${suffix}`] : [] } });
      expect(made.status()).toBe(201);
    }
    for (const [origin, target] of [[grouped, ungrouped], [ungrouped, grouped]]) {
      const peerHeaders = { ...headers, 'X-Amux-Worker': origin };
      const roster = await getSessionsResilient(request, peerHeaders);
      expect(roster.ok()).toBeTruthy();
      expect((await roster.json()).map((row: any) => row.name)).toContain(target);
      const made = await request.post('/api/board', { headers,
        data: { title: `Ungrouped awareness ${target}`, session: target, status: 'todo', type: 'chore' } });
      expect(made.status()).toBe(201);
      const card = await made.json();
      const detail = await request.get(`/api/board/${encodeURIComponent(card.id)}`, { headers: peerHeaders });
      expect(detail.ok()).toBeTruthy();
      expect(await detail.json()).toMatchObject({ id: card.id, session: target, title: card.title });
      await info.attach(`awareness-${origin}`, { body: JSON.stringify({ origin, target, card }), contentType: 'application/json' });
    }
  } finally {
    await deleteOwnedWorkers(page, request, headers, [grouped, ungrouped]);
  }
});
