// Triage on the board (owner, 2026-10-31: "tell the worker to capture the
// task in a issue and keep it there dont automatically flush it"; "triage
// sounds right"; "implement it into the board view"). A card in triage carries
// the `triage` tag, which board-drive honours: it stays in backlog and nothing
// picks it up until it is accepted. Real server, no stubs.
import { test, expect, Page } from './fixtures';

async function card(page: Page, id: string): Promise<any> {
  return page.evaluate(i => fetch('/api/board/' + i, { headers: (window as any)._authHeaders() }).then(r => r.json()), id);
}

test('a card goes into triage from the board and is accepted from its detail', async ({ page }) => {
  const vp = page.viewportSize()!;
  // Backlog and To Do start collapsed for a new browser; open them, as a person would once.
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.setItem('amux_col_collapsed', '[]'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function' && typeof (window as any)._boardSetTriage === 'function');
  const created = await page.evaluate(() => fetch('/api/board', {
    method: 'POST', headers: (window as any)._authHeaders({ 'Content-Type': 'application/json' }),
    body: JSON.stringify({ title: 'Rustfs storage idea, keep for later', status: 'todo' }),
  }).then(r => r.json()));
  const id: string = created.id;
  expect(id).toBeTruthy();
  await page.evaluate(() => (window as any).switchView('board'));
  await page.evaluate(() => (window as any).fetchBoard());
  const cardEl = page.locator('.board-card[data-id="' + id + '"]').first();
  await expect(cardEl).toBeVisible();

  if (vp.width > 600) {
    // Desktop: right-click, Put in triage.
    await cardEl.click({ button: 'right' });
    await expect(page.locator('#board-ctx-triage')).toHaveText(/Put in triage/);
    await page.locator('#board-ctx-triage').click();
  } else {
    // Phone: the card's detail carries the same toggle.
    await cardEl.click();
    await expect(page.locator('#bd-triage-btn')).toHaveText('Triage');
    expect((await page.locator('#bd-triage-btn').boundingBox())!.height, 'touch target').toBeGreaterThanOrEqual(36);
    await page.locator('#bd-triage-btn').click();
  }
  await expect.poll(async () => (await card(page, id)).tags || []).toContain('triage');
  expect((await card(page, id)).status, 'a todo card put in triage moves to backlog').toBe('backlog');
  await expect.poll(async () => String((await card(page, id)).desc || '')).toContain('In triage');

  // The filter row leads with Triage and its count.
  await page.evaluate(() => (window as any).renderBoardFilters());
  await expect(page.locator('#board-triage-chip')).toContainText('Triage');

  // Accept from the card's detail (a backlog card may sit in a hidden column):
  // the tag goes, the card stays in backlog.
  await page.evaluate(i => (window as any).openBoardDetail(i), id);
  await expect(page.locator('#bd-triage-btn')).toHaveText('Accept');
  await page.locator('#bd-triage-btn').click();
  await expect.poll(async () => (await card(page, id)).tags || []).not.toContain('triage');
  expect((await card(page, id)).status).toBe('backlog');
  await expect(page.locator('#bd-triage-btn')).toHaveText('Triage');
});
