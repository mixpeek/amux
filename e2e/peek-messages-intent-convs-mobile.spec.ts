// A worker's Messages tab lists the conversations from Intent routed to it, at
// the top. On a phone it does not (owner, 2026-11-19: "remove this on messages
// page mobile"): the messages are the screen there, so the list is neither
// fetched nor shown. A desktop keeps it.
import { test, expect } from './fixtures';

const W = 'zz-convs-worker';

test('the Intent conversations list shows on desktop and not on a phone', async ({ page }, info) => {
  const phone = info.project.name !== 'desktop';
  let asked = 0;
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: W, dir: '/tmp/convs', running: true, status: 'idle', flags: '', steering: [] }] }));
  await page.route(/\/api\/intent\/outcomes(?:\?.*)?$/, r => {
    asked++;
    return r.fulfill({ json: { outcomes: [
      { id: 'oc-1', worker: W, title: 'Keep every CI run under $1', status: 'ready', updated_ms: Date.now() }] } });
  });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any).setPeekTab === 'function');
  await page.evaluate((w) => { (window as any).openPeek(w); (window as any).setPeekTab('messages'); }, W);
  const convs = page.locator('#peek-msg-convs');
  if (!phone) {
    await expect(convs).toContainText('Conversations from Intent');
    await expect(convs).toContainText('Keep every CI run under $1');
    return;
  }
  await page.waitForTimeout(1500);
  await expect(convs).toBeHidden();
  await expect(convs).toBeEmpty();
  // Home cards read the same endpoint, so count only what the Messages tab adds.
  const before = asked;
  await page.evaluate(() => (globalThis as any).eval('_peekConvsLoad')());
  expect(asked, 'a phone does not fetch the list').toBe(before);
});
