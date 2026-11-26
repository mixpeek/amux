// Worker channels are Messages direct messages (primitive integrity audit,
// 2026-11-26, step 3). The channel drawer kept its own transcript files beside
// Messages (api/channels.rs, ~/.amux/channels/<a>__<b>.jsonl); a channel is now
// the two workers' direct message, `ww:<a>|<b>`. Against the suite's real
// server, no route stubs: one worker writes to another, the owner sees that
// conversation in Messages with the worker as author, and the @mention path
// that used to open the drawer opens the same conversation and sends there.
// Runs on desktop and on a 375px phone.
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';
import fs from 'fs';
import path from 'path';

const phone = (page: Page) => (page.viewportSize()?.width || 1000) <= 600;

async function api(page: Page, method: string, url: string, body?: unknown, worker?: string) {
  return page.evaluate(async ({ method, url, body, worker }) => {
    const extra: Record<string, string> = { 'Content-Type': 'application/json' };
    if (worker) extra['X-Amux-Session'] = worker;
    const r = await fetch(url, { method, headers: (window as any)._authHeaders(extra), body: body === undefined ? undefined : JSON.stringify(body) });
    return { status: r.status, json: await r.json().catch(() => ({})) };
  }, { method, url, body, worker });
}

test('two workers write in a Messages direct message, and an @mention opens and sends there', async ({ page }, info) => {
  const tag = info.project.name.replace(/[^a-z0-9-]/gi, '-').toLowerCase();
  const a = `pw-pair-a-${tag}`, b = `pw-pair-b-${tag}`;
  const conv = `ww:${a}|${b}`;
  const home = String(info.project.metadata?.home || '');
  expect(home, 'premise: the config names this project\'s AMUX_HOME').not.toBe('');
  const envs = [a, b].map(n => path.join(home, 'sessions', `${n}.env`));
  fs.mkdirSync(path.join(home, 'sessions'), { recursive: true });
  // The receiver is paused, so a delivery is recorded and queued and no agent
  // starts. The writer is not: a paused worker may not write to another. One
  // group, so the peer send policy lets one write to the other.
  fs.writeFileSync(envs[0], `CC_TAGS=pw-pair-${tag}\n`);
  fs.writeFileSync(envs[1], `CC_PAUSED=1\nCC_TAGS=pw-pair-${tag}\n`);
  try {
    await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_cv_sel'); });
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any)._cvSelect === 'function' && typeof (window as any).channelOpen === 'function');

    // Worker a writes to worker b by name; the server files it under both.
    const words = `rebase onto main before the demo ${Date.now()}`;
    const sent = await api(page, 'POST', `/api/conversations/ww:${b}/posts`, { body: words }, a);
    expect(sent.status, JSON.stringify(sent.json)).toBe(201);
    expect(sent.json.conv).toBe(conv);
    expect(sent.json.author).toBe(`worker:${a}`);
    expect(sent.json.mentions).toEqual([`worker:${b}`]);
    // The retired route says where channels went instead of a bare 404.
    const old = await api(page, 'GET', `/api/channels/${a}/${b}/messages`);
    expect(old.status).toBe(410);
    expect(String(old.json.error)).toContain('ww:');

    // The owner sees the conversation in Messages, listed with people.
    await page.evaluate(() => (window as any).switchView('messages'));
    const item = page.locator(`#cv-people .cv-item[data-cv="${conv}"]`);
    await expect(item, 'the two workers\' DM is listed').toHaveCount(1, { timeout: 10_000 });
    await expect(item).toContainText(`${a} ↔ ${b}`);
    await item.click();
    await expect(page.locator('#cv-title')).toHaveText(`${a} ↔ ${b}`);
    await expect(page.locator('#cv-meta')).toContainText(`Direct message between ${a} and ${b}`);
    const first = page.locator('#cv-posts .cv-post', { hasText: words }).first();
    await expect(first).toBeVisible();
    await expect(first, 'attributed to the worker that wrote it').toContainText(a);

    // The worker composer's @mention (what opened the drawer) opens the same
    // conversation with the draft addressed to the worker named.
    if (phone(page)) await page.evaluate(() => (window as any)._cvBack());
    await page.evaluate(() => (window as any).switchView('board'));
    await page.evaluate(([x, y]) => (window as any).channelOpen(x, y, 'did that land?'), [a, b]);
    await expect(page.locator('#messages-view')).toBeVisible();
    await expect(page.locator('#cv-title')).toHaveText(`${a} ↔ ${b}`);
    await expect(page.locator('#cv-input')).toHaveValue(`@${b} did that land?`);
    await expect(page.locator('#channel-drawer'), 'the drawer is gone').toHaveCount(0);
    await page.locator('#cv-send').click();
    const mine = page.locator('#cv-posts .cv-post', { hasText: 'did that land?' }).first();
    await expect(mine).toBeVisible();

    // The server has it as the owner's post, addressed to b only.
    const got = await api(page, 'GET', `/api/conversations/${encodeURIComponent(conv)}/posts`);
    expect(got.status).toBe(200);
    const p = (got.json.posts as any[]).find(x => String(x.body).includes('did that land?'));
    expect(p, JSON.stringify(got.json.posts)).toBeTruthy();
    expect(p.author_key).toBe('person:owner');
    expect(p.mentions).toEqual([`worker:${b}`]);

    // A re-render keeps it (SSE or poll), and on a phone the composer row fits.
    await page.evaluate(() => (window as any)._cvLoadList());
    await expect(mine).toBeVisible();
    const vw = page.viewportSize()!.width;
    const send = (await page.locator('#cv-send').boundingBox())!;
    expect(send.x + send.width).toBeLessThanOrEqual(vw + 1);
  } finally {
    await cleanup('pair worker envs', () => { for (const f of envs) fs.rmSync(f, { force: true }); }, info);
  }
});
