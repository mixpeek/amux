// Ethan, 2026-10-01 (iPhone): the peek header's NEEDS INPUT chip was squeezed
// between the worker name and the model pill and clipped mid-word; tapping it
// must open the card with somewhere to answer. And the group row's Reset goes
// first. No real data changes: the worker, card and every write are fakes.
import { test, expect, Page, allowUnusedRoute } from './fixtures';

const ASK = 'Should I go ahead with the shard move to the larger spot instance tonight, or hold until tomorrow?';

async function boot(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any)._bdOpenAnswer === 'function');
}

test('needs-input chip has its own row, is not clipped, and opens the card with the answer focused', async ({ page }) => {
  const card: any = { id: 'NI-9001', title: 'Shard move decision', status: 'needsyou', session: 'ni-worker', desc: '', archived: false };
  const sent: any[] = [];
  const patches: any[] = [];
  await page.route('**/api/sessions/ni-worker/send', async r => { sent.push(JSON.parse(r.request().postData() || '{}')); await r.fulfill({ json: { ok: true, submitted: true } }); });
  await page.route('**/api/sessions/ni-worker/**', r => r.request().method() === 'GET' ? r.fulfill({ json: { ok: true, output: '', history: '' } }) : r.fallback());
  await page.route('**/api/board/NI-9001**', async r => {
    if (r.request().method() === 'PATCH') {
      const p = JSON.parse(r.request().postData() || '{}');
      patches.push(p);
      if (p.desc_append) card.desc += '\n' + p.desc_append;
      if (p.status) card.status = p.status;
      return r.fulfill({ json: { ok: true, ...card } });
    }
    return r.fulfill({ json: card });
  });
  await page.route('**/api/needs-input/log', r => r.fulfill({ json: { ok: true } }));
  const worker = { name: 'ni-worker', running: true, status: 'waiting', waiting_reason: 'owner', lifecycle: 'active',
    owner_block: { card: card.id, ask: ASK }, tags: [], dir: '/tmp', flags: '', provider: 'claude', model: 'claude-opus-5-5' };
  // Serve the fake worker from /api/sessions too. Pushed only into memory, a
  // routine sessions refresh replaced the list mid-test and the title row lost
  // its "needs input" status (iOS, run 36989151604).
  const sessionsList = /\/api\/sessions(?:\?.*)?$/;
  // The live stream also replaces the list with its own snapshots; hold it
  // closed so only this route supplies sessions.
  await page.route('**/api/events**', r => r.abort());
  allowUnusedRoute(page, '**/api/events**');
  // The fleet is just this worker: forwarding to the real server made the
  // forced refresh below wait on a loaded host for 30s.
  await page.route(sessionsList, r => r.request().method() === 'GET' ? r.fulfill({ json: [worker] }) : r.fallback());
  allowUnusedRoute(page, sessionsList);
  await boot(page);
  await page.evaluate(({ card, worker }) => {
    const g = globalThis as any;
    if (!g.eval('sessions').some((x: any) => x.name === worker.name)) g.eval('sessions').push({ ...worker });
    g.eval('boardItems').push({ ...card });
    g.openPeek('ni-worker');
  }, { card, worker });
  await page.waitForFunction(() => (document.getElementById('peek-overlay') as HTMLElement).dataset.session === 'ni-worker');
  await page.evaluate(() => (globalThis as any).updatePeekStatus());
  const row = page.locator('#peek-needs-input-row');
  await expect(row).toBeVisible();
  const chip = row.locator('.status-badge.needs-input');
  await expect(chip).toBeVisible();
  // Its own row, below the title row; inside the viewport; not clipped.
  const geo = await page.evaluate(() => {
    const r = document.getElementById('peek-needs-input-row')!.getBoundingClientRect();
    const t = document.getElementById('peek-title-row')!.getBoundingClientRect();
    const c = document.querySelector('#peek-needs-input-row .status-badge.needs-input') as HTMLElement;
    const cr = c.getBoundingClientRect();
    return { rowTop: r.top, titleBottom: t.bottom, left: cr.left, right: cr.right, vw: innerWidth, h: cr.height,
      titleHasChip: !!document.querySelector('#peek-title-row .status-badge.needs-input') };
  });
  expect(geo.rowTop).toBeGreaterThanOrEqual(geo.titleBottom - 1);
  expect(geo.left).toBeGreaterThanOrEqual(0);
  expect(geo.right).toBeLessThanOrEqual(geo.vw + 0.5);
  expect(geo.h).toBeGreaterThanOrEqual(44);
  expect(geo.titleHasChip).toBe(false);
  // A sessions refresh in this window is what CI hit; force one so the race is
  // exercised every run instead of by luck.
  await page.evaluate(() => (globalThis as any).fetchSessions());
  await page.waitForTimeout(500);
  await page.evaluate(() => (globalThis as any).updatePeekStatus());
  expect(await page.evaluate(() => (globalThis as any).eval('sessions').some((x: any) => x.name === 'ni-worker'))).toBe(true);
  await expect(page.locator('#peek-title-row')).toContainText('needs input');
  // A status update must not rebuild the chip: polls run constantly, and a
  // button replaced mid-tap swallows the tap.
  // Measured inside one synchronous call, so a background poll that really
  // changed the chip cannot be mistaken for (or hide) a needless rebuild.
  expect(await page.evaluate(() => {
    const row = document.getElementById('peek-needs-input-row')!;
    const seen = new MutationObserver(() => {});
    seen.observe(row, {childList: true, subtree: true});
    (globalThis as any).updatePeekStatus();
    const rebuilt = seen.takeRecords().length;
    seen.disconnect();
    return rebuilt;
  })).toBe(0);
  // Tap: the card opens with the answer box focused.
  await chip.click();
  await expect(page.locator('#board-detail-overlay')).toHaveClass(/active/);
  await expect(page.locator('#bd-answer')).toBeVisible();
  await expect(page.locator('#bd-answer-ask')).toContainText('shard move');
  await expect(page.locator('#bd-answer-text')).toBeFocused();
  // Opening the card only READ it: no "<chip>: done" toast over the answer box.
  await page.waitForTimeout(1200);
  await expect(page.locator('#toast')).not.toContainText(': done');
  // Answer: through the triage path, to the worker and onto the card.
  await page.fill('#bd-answer-text', 'Go ahead tonight.');
  await page.click('#bd-answer-send');
  await expect(page.locator('#toast')).toContainText('Answered', { timeout: 10000 });
  // The toast must say which it was, never claim delivery it has not seen.
  await expect(page.locator('#toast')).toContainText(/queued for ni-worker|sent to ni-worker/);
  // An owner message is accepted durably into the device outbox and delivered
  // by the outbox replay right after (by design: the composer never waits on
  // the terminal). So the toast can precede the HTTP send, and on WebKit it
  // does (CI run 36935815491). Wait for the delivery, then require exactly one:
  // a second, duplicate send would still fail here.
  await expect.poll(() => sent.length, { timeout: 15000 }).toBeGreaterThanOrEqual(1);
  await page.waitForTimeout(1500);
  expect(sent).toHaveLength(1);
  expect(sent[0].text).toContain('Go ahead tonight.');
  expect(String(sent[0].msg_id)).toMatch(/^triage-/);
  expect(patches.some(p => String(p.desc_append || '').includes('Owner reply by owner in the card: Go ahead tonight.'))).toBe(true);
  expect(patches.some(p => p.status === 'todo')).toBe(true);
  await expect(page.locator('#bd-answer')).toBeHidden();
});

