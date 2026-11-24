// Deleting the worker an open peek is showing returns to the workers overview.
//
// Before this, the peek stayed on the deleted worker: a dead terminal, a
// poller that 404s, and nothing on screen saying it was gone. The overview is
// where the deletion is visible, and its filters (localStorage) are untouched.
// Delete and sessions are routed; the peek, the confirm dialog and deleteSession
// are real.
import {test, expect, Page} from './fixtures';

async function setup(page: Page, deletes: string[], deleteStatus = 200) {
  let alive = true;
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: alive
    ? [{name: 'doomed-worker', running: true, status: 'idle', dir: '/tmp/doomed'}]
    : []}));
  await page.route(/\/api\/sessions\/doomed-worker\/peek\?/, r => r.fulfill({json: {name: 'doomed-worker', live: 'Worker output', history: ''}}));
  await page.route(/\/api\/sessions\/doomed-worker\/subagents$/, r => r.fulfill({json: {session: 'doomed-worker', subagents: []}}));
  await page.route('**/api/client-debug', r => r.fulfill({json: {ok: true}}));
  await page.route(/\/api\/sessions\/doomed-worker\/delete$/, r => {
    deletes.push(r.request().method());
    if (deleteStatus === 200) alive = false;
    return r.fulfill({status: deleteStatus, json: deleteStatus === 200 ? {ok: true} : {ok: false, error: 'refused'}});
  });
  await page.goto('/', {waitUntil: 'domcontentloaded'});
  await page.waitForFunction("typeof openPeek === 'function' && typeof deleteSession === 'function'");
  await page.evaluate(() => (window as any).openPeek('doomed-worker'));
  await expect(page.locator('#peek-overlay')).toHaveClass(/active/);
}

async function confirmDelete(page: Page) {
  const run = page.evaluate(() => (window as any).deleteSession('doomed-worker'));
  await expect(page.locator('#modal-msg')).toHaveText('Delete worker "doomed-worker"?');
  await page.locator('#modal-btns .btn.danger').click();
  await run;
}

test('deleting the peeked worker closes the peek and lands on the workers overview', async ({page}, testInfo) => {
  const deletes: string[] = [];
  await setup(page, deletes);
  await confirmDelete(page);
  expect(deletes).toEqual(['POST']);
  await expect(page.locator('#peek-overlay')).not.toHaveClass(/active/);
  await expect.poll(() => page.evaluate('activeView')).toBe('sessions');
  await expect.poll(() => page.evaluate('peekSession')).toBeNull();
  expect(await page.evaluate(() => location.hash)).not.toContain('peek=');
  await expect(page.locator('#toast')).toContainText('Worker "doomed-worker" deleted');
  await page.screenshot({path: testInfo.outputPath('overview-after-delete.png')});
});

test('a refused delete keeps the peek open on the still-live worker', async ({page}) => {
  const deletes: string[] = [];
  await setup(page, deletes, 500);
  await confirmDelete(page);
  expect(deletes).toEqual(['POST']);
  await expect(page.locator('#peek-overlay')).toHaveClass(/active/);
  await expect.poll(() => page.evaluate('peekSession')).toBe('doomed-worker');
});
