// The Chat tab has the terminal's shortcut chips and Send/Queue (Ethan,
// 2026-10-13: "in chat view we should have the same input button shortcuts
// and send/queue like terminal worker view"). The chips act on the chat the
// composer sends to: Continue goes to the chat, Esc and Ctrl C stop its reply,
// Up recalls history. No real worker is messaged: sends are captured.
import { test, expect } from './fixtures';

const CC_W = { name: 'cc-w', status: 'active', running: true, chat_companion: true, renderer: 'terminal' };

async function boot(page: any) {
  // The fixture worker is served by /api/sessions too. Pushed only into the
  // in-memory list, the next sessions refresh dropped it, the Chat tab lost
  // its chat target and hid the chip row (red on a loaded CI shard, green
  // locally where the refresh landed after the assertions).
  await page.route(/\/api\/sessions(?:\?.*)?$/, (r: any) => r.request().method() === 'GET'
    ? r.fulfill({ json: [CC_W] }) : r.fallback());
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).renderChips === 'function'
    && typeof (window as any).setPeekTab === 'function');
}

test('Chat tab shows the shortcut chips and they act on the chat', async ({ page }) => {
  await boot(page);
  const interrupts: string[] = [];
  await page.route(/\/chat\/interrupt$/, async (route) => {
    interrupts.push(route.request().url());
    await route.fulfill({ json: { ok: true, interrupted: true, detail: '' } });
  });
  await page.evaluate((cc: any) => {
    const g = globalThis as any;
    if (!g.eval('sessions').some((x: any) => x.name === 'cc-w')) g.eval('sessions').push(cc);
    g.__sent = [];
    g.eval('doSend = async (s, t) => { globalThis.__sent.push([s, t]); return "sent"; }');
    g.eval('_chatMount = () => {}');
    g.eval('_chatUnmount = () => {}');
    g.eval('_chatShowing = () => false');
    g.eval('_sendMode = "send"');
    g.openPeek('cc-w');
  }, CC_W);
  await expect(page.locator('#peek-overlay')).toBeVisible();
  const chips = page.locator('#peek-chips');

  // Terminal tab: every default chip, Space included.
  await page.evaluate(() => (globalThis as any).setPeekTab('terminal'));
  await expect(chips.locator('.chip', { hasText: 'Space' })).toHaveCount(1);

  // A sessions refresh mid-test (what made CI red) must not drop the worker.
  await page.evaluate(() => (globalThis as any)._fetchSessionsOnce());

  // Chat tab: the row is there, Space (no chat meaning) is not.
  await page.evaluate(() => {
    const g = globalThis as any;
    g.eval('_chatShowing = () => true');
    document.getElementById('peek-body')!.classList.add('peek-chat');
    g.setPeekTab('chat');
  });
  for (const label of ['Continue', 'Enter', 'Up', 'Down', 'Esc', 'Ctrl C']) {
    await expect(chips.locator('.chip', { hasText: label }).first(), label).toBeVisible();
  }
  await expect(chips.locator('.chip', { hasText: 'Space' })).toHaveCount(0);

  // Continue goes to the chat, not the worker's terminal.
  await chips.locator('.chip', { hasText: 'Continue' }).click();
  await expect.poll(() => page.evaluate(() => (globalThis as any).__sent)).toEqual([['cc-w@chat', 'continue']]);

  // Esc stops the chat's reply through the chat interrupt.
  await chips.locator('.chip', { hasText: 'Esc' }).click();
  await expect.poll(() => interrupts.length).toBe(1);
  expect(interrupts[0]).toContain('/api/sessions/cc-w%40chat/chat/interrupt');

  // Send/Queue is the same split control, and its tooltip describes the chat.
  const main = page.locator('#peek-overlay .send-split-main');
  await expect(page.locator('#peek-overlay .send-split-arrow')).toBeVisible();
  await expect(main).toHaveAttribute('title', /the chat answers/);
  await page.locator('#peek-overlay .send-split-arrow').click();
  await expect(main).toHaveText('Queue');
  await expect(main).toHaveAttribute('title', /the chat answers after its current reply/);
  await page.locator('#peek-overlay .send-split-arrow').click();
  await expect(main).toHaveText('Send');

  // Back on the terminal tab the tooltip is the worker's again.
  await page.evaluate(() => {
    const g = globalThis as any;
    g.eval('_chatShowing = () => false');
    document.getElementById('peek-body')!.classList.remove('peek-chat');
    g.setPeekTab('terminal');
  });
  await expect(main).toHaveAttribute('title', 'Send now');
  await expect(chips.locator('.chip', { hasText: 'Space' })).toHaveCount(1);
});
