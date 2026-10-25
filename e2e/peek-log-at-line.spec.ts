// A search hit names a line of a worker's terminal log, usually far older than
// the loaded scrollback. Tapping it must open the log AT that line (Ethan,
// 2026-10-18: "these should go to that location in the terminal/log"), from
// both places hits appear: the worker card's log-search matches and the peek
// find bar's "all logs" results.
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';
import fs from 'node:fs';
import path from 'node:path';
import { scratchpadDir } from './scratchpad-dir';

let NAME = '';
const TOTAL = 5000, AT = 4321;

let fixture: { request: any; auth: any; ui: string } | null = null;
test.afterEach(async () => {
  if (!fixture) return;
  const f = fixture; fixture = null;
  await cleanup('delete fixture worker', () => f.request.post(`/api/sessions/${encodeURIComponent(NAME)}/delete`,
    { headers: { ...f.auth, 'X-Amux-UI-Token': f.ui }, data: {} }));
});

async function seedLog(request: any, page: Page, project: string) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)}`, 'Content-Type': 'application/json' };
  NAME = `logat-${project}-${Date.now()}`;
  expect((await request.post('/api/sessions', { headers: auth, data: { name: NAME, dir: '/tmp', start: false } })).status()).toBe(201);
  fixture = { request, auth, ui: await page.evaluate(() => (window as any)._AMUX_UI_TOKEN as string) };
  // The test server's AMUX_HOME holds its scratchpad (playwright.config.ts).
  const home = path.dirname(await scratchpadDir(request, auth));
  fs.mkdirSync(path.join(home, 'logs'), { recursive: true });
  const lines = Array.from({ length: TOTAL }, (_, i) => `\x1b[32mrow ${i + 1}\x1b[0m filler text`);
  lines[AT - 1] = '\x1b[1mCapSolver prepaid, $6 left\x1b[0m needle-here';
  lines[AT - 3] = 'CapSolver mentioned two lines earlier';
  fs.writeFileSync(path.join(home, 'logs', `${NAME}.log`), lines.join('\n') + '\n');
  // Fetch the worker list rather than wait for the next poll: a CI mobile
  // shard took longer than 15s to poll it on its own.
  await expect.poll(() => page.evaluate(async (n) => {
    await (window as any).fetchSessions();
    return (0, eval)('sessions').some((x: any) => x.name === n);
  }, NAME), { timeout: 30_000 }).toBe(true);
  return auth;
}

async function expectAtHit(page: Page) {
  const hit = page.locator('#peek-body .peek-logat-hit');
  await expect(hit).toContainText('needle-here');
  await expect(page.locator('#peek-body .peek-logat-bar').first()).toContainText(`line ${AT.toLocaleString()} of ${TOTAL.toLocaleString()}`);
  // On screen inside the peek body, not merely in the DOM.
  await expect.poll(async () => page.evaluate(() => {
    const b = document.getElementById('peek-body')!.getBoundingClientRect();
    const h = document.querySelector('#peek-body .peek-logat-hit')!.getBoundingClientRect();
    const body = document.getElementById('peek-body')!;
    return JSON.stringify({ on: h.top >= b.top && h.bottom <= b.bottom, ht: Math.round(h.top), bt: Math.round(b.top), bb: Math.round(b.bottom), st: body.scrollTop, sh: body.scrollHeight });
  }), { timeout: 5000 }).toContain('"on":true');
}

test('the log endpoint returns the numbered lines around a line, ANSI stripped', async ({ page, request }, info) => {
  const auth = await seedLog(request, page, info.project.name);
  const d = await (await request.get(`/api/sessions/${NAME}/log?at_line=${AT}&before=2&after=1`, { headers: auth })).json();
  expect(d).toMatchObject({ at_line: AT, first: AT - 2, last: AT + 1, total: TOTAL });
  expect(d.lines).toEqual(['CapSolver mentioned two lines earlier', `row ${AT - 1} filler text`, 'CapSolver prepaid, $6 left needle-here', `row ${AT + 1} filler text`]);
  const past = await request.get(`/api/sessions/${NAME}/log?at_line=${TOTAL + 1}`, { headers: auth });
  expect(past.status()).toBe(404);
});

test('an "all logs" result in the peek find bar opens the log at its line', async ({ page, request }, info) => {
  await seedLog(request, page, info.project.name);
  await page.route('**/api/search?*', r => r.fulfill({ json: { total: 1, took_ms: 1, hits: [
    { type: 'log', id: `${NAME}.log:${AT}`, snippet: '<mark>CapSolver</mark> prepaid, $6 left' }] } }));
  await page.evaluate((n) => { (window as any).openPeek(n, { query: 'CapSolver' }); }, NAME);
  await page.evaluate(() => (window as any)._peekDeepSearch());
  const result = page.locator('#peek-deep-results .peek-deep-hit[role="button"]');
  await expect(result).toBeVisible();
  await result.click();
  await expect(page.locator('#peek-deep-results')).toBeHidden();
  await expectAtHit(page);

  // Earlier lines load above without moving the hit off screen.
  await page.locator('#peek-body .peek-logat-bar button', { hasText: 'Earlier' }).click();
  await expect.poll(() => page.evaluate(() => (0, eval)('_peekLogAt.first'))).toBe(AT - 420);
  await expectAtHit(page);
  await page.locator('#peek-body .peek-logat-bar button', { hasText: 'Back to live' }).click();
  await expect(page.locator('#peek-body .peek-logat-hit')).toHaveCount(0);
});

test('a log match on the worker card opens the log at its line', async ({ page, request }, info) => {
  await seedLog(request, page, info.project.name);
  await page.evaluate(([n, at]) => {
    (0, eval)(`logSearchMode = true; searchQuery = 'CapSolver'; _logMatches = { ${JSON.stringify(n)}: [{ line: ${at}, text: 'CapSolver prepaid, $6 left' }] }; render();`);
  }, [NAME, AT] as const);
  const hit = page.locator('.card-log-hit', { hasText: `${NAME}:${AT}` });
  await expect(hit).toBeVisible();
  await hit.click();
  await expect(page.locator('#peek-overlay')).toBeVisible();
  await expectAtHit(page);
});
