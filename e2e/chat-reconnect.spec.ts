// Ethan, 2026-11-27, over a worker's Chat tab stuck on "Stream disconnected.
// Reconnect": "We need better handling of intermittent data connectivity".
// A dropped chat stream retried once; when the network was still down that
// retry failed and nothing tried again. These run against the real server's
// chat history and stream, with the network cut by failing those requests.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

declare const _chat: any;
type Page = import('@playwright/test').Page;

const CHAT = /\/api\/sessions\/[^/]+\/chat(\/stream)?(\?.*)?$/;

async function mount(page: Page, info: any) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._chatMount === 'function', { timeout: 20000 });
  const name = `e2e-chat-net-${info.project.name}-${Date.now()}`;
  const status = await page.evaluate(async n => (await fetch('/api/sessions', { method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ name: n, dir: '/tmp' }) })).status, name);
  expect(status).toBe(201);
  await expect.poll(() => page.evaluate(async w => { await (window as any).fetchSessions(); return ((0, eval)('sessions') || []).some((s: any) => s.name === w); }, name), { timeout: 30000 }).toBe(true);
  await page.evaluate(n => { const w = window as any; w.openPeek(n); w.setPeekTab('chat'); w._chatUnmount(); w._chatMount(n); }, name);
  await expect.poll(() => connected(page), { timeout: 20000, message: 'the chat stream opens' }).toBe(true);
  return name;
}
// Open and past any outage: the stream has delivered since it came back.
const connected = (page: Page) => page.evaluate(() => !!_chat.es && _chat.es.readyState === 1 && !_chat.downSince);
// The network goes away: every chat request fails, and the open stream drops
// the way the browser drops it on a network error (closed, then onerror).
async function cut(page: Page) {
  let failed = 0;
  await page.route(CHAT, r => { failed++; return r.abort('internetdisconnected'); });
  await page.evaluate(() => { const es = _chat.es; es.close(); es.onerror(new Event('error')); });
  return () => failed;
}
const note = (page: Page) => page.locator('#peek-body .chat-reconnecting');

test('a dropped chat stream keeps retrying through an outage and comes back on its own', async ({ page }, info) => {
  const name = await mount(page, info);
  try {
    const failed = await cut(page);
    // A blip says nothing.
    await page.waitForTimeout(2500);
    await expect(note(page)).toHaveCount(0);
    // A real outage says so, once, with a way to retry now.
    await expect(note(page)).toBeVisible({ timeout: 4000 });
    await expect(note(page)).toContainText('Reconnecting');
    await expect(note(page).locator('.chat-retry-now')).toBeVisible();
    await expect(page.locator('#peek-body')).not.toContainText('Stream disconnected');
    // One line for the outage: no raw fetch error, no "No messages yet".
    await expect(page.locator('#peek-body')).not.toContainText('Could not load');
    await expect(page.locator('#peek-body')).not.toContainText('No messages yet');
    await expect(page.locator('#peek-body .chat-offline-note')).toHaveCount(1);
    // It keeps trying while the network is down (before, one failed reload ended it).
    await expect.poll(failed, { timeout: 15000, message: 'retries continue during the outage' }).toBeGreaterThanOrEqual(3);
    await page.screenshot({ path: info.outputPath('reconnecting.png') });
    // The network comes back: no click, the stream reopens and the line goes.
    await page.unroute(CHAT);
    await expect.poll(() => connected(page), { timeout: 20000, message: 'reconnected without a click' }).toBe(true);
    await expect(note(page)).toHaveCount(0);
  } finally {
    await cleanup('restore the network and delete the test worker', async () => {
      await page.unroute(CHAT).catch(() => {});
      await page.evaluate(n => fetch('/api/sessions/' + encodeURIComponent(n), { method: 'DELETE' }), name);
    }, info);
  }
});

test('coming back online retries at once instead of waiting out the backoff', async ({ page }, info) => {
  const name = await mount(page, info);
  try {
    const failed = await cut(page);
    // Let the backoff grow past a few seconds.
    await expect.poll(failed, { timeout: 15000 }).toBeGreaterThanOrEqual(3);
    const attempts = await page.evaluate(() => _chat.attempts);
    expect(attempts, 'the next try is at least 4 s away').toBeGreaterThanOrEqual(3);
    await page.unroute(CHAT);
    const t0 = Date.now();
    await page.evaluate(() => window.dispatchEvent(new Event('online')));
    await expect.poll(() => connected(page), { timeout: 3000, message: 'reconnected on the online event' }).toBe(true);
    expect(Date.now() - t0).toBeLessThan(3000);
  } finally {
    await cleanup('restore the network and delete the test worker', async () => {
      await page.unroute(CHAT).catch(() => {});
      await page.evaluate(n => fetch('/api/sessions/' + encodeURIComponent(n), { method: 'DELETE' }), name);
    }, info);
  }
});

test('Retry now reconnects, and on a phone it is a full-size target in one line', async ({ page }, info) => {
  const name = await mount(page, info);
  try {
    await cut(page);
    await expect(note(page)).toBeVisible({ timeout: 8000 });
    const retry = note(page).locator('.chat-retry-now');
    if (info.project.name !== 'desktop') {
      const box = (await retry.boundingBox())!;
      expect(box.height, 'touch target').toBeGreaterThanOrEqual(44);
      const line = (await note(page).boundingBox())!;
      expect(line.x + line.width).toBeLessThanOrEqual(page.viewportSize()!.width);
      await page.screenshot({ path: info.outputPath('phone.png') });
    }
    await page.unroute(CHAT);
    await retry.click();
    await expect.poll(() => connected(page), { timeout: 5000, message: 'Retry now reconnected' }).toBe(true);
    await expect(note(page)).toHaveCount(0);
  } finally {
    await cleanup('restore the network and delete the test worker', async () => {
      await page.unroute(CHAT).catch(() => {});
      await page.evaluate(n => fetch('/api/sessions/' + encodeURIComponent(n), { method: 'DELETE' }), name);
    }, info);
  }
});
