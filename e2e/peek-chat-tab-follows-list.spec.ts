// Chat is on by default, and a new worker is not in the client's list when its
// Chat tab first opens. The "Chat is off" panel must not stay over the composer
// once the list says otherwise, and a worker turned off elsewhere gets the panel.
import { test, expect } from './fixtures';

const W = { name: 'chat-list-w', status: 'idle', running: true, renderer: 'terminal', provider: 'claude', dir: '/tmp' };

test('the Chat tab follows the session list both ways', async ({ page }) => {
  let list: any[] = [];
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.request().method() === 'GET' ? r.fulfill({ json: list }) : r.continue());
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(n => { const w = window as any; w.openPeek(n); w.setPeekTab('chat'); }, W.name);
  const panel = page.locator('#peek-chat-panel');
  await expect(panel).toHaveClass(/active/);
  const refresh = () => page.evaluate(() => (window as any).fetchSessions());
  list = [{ ...W, chat_companion: true }];
  await expect(async () => { await refresh(); await expect(panel).not.toHaveClass(/active/, { timeout: 500 }); }).toPass({ timeout: 15_000 });
  await expect(page.locator('#peek-cmd-input')).toBeVisible();
  list = [{ ...W, chat_companion: false }];
  await expect(async () => { await refresh(); await expect(panel).toHaveClass(/active/, { timeout: 500 }); }).toPass({ timeout: 15_000 });
});
