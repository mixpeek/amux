// A message row's menu reads the worker's answer from its transcript, so an answer that
// scrolled away can be read from the message that asked for it (MSG-103328). Only
// a person's message has one, and every kind of non-answer says which it is.
import { test, expect } from './fixtures';

const answers: Record<string, { body: any; status?: number; text?: string; note?: RegExp }> = {
  '91001': { body: { id: 91001, session: 'w', measured: true, found: true, ended: 'end_turn', text: 'Yes, **three** parts. <img src=x>' }, text: 'Yes, three parts. <img src=x>' },
  '91002': { body: { id: 91002, session: 'w', measured: true, found: true, ended: '', text: 'So far.' }, text: 'So far.', note: /still open/ },
  '91003': { body: { id: 91003, session: 'w', measured: true, found: true, ended: 'next_input', text: '' }, note: /Another message reached w/ },
  '91004': { body: { id: 91004, session: 'w', measured: true, found: true, ended: '', text: '' }, note: /w has not answered yet/ },
  '91005': { body: { id: 91005, session: 'w', measured: true, found: false, ended: '', text: '' }, note: /not in the current transcript of w/ },
  '91006': { body: { id: 91006, session: 'w', measured: false, why_unmeasured: "'w' has no Claude Code transcript" }, note: /Could not read it: 'w' has no Claude Code transcript/ },
};

test('Show answer shows the reply, or says why there is none', async ({ page }) => {
  await page.route('**/api/history/*/reply', route => {
    const id = route.request().url().split('/').slice(-2)[0];
    return route.fulfill({ json: answers[id].body });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cmdHistItemHTML === 'function');
  await page.evaluate(ids => {
    const w = window as any;
    const rows = ids.map(id => ({ id: +id, text: 'asked ' + id, type: 'user', kind: 'human', session: 'w', ts: Date.now() }));
    rows.push({ id: 91099, text: 'a nudge', type: 'system', kind: 'amux', session: 'w', ts: Date.now() });
    const host = document.createElement('div');
    host.id = 'answer-fixture';
    host.innerHTML = rows.map(r => w._cmdHistItemHTML(r, w._msgCtxMessages())).join('');
    document.body.prepend(host);
    host.style.cssText = 'position:fixed;inset:0;z-index:calc(var(--layer-confirmation) - 1);overflow:auto;background:var(--bg);';
  }, Object.keys(answers));

  await expect(page.locator('#answer-fixture .msg-menu-answer')).toHaveCount(Object.keys(answers).length);
  for (const [id, want] of Object.entries(answers)) {
    const row = page.locator('#answer-fixture [data-msg-key]', { hasText: 'MSG-' + id });
    await row.hover();
    await row.locator('.pm-dots').click();
    await row.locator('.msg-menu-answer').click();
    const modal = page.locator('#modal-backdrop.open');
    await expect(modal).toContainText('Answer to MSG-' + id);
    if (want.text) await expect(modal.locator('.msg-answer-text')).toHaveText(want.text);
    else await expect(modal.locator('.msg-answer-text')).toHaveCount(0);
    if (want.note) await expect(modal.locator('.msg-answer-note')).toHaveText(want.note);
    else await expect(modal.locator('.msg-answer-note')).toHaveCount(0);
    await expect(modal.locator('img')).toHaveCount(0);
    await modal.getByRole('button', { name: 'OK' }).click();
    await expect(modal).toHaveCount(0);
  }
});

test('a refused read says so and opens nothing', async ({ page }) => {
  await page.route('**/api/history/*/reply', route => route.fulfill({ status: 404, json: { error: 'no message 91007' } }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._msgShowAnswer === 'function');
  await page.evaluate(() => (window as any)._msgShowAnswer('91007'));
  await expect(page.getByText(/Could not read the answer to MSG-91007: no message 91007/)).toBeVisible();
  await expect(page.locator('#modal-backdrop.open')).toHaveCount(0);
});
