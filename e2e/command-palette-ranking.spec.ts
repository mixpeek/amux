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

type Hist = { visits?: any[], picks?: any[] };
// The palette's history (api/palette_history.rs) is faked per test, so one
// test's opens cannot reorder another's; `posted` collects what the page
// records. palette-history.spec.ts covers the real endpoint.
async function boot(page: Page, hits: any[] = [], opts: { delayMs?: number, hidden?: number, emptyOnly?: boolean, history?: Hist, posted?: any[] } = {}) {
  const asked: string[] = [];
  await page.route(/\/api\/palette\/history$/, r => r.fulfill({ json: { visits: opts.history?.visits || [], picks: opts.history?.picks || [], measured: true, n_considered: 0 } }));
  await page.route(/\/api\/palette\/visit$/, r => { opts.posted?.push(JSON.parse(r.request().postData() || '{}')); return r.fulfill({ json: { ok: true } }); });
  allowUnusedRoute(page, /\/api\/palette\/history$/);
  allowUnusedRoute(page, /\/api\/palette\/visit$/);
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
  const now = Date.now();
  await boot(page, [], { emptyOnly: true, history: { visits: [
    { kind: 'worker', id: 'gone-worker', title: 'gone-worker', visits: 9, last_at: now },
    { kind: 'worker', id: 'zz-quiet', title: 'zz-quiet', visits: 3, last_at: now - 60_000 },
    { kind: 'card', id: 'R-1', title: 'Market research plan', visits: 1, last_at: now - 120_000 },
  ] } });
  await page.evaluate(() => (window as any).openCmdk());
  await expect(page.locator('#cmdk-list .cmdk-section').first()).toHaveText('Recent');
  const sections = await page.locator('#cmdk-list .cmdk-section').allTextContents();
  expect(sections).toContain('Actions');
  const rows = await rowsText(page);
  expect(rows.slice(0, 2)).toEqual(['Worker:zz-quiet', 'Card:Market research plan']);
  expect(rows.some(r => r.includes('gone-worker')), 'a worker that no longer exists is not offered').toBe(false);
  expect(rows).toContain('Action:New worker');
});

test('the highlighted row stays put when slower results arrive', async ({ page }) => {
  const posted: any[] = [];
  await boot(page, [card('Z-1', 'zz research exact', 'doing', 5, 200)], { delayMs: 900, posted });
  const input = await search(page, 'zz');
  await expect(page.locator('#cmdk-list .cmdk-row').nth(1)).toBeVisible();
  await input.press('ArrowDown');
  const chosen = await page.locator('#cmdk-list .cmdk-row.sel .cmdk-title').textContent();
  await expect(page.locator('#cmdk-list .cmdk-row', { hasText: 'zz research exact' })).toBeVisible({ timeout: 5000 });
  await expect(page.locator('#cmdk-list .cmdk-row.sel .cmdk-title'), 'same thing still highlighted').toHaveText(chosen || '');
  // Enter opens what is highlighted, and is recorded with what was typed.
  await page.route(/\/api\/sessions\/zz-quiet\/peek\?.*/, r => r.fulfill({ json: { output: 'idle\n', history: '' } }));
  await input.press('Enter');
  await expect.poll(() => posted.find(v => v.title === chosen)).toMatchObject({ query: 'zz' });
});

// Owner, 2026-11-23: rank by where you actually go. What you chose for a query
// comes first the next time you type it, or the start of it.
test('what you chose for a query ranks first when you type it again', async ({ page }) => {
  const asked = await boot(page, [], { history: { picks: [
    { query: 'zz-q', kind: 'worker', id: 'zz-quiet', picks: 2, last_at: Date.now() - 3_600_000 },
  ] } });
  allowUnusedRoute(page, /\/api\/palette\?.*/);
  await search(page, 'zz-r');
  await expect(page.locator('#cmdk-list .cmdk-row').first()).toContainText('zz-research-lane');   // nothing learned for this
  await page.locator('#cmdk-input').fill('zz');
  await expect(page.locator('#cmdk-list .cmdk-row').first()).toContainText('zz-quiet');   // the start of "zz-q"
  await page.locator('#cmdk-input').fill('z');
  await expect(page.locator('#cmdk-list .cmdk-row').first()).not.toContainText('zz-quiet');   // one letter learns nothing
  void asked;
});

// Opens from anywhere count, not only from Cmd-K; the page restoring its last
// tab on load does not.
test('opening a tab, a group, a worker, a file or a card anywhere is recorded', async ({ page }) => {
  const posted: any[] = [];
  await boot(page, [], { emptyOnly: true, posted });
  allowUnusedRoute(page, /\/api\/palette\?.*/);
  await page.waitForTimeout(3200);   // past the load's own restore
  expect(posted, 'loading the page records nothing').toEqual([]);
  await page.locator('#tab-board').click();
  await expect.poll(() => posted.map(v => v.kind + ':' + v.id)).toContain('view:board');
  await page.evaluate(() => (window as any).switchView('sessions'));
  await page.evaluate(() => (window as any).toggleTagFilter('zzlab'));
  await page.route(/\/api\/sessions\/zz-quiet\/peek\?.*/, r => r.fulfill({ json: { output: 'idle\n', history: '' } }));
  allowUnusedRoute(page, /\/api\/sessions\/zz-quiet\/peek\?.*/);
  await page.evaluate(() => (window as any).openPeek('zz-quiet'));
  await page.evaluate(() => (window as any).closePeek && (window as any).closePeek());
  await page.route(/\/api\/file\?.*/, r => r.fulfill({ json: { path: '/tmp/zz-notes.md', content: '# hi', size: 4, mtime: 0 } }));
  allowUnusedRoute(page, /\/api\/file\?.*/);
  await page.evaluate(() => (window as any).openFilePreview('/tmp/zz-notes.md'));
  await page.route(/\/api\/board\/ZZ-7(?:\?.*)?$/, r => r.fulfill({ json: { id: 'ZZ-7', title: 'Ship the thing', status: 'todo', session: 'zz-quiet' } }));
  allowUnusedRoute(page, /\/api\/board\/ZZ-7(?:\?.*)?$/);
  await page.evaluate(() => (window as any).openBoardDetail('ZZ-7'));
  await expect.poll(() => posted.map(v => v.kind + ':' + v.id).sort()).toEqual(
    ['card:ZZ-7', 'file:/tmp/zz-notes.md', 'group:zzlab', 'view:board', 'view:sessions', 'worker:zz-quiet']);
  expect(posted.find(v => v.kind === 'card').title).toBe('Ship the thing');
  expect(posted.every(v => !v.query), 'none was chosen from Cmd-K').toBe(true);
  // The Recent rows read what was just recorded, before the server answers again.
  await page.evaluate(() => (window as any).openCmdk());
  await expect(page.locator('#cmdk-list .cmdk-row').first()).toContainText('Ship the thing');
});

test('a fuzzy guess finds a worker when nothing else matches', async ({ page }) => {
  await boot(page);
  await search(page, 'zzrsln');
  await expect(page.locator('#cmdk-foot')).toContainText('searched');
  const first = page.locator('#cmdk-list .cmdk-row').first();
  await expect(first).toContainText('zz-research-lane');
  expect(await first.locator('.cmdk-title mark').count()).toBeGreaterThan(0);
});
