// Owner, 2026-11-24, over a To-Do.md of "@worker : task" lines: "In a md file
// i want a quick way to ask for example "status" of each of these ... Maybe
// highlight and all highlighted @ workers I can send a mass message? But then I
// also want to see all the results at once". Select lines in the Preview and an
// "Ask N workers" button appears (the file menu's "Ask every @worker…" asks the
// whole file). Each worker gets the question and its own line; its reply shows
// under that line, a bar counts replies, and the file itself is never written.
import { test, expect, Page, allowUnusedRoute } from './fixtures';
import { mkdtempSync, writeFileSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const WS = ['ask-a', 'ask-b', 'ask-c', 'ask-d'];
const DIR = mkdtempSync(join(tmpdir(), 'amux-e2e-mdask-'));
const TODO = join(DIR, 'To-Do.md');
const SRC = '# To-Do\n\n- [ ] @ask-a : ship the beta\n- [ ] @ask-b : mongo -> mvs\n- [ ] @ask-c : reduce expenses\n- [ ] @ask-d : refactor docs\n';
writeFileSync(TODO, SRC);

type Stub = { sent: Record<string, string[]>; last: Record<string, { text: string; at: string }>; failSend: Set<string>; failReadOnce: Set<string> };

async function boot(page: Page): Promise<Stub> {
  const old = new Date(Date.now() - 3600_000).toISOString();
  const st: Stub = { sent: {}, last: {}, failSend: new Set(), failReadOnce: new Set() };
  for (const w of WS) { st.sent[w] = []; st.last[w] = { text: 'An older reply from ' + w, at: old }; }
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: WS.map(name => ({
    name, dir: DIR, desc: 'md ask test worker', running: name !== 'ask-c', status: name === 'ask-c' ? 'stopped' : 'idle' })) }));
  await page.route(/\/api\/sessions\/ask-[a-d]\/last-message$/, r => {
    const w = r.request().url().split('/api/sessions/')[1].split('/')[0];
    if (st.failReadOnce.delete(w)) return r.fulfill({ status: 503, json: { error: 'busy' } });
    return r.fulfill({ json: { name: w, text: st.last[w].text, at: st.last[w].at, empty: false } });
  });
  await page.route(/\/api\/sessions\/ask-[a-d]\/send$/, r => {
    const w = r.request().url().split('/api/sessions/')[1].split('/')[0];
    if (st.failSend.has(w)) return r.fulfill({ status: 500, json: { ok: false, error: 'tmux is gone' } });
    st.sent[w].push(JSON.parse(r.request().postData() || '{}').text);
    return r.fulfill({ json: { ok: true, message: w === 'ask-c' ? 'queued' : 'sent' } });
  });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function'
    && (globalThis as any).eval('sessions').length === 4);
  await page.evaluate(p => (window as any).openFilePreview(p), TODO);
  await expect(page.locator('#file-body li .md-worker-link')).toHaveCount(4);
  return st;
}

// A real drag across the first three lines, as a hand selects them.
async function dragSelectFirstThree(page: Page) {
  const items = page.locator('#file-body li');
  const a = await items.nth(0).locator('.md-worker-link').boundingBox();
  const c = await items.nth(2).boundingBox();
  await page.mouse.move(a!.x - 4, a!.y + a!.height / 2);
  await page.mouse.down();
  await page.mouse.move(c!.x + c!.width - 4, c!.y + c!.height / 2, { steps: 8 });
  await page.mouse.up();
}

const reply = (page: Page, w: string) => page.locator('#file-body .md-ask-reply[data-worker="' + w + '"]');

