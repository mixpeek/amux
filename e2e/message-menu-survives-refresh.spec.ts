// The Messages lists re-render on every poll and SSE event. A row's open menu has
// to survive that, or a tap on Pin, Show answer or Copy lands on nothing. Nightly
// failed on this in worker-message-pin (2026-11-03).
import { test, expect } from './fixtures';

test('an open row menu survives a refresh, and a refresh that changes nothing keeps the row', async ({ page }, testInfo) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  const name = 'zz-menu-' + testInfo.workerIndex + '-' + Date.now();
  await page.goto('/');
  await page.evaluate(async n => {
    const post = (u: string, b: object) => fetch(u, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(b) });
    await post('/api/sessions', { name: n, dir: '/tmp', start: false });
    await post('/api/history', { text: 'first message', session: n });
    await post('/api/history', { text: 'second message', session: n });
  }, name);
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(n => { const g = window as any; g.openPeek(n); g.setPeekTab('messages'); }, name);
  const rows = page.locator('#peek-messages-list .pm-row');
  await expect(rows).toHaveCount(2);
  const row = rows.filter({ hasText: 'first message' });
  await row.locator('.pm-dots').click();
  const menu = row.locator('.msg-menu');
  await expect(menu).toBeVisible();

  // Nothing changed: the same element stays, so a tap in flight still lands.
  const kept = await page.evaluate(() => {
    const g = window as any;
    const list = document.getElementById('peek-messages-list')!;
    const before = list.querySelector('.msg-menu.open');
    g._peekMessagesRender();
    return !!before && list.querySelector('.msg-menu.open') === before;
  });
  expect(kept, 'a refresh with no change must not replace the row').toBe(true);

  // The list changed: the rows are rebuilt and the same row's menu stays open.
  const rebuilt = await page.evaluate(() => {
    const g = window as any;
    const list = document.getElementById('peek-messages-list')!;
    const before = list.querySelector('.msg-menu.open');
    g._pmSelAll([list.querySelector('[data-msg-key]')!.getAttribute('data-msg-key')]);
    return list.querySelector('.msg-menu.open') !== before;
  });
  expect(rebuilt, 'selecting a row should rebuild the list').toBe(true);
  await expect(menu).toBeVisible();
  await expect(page.locator('#peek-messages-list .msg-menu.open')).toHaveCount(1);

  // A tap anywhere else still closes it.
  await page.locator('#peek-messages-list').evaluate(l => (l as HTMLElement).click());
  await expect(page.locator('#peek-messages-list .msg-menu.open')).toHaveCount(0);
});
