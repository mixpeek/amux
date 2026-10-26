// Races outside the worker tabs (Ethan, 2026-10-24: "theres race conditions
// all over the app"): a second press while the first is in flight, a reply to
// an older action landing over a newer one, a slow load wiping edits made
// while it was out, and a badge refreshed on a different clock than the data
// it counts. Every route here is a fake.
import { test, expect, Page, allowUnusedRoute } from './fixtures';

async function boot(page: Page) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).updateBoardItem === 'function' && typeof (window as any)._mapLoad === 'function');
}
const slow = (ms: number) => new Promise(res => setTimeout(res, ms));

test('calendar Save pressed twice creates one event', async ({ page }) => {
  const posts: string[] = [];
  await page.route('**/api/cal-events', async r => {
    if (r.request().method() !== 'POST') return r.fulfill({ json: [] });
    posts.push(r.request().postData() || '');
    await slow(800);
    await r.fulfill({ json: { id: 'EVT-race' } });
  });
  await boot(page);
  await page.evaluate(() => {
    const g = window as any;
    g.openEventModal(null, '2026-10-24T10:00', '2026-10-24T10:30', false);
    (document.getElementById('ev-title') as HTMLInputElement).value = 'race event';
    g.saveEvent(''); g.saveEvent('');
  });
  await page.waitForTimeout(1500);
  expect(posts.length, 'one POST for two presses').toBe(1);
});

test('a new note saved twice quickly is one note, and the second save carries the newer text', async ({ page }) => {
  const puts: any[] = [];
  await page.route('**/api/file', async r => {
    if (r.request().method() !== 'PUT') return r.fallback();
    const body = JSON.parse(r.request().postData() || '{}');
    puts.push(body);
    await slow(600);
    await r.fulfill({ json: { ok: true, path: body.path } });
  });
  await boot(page);
  await page.evaluate(() => {
    const g = window as any;
    g.eval("_fileData = { path: '/tmp/race-notes/untitled.md', content: '', is_markdown: true, _isNew: true }");
    const ta = document.getElementById('file-edit-ta') as HTMLTextAreaElement;
    ta.value = 'first';
    g._fileSave();
    ta.value = 'first and more';
    g._fileSave(); g._fileSave();
  });
  await expect.poll(() => puts.length, { timeout: 5000 }).toBe(2);
  await page.waitForTimeout(800);
  expect(puts.length, 'one save running, one queued, the third press joined the queue').toBe(2);
  expect(puts.map(p => p.path), 'no "-2" duplicate').toEqual(['/tmp/race-notes/untitled.md', '/tmp/race-notes/untitled.md']);
  expect(puts[0].create_only).toBe(true);
  expect(puts[1].create_only, 'the second save overwrites the note the first created').toBeFalsy();
  expect(puts[1].content).toBe('first and more');
});

test('two quick edits to one card both land: the second carries the rev the first returned', async ({ page }) => {
  const patches: any[] = [];
  let rev = 7;
  // The server's copy of the card. The board list serves it too: a refresh
  // from the real server has no RACE-1 and used to drop the injected card
  // mid-test, so the second edit lost the first one's rev (CI f2bb59c75,
  // desktop and mobile: expect_rev [7, 7]).
  const saved: any = { id: 'RACE-1', title: 'race card', status: 'todo', session: '', rev };
  await page.route(/\/api\/board\?archived=0/, r => r.fulfill({ json: [{ ...saved }] }));
  await page.route(/\/api\/board\/RACE-1$/, async r => {
    if (r.request().method() !== 'PATCH') return r.fallback();
    const body = JSON.parse(r.request().postData() || '{}');
    patches.push(body);
    await slow(400);
    if (body.expect_rev !== undefined && body.expect_rev !== rev) return r.fulfill({ status: 409, json: { error: 'rev conflict' } });
    rev += 1;
    if (body.status) saved.status = body.status;
    if (body.session !== undefined) saved.session = body.session;
    saved.rev = rev;
    await r.fulfill({ json: { ...saved } });
  });
  await boot(page);
  const result = await page.evaluate(async () => {
    const g = window as any;
    g.eval("boardItems.push({id:'RACE-1', title:'race card', status:'todo', session:'', rev:7})");
    const a = g.updateBoardItem('RACE-1', { status: 'doing' });
    const b = g.updateBoardItem('RACE-1', { session: 'someone' });
    const [ra, rb] = await Promise.all([a, b]);
    return { a: !!ra, b: !!rb, card: g.eval("boardItems.find(i => i.id === 'RACE-1')") };
  });
  expect(patches.map(p => p.expect_rev)).toEqual([7, 8]);
  expect(result.a && result.b, 'neither edit was refused').toBe(true);
  expect(result.card.session, 'the second edit is not rolled back').toBe('someone');
});

