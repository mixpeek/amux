// One device notification per worker transition, however many tabs are open
// (implicit state machines inventory 2026-10-31, #30). Every tab runs its own
// edge detector, so a "needs input" alerted once per tab and Mac-app window,
// and a tab back from sleep announced hour-old changes as new.
import { test, expect } from './fixtures';

test('two tabs claim a notification once, and a stale tab re-baselines', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._notifClaimOnce === 'function');
  const other = await page.context().newPage();
  await other.goto('/');
  await other.waitForFunction(() => typeof (window as any)._notifClaimOnce === 'function');

  const claim = (p: typeof page, sig: string) => p.evaluate(sig => (window as any)._notifClaimOnce(sig), sig);
  // Both tabs race for the same transition: exactly one wins.
  const [a, b] = await Promise.all([claim(page, 'w1 needs input|q|w1'), claim(other, 'w1 needs input|q|w1')]);
  expect([a, b].filter(Boolean).length, `a=${a} b=${b}`).toBe(1);
  // A different transition is its own notification.
  expect(await claim(other, 'w2 finished|x|w2')).toBe(true);
  expect(await claim(page, 'w2 finished|x|w2')).toBe(false);

  // A tab that saw nothing for over a minute re-baselines instead of firing.
  const fired = await page.evaluate(() => {
    const calls: string[] = [];
    (window as any)._fireSessionNotif = (_n: string, t: string) => calls.push(t);
    eval('_initialLoad = false; Object.keys(_prevSessionState).forEach(k => delete _prevSessionState[k]);');
    const w = { name: 'stale-tab', running: true, status: 'active' };
    (window as any)._checkSessionTransitions([w]);
    eval('_transitionsCheckedAt = Date.now() - 120000;');
    (window as any)._checkSessionTransitions([{ ...w, status: 'idle' }]);
    const afterStale = calls.length;
    // Fresh again: a real change in the next update does fire.
    (window as any)._checkSessionTransitions([{ ...w, status: 'blocked' }]);
    return { afterStale, after: calls.length, calls };
  });
  expect(fired.afterStale, 'the stale diff announced nothing: ' + fired.calls.join(', ')).toBe(0);
  expect(fired.after, 'a fresh change still notifies').toBe(1);
  await other.close();
});
