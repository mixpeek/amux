// The X on a card's "Waiting on you" box dismisses the ask for good, after an
// "are you sure" (Ethan, 2026-10-12). It used to only hide the box, so the card
// stayed in needsyou and kept flagging. Cancel in the confirm changes nothing.
// It sends NOTHING to the worker (Ethan, 2026-10-21: "it should just dismiss it
// and it shouldnt send anything to the worker"): a note on the card, needsyou
// to backlog (todo would be dispatched back to the worker), and a permanent
// server-side snooze so it stays gone on every device.
// The worker, the card and every write are fakes.
import { test, expect, allowUnusedRoute } from './fixtures';

for (const start of ['needsyou', 'backlog']) test(`the X asks first, sends nothing to the worker and dismisses for good (card in ${start})`, async ({ page }) => {
  const card: any = { id: 'NI-9002', title: 'Allow the debugging prompt', status: start, session: 'nid-worker', desc: '', archived: false,
    ask_question: 'Can you click Allow so I can mint the key myself?' };
  const sent: any[] = [];
  const patches: any[] = [];
  const logged: any[] = [];
  await page.route('**/api/sessions/nid-worker/send', async r => { sent.push(JSON.parse(r.request().postData() || '{}')); await r.fulfill({ json: { ok: true, submitted: true } }); });
  allowUnusedRoute(page, '**/api/sessions/nid-worker/send');   // the point: a dismiss never sends
  await page.route('**/api/board/NI-9002**', async r => {
    if (r.request().method() === 'PATCH') {
      const p = JSON.parse(r.request().postData() || '{}');
      patches.push(p);
      if (p.desc_append) card.desc += '\n' + p.desc_append;
      if (p.status) card.status = p.status;
      return r.fulfill({ json: { ok: true, ...card } });
    }
    return r.fulfill({ json: card });
  });
  const snoozes: any[] = [];
  await page.route('**/api/needs-input/snooze', r => { snoozes.push(JSON.parse(r.request().postData() || '{}')); return r.fulfill({ json: { ok: true } }); });
  await page.route('**/api/needs-input/log', r => { logged.push(JSON.parse(r.request().postData() || '{}')); return r.fulfill({ json: { ok: true } }); });
  const worker = { name: 'nid-worker', running: true, status: 'idle', lifecycle: 'active', tags: [], dir: '/tmp', provider: 'claude' };
  await page.route('**/api/events**', r => r.abort());
  allowUnusedRoute(page, '**/api/events**');
  const sessionsList = /\/api\/sessions(?:\?.*)?$/;
  await page.route(sessionsList, r => r.request().method() === 'GET' ? r.fulfill({ json: [worker] }) : r.fallback());
  allowUnusedRoute(page, sessionsList);
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._bdOpenAnswer === 'function');
  await page.evaluate(({ card, worker }) => {
    const g = globalThis as any;
    if (!g.eval('sessions').some((x: any) => x.name === worker.name)) g.eval('sessions').push({ ...worker });
    g.eval('boardItems').push({ ...card });
    g._bdOpenAnswer(card.id, worker.name);
  }, { card, worker });
  const box = page.locator('#bd-answer');
  await expect(box).toBeVisible();
  const x = page.locator('#bd-answer-cancel');
  await expect(x).toHaveAttribute('aria-label', 'Dismiss this ask for good');

  // Cancel in the confirm: the box stays, nothing is sent or written.
  await x.click();
  const modal = page.locator('#modal-backdrop');
  await expect(modal).toHaveClass(/open/);
  await expect(page.locator('#modal-msg')).toContainText('Dismiss this ask for good?');
  await expect(page.locator('#modal-msg')).toContainText('nid-worker');
  await modal.getByRole('button', { name: 'Cancel' }).click();
  await expect(modal).not.toHaveClass(/open/);
  await expect(box).toBeVisible();
  await page.waitForTimeout(800);
  expect(sent).toHaveLength(0);
  expect(patches).toHaveLength(0);
  expect(snoozes).toHaveLength(0);
  expect(card.status).toBe(start);

  // Confirm: nothing goes to the worker; the card is noted, leaves needsyou for
  // backlog (never todo, which is dispatched), and the ask is snoozed for good.
  await x.click();
  await expect(page.locator('#modal-msg')).toContainText('Nothing is sent to nid-worker');
  await modal.getByRole('button', { name: 'Dismiss' }).click();
  await expect(page.locator('#toast')).toContainText('Dismissed', { timeout: 10000 });
  await expect.poll(() => snoozes.length, { timeout: 15000 }).toBe(1);
  await page.waitForTimeout(1500);
  expect(sent).toHaveLength(0);
  expect(snoozes[0].key).toBe('card:NI-9002');
  expect(snoozes[0].until - Date.now() / 1000).toBeGreaterThan(365 * 86400);
  expect(patches.some(p => String(p.desc_append || '').includes('Dismissed without an answer by owner in the card (nothing sent to the worker)'))).toBe(true);
  expect(patches.some(p => p.status === 'todo')).toBe(false);
  expect(card.status).toBe('backlog');
  expect(logged.some(l => l.action === 'dismiss' && l.outcome === 'ok')).toBe(true);
  await expect(box).toBeHidden();
  // A re-render of the open card keeps it gone.
  await page.evaluate(c => (globalThis as any)._bdShowAnswer({ ...c }, false), card);
  await expect(box).toBeHidden();
});