test('select three lines, ask them, and each reply shows under its line', async ({ page }) => {
  const st = await boot(page);
  st.failSend.add('ask-b');
  await dragSelectFirstThree(page);
  const btn = page.locator('#md-ask-sel');
  await expect(btn).toBeVisible();
  await expect(btn).toHaveText('Ask 3 workers');
  await page.screenshot({ path: test.info().outputPath('md-ask-selection.png') });
  await btn.click();

  // The prompt says how many, that one is off and will be started, and that each gets its line.
  await expect(page.locator('#modal-msg')).toContainText('Ask 3 workers (1 off: the message starts it)');
  const input = page.locator('#modal-prompt-input');
  await expect(input).toHaveValue('What is your status?');
  await input.fill('status');
  await input.press('Enter');

  // a and c get the question and their own line; b's send fails; d was not selected.
  await expect.poll(() => st.sent['ask-a'].length).toBe(1);
  expect(st.sent['ask-a'][0]).toBe('status\n\n(Your line in To-Do.md: "ship the beta")');
  await expect.poll(() => st.sent['ask-c'].length).toBe(1);
  expect(st.sent['ask-c'][0]).toBe('status\n\n(Your line in To-Do.md: "reduce expenses")');
  expect(st.sent['ask-b']).toEqual([]);
  expect(st.sent['ask-d']).toEqual([]);

  const bar = page.locator('#file-body .md-ask-bar');
  await expect(bar).toContainText('Asked: “status”');
  await expect(bar).toContainText('0 of 3 replied · 2 waiting · 1 not sent');
  await expect(reply(page, 'ask-a')).toHaveText(/Waiting…/);
  await expect(reply(page, 'ask-c')).toHaveText(/Queued for its next pause/);
  await expect(reply(page, 'ask-b')).toHaveAttribute('data-state', 'failed');
  await expect(reply(page, 'ask-b')).toContainText('Not sent: tmux is gone');
  await expect(reply(page, 'ask-d')).toHaveCount(0);
  // The older reply each worker already had is not taken as the answer.
  await page.waitForTimeout(5600);
  await expect(reply(page, 'ask-a')).toHaveAttribute('data-state', 'waiting');

  // a answers; its reply lands under its own line, and the bar counts it.
  st.last['ask-a'] = { text: 'Beta **ships** Friday; QA is green.', at: new Date().toISOString() };
  await expect(reply(page, 'ask-a')).toHaveAttribute('data-state', 'replied', { timeout: 15000 });
  await expect(reply(page, 'ask-a')).toContainText('Beta ships Friday; QA is green.');   // plain text, no markdown marks
  await expect(reply(page, 'ask-a')).toContainText('just now');
  await expect(bar).toContainText('1 of 3 replied');
  const placed = await reply(page, 'ask-a').evaluate(el => el.closest('li')!.textContent!.includes('ship the beta'));
  expect(placed, 'the reply sits in its own line').toBe(true);
  st.last['ask-c'] = { text: 'Cut two vendors.', at: new Date().toISOString() };
  await expect(reply(page, 'ask-c')).toContainText('Cut two vendors.', { timeout: 15000 });
  await expect(bar).toContainText('2 of 3 replied · 1 not sent');
  await page.screenshot({ path: test.info().outputPath('md-ask-replies.png') });

  // A re-render keeps the answers; the file on disk is untouched.
  await page.evaluate(() => (globalThis as any).eval('_mdxRenderPreview')());
  await expect(reply(page, 'ask-a')).toContainText('Friday');
  expect(readFileSync(TODO, 'utf8')).toBe(SRC);

  // Clicking a reply opens that worker's card with the whole reply, to ask more.
  await reply(page, 'ask-a').click();
  const card = page.locator('#md-task-card');
  await expect(card).toBeVisible();
  await expect(card.locator('.mtc-name')).toHaveText('@ask-a');
  await expect(card.locator('.mtc-reply strong')).toHaveText('ships');
  await card.locator('.mtc-close').click();

  // Closing and reopening the file shows them again; Clear removes them.
  await page.evaluate(() => (window as any).closeFilePreview());
  await page.evaluate(p => (window as any).openFilePreview(p), TODO);
  await expect(reply(page, 'ask-c')).toContainText('Cut two vendors.');
  await bar.locator('.md-ask-clear').click();
  await expect(page.locator('#file-body .md-ask-reply, #file-body .md-ask-bar')).toHaveCount(0);
});

test('the file menu asks every @worker in the file', async ({ page }) => {
  const st = await boot(page);
  await page.locator('#file-menu-btn').click();
  const item = page.locator('#file-tab-askall');
  await expect(item).toBeVisible();
  await item.click();
  await expect(page.locator('#modal-prompt-input')).toBeVisible();
  await page.locator('#modal-prompt-input').press('Enter');
  for (const w of WS) await expect.poll(() => st.sent[w].length, w).toBe(1);
  expect(st.sent['ask-d'][0]).toBe('What is your status?\n\n(Your line in To-Do.md: "refactor docs")');
  await expect(page.locator('#file-body .md-ask-bar')).toContainText('0 of 4 replied · 4 waiting');
});

