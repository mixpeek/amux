// Implicit state machines, second pass (2026-11-06,
// docs/design/implicit-state-machines.md #41 and #43).
import { test, expect } from './fixtures';

const base = { name: 'sm-two', provider: 'claude', model: 'claude-sonnet-5', running: true, status: 'idle', dir: '/tmp' };

// #41: the composer lock belongs to ONE send. After the 20 s watch gives the
// button back and a second send starts, the first send's late finish must not
// unlock the composer under the second.
test('a late finish of an earlier send does not unlock the composer under a newer one', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [base] }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._composerLockTake === 'function');
  const got = await page.evaluate(() => {
    const w = window as any;
    const first = w._composerLockTake('sm-two', 'one');
    // What the 20 s watch does when it gives the button back.
    eval('_composerPendingSends.delete("sm-two")');
    const second = w._composerLockTake('sm-two', 'two');
    w._composerLockRelease('sm-two', first);          // the first send finishes late
    const heldAfterStale = eval('_composerPendingSends.get("sm-two")') === second;
    w._syncComposerPending();
    const btn = document.querySelector('.card[data-session="sm-two"] .send-split-main') as HTMLButtonElement | null;
    const disabledAfterStale = btn ? btn.disabled : null;
    w._composerLockRelease('sm-two', second);
    const freed = !eval('_composerPendingSends.has("sm-two")');
    w._syncComposerPending();
    return { heldAfterStale, disabledAfterStale, freed, enabledAfter: btn ? !btn.disabled : null };
  });
  expect(got.heldAfterStale, 'the newer send still holds the lock').toBe(true);
  if (got.disabledAfterStale !== null) expect(got.disabledAfterStale, 'its button stays disabled').toBe(true);
  expect(got.freed, 'its own finish releases it').toBe(true);
  if (got.enabledAfter !== null) expect(got.enabledAfter).toBe(true);
});

// #43: an answer that arrives after a new thread was started belongs to a
// conversation that is gone; it must not land in the new one.
test('an Ask answer that arrives after a new thread is dropped', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [base] }));
  let release: () => void = () => {};
  const gate = new Promise<void>(res => { release = res; });
  let asked = 0;
  await page.route(/\/api\/history\/ask$/, async r => {
    asked++;
    await gate;
    await r.fulfill({ json: { measured: true, answer: 'the old answer', window_days: 14, n_available: 3, n_considered: 3 } });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._askRun === 'function' && !!document.getElementById('ask-q'));
  await page.evaluate(() => { (document.getElementById('ask-q') as HTMLInputElement).value = 'did it ship?'; (window as any)._askRun(false); });
  await expect.poll(() => asked).toBe(1);
  await page.evaluate(() => (window as any)._askNewThread(false));
  release();
  // The control: the reply did arrive, and the panel stayed on the new thread.
  await expect.poll(() => page.evaluate(() => eval('_askThread.length'))).toBe(0);
  await page.waitForTimeout(300);
  expect(await page.evaluate(() => eval('_askThread.length')), 'the stale answer is not in the new thread').toBe(0);
  expect(await page.locator('#ask-answer').innerText()).not.toContain('the old answer');

  // Control: with no new thread in between, the same answer does land.
  await page.unroute(/\/api\/history\/ask$/);
  await page.route(/\/api\/history\/ask$/, r => r.fulfill({ json: { measured: true, answer: 'the fresh answer', window_days: 14, n_available: 3, n_considered: 3 } }));
  await page.evaluate(() => { (document.getElementById('ask-q') as HTMLInputElement).value = 'and now?'; (window as any)._askRun(false); });
  await expect.poll(() => page.evaluate(() => eval('_askThread.length'))).toBe(1);
});
