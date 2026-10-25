// Cmd-K ranking (Ethan, 2026-10-24): exact names and ids first, then prefix
// and word matches, then fuzzy; titles over bodies; active work over old
// completed work; archived and discarded only when asked; 8 rows first;
// recent places when the box is empty; and the highlighted row holds still
// while results arrive. Workers, groups and the card index are fakes.
import { test, expect, Page, allowUnusedRoute } from './fixtures';

const W = [
  { name: 'zz-research-lane', dir: '/tmp/r', running: true, status: 'working', flags: '', desc: 'Deep dives', tags: ['zzlab'] },
  { name: 'zz-quiet', dir: '/tmp/q', running: true, status: 'idle', flags: '', desc: 'Writes the quarterly research digest', tags: [] },
  { name: 'zz-old-research', dir: '/tmp/o', running: false, status: 'idle', flags: '', desc: '', archived: true, tags: [] },
];
const card = (id: string, title: string, status: string, tier: number, boost: number, extra: object = {}) =>
  ({ kind: 'card', id, title, status, session: 'zz-quiet', archived: false, updated: 0, tier, boost, score: tier * 10000 + boost, match: tier === 4 ? 'word' : 'title', snippet: null, ...extra });

async function boot(page: Page, hits: any[] = [], opts: { delayMs?: number, hidden?: number, emptyOnly?: boolean } = {}) {
  const asked: string[] = [];
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: W }));
  await page.route(/\/api\/groups(?:\?.*)?$/, r => r.fulfill({ json: { groups: [{ name: 'research', goal: 'Find things out' }, { name: 'zzlab' }] } }));
  await page.route(/\/api\/palette\?.*/, async r => {
    asked.push(r.request().url());
    if (opts.delayMs) await new Promise(res => setTimeout(res, opts.delayMs));
    const u = new URL(r.request().url());
    const withHidden = u.searchParams.get('hidden') === '1';
    const shown = withHidden ? hits : hits.filter(h => !h.archived && h.status !== 'discarded');
    await r.fulfill({ json: { hits: shown, total: shown.length, hidden: withHidden ? 0 : (opts.hidden ?? hits.length - shown.length), measured: true, n_considered: 100, stale: false } });
  });
  if (opts.emptyOnly) allowUnusedRoute(page, /\/api\/palette\?.*/);   // an empty box never asks for cards
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openCmdk === 'function'
    && (0, eval)('sessions').some((s: any) => s.name === 'zz-research-lane'));
  return asked;
}

async function search(page: Page, q: string) {
  await page.evaluate(() => (window as any).openCmdk());
  const input = page.locator('#cmdk-input');
  await input.fill(q);
  return input;
}
const rowsText = (page: Page) => page.locator('#cmdk-list .cmdk-row').evaluateAll(els => els.map(e =>
  (e.querySelector('.cmdk-kind')?.textContent || '') + ':' + (e.querySelector('.cmdk-title')?.textContent || '')));

test('"research" puts the group and workers and open cards above old completed work', async ({ page }) => {
  await boot(page, [
    card('R-9', 'Research spike from spring', 'done', 4, 60 - 300),
    card('R-1', 'Market research plan', 'doing', 4, 60 + 80 + 40),
    card('R-5', 'Research bucket cleanup', 'todo', 4, 60 + 60 + 40, { archived: true }),
  ]);
  await search(page, 'research');
  await expect(page.locator('#cmdk-list .cmdk-row', { hasText: 'Market research plan' })).toBeVisible();
  const rows = await rowsText(page);
  const at = (s: string) => rows.findIndex(r => r === s);
  expect(at('Group:research'), 'the exact name first').toBe(0);
  expect(at('Worker:zz-research-lane')).toBeGreaterThan(0);
  expect(at('Worker:zz-research-lane'), 'a busy worker matching by word beats any card').toBeLessThan(at('Card:Market research plan'));
  expect(at('Card:Market research plan'), 'active work above old completed work').toBeLessThan(at('Card:Research spike from spring'));
  expect(at('Worker:zz-quiet'), 'a description match ranks under every title match').toBeGreaterThan(at('Card:Research spike from spring'));
  expect(rows.some(r => r.includes('zz-old-research')), 'archived worker hidden').toBe(false);
  expect(rows.some(r => r.includes('Research bucket cleanup')), 'archived card hidden').toBe(false);
  await expect(page.locator('#cmdk-list .cmdk-row.cmdk-meta', { hasText: 'Show 2 archived or discarded' })).toBeVisible();
  // Matches are marked; a body match shows a snippet around the word.
  await expect(page.locator('#cmdk-list .cmdk-row').first().locator('.cmdk-title mark')).toHaveText('research');
  const quiet = page.locator('#cmdk-list .cmdk-row', { hasText: 'zz-quiet' });
  await expect(quiet.locator('.cmdk-snippet mark')).toHaveText('research');
});

