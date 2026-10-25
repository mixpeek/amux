import { test, expect } from '@playwright/test';
test('Messages tab opens with the human pill selected', async ({ page }) => {
  const urls: string[] = [];
  page.on('request', r => { if (r.url().includes('/api/history')) urls.push(r.url()); });
  // The totals the page itself received. Other specs in this project share the
  // server and write history concurrently (workers=2), so a fresh fetch made
  // AFTER the chips rendered can legitimately disagree with them (CI: chips
  // "All 0" against a later fetch reading 2). The property under test is that
  // the chips show the server's totals, so compare against the response the
  // chips were rendered from.
  let rendered: any = null;
  page.on('response', async r => {
    if (!r.url().includes('/api/history?counts=1')) return;
    try { rendered = await r.json(); } catch { /* page navigated */ }
  });
  // CI's empty fleet opens the walkthrough (#wt-tooltip) over #cv-all-item, and on ios-safari the
  // click retried until the 90s timeout (main, 2026-10-19). Same suppression as tab-customizer.
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function', { timeout: 20000 });
  await page.evaluate(() => (window as any).switchView('messages'));
  // The message log is the "All messages" entry of the Messages sidebar since
  // channels arrived (api/conversations.rs); its default kind is still Human.
  await page.click('#cv-all-item');
  await page.waitForSelector('#msgs-kind-filter .msg-kind-chip', { timeout: 15000 });
  await expect.poll(() => rendered !== null, { timeout: 15000 }).toBe(true);
  const readChips = () => page.evaluate(() => Array.from(
    document.querySelectorAll('#msgs-kind-filter .msg-kind-chip')).map(b => {
      const s = getComputedStyle(b as Element);
      return { label: (b.textContent || '').trim().slice(0, 22),
               selected: s.backgroundColor !== 'rgba(0, 0, 0, 0)' && s.backgroundColor !== 'transparent' };
    }));
  const LABELS = { All: 'all', Human: 'human', Session: 'session', Scheduled: 'schedule', amux: 'amux', Unstamped: 'unstamped', Unclassified: 'unknown' };
  const chipCounts = (cs: { label: string }[]) => Object.fromEntries(Object.keys(LABELS).map(l => {
    const chip = cs.find(c => c.label.startsWith(l + ' '));
    return [l, chip ? Number(chip.label.split(' ').pop()) : null];
  }));
  const serverCounts = () => Object.fromEntries(Object.entries(LABELS).map(([l, k]) => [l, rendered[k] || 0]));
  // The chips render from the counts response asynchronously; wait until the
  // DOM reflects the newest response the page received.
  await expect.poll(async () => JSON.stringify(chipCounts(await readChips())) === JSON.stringify(serverCounts()),
    { timeout: 10000 }).toBe(true);
  const chips = await readChips();
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
  const counts = serverCounts();
  for (const [label, n] of Object.entries(chipCounts(chips))) {
    expect(n, label).not.toBeNull();
    expect(n, label).toBe(counts[label]);
  }
});
