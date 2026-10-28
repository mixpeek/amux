// AA-53: a send from a worker's own page (merged into its Messages thread from
// message history) is the same person as their Messages posts: the name and
// picture, never the raw `member:<email>` stamp with an "M" initial. The row is
// seeded on the REAL server, so this pins the server's mapping (history.rs
// put_author), not a stub of it.
//
// AA-51: a path in a post opens amux's file viewer, with or without an
// extension, with spaces when it is a whole code span or its own line. Fenced
// code and URLs stay as they are.
import { test, expect, Page } from './fixtures';

const EMAIL = 'ethan@mixpeek.com';
const ME = { email: EMAIL, key: 'person:' + EMAIL, kind: 'person', name: 'ethan', scope_level: 'global', scope_name: '' };
const WORKER = 'aa53-page-worker';
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==', 'base64');

async function setup(page: Page, conv: string, posts: any[], seed?: () => Promise<void>) {
  const list = { measured: true, n_considered: 1, me: ME,
    channels: [{ conv: 'g:amux', name: 'amux', unread: 0, mentions: 0, last_ts: 0 }],
    people: [{ conv: 'p:' + EMAIL + '|' + EMAIL, email: EMAIL, name: 'ethan', me: true, online: true, unread: 0, mentions: 0 }],
    workers: [{ conv: 'w:' + WORKER, name: WORKER, online: false, unread: 0, mentions: 0, last_ts: 0 }] };
  await page.route(/\/api\/conversations(\?.*)?$/, r => r.fulfill({ json: list }));
  await page.route(/\/api\/conversations\/[^/]+\/posts(\?.*)?$/, r => r.fulfill({ json: {
    measured: true, n_considered: posts.length, people: [ME], workers: [], mentionable_workers: [], typing: [], posts } }));
  await page.route(/\/api\/conversations\/[^/]+\/(read|typing)$/, r => r.fulfill({ json: { ok: true } }));
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_cv_sel'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvSelect === 'function');
  if (seed) await seed();
  await page.evaluate(() => (window as any).switchView('messages'));
  await page.evaluate(c => (0, eval)("_cvSelect('" + c + "')"), conv);
}

test('a member send from the worker page shows the same name and picture as their Messages post', async ({ page }) => {
  const text = 'page send ' + Date.now();
  await page.route(/\/api\/avatars$/, r => r.fulfill({ json: { [EMAIL]: 7 } }));
  await page.route(/\/api\/avatars\/[^/?]+\?/, r => r.fulfill({ contentType: 'image/png', body: PNG }));
  await setup(page, 'w:' + WORKER, [{ id: 1, conv: 'w:' + WORKER, author_key: 'person:' + EMAIL, author_name: 'ethan',
    body: 'direct post', mentions: [], deliveries: [], ts: Date.now() - 60000 }], async () => {
    // The row a member's send from the worker page writes (origin is the server's stamp).
    const status = await page.evaluate(b => fetch('/api/history', { method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(b) }).then(r => r.status), { text, type: 'user', session: WORKER, origin: 'member:' + EMAIL });
    expect(status, 'seed a page-send history row').toBeLessThan(300);
  });
  const direct = page.locator('.cv-post', { hasText: 'direct post' });
  const fromPage = page.locator('.cv-post', { hasText: text });
  await expect(fromPage).toHaveCount(1);
  await expect(fromPage.locator('.cv-post-tag')).toHaveText('sent from its page');
  await expect(fromPage.locator('.cv-post-who')).toContainText('ethan');
  await expect(fromPage.locator('.cv-post-who')).not.toContainText('member:');
  await expect(fromPage.locator('.cv-post-who'), 'the same key as the viewer, so it reads as you').toContainText('(you)');
  const pic = fromPage.locator('img.cv-av');
  await expect(pic, 'their picture, not a letter').toHaveCount(1);
  await expect(pic).toHaveAttribute('src', /\/api\/avatars\/ethan%40mixpeek\.com/);
  expect(await pic.getAttribute('src')).toBe(await direct.locator('img.cv-av').getAttribute('src'));
});

const FOLDER = '/home/amux/wexus/runs/2026-10-24-prod/for-elliot';
const TILDE = '~/wexus/runs/2026-10-24-prod';
const SPACED = [1, 2, 3].map(i => FOLDER + '/Coeur Wine Co - AR Reconciliation ' + i + '.xlsx');
const LINE = '/home/amux/wexus/runs/a b/Report Q3.xlsx';
const FENCED = '/home/amux/in-fence/x.txt';
const URL = 'https://host.example/f/home/amux/x';
const BODY = [
  'The files are in ' + FOLDER + ', and ' + TILDE + '.',
  '',
  ...SPACED.map(p => '`' + p + '`'),
  '',
  LINE,
  '',
  '```',
  'cat ' + FENCED,
  '```',
  '',
  'Shared at ' + URL,
].join('\n');

test('bare, spaced and folder paths in a post open the file viewer; fenced code and URLs do not', async ({ page }) => {
  await page.route(/\/api\/ls\?/, r => r.fulfill({ json: { path: FOLDER, entries: [] } }));
  await setup(page, 'g:amux', [{ id: 1, conv: 'g:amux', author_key: 'worker:ed', author_name: 'ed', body: BODY, mentions: [], deliveries: [], ts: Date.now() }]);
  const body = page.locator('.cv-post-body').first();
  await expect(body).toBeVisible();
  const links = body.locator('.file-link');
  const paths = await links.evaluateAll(els => els.map(e => (e as HTMLElement).dataset.path));
  expect(paths).toEqual([FOLDER, TILDE, ...SPACED, LINE]);
  for (const p of SPACED) await expect(body.locator('code .file-link', { hasText: p }), 'a whole code span links as one path').toHaveText(p);
  await expect(body.locator('pre .file-link'), 'fenced code stays plain').toHaveCount(0);
  await expect(body.locator('pre')).toContainText(FENCED);
  await expect(body.locator('a .file-link'), 'a URL is not split into a path link').toHaveCount(0);
  await expect(body.locator('a', { hasText: URL })).toHaveCount(1);

  await page.evaluate(() => {
    const w = window as any; w.__opened = [];
    w.openFilePreview = (p: string) => w.__opened.push(['file', p]);
    w.openExplore = (p: string) => w.__opened.push(['folder', p]);
  });
  await links.filter({ hasText: SPACED[0] }).click();
  await links.filter({ hasText: FOLDER }).first().click();
  await expect.poll(() => page.evaluate(() => (window as any).__opened)).toEqual([['file', SPACED[0]], ['folder', FOLDER]]);
});