test('Show archived brings hidden things back, ranked below live ones', async ({ page }) => {
  const asked = await boot(page, [
    card('R-1', 'Market research plan', 'doing', 4, 180),
    card('R-5', 'Research bucket cleanup', 'todo', 4, 160 - 400, { archived: true }),
  ]);
  await search(page, 'research');
  await expect(page.locator('#cmdk-list .cmdk-row', { hasText: 'Market research plan' })).toBeVisible();
  await page.locator('.cmdk-hidden-toggle').click();
  await expect(page.locator('#cmdk-list .cmdk-row', { hasText: 'Research bucket cleanup' })).toBeVisible();
  await expect(page.locator('#cmdk-list .cmdk-row', { hasText: 'zz-old-research' })).toBeVisible();
  expect(asked.some(u => u.includes('hidden=1'))).toBe(true);
  await expect(page.locator('.cmdk-hidden-toggle')).toHaveText('Hide archived');
  const rows = await rowsText(page);
  expect(rows.indexOf('Card:Market research plan')).toBeLessThan(rows.indexOf('Card:Research bucket cleanup'));
  await expect(page.locator('#cmdk-input'), 'focus stays in the box').toBeFocused();
});

test('8 results first, then a row that opens the rest', async ({ page }) => {
  await boot(page, Array.from({ length: 12 }, (_, i) => card('N-' + i, 'Nimbus task ' + i, 'todo', 4, 100 - i)));
  const input = await search(page, 'nimbus');
  await expect(page.locator('#cmdk-list .cmdk-row.cmdk-meta', { hasText: 'Show 4 more' })).toBeVisible();
  expect(await page.locator('#cmdk-list .cmdk-row:not(.cmdk-meta)').count()).toBe(8);
  for (let i = 0; i < 8; i++) await input.press('ArrowDown');
  await input.press('Enter');
  expect(await page.locator('#cmdk-list .cmdk-row:not(.cmdk-meta)').count()).toBe(12);
  await expect(page.locator('#cmdk-list .cmdk-row.sel')).toContainText('Nimbus task 8');
});

test('an empty box shows recent places, then workers waiting, then actions', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux-cmdk-usage', JSON.stringify({
    'worker:zz-quiet': [3, Date.now() - 60_000, 'zz-quiet'],
    'card:R-1': [1, Date.now() - 120_000, 'Market research plan'],
    'worker:gone-worker': [9, Date.now(), 'gone-worker'],
  })));
  await boot(page, [], { emptyOnly: true });
  await page.evaluate(() => (window as any).openCmdk());
  const sections = await page.locator('#cmdk-list .cmdk-section').allTextContents();
  expect(sections[0]).toBe('Recent');
  expect(sections).toContain('Actions');
  const rows = await rowsText(page);
  expect(rows.slice(0, 2)).toEqual(['Worker:zz-quiet', 'Card:Market research plan']);
  expect(rows.some(r => r.includes('gone-worker')), 'a worker that no longer exists is not offered').toBe(false);
  expect(rows).toContain('Action:New worker');
});

test('the highlighted row stays put when slower results arrive', async ({ page }) => {
  await boot(page, [card('Z-1', 'zz research exact', 'doing', 5, 200)], { delayMs: 900 });
  const input = await search(page, 'zz');
  await expect(page.locator('#cmdk-list .cmdk-row').nth(1)).toBeVisible();
  await input.press('ArrowDown');
  const chosen = await page.locator('#cmdk-list .cmdk-row.sel .cmdk-title').textContent();
  await expect(page.locator('#cmdk-list .cmdk-row', { hasText: 'zz research exact' })).toBeVisible({ timeout: 5000 });
  await expect(page.locator('#cmdk-list .cmdk-row.sel .cmdk-title'), 'same thing still highlighted').toHaveText(chosen || '');
  // Enter opens what is highlighted, and is recorded as a recent place.
  await page.route(/\/api\/sessions\/zz-quiet\/peek\?.*/, r => r.fulfill({ json: { output: 'idle\n', history: '' } }));
  await input.press('Enter');
  const usage = await page.evaluate(() => JSON.parse(localStorage.getItem('amux-cmdk-usage') || '{}'));
  expect(Object.values(usage).some((e: any) => e[2] === chosen)).toBe(true);
});

test('a fuzzy guess finds a worker when nothing else matches', async ({ page }) => {
  await boot(page);
  await search(page, 'zzrsln');
  await expect(page.locator('#cmdk-foot')).toContainText('searched');
  const first = page.locator('#cmdk-list .cmdk-row').first();
  await expect(first).toContainText('zz-research-lane');
  expect(await first.locator('.cmdk-title mark').count()).toBeGreaterThan(0);
});
