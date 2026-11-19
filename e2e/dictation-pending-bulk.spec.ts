// Pending dictation clips can be retried or cleared all at once (owner,
// 2026-11-19, a "11+5" Dictation tab with 5 pending: "I should have a way to
// clear, try again or whatever these"). The two buttons sit on the PENDING
// heading's own row, so a phone gets no new row, and they are 44px to tap there.
import { test, expect, Page } from './fixtures';

const W = 'zz-dict-bulk';

async function boot(page: Page, clips: number) {
  const sent: number[] = [];
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: W, dir: '/tmp/dict-bulk', running: true, status: 'idle', flags: '', steering: [] }] }));
  page.on('request', r => { if (/\/api\/dictate\?/.test(r.url())) sent.push((r.postDataBuffer() || Buffer.alloc(0)).length); });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => (globalThis as any).eval('typeof _idb') === 'object'
    && (globalThis as any).eval('typeof _dictOutboxAdd') === 'function' && typeof (window as any).openPeek === 'function');
  // Hold the app's own send pass so the clips stay pending until a button sends them.
  await page.evaluate(async (n) => {
    const g = globalThis as any;
    g.eval('_dictFlushing = Date.now()');
    for (let i = 0; i < n; i++) {
      const id = await g.eval('_dictOutboxAdd')(new Blob([new Uint8Array(1500 + i).fill(5)], { type: 'audio/webm' }), 'audio/webm', 3000);
      await g.eval('_dictOutboxPatch')(id, { state: 'failed', attempts: 6, last: Date.now(), error: 'Load failed' });
    }
  }, clips);
  await page.evaluate((w) => { (window as any).openPeek(w); (window as any).setPeekTab('dictation'); }, W);
  await expect(page.locator('#dict-outbox-strip .dict-pending-head')).toBeVisible();
  return sent;
}

test('the PENDING heading holds both buttons on one row, sized for a finger on a phone', async ({ page }, info) => {
  await boot(page, 3);
  const head = page.locator('#dict-outbox-strip .dict-pending-head');
  const label = (await head.locator('span').first().boundingBox())!;
  for (const name of ['Try all again', 'Clear all']) {
    const b = (await head.getByRole('button', { name }).boundingBox())!;
    expect(Math.abs((b.y + b.height / 2) - (label.y + label.height / 2)), name + ' sits on the heading row').toBeLessThan(b.height);
    expect(b.x + b.width, name + ' is inside the screen').toBeLessThanOrEqual(page.viewportSize()!.width + 1);
    if (info.project.name !== 'desktop') expect(b.height, name + ' is a 44px target on a phone').toBeGreaterThanOrEqual(44);
  }
  await page.screenshot({ path: info.outputPath('dictation-pending-bulk.png') });
});

test('Clear all asks once, then removes every pending clip', async ({ page }) => {
  const sent = await boot(page, 3);
  await page.locator('#dict-outbox-strip').getByRole('button', { name: 'Clear all' }).click();
  await expect(page.locator('#modal-msg')).toContainText('Delete the 3 recordings');
  await page.locator('#modal-btns').getByRole('button', { name: 'Delete 3' }).click();
  await expect(page.locator('#dict-outbox-strip .dict-pending-head')).toHaveCount(0);
  const left = await page.evaluate(async () => ((await (globalThis as any).eval('_idb').get('dict_pending')) || []).filter((x: any) => x.state !== 'done').length);
  expect(left).toBe(0);
  expect(sent, 'nothing was sent').toEqual([]);
});

test('Cancel on Clear all keeps every clip', async ({ page }) => {
  await boot(page, 2);
  await page.locator('#dict-outbox-strip').getByRole('button', { name: 'Clear all' }).click();
  await page.locator('#modal-btns').getByRole('button', { name: 'Cancel' }).click();
  const left = await page.evaluate(async () => ((await (globalThis as any).eval('_idb').get('dict_pending')) || []).length);
  expect(left).toBe(2);
});

test('Try all again sends every clip that gave up, past its backoff', async ({ page }) => {
  const sent = await boot(page, 3);
  await page.route(/\/api\/dictate\?.*/, r => r.fulfill({ json: { text: 'sent again' } }));
  await page.evaluate(() => (globalThis as any).eval('_dictFlushing = 0'));
  await page.locator('#dict-outbox-strip').getByRole('button', { name: 'Try all again' }).click();
  await expect.poll(() => sent.length).toBe(3);
  await expect(page.locator('#dict-outbox-strip .dict-pending-head')).toHaveCount(0);
});

// Owner, 2026-11-19, on the Mac app: "4 pending but theres nothing". There the
// store answered slowly (dict_storage_hung): the count's read came back, the
// list's own read gave up after 5 s and showed none. The list and both buttons
// now use the rows the count read.
test('a list read that gives up still shows the clips the count found, and Clear all clears them', async ({ page }) => {
  await boot(page, 2);
  await page.evaluate(() => {
    (globalThis as any).eval('_dictOutbox = async () => []');   // the read that timed out on the Mac app
    (window as any).setPeekTab('terminal'); (window as any).setPeekTab('dictation');
  });
  await expect(page.locator('#dict-pending')).toHaveText('2 pending');
  await expect(page.locator('#dict-outbox-strip .dict-item-pending')).toHaveCount(2);
  await page.locator('#dict-outbox-strip').getByRole('button', { name: 'Clear all' }).click();
  await expect(page.locator('#modal-msg')).toContainText('Delete the 2 recordings');
  await page.locator('#modal-btns').getByRole('button', { name: 'Delete 2' }).click();
  await expect(page.locator('#dict-outbox-strip .dict-pending-head')).toHaveCount(0);
  await expect(page.locator('#dict-pending')).toHaveText('');
});
