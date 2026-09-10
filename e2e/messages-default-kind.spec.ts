import { test, expect } from '@playwright/test';
test('Messages tab opens with the human pill selected', async ({ page }) => {
  const urls: string[] = [];
  page.on('request', r => { if (r.url().includes('/api/history')) urls.push(r.url()); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function', { timeout: 20000 });
  await page.evaluate(() => (window as any).switchView('messages'));
  await page.waitForSelector('#msgs-kind-filter .msg-kind-chip', { timeout: 15000 });
  await page.waitForTimeout(1200);
  const chips = await page.evaluate(() => Array.from(
    document.querySelectorAll('#msgs-kind-filter .msg-kind-chip')).map(b => {
      const s = getComputedStyle(b as Element);
      return { label: (b.textContent || '').trim().slice(0, 22),
               selected: s.backgroundColor !== 'rgba(0, 0, 0, 0)' && s.backgroundColor !== 'transparent' };
    }));
  console.log('[CHIPS] ' + JSON.stringify(chips));
  console.log('[KIND-VAR] ' + await page.evaluate(() => (0, eval)('_msgsKind')));
  console.log('[FETCHES] ' + JSON.stringify(urls.filter(u => !u.includes('counts=1')).map(u => u.split('/api/')[1])));
  console.log('[COUNTS-FETCH-UNFILTERED] ' + urls.filter(u => u.includes('counts=1')).every(u => !u.includes('kind=')));
  const sel = chips.filter(c => c.selected).map(c => c.label);
  expect(sel.join(','), 'exactly one chip selected, and it is Human').toMatch(/Human/i);
  // A non-empty history can legitimately contain only human messages. Compare
  // each chip with the unfiltered backend total instead of requiring every kind.
  expect(urls.some(u => u.includes('counts=1'))).toBe(true);
  expect(urls.filter(u => u.includes('counts=1')).every(u => !u.includes('kind='))).toBe(true);
  const counts = await page.evaluate(async () => (await fetch('/api/history?counts=1')).json());
  for (const [label, key] of Object.entries({ All: 'all', Human: 'human', Session: 'session', Scheduled: 'schedule', amux: 'amux', Unstamped: 'unstamped', Unclassified: 'unknown' })) {
    const chip = chips.find(c => c.label.startsWith(label + ' '));
    expect(chip, label).toBeTruthy();
    expect(Number(chip!.label.split(' ').pop()), label).toBe(counts[key] || 0);
  }
});
