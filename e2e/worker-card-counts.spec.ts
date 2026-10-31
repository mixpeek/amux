import { test, expect, Page } from '@playwright/test';
import { cleanup } from './teardown';

// Ethan, 2026-08-11: "on the sched row in the card view of worker list page
// homepage put # of board items (total)".
//
// Seeds its own worker + cards: the e2e server starts with an empty board, and
// a count feature tested against zero data passes without rendering anything.

async function appTokens(page: Page): Promise<{ token: string; ui: string }> {
  // The app's own bootstrap, not networkidle: the dashboard polls, so under
  // load the network never goes quiet and the wait timed out (AF-1108).
  await page.waitForFunction(() => !!(window as any)._AMUX_AUTH_TOKEN, null, { timeout: 30000 });
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const ui = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN as string);
  expect(token, 'served bootstrap must inject a non-empty auth token').toBeTruthy();
  return { token, ui };
}

test('worker card shows the truthful board status breakdown on the sched row', async ({ page, request }, testInfo) => {
  test.setTimeout(60_000);
  await page.goto('/');
  const { token, ui } = await appTokens(page);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };

  // A worker IS its env file, so create one the fleet list will show.
  // PER-PROJECT NAME: desktop and mobile run this test CONCURRENTLY against
  // ONE shared server, so a fixed name had both projects seeding the same
  // worker — CI read "2 doing · 6 active", every figure exactly doubled.
  // PER-ATTEMPT TOO (AF-1108): a retry runs against the same server, and the
  // first attempt's cards were still there, so every retry read "2 doing ·
  // 2 todo · 2 backlog" and could never pass.
  const worker = `e2e-count-${testInfo.project.name}-${testInfo.retry}-${Date.now()}`;
  const cards: string[] = [];
  try {
    const made = await request.post('/api/sessions', {
      headers: auth,
      data: { name: worker, dir: '/tmp', desc: 'e2e count fixture' },
    });
    expect(made.ok(), 'creating the fixture worker must succeed').toBeTruthy();

    // One item in each actionable pre-terminal state. The card must preserve
    // those distinctions: parked work is not evidence that a worker is active.
    for (const st of ['doing', 'todo', 'backlog']) {
      const res = await request.post('/api/board', {
        headers: auth,
        data: { title: `e2e count ${st} ${worker}`, status: st, session: worker, type: 'chore' },
      });
      expect(res.ok(), `seeding a ${st} card must succeed`).toBeTruthy();
      cards.push((await res.json()).id);
    }

    await page.reload();
    await appTokens(page);
    // Ask for the list until the worker is in it. One page load is not enough
    // under load: /api/sessions can answer 503 (builder busy), the dashboard
    // shows "Sync error" with an empty list, and the card never appears.
    await expect.poll(() => page.evaluate(async (n) => {
      try { await (window as any).fetchSessions(); } catch { return false; }
      return (0, eval)('sessions').some((s: any) => s.name === n);
    }, worker), { timeout: 45_000, intervals: [1000, 2000, 3000] }).toBe(true);

    const card = page.locator(`.session-card:has-text("${worker}"), [data-session="${worker}"]`).first();
    await expect(card, 'the seeded worker must appear in the card view').toBeVisible({ timeout: 20000 });

    // The ASSERTION IS THE NUMBER, not merely that a badge is present: a counter
    // that renders the wrong figure is worse than none. It is a retrying
    // assertion: the counts can render a moment after the card does.
    //
    // This used to flatten todo/backlog into "active", which made idle workers
    // look busy. Assert the complete text and semantic hooks so that misleading
    // aggregation cannot return unnoticed.
    const meta = card.locator('.meta-count');
    await expect(meta).toContainText('1 doing · 1 todo · 1 backlog', { timeout: 15000 });
    await expect(card.locator('.mc-doing')).toHaveText('1');
    await expect(card.locator('.mc-todo')).toHaveText('1');
    await expect(card.locator('.mc-backlog')).toHaveText('1');
  } finally {
    for (const id of cards) {
      await cleanup(`delete fixture card ${id}`, () => request.delete(`/api/board/${id}`, { headers: auth }), testInfo);
    }
    await cleanup('delete fixture worker', () => request.post(`/api/sessions/${encodeURIComponent(worker)}/delete`,
      { headers: { ...auth, 'X-Amux-UI-Token': ui }, data: {} }), testInfo);
  }
});
