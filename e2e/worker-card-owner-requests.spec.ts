import { test, expect, Page } from './fixtures';

// AF-1060: Ethan asked "is it done" by message four times in a day (MSG-100790,
// MSG-100871, MSG-100902, MSG-100737) while the requests ledger already knew.
// The worker card now says how many of his asks are open and on main, on its
// one meta line, and a tap lists them.

async function appToken(page: Page): Promise<string> {
  const tok = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  expect(tok, 'served bootstrap must inject a non-empty auth token').toBeTruthy();
  return tok;
}

test('a worker card shows the owner requests that are open or on main, and lists them', async ({ page, request }, testInfo) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  const token = await appToken(page);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const worker = `e2e-asks-${testInfo.project.name}`;
  await request.post('/api/sessions', { headers: auth, data: { name: worker, dir: '/tmp', desc: 'e2e asks fixture' } });
  // Board cards in many states, so the meta line is LONGER than a phone card
  // (live at 375px on 2026-10-30, 7 of 36 lines ran past the edge).
  for (const st of ['doing', 'todo', 'backlog', 'review', 'done', 'needsyou']) {
    await request.post('/api/board', { headers: auth, data: { title: `e2e asks ${st}`, status: st, session: worker, type: 'chore' } });
  }

  const seen: string[] = [];
  await page.route(/\/api\/task-history\/requests\?/, route => {
    const session = new URL(route.request().url()).searchParams.get('session') || '';
    seen.push(session);
    const asks = session === worker;
    route.fulfill({ json: {
      measured: true, n_considered: asks ? 4 : 0, session, days: 7, repo: 'amux', deploys: true,
      counts: { open: asks ? 2 : 0, on_main: asks ? 1 : 0, live: asks ? 1 : 0, closed: 0 },
      requests: asks ? [
        { id: 'MSG-1', status: 'open', evidence: '', age_h: 3.5, text: 'make cmd k open the worker tab' },
        { id: 'MSG-2', status: 'open', evidence: '', age_h: 1, text: 'fix the editor button when zoomed' },
        { id: 'MSG-3', status: 'on-main', evidence: 'abc123def', age_h: 5, text: 'push to remote' },
        { id: 'MSG-4', status: 'live', evidence: 'def456abc', age_h: 9, text: 'already live' },
      ] : [],
    } });
  });

  await page.reload();
  await page.waitForLoadState('networkidle');
  const card = page.locator(`.session-card:has-text("${worker}"), [data-session="${worker}"]`).first();
  await expect(card).toBeVisible({ timeout: 15000 });
  const req = card.locator('.mc-req');
  await expect(req, 'the request count arrives once its fetch lands').toBeVisible({ timeout: 15000 });
  await expect(req).toHaveText('2 asks open · 1 on main');
  expect(seen).toContain(worker);
  // On the one meta line, not a new row.
  await expect(card.locator('.meta-count .mc-req')).toHaveCount(1);
  const box = (await req.boundingBox())!;
  if ((page.viewportSize()?.width || 1280) <= 600) {
    expect(box.height, 'a 44px touch target on a phone').toBeGreaterThanOrEqual(44);
  }
  // The count leads the line, so a line too long for the card never hides it.
  const cardBox = (await card.boundingBox())!;
  expect(box.x + box.width, 'the request count is inside the card, not clipped past its edge').toBeLessThanOrEqual(cardBox.x + cardBox.width);
  expect(await card.locator('.meta-count').evaluate(e => e.firstElementChild?.matches('[data-owner-req]')), 'it leads the meta line').toBe(true);

  await card.screenshot({ path: testInfo.outputPath(`owner-requests-card-${testInfo.project.name}.png`) });
  await req.click();
  const modal = page.locator('#modal-msg');
  await expect(modal).toContainText('Your requests to ' + worker);
  await expect(modal).toContainText('make cmd k open the worker tab');
  await expect(modal).toContainText('MSG-3 · on main, not live yet · abc123def');
  await expect(modal, 'live and closed asks are done, so not listed').not.toContainText('already live');
  await page.screenshot({ path: testInfo.outputPath(`owner-requests-${testInfo.project.name}.png`) });
  await page.locator('#modal-btns .btn').first().click();

  // A worker with no open asks and no board items adds no empty row.
  const quiet = `e2e-quiet-${testInfo.project.name}`;
  await request.post('/api/sessions', { headers: auth, data: { name: quiet, dir: '/tmp', desc: 'e2e quiet fixture' } });
  await page.reload();
  await page.waitForLoadState('networkidle');
  const qcard = page.locator(`.session-card:has-text("${quiet}"), [data-session="${quiet}"]`).first();
  await expect(qcard).toBeVisible({ timeout: 15000 });
  await expect.poll(() => seen.includes(quiet), { timeout: 15000 }).toBe(true);
  await expect(qcard.locator('.meta-count'), 'no board items and no asks: no meta row').toBeHidden();
  await request.delete(`/api/sessions/${quiet}`, { headers: auth });

  await request.delete(`/api/sessions/${worker}`, { headers: auth });
});
