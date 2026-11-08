import { test, expect } from './fixtures';

const NAME = 'toolbar-cold-start';
const TEXT = 'Review the saved human request and verify the worker toolbar';
const worker = { name: NAME, dir: '/tmp/', tags: [], running: true, status: 'idle',
  provider: 'codex', flags: '', active_model: 'gpt-6-astra' };
const message = { id: 1, text: TEXT, session: NAME, type: 'direct', origin: 'human', ts: 1000, time: 1000 };

async function cachedWorker(page: import('@playwright/test').Page) {
  await page.addInitScript(({ worker, message }) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.setItem('amux_sessions_cache', JSON.stringify([worker]));
    localStorage.setItem('amux_cmd_history', JSON.stringify([message]));
  }, { worker, message });
  await page.route('**/api/sessions', r => r.fulfill({ json: [worker] }));
  await page.route('**/api/history?*', r => r.fulfill({ json: [message] }));
  // The real endpoint names its worker; anonymous frames are correctly refused.
  await page.route('**/api/sessions/' + NAME + '/peek?*', r => r.fulfill({
    json: { name: NAME, output: '› ' + TEXT + '\nAssistant response\n', history: '' },
  }));
}

test('a cached worker deep link loads scoped history after all selection state initializes', async ({ page }) => {
  await cachedWorker(page);
  const errors: string[] = [];
  page.on('pageerror', e => errors.push(e.message));
  // This pins the order of boot, not its speed: measured 2026-11-02, the request
  // fired 8.0s after goto at load 22-39 on 28 cores, 4.7s of it fetching and
  // parsing app.js, so a 5s limit failed every run on a busy Mac.
  const scopedHistory = page.waitForRequest(r => r.url().includes('/api/history?')
    && new URL(r.url()).searchParams.get('session') === NAME, { timeout: 30_000 });
  await page.goto('/#peek=' + NAME);
  await scopedHistory;
  await expect.poll(() => page.evaluate(() => eval('_peekMsgRows?.length'))).toBe(1);
  await expect(page.locator('#peek-overlay')).toBeVisible();
  await expect(page.locator('#peek-body .peek-prompt-human')).toContainText(TEXT);
  await page.getByRole('button', {name:'Filter messages',exact:true}).click();
  await page.locator('[name="peek-filter-source"][value="human"]').check();
  await page.getByRole('dialog', {name:'Filter worker messages'}).getByRole('button', {name:'Done',exact:true}).click();
  await expect(page.locator('#peek-msg-count')).toHaveText('1');
  expect(errors.filter(e => e.includes('before initialization'))).toEqual([]);
});

test('client action failures reach the diagnostic log instead of only a toast', async ({ page }) => {
  await cachedWorker(page);
  const beacons: any[] = [];
  await page.route('**/api/client-debug', async r => {
    beacons.push(r.request().postDataJSON());
    await r.fulfill({ json: { ok: true } });
  });
  await page.goto('/#peek=' + NAME);
  await expect.poll(() => page.evaluate(() => eval('_peekMsgRows?.length'))).toBe(1);
  await page.evaluate(() => window.dispatchEvent(new ErrorEvent('error', { message: 'toolbar-observability-probe' })));
  await expect.poll(() => beacons.find(b => b.kind === 'client-action-error')).toMatchObject({
    verdict: 'script error', message: 'toolbar-observability-probe', measured: true, n_considered: 1,
  });
});
