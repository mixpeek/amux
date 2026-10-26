import { test, expect } from './fixtures';

// A 20,000-line scrollback drawn whole is ~57,000 elements, rebuilt on every
// history refresh; the Mac app's worker tabs grew to 1-1.7 GB with it
// (2026-10-25). Only the recent lines are drawn, and every way back to the
// rest (the bar, Load earlier, a search) draws all of it.
const LINES = 5000;
const raw = Array.from({ length: LINES }, (_, i) => `scrollback line ${i + 1} here`).join('\n');

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekHistHtmlFor === 'function');
  await page.evaluate(raw => {
    eval("peekSession='hist-worker'; _peekMsgRowsFor=peekSession; _peekMsgRows=[]; _peekMsgNavKind='all'; _peekMsgIndex=-1; peekSearchQuery=''; _peekHistAll=false; _peekEarlier={chunks:[],loadedKb:0,done:true,hidden:false,loading:false};");
    const ov = document.getElementById('peek-overlay')!; ov.hidden = false; ov.inert = false;
    ov.setAttribute('aria-hidden', 'false'); ov.classList.add('active');
    (window as any)._stopPeekPoll();
    eval('_peekHistoryRaw=' + JSON.stringify(raw) + '; _peekHistoryHTML=_peekHistHtmlFor(_peekHistoryRaw); _lastLiveHTML="live frame"; lastPeekHTML=_peekEarlierHTML()+_peekHistoryHTML+_lastLiveHTML;');
    (window as any).applyPeekSearch(false, false);
  }, raw);
});

const hist = (page: import('@playwright/test').Page) => page.locator('#pk-hist');

test('only the recent scrollback is drawn, behind one bar naming the rest', async ({ page }) => {
  const hidden = LINES - 3000;
  await expect(hist(page).locator('.peek-hist-more')).toHaveText(`▲ Show ${hidden.toLocaleString()} earlier lines`);
  await expect(hist(page)).toContainText(`scrollback line ${LINES} here`);
  await expect(hist(page)).toContainText(`scrollback line ${hidden + 1} here`);
  await expect(hist(page)).not.toContainText(`scrollback line ${hidden} here`);
  // The saved-output bar waits behind it rather than stacking a second bar.
  await expect(page.locator('#peek-body .peek-earlier-bar')).toHaveCount(1);
});

test('the bar draws the whole scrollback and keeps the reader in place', async ({ page }) => {
  // The lines are one text run, so the reader's place is their distance from
  // the end of the scrollback, which drawing earlier lines must not change.
  const r = await page.evaluate(() => {
    const b = document.getElementById('peek-body')!;
    b.scrollTop = b.scrollHeight - b.clientHeight - 300;
    const before = b.scrollHeight - b.scrollTop, grew = b.scrollHeight;
    (window as any)._peekHistExpand(true);
    return { before, after: b.scrollHeight - b.scrollTop, grew: b.scrollHeight - grew };
  });
  expect(r.grew).toBeGreaterThan(1000);
  expect(Math.abs(r.after - r.before)).toBeLessThan(4);
  await expect(hist(page).locator('.peek-hist-more')).toHaveCount(0);
  await expect(hist(page)).toContainText('scrollback line 1 here');
});

test('a search finds a line that was not drawn', async ({ page }) => {
  await page.evaluate(() => { eval("peekSearchQuery='scrollback line 7 here'"); (window as any).applyPeekSearch(false, false); });
  await expect(page.locator('#peek-body .peek-highlight')).toHaveCount(1);
  await expect(hist(page).locator('.peek-hist-more')).toHaveCount(0);
});

test('Load earlier output draws the scrollback in memory before asking the server', async ({ page }) => {
  const asked: string[] = [];
  page.on('request', r => { if (r.url().includes('/hist-worker/')) asked.push(r.url()); });
  const verdict = await page.evaluate(() => (window as any)._peekLoadEarlier({ quiet: true }));
  expect(verdict).toBe('loaded');
  expect(asked).toEqual([]);
  await expect(hist(page)).toContainText('scrollback line 1 here');
});

test('a short scrollback is drawn whole with no bar', async ({ page }) => {
  await page.evaluate(() => {
    eval('_peekHistoryHTML=_peekHistHtmlFor("one\\ntwo\\nthree"); lastPeekHTML=_peekEarlierHTML()+_peekHistoryHTML+_lastLiveHTML;');
    (window as any).applyPeekSearch(false, false);
  });
  await expect(hist(page).locator('.peek-hist-more')).toHaveCount(0);
  await expect(hist(page)).toContainText('three');
});
