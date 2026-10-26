// A Chat tab send still in this device's outbox shows what happened to it.
// Sandbox, 2026-10-25: two messages to mtp-demo's Chat tab sat at "waiting to
// send" for hours after the server had refused both ("chat worker is not
// running"), with no retry and no reason. A refused send now says "Not sent",
// gives the server's reason, and offers Send again and Remove, as the Messages
// tab does. No real worker is messaged: the outbox and the chat are set in the
// page, and the retry is caught before it reaches the network.
import { test, expect, allowUnusedRoute } from './fixtures';

async function boot(page: any) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._chatRender === 'function'
    && typeof (window as any)._chatPendingBubble === 'function');
}

async function show(page: any, entry: Record<string, unknown>) {
  return page.evaluate((entry: Record<string, unknown>) => {
    const g = globalThis as any;
    g.eval('_chatShowing = () => true');
    const chat = g.eval('_chat');
    chat.name = 'pend-probe@chat';
    chat.messages = [];
    chat.streaming = null;
    const q = g.eval('offlineQueue');
    q.splice(0, Infinity, Object.assign({ id: 'op-pend', url: '/api/sessions/pend-probe%40chat/send', timestamp: Date.now(),
      options: { method: 'POST', body: JSON.stringify({ text: 'is the page public yet?', msg_id: 'm-pend' }) } }, entry));
    g._chatRender();
    const b = document.querySelector('#peek-body .chat-pending') as HTMLElement | null;
    return b ? { meta: (b.querySelector('.chat-meta') as HTMLElement).innerText.replace(/\s+/g, ' ').trim(),
      retry: !!b.querySelector('.chat-pending-retry'), remove: !!b.querySelector('.chat-pending-remove'),
      refused: b.classList.contains('is-refused'), count: document.querySelectorAll('#peek-body .chat-pending').length } : null;
  }, entry);
}

test('a send not yet tried says it is waiting, with nothing to press', async ({ page }) => {
  await boot(page);
  const b = await show(page, { not_attempted: true });
  expect(b?.meta).toBe('waiting to send');
  expect(b?.retry).toBe(false);
  expect(b?.count).toBe(1);
});

test('a send the server refused says so, with the reason, Send again and Remove', async ({ page }) => {
  await boot(page);
  const b = await show(page, { attempts: 1, state: 'blocked', needs_decision: 'refused', error: 'chat worker is not running' });
  expect(b?.meta).toContain('Not sent');
  expect(b?.meta).toContain('chat worker is not running');
  expect(b?.refused).toBe(true);
  expect(b?.retry && b?.remove).toBe(true);
  expect(b?.count, 'one send is one bubble').toBe(1);
  // Send again clears the refusal and sends it again (caught here, never a real worker).
  await page.route('**/api/sessions/pend-probe%40chat/send', (r: any) => r.fulfill({ json: { ok: true, msg_id: 'm-pend' } }));
  allowUnusedRoute(page, '**/api/sessions/pend-probe%40chat/send');   // the resend may not reach the network before the check
  // The chat body is rendered off screen here (no peek is open), so press it from the page.
  await page.evaluate(() => (document.querySelector('#peek-body .chat-pending-retry') as HTMLButtonElement).click());
  await expect.poll(() => page.evaluate(() => {
    const q = (globalThis as any).eval('offlineQueue');
    return q.length ? (q[0].needs_decision || 'none') : 'gone';
  })).not.toBe('refused');
  await page.evaluate(() => (globalThis as any)._updatePendingPill());
  await expect(page.locator('#peek-body .chat-pending.is-refused')).toHaveCount(0);
});

test('a send sent but not yet confirmed says it is awaiting confirmation', async ({ page }) => {
  await boot(page);
  const b = await show(page, { attempts: 1, attempted_at: Date.now() });
  expect(b?.meta).toContain('Awaiting confirmation');
  expect(b?.retry).toBe(true);
});
