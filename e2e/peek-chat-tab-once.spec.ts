// A chat worker showed two "Chat" tabs side by side (owner screenshot, 2026-11-20): its first
// tab, relabelled Chat for the chat renderer, and the companion Chat tab. A chat worker keeps
// one; a terminal worker keeps both Terminal and Chat.
import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';

const WORKERS = [
  { name: 'ct-chat', dir: '/tmp/ct', running: true, status: 'idle', renderer: 'chat', flags: '', steering: [] },
  { name: 'ct-term', dir: '/tmp/ct', running: true, status: 'idle', renderer: 'terminal', flags: '', steering: [] },
];

async function boot(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: WORKERS }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === 'ct-chat'));
}

// The labels of the tabs a person can see, in order.
const visibleTabs = (page: Page) => page.locator('.peek-tabs .peek-tab:visible .tab-lbl').allInnerTexts();

test('a chat worker shows one Chat tab, a terminal worker shows Terminal and Chat', async ({ page }) => {
  await boot(page);

  await page.evaluate(() => (globalThis as any).openPeek('ct-chat'));
  await expect(page.locator('#peek-overlay')).toBeVisible();
  await expect(page.locator('#peek-tab-terminal .tab-lbl')).toHaveText('Chat');
  await expect(page.locator('#peek-tab-chat')).toBeHidden();
  expect((await visibleTabs(page)).filter(t => t.trim() === 'Chat')).toHaveLength(1);

  // A remembered or linked 'chat' tab opens the worker's own chat.
  await page.evaluate(() => (globalThis as any).setPeekTab('chat'));
  expect(await page.evaluate(() => (globalThis as any).eval('_peekTab'))).toBe('terminal');
  await expect(page.locator('#peek-tab-terminal')).toHaveClass(/active/);

  // Another worker, then back: the tab row is redrawn for each.
  await page.evaluate(() => (globalThis as any).openPeek('ct-term'));
  await expect(page.locator('#peek-tab-terminal .tab-lbl')).toHaveText('Terminal');
  await expect(page.locator('#peek-tab-chat')).toBeVisible();
  expect((await visibleTabs(page)).filter(t => t.trim() === 'Chat')).toHaveLength(1);

  await page.evaluate(() => (globalThis as any).openPeek('ct-chat'));
  await expect(page.locator('#peek-tab-chat')).toBeHidden();
  expect((await visibleTabs(page)).filter(t => t.trim() === 'Chat')).toHaveLength(1);
});