test('a slow full-board load does not wipe an edit made while it loaded', async ({ page }) => {
  await page.route(/\/api\/board\?done_limit=0&full=1/, async r => {
    await slow(1000);
    await r.fulfill({ json: [{ id: 'RACE-2', title: 'old title', status: 'todo', rev: 1 }] });
  });
  await page.route(/\/api\/board\/RACE-2$/, r => r.fulfill({ json: { id: 'RACE-2', title: 'new title', status: 'todo', rev: 2 } }));
  // The regular board poll serves the server's copy (after the edit): a read
  // from the real server has no RACE-2 and dropped the injected card, so the
  // final lookup found nothing (2 of 3 local runs on main, "reading 'title'").
  await page.route(/\/api\/board\?archived=0/, r => r.fulfill({ json: [{ id: 'RACE-2', title: 'new title', status: 'todo', rev: 2 }] }));
  const superseded: string[] = [];
  page.on('request', r => { const b = r.postData() || ''; if (b.includes('board_full_load_superseded')) superseded.push(b); });
  await boot(page);
  await page.evaluate(async () => {
    const g = window as any;
    g.eval("boardItems.push({id:'RACE-2', title:'old title', status:'todo', rev:1}); boardSearchQuery = 'title'; _boardFullTs = 0");
    g._boardEnsureFull();
    await g.updateBoardItem('RACE-2', { title: 'new title' });
  });
  await page.waitForTimeout(1500);
  const card = await page.evaluate(() => (window as any).eval("boardItems.find(i => i.id === 'RACE-2')"));
  expect(card.title, 'the edit survives the full load that started before it').toBe('new title');
  expect(superseded.length).toBe(1);
});

test('a pin dropped while the map loads is kept', async ({ page }) => {
  await page.route('**/api/map', async r => { await slow(1000); await r.fulfill({ json: { pins: [{ id: 'p-old', name: 'old', lat: 1, lng: 1, rev: 1 }], tags: [], settings: {} } }); });
  await page.route(/\/api\/map\/pins\/.*/, r => r.fulfill({ json: { pin: { id: 'p-new', name: 'dropped now', lat: 2, lng: 2, rev: 1 } } }));
  await page.route('**/api/map/meta', r => r.fulfill({ json: { ok: true } }));
  allowUnusedRoute(page, '**/api/map/meta');   // sent only when tags changed before the load
  await boot(page);
  await page.evaluate(() => {
    const g = window as any;
    g._mapLoad();
    g._mapPutPin({ id: 'p-new', name: 'dropped now', lat: 2, lng: 2 });
  });
  await page.waitForTimeout(1500);
  const ids = await page.evaluate(() => (window as any).eval('_mapPins').map((p: any) => p.id).sort());
  expect(ids, "the server's pin and the one dropped during the load").toEqual(['p-new', 'p-old']);
});

test('Run pressed twice runs the schedule once', async ({ page }) => {
  const runs: string[] = [];
  await page.route(/\/api\/schedules\/SCHED-RACE\/run$/, async r => { runs.push(r.request().url()); await slow(700); await r.fulfill({ json: { ok: true, status: 'ok' } }); });
  await boot(page);
  await page.evaluate(() => { const g = window as any; g.runScheduleNow('SCHED-RACE'); g.runScheduleNow('SCHED-RACE'); });
  await page.waitForTimeout(1500);
  expect(runs.length).toBe(1);
});

test('an empty Send tapped twice submits the suggestion once', async ({ page }) => {
  await boot(page);
  const calls = await page.evaluate(async () => {
    const g = window as any;
    let n = 0;
    g._submitSuggestion = () => { n++; return new Promise(res => setTimeout(res, 500)); };
    const inp = document.createElement('textarea'); inp.id = 'input-zz-race'; document.body.appendChild(inp);
    g.sendFromInput('zz-race'); g.sendFromInput('zz-race');
    await new Promise(res => setTimeout(res, 700));
    return n;
  });
  expect(calls).toBe(1);
});

test('the needs-you bell refreshes soon after the board changes', async ({ page }) => {
  let digests = 0;
  await page.route('**/api/debug/needsyou-digest', r => { digests++; return r.fulfill({ json: { measured: true, items: [], total: 0 } }); });
  await boot(page);
  await page.waitForTimeout(500);
  const before = digests;
  await page.evaluate(() => { const g = window as any; g.eval("boardItems.push({id:'RACE-3', title:'answered', status:'done', updated: 99})"); g._nudgeWorkersOnBoardChange(); });
  await expect.poll(() => digests, { timeout: 6000 }).toBeGreaterThan(before);
});

test('a steady stream of board changes cannot hold the needs-you bell stale', async ({ page }) => {
  let digests = 0;
  await page.route('**/api/debug/needsyou-digest', r => { digests++; return r.fulfill({ json: { measured: true, items: [], total: 0 } }); });
  await boot(page);
  await page.waitForTimeout(500);
  const before = digests;
  // A change every second for six seconds. A timer restarted on each change
  // would not fire until three seconds after the last one.
  await page.evaluate(async () => {
    const g = window as any;
    for (let i = 0; i < 6; i++) {
      g.eval(`boardItems.push({id:'STREAM-${i}', title:'moved', status:'done', updated: ${100 + i}})`);
      g._nudgeWorkersOnBoardChange();
      await new Promise(res => setTimeout(res, 1000));
    }
  });
  expect(digests, 'the bell refreshed while the board kept changing').toBeGreaterThan(before);
});
