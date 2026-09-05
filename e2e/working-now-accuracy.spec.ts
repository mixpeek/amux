import { test, expect } from './fixtures';

test('one active worker marks exactly its claimed card as Working now', async ({ page, request }, testInfo) => {
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const worker = `working-now-${testInfo.project.name}-${Date.now()}`;
  const cards: string[] = [];

  try {
    expect((await request.post('/api/sessions', {
      headers: auth,
      data: { name: worker, dir: '/tmp', tags: ['e2e-working-now'] },
    })).status()).toBe(201);

    for (let i = 1; i <= 4; i += 1) {
      const made = await request.post('/api/board', {
        headers: auth,
        data: {
          title: `concurrent-looking task ${i}`,
          status: 'doing',
          session: worker,
          owner_type: 'agent',
          type: 'chore',
        },
      });
      expect(made.ok()).toBeTruthy();
      cards.push((await made.json()).id);
    }

    // A stopped fixture has no physical pane, so the real status projection
    // correctly refuses to call it active even after a synthetic hook report.
    // Keep the real session and board projections, changing only the one field
    // this renderer consumes to reproduce an active hook while avoiding a real
    // model launch in CI.
    const sessionResponse = await request.get('/api/sessions', { headers: auth });
    const sessionRows = await sessionResponse.json();
    const row = sessionRows.find((s: any) => s.name === worker);
    const claimed = row?.task_board_id || '';
    expect(claimed, 'the real server must project exactly one current doing card').toBeTruthy();
    row.status = 'active';
    await page.route('**/api/sessions', route => route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(sessionRows),
    }));

    await page.goto('/');
    await page.locator('#tab-board').click();
    await page.locator('#board-search').fill(`worker:${worker}`);
    await expect(page.locator('.board-card-live-label')).toHaveCount(1, { timeout: 15_000 });
    await expect(page.locator(`.board-card[data-id="${claimed}"] .board-card-live-label`)).toHaveText('Working now');
    for (const id of cards.filter(id => id !== claimed)) {
      await expect(page.locator(`.board-card[data-id="${id}"] .board-card-live-label`)).toHaveCount(0);
    }
    await expect(page.getByText('no board task claimed', { exact: false })).toHaveCount(0);
  } finally {
    for (const id of cards) await request.delete(`/api/board/${id}`, { headers: auth }).catch(() => {});
    await request.delete(`/api/sessions/${worker}`, { headers: auth }).catch(() => {});
  }
});
