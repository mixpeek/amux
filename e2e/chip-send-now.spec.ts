// Send Now (Ethan, 2026-10-26: "create an amux default input in the buttons
// things that does Ctrl + X, release, then Ctrl + S for Send Now"). The chip
// presses Ctrl+X, lets go, then presses Ctrl+S, as two separate keystrokes to
// the worker's terminal. No real worker is touched: the key requests are
// captured.
import { test, expect } from './fixtures';

const W = { name: 'sendnow-w', status: 'active', running: true, renderer: 'terminal' };

async function boot(page: any, chips?: object[]) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, (r: any) => r.request().method() === 'GET'
    ? r.fulfill({ json: [W] }) : r.fallback());
  await page.addInitScript((saved: any) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    if (saved) localStorage.setItem('amux_chips', JSON.stringify(saved));
  }, chips || null);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).renderChips === 'function');
}

test('Send Now presses Ctrl+X, then Ctrl+S, in that order, as two keystrokes', async ({ page }) => {
  await boot(page);
  const keys: string[] = [];
  await page.route(/\/api\/sessions\/sendnow-w\/keys$/, async (route: any) => {
    keys.push(route.request().postDataJSON().keys);
    await route.fulfill({ json: { ok: true, accepted: true, effect: 'sent' } });
  });
  await page.evaluate((w: any) => {
    const g = globalThis as any;
    if (!g.eval('sessions').some((x: any) => x.name === w.name)) g.eval('sessions').push(w);
    g.openPeek(w.name);
    g.setPeekTab('terminal');
  }, W);
  const chip = page.locator('#peek-chips .chip', { hasText: 'Send Now' });
  await expect(chip).toBeVisible();
  await chip.click();
  await expect.poll(() => keys).toEqual(['C-x', 'C-s']);
});

test('a customized chip set gets Send Now once, after Enter, and keeps it removed', async ({ page }) => {
  const mine = [{ id: 'continue', label: 'Continue', action: 'send', value: 'continue' },
    { id: 'enter', label: 'Enter', action: 'keys', value: 'Enter' }, { id: 'yes', label: 'Yes', action: 'send', value: 'yes' }];
  await boot(page, mine);
  const ids = () => page.evaluate(() => (globalThis as any)._getChips().map((c: any) => c.id));
  expect(await ids()).toEqual(['continue', 'enter', 'sendnow', 'yes']);
  // Removed by the owner: not added back.
  await page.evaluate((m: any) => localStorage.setItem('amux_chips', JSON.stringify(m)), mine);
  expect(await ids()).toEqual(['continue', 'enter', 'yes']);
});
