import { test, expect, Page } from '@playwright/test';

// Ethan, 2026-08-11: "on the sched row in the card view of worker list page
// homepage put # of board items (total)".
//
// Seeds its own worker + cards: the e2e server starts with an empty board, and
// a count feature tested against zero data passes without rendering anything.

async function appToken(page: Page): Promise<string> {
  const tok = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  expect(tok, 'served bootstrap must inject a non-empty auth token').toBeTruthy();
  return tok;
}

test('worker card shows the truthful board status breakdown on the sched row', async ({ page, request }, testInfo) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  const token = await appToken(page);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };

  // A worker IS its env file, so create one the fleet list will show.
  // PER-PROJECT NAME: desktop and mobile run this test CONCURRENTLY against
  // ONE shared server, so a fixed name had both projects seeding the same
  // worker — CI read "2 doing · 6 active", every figure exactly doubled. The
  // first CI run of this spec is what caught it; locally a single project
  // never collides with itself.
  const worker = `e2e-count-${testInfo.project.name}`;
  await request.post('/api/sessions', {
    headers: auth,
    data: { name: worker, dir: '/tmp', desc: 'e2e count fixture' },
  });

  // One item in each actionable pre-terminal state. The card must preserve
  // those distinctions: parked work is not evidence that a worker is active.
  for (const st of ['doing', 'todo', 'backlog']) {
    const res = await request.post('/api/board', {
      headers: auth,
      data: { title: `e2e count ${st}`, status: st, session: worker, type: 'chore' },
    });
    expect(res.ok(), `seeding a ${st} card must succeed`).toBeTruthy();
  }

  await page.reload();
  await page.waitForLoadState('networkidle');

  const card = page.locator(`.session-card:has-text("${worker}"), [data-session="${worker}"]`).first();
  await expect(card, 'the seeded worker must appear in the card view').toBeVisible({ timeout: 15000 });

  const meta = card.locator('.meta-count');
  await expect(meta).toBeVisible();
  const text = (await meta.textContent()) || '';

  // The ASSERTION IS THE NUMBER, not merely that a badge is present: a counter
  // that renders the wrong figure is worse than none.
  //
  // This used to flatten todo/backlog into "active", which made idle workers
  // look busy. Assert the complete text and semantic hooks so that misleading
  // aggregation cannot return unnoticed.
  expect(text).toContain('1 doing · 1 todo · 1 backlog');
  await expect(card.locator('.mc-doing')).toHaveText('1');
  await expect(card.locator('.mc-todo')).toHaveText('1');
  await expect(card.locator('.mc-backlog')).toHaveText('1');

  // cleanup
  await request.delete(`/api/sessions/${worker}`, { headers: auth });
});
