// A move the server refuses goes back to its column at once, with the reason in
// a toast. It used to stay where it was dropped until a reload (VID-032).
import { test, expect } from './fixtures';

test('a refused board move snaps back, and a newer move or an accepted one does not', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).moveBoardItem === 'function');
  const headers = { 'Content-Type': 'application/json' };
  const id = await page.evaluate(async h => {
    const r = await fetch('/api/board', { method: 'POST', headers: h, body: JSON.stringify({ title: 'zz refused move ' + Date.now(), status: 'todo' }) });
    return (await r.json()).id;
  }, headers);
  await page.evaluate(() => (window as any).switchView('board'));
  await page.waitForFunction(i => (window as any).eval('boardItems').some((b: any) => b.id === i), id);
  const status = () => page.evaluate(i => (window as any).eval('boardItems').find((b: any) => b.id === i).status, id);
  let refuse = true;
  await page.route('**/api/board/' + id, route => route.request().method() === 'PATCH' && refuse
    ? route.fulfill({ status: 409, json: { error: 'this card needs its gate first' } })
    : route.continue());
  // No board read may repair the card: only the rollback can put it back.
  await page.route(/\/api\/board(\?|$)/, route => refuse && route.request().method() === 'GET' ? route.abort() : route.continue());
  await page.evaluate(() => fetch('/api/board').catch(() => null));
  await page.evaluate(i => (window as any).moveBoardItem(i, 'done'), id);
  await expect.poll(status).toBe('todo');
  await expect(page.locator('#toast, .toast').first()).toContainText('gate');
  refuse = false;
  await page.evaluate(i => (window as any).moveBoardItem(i, 'backlog'), id);
  await page.waitForTimeout(500);
  expect(await status()).toBe('backlog');
});
