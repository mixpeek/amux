import { test, expect } from './fixtures';
import { getSessionsResilient } from './lifecycle/evidence';

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
          // One causal claim, not aggregate Doing order, is the runtime truth.
          // Leave the first card claimable so the real claim endpoint emits
          // task.claimed; the other three are deliberate unrelated Doing rows.
          status: i === 1 ? 'todo' : 'doing',
          session: worker,
          owner_type: 'agent',
          type: 'chore',
        },
      });
      expect(made.ok()).toBeTruthy();
      cards.push((await made.json()).id);
    }

    const claim = await request.post(`/api/board/${cards[0]}/claim`, {
      headers: { ...auth, 'X-Amux-Worker': worker },
    });
    expect(claim.ok(), 'fixture must create one exact causal owner').toBeTruthy();
    const claimed = (await claim.json()).id as string;
    expect(claimed).toBe(cards[0]);

    const claimedCardResponse = await request.get(`/api/board/${claimed}`, { headers: auth });
    expect(claimedCardResponse.ok()).toBeTruthy();
    expect(await claimedCardResponse.json()).toMatchObject({
      id: claimed,
      status: 'doing',
      session: worker,
    });

    // A stopped fixture has no physical pane, so the real status projection
    // correctly refuses to call it active even after a synthetic hook report.
    // Keep the real session and board projections, changing only the one field
    // this renderer consumes to reproduce an active hook while avoiding a real
    // model launch in CI.
    const sessionResponse = await getSessionsResilient(request, auth);
    const sessionRows = await sessionResponse.json();
    const row = sessionRows.find((s: any) => s.name === worker);
    expect(row?.runtime_board?.verdict).toBe('not-running');
    expect(row?.task_board_id).toBe('');

    // Preserve the real server's exact claimed identity while supplying only
    // the physical runtime state this renderer needs. Starting an actual tmux
    // model process would turn a deterministic browser golden into an external
    // side effect; a stopped registered worker is correctly `not-running`.
    row.status = 'active';
    row.running = true;
    row.task_board_id = claimed;
    row.runtime_board = {
      measured: true,
      n_considered: 4,
      verdict: 'linked',
      status: 'linked',
      card_id: claimed,
      card_live: true,
      violation: false,
    };
    await page.route(/\/api\/sessions(?:\?.*)?$/, route => route.fulfill({
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
