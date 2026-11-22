// The worker's Computer tab: each step it took in this Mac's apps, with what
// it saw (owner, 2026-11-22: Athena "sends screenshots to the chat so users
// can see whats happening"). The server writes one row per call to
// <AMUX_HOME>/computer/mac-activity.jsonl and serves it at
// GET /api/computer/mac/activity; this spec writes rows there as the server
// does, opens the worker through the real UI, and checks the tab shows them
// newest first, marks refusals, loads each screenshot, opens one in the file
// viewer, picks up a new step live, is a Split option, and fits a phone.
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';
import fs from 'node:fs';
import path from 'node:path';
import { scratchpadDir } from './scratchpad-dir';

// A 4x3 red PNG.
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAQAAAADCAIAAAA7ljmRAAAAEklEQVR4nGP4z8AARwzEcQAA/rgL9XlE6PQAAAAASUVORK5CYII=', 'base64');

let NAME = '';
let fixture: { request: any; auth: any; ui: string } | null = null;
test.afterEach(async () => {
  if (!fixture) return;
  const f = fixture; fixture = null;
  await cleanup('delete fixture worker', () => f.request.post(`/api/sessions/${encodeURIComponent(NAME)}/delete`,
    { headers: { ...f.auth, 'X-Amux-UI-Token': f.ui }, data: {} }));
});

async function seed(request: any, page: Page, project: string) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)}`, 'Content-Type': 'application/json' };
  NAME = `cu-${project}-${Date.now()}`.replace(/[^a-zA-Z0-9-]/g, '-');
  expect((await request.post('/api/sessions', { headers: auth, data: { name: NAME, dir: '/tmp', start: false } })).status()).toBe(201);
  fixture = { request, auth, ui: await page.evaluate(() => (window as any)._AMUX_UI_TOKEN as string) };
  const home = path.dirname(await scratchpadDir(request, auth));
  const shots = path.join(home, 'computer-screenshots');
  fs.mkdirSync(shots, { recursive: true });
  fs.mkdirSync(path.join(home, 'computer'), { recursive: true });
  const t = Date.now() - 60_000;
  const shot = path.join(shots, `mac-${NAME}-${t}.png`);
  fs.writeFileSync(shot, PNG);
  const rows = [
    { ts: t, worker: NAME, tool: 'get_window_state', apps: 'Spotify (com.spotify.client)', verdict: 'called', screenshot: shot, ms: 120 },
    { ts: t + 1000, worker: NAME, tool: 'click', apps: 'Spotify (com.spotify.client)', verdict: 'called', screenshot: null, ms: 80 },
    { ts: t + 2000, worker: NAME, tool: 'launch_app', apps: 'Terminal', verdict: 'refused', code: 'app_blocked', screenshot: null, ms: 0 },
    // Another worker's row must not show on this one.
    { ts: t + 3000, worker: NAME + '-other', tool: 'click', apps: 'Notes', verdict: 'called', screenshot: null, ms: 1 },
  ];
  const file = path.join(home, 'computer', 'mac-activity.jsonl');
  fs.appendFileSync(file, rows.map(r => JSON.stringify(r)).join('\n') + '\n');
  await expect.poll(() => page.evaluate(async (n) => {
    await (window as any).fetchSessions();
    return (0, eval)('sessions').some((x: any) => x.name === n);
  }, NAME), { timeout: 30_000 }).toBe(true);
  return { file, t, shot };
}

async function openComputerTab(page: Page) {
  await page.evaluate((n) => { (window as any).openPeek(n); }, NAME);
  const tab = page.locator('#peek-tab-computer');
  await tab.scrollIntoViewIfNeeded();
  await tab.click();
  await expect(page.locator('#peek-computer-panel')).toBeVisible();
}

test('the Computer tab shows the worker\'s steps, newest first, with what it saw', async ({ page, request }, info) => {
  const { file, t, shot } = await seed(request, page, info.project.name);
  await openComputerTab(page);
  const rows = page.locator('#peek-computer-list .pcu-row');
  await expect(rows).toHaveCount(3);
  await expect(rows.nth(0)).toContainText('launch_app');
  await expect(rows.nth(0)).toContainText('app blocked');
  await expect(rows.nth(0)).toHaveClass(/pcu-refused/);
  await expect(rows.nth(1)).toContainText('click');
  await expect(rows.nth(2)).toContainText('get_window_state');
  await expect(page.locator('#peek-computer-list')).not.toContainText('Notes');
  await expect(page.locator('#peek-computer-count')).toHaveText('3 steps');
  await expect(page.locator('#peek-tab-computer-count')).toHaveText('3');
  // The screenshot loads (an authenticated image, not a broken one).
  const img = rows.nth(2).locator('.pcu-shot img');
  await expect.poll(() => img.evaluate((i: HTMLImageElement) => i.complete && i.naturalWidth), { timeout: 10_000 }).toBe(4);

  // A new step shows within a few seconds, with no reload.
  fs.appendFileSync(file, JSON.stringify({ ts: t + 5000, worker: NAME, tool: 'type_text', apps: 'Spotify (com.spotify.client)', verdict: 'called', screenshot: null, ms: 40 }) + '\n');
  await expect(rows).toHaveCount(4, { timeout: 8000 });
  await expect(rows.nth(0)).toContainText('type_text');

  // A screenshot opens in the file viewer.
  await rows.nth(3).locator('.pcu-shot').click();
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 10_000 });
  await expect(page.locator('#file-title')).toContainText(path.basename(shot));
});

test('Computer is one of the Split options, and an empty worker says what will appear', async ({ page, request }, info) => {
  await seed(request, page, info.project.name);
  await page.evaluate((n) => { (window as any).openPeek(n); }, NAME);
  await expect.poll(() => page.evaluate(() => (0, eval)('_peekSplitViewOptions()').map((o: string[]) => o[0]))).toContain('computer');
  // A worker with no rows.
  const other = NAME + '-empty';
  const auth = fixture!.auth;
  expect((await request.post('/api/sessions', { headers: auth, data: { name: other, dir: '/tmp', start: false } })).status()).toBe(201);
  try {
    await page.evaluate(async (n) => { await (window as any).fetchSessions(); (window as any).openPeek(n); }, other);
    await page.locator('#peek-tab-computer').click();
    await expect(page.locator('#peek-computer-list')).toContainText('No computer use yet');
  } finally {
    await request.post(`/api/sessions/${encodeURIComponent(other)}/delete`, { headers: { ...auth, 'X-Amux-UI-Token': fixture!.ui }, data: {} });
  }
});

test('on a phone the tab keeps one row of controls and the steps get the room', async ({ page, request }, info) => {
  await page.setViewportSize({ width: 375, height: 760 });
  await seed(request, page, info.project.name);
  await openComputerTab(page);
  await expect(page.locator('#peek-computer-list .pcu-row')).toHaveCount(3);
  const m = await page.evaluate(() => {
    const bar = document.querySelector('#peek-computer-panel .pcu-bar')!.getBoundingClientRect();
    const list = document.getElementById('peek-computer-list')!.getBoundingClientRect();
    return { barH: bar.height, overflowX: document.documentElement.scrollWidth > window.innerWidth, listTop: list.top, listH: list.height };
  });
  if (process.env.PCU_SHOT) await page.screenshot({ path: process.env.PCU_SHOT });
  expect(m.barH).toBeLessThanOrEqual(56);
  expect(m.overflowX).toBe(false);
  const shot = page.locator('#peek-computer-list .pcu-shot').first();
  const box = await shot.boundingBox();
  expect(box && box.height >= 44 ? 'ok' : JSON.stringify(box)).toBe('ok');
});
