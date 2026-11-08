// A chip in the peek sends to the worker or presses its key; it never just
// fills the composer (Ethan, 2026-11-02: "when i press these it should be send
// not put in the input"). Each press must run the chip it shows, even after
// the saved chip list changed under a rendered bar (another device's sync, a
// migration): the bar used to look the chip up by position at click time.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

test('peek chips run the chip they show and leave the composer empty', async ({ page, request }, testInfo) => {
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const name = `chips-${testInfo.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp' } })).ok()).toBeTruthy();
  try {
    await page.reload();
    await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
    await page.evaluate(() => {
      const w = window as any;
      w._chipCalls = [];
      w.doSend = async (_n: string, t: string) => { w._chipCalls.push('send ' + t); return 'sent'; };
      w.doKeys = async (_n: string, k: string) => { w._chipCalls.push('keys ' + k); return { accepted: true }; };
      w._saveChips([
        { id: 'continue', label: 'Continue', action: 'send', value: 'continue' },
        { id: 'yes', label: 'Yes', action: 'send', value: 'yes' },
        { id: 'custom-1', label: 'Status?', action: 'send', value: 'status?' },
        { id: 'esc', label: 'Esc', action: 'keys', value: 'Escape' },
      ]);
    });
    await page.evaluate(n => (window as any).openPeek(n), name);
    await expect(page.locator('#peek-chips .chip', { hasText: 'Status?' })).toBeVisible();
    for (const label of ['Continue', 'Status?', 'Esc']) {
      await page.locator('#peek-chips .chip', { hasText: label }).first().click();
      await expect(page.locator('#peek-cmd-input'), `${label} must not fill the composer`).toHaveValue('');
    }
    // The saved list moves under the rendered bar: a slash chip now sits where
    // Yes was. A press on Yes must still send "yes", not fill the input, and
    // the bar then catches up with the new list.
    await page.evaluate(() => {
      const yes = (document.querySelector('#peek-chips') as any)._chips.findIndex((c: any) => c.id === 'yes');
      const moved = Array.from({ length: yes + 1 }, (_, i) => ({ id: 'sk-' + i, label: '/s' + i, action: 'slash', value: '/s' + i }));
      localStorage.setItem('amux_chips', JSON.stringify(moved));
    });
    await page.locator('#peek-chips .chip', { hasText: 'Yes' }).first().click();
    await expect(page.locator('#peek-cmd-input'), 'Yes must not fill the composer').toHaveValue('');
    await expect(page.locator('#peek-chips .chip', { hasText: 'Yes' })).toHaveCount(0);
    const calls = await page.evaluate(() => (window as any)._chipCalls.map((c: string) => c.replace(/^send \[[^\]]*\] /, 'send ')));
    expect(calls).toEqual(['send continue', 'send status?', 'keys Escape', 'send yes']);
  } finally {
    await cleanup('delete fixture worker', () => request.delete(`/api/sessions/${name}`, { headers: auth }), testInfo);
  }
});