test('a selection with no @worker in it offers nothing, and cancelling sends nothing', async ({ page }) => {
  const st = await boot(page);
  // Nothing is sent here, which is the point: the send and reply stubs stay unused.
  allowUnusedRoute(page, /\/api\/sessions\/ask-[a-d]\/last-message$/);
  allowUnusedRoute(page, /\/api\/sessions\/ask-[a-d]\/send$/);
  const h = page.locator('#file-body h1');
  const b = await h.boundingBox();
  await page.mouse.move(b!.x + 2, b!.y + b!.height / 2);
  await page.mouse.down();
  await page.mouse.move(b!.x + 60, b!.y + b!.height / 2, { steps: 4 });
  await page.mouse.up();
  await page.waitForTimeout(400);
  await expect(page.locator('#md-ask-sel')).toBeHidden();
  await dragSelectFirstThree(page);
  await page.locator('#md-ask-sel').click();
  await page.locator('#modal-prompt-input').press('Escape');
  await page.waitForTimeout(500);
  expect(Object.values(st.sent).flat()).toEqual([]);
  await expect(page.locator('#file-body .md-ask-bar')).toHaveCount(0);
});

test('on a phone the bar is one row and each reply is a 44px target', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 740 });
  const st = await boot(page);
  await page.evaluate(() => (globalThis as any).eval('_mdAskAll')());
  await page.locator('#modal-prompt-input').press('Enter');
  for (const w of WS) await expect.poll(() => st.sent[w].length).toBe(1);
  st.last['ask-a'] = { text: 'A long answer that will not fit on one phone line so it must be cut with an ellipsis rather than wrapping.', at: new Date().toISOString() };
  await expect(reply(page, 'ask-a')).toHaveAttribute('data-state', 'replied', { timeout: 15000 });
  const g = await page.evaluate(() => {
    const bar = document.querySelector('#file-body .md-ask-bar')!.getBoundingClientRect();
    const kids = [...document.querySelectorAll('#file-body .md-ask-bar > *')].map(k => k.getBoundingClientRect());
    const rs = [...document.querySelectorAll('#file-body .md-ask-reply')].map(r => r.getBoundingClientRect());
    const clear = document.querySelector('#file-body .md-ask-clear')!.getBoundingClientRect();
    return { clearOneLine: clear.height <= 48, barH: bar.height, oneRow: kids.every(k => k.top >= bar.top - 1 && k.bottom <= bar.bottom + 1),
      tall: rs.every(r => r.height >= 44), fits: rs.every(r => r.right <= innerWidth) };
  });
  expect(g.oneRow, 'the bar keeps one row').toBe(true);
  expect(g.barH).toBeLessThanOrEqual(64);
  expect(g.clearOneLine, 'Clear stays on one line').toBe(true);
  expect(g.tall, '44px touch targets').toBe(true);
  expect(g.fits, 'no reply runs off the screen').toBe(true);
  await page.screenshot({ path: test.info().outputPath('md-ask-phone.png') });
});

// Land review F1: the read before the ask fails, so there is no baseline. The
// worker's older message must not then be shown as its answer.
test('a failed read before the ask never shows the older message as the answer', async ({ page }) => {
  const st = await boot(page);
  st.failReadOnce.add('ask-d');
  await page.evaluate(() => (globalThis as any).eval('_mdAskAll')());
  await page.locator('#modal-prompt-input').press('Enter');
  await expect.poll(() => st.sent['ask-d'].length).toBe(1);
  await page.waitForTimeout(11500);   // two polls: both read only the older message
  await expect(reply(page, 'ask-d')).toHaveAttribute('data-state', 'waiting');
  await expect(reply(page, 'ask-d')).not.toContainText('An older reply');
  st.last['ask-d'] = { text: 'Docs refactor is half done.', at: new Date().toISOString() };
  await expect(reply(page, 'ask-d')).toContainText('Docs refactor is half done.', { timeout: 15000 });
});