test('group row: Reset is the first control when a group is active, absent otherwise', async ({ page }) => {
  const workers = [
    { name: 'gr-a', tags: ['alpha'], running: true, status: 'idle', lifecycle: 'active', dir: '/tmp' },
    { name: 'gr-b', tags: ['beta'], running: true, status: 'idle', lifecycle: 'active', dir: '/tmp' },
  ];
  // The fleet must have one source of truth through boot, refresh and SSE.
  // CI's real inventory replaced the memory-only workers with [] mid-assertion.
  await page.route('**/api/events**', r => r.abort());
  allowUnusedRoute(page, '**/api/events**');
  const sessionsList = /\/api\/sessions(?:\?.*)?$/;
  await page.route(sessionsList, r => r.request().method() === 'GET'
    ? r.fulfill({ json: workers }) : r.fallback());
  await boot(page);
  await page.evaluate(() => (globalThis as any).fetchSessions());
  const first = () => page.evaluate(() => {
    const el = document.getElementById('tag-filters')!.firstElementChild as HTMLElement | null;
    return { cls: el?.className || '', text: el?.textContent || '', resets: document.querySelectorAll('#tag-filters .tag-reset-btn').length };
  });
  await page.evaluate(() => {
    const g = globalThis as any;
    g.eval('activeTag = ""; hiddenTags.clear()');
    g.render();
  });
  expect((await first()).resets).toBe(0);
  await page.evaluate(() => { (globalThis as any).eval('activeTag = "alpha"'); (globalThis as any).render(); });
  // Force the same inventory refresh that replaced the memory-only fleet in CI.
  await page.evaluate(() => (globalThis as any).fetchSessions());
  const f = await first();
  expect(f.cls).toContain('tag-reset-btn');
  expect(f.text).toContain('Reset');
  expect(f.resets).toBe(1);
});
