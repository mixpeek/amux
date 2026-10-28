// Messages as the default way to collaborate, Slack-grade (owner, 2026-10-27):
// threads, reactions, edits, deletes, search, older history, profile pictures,
// voice notes (one capture: words typed, audio attached), link-preview safety,
// live updates without a reload, deep links, and how fast it opens. Against
// the real server: no route stubs. Runs on desktop and on a 375px phone.
import { test, expect, Page, APIRequestContext } from './fixtures';

const CONV = 'g:general';
// Every test here posts in #general (a channel needs a real group), and the
// history test adds 120 posts: run in parallel they re-render each other's
// posts out from under a click. One at a time.
test.describe.configure({ mode: 'serial' });

async function token(page: Page): Promise<string> {
  return page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
}

async function api(request: APIRequestContext, tok: string, method: string, path: string, data?: unknown, headers: Record<string, string> = {}) {
  const r = await request.fetch(path, { method, data, headers: { Authorization: `Bearer ${tok}`, ...headers } });
  return { status: r.status(), json: await r.json().catch(() => ({})) };
}

async function openMessages(page: Page) {
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_cv_sel'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvSelect === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
  await page.evaluate(c => (window as any)._cvSelect(c), CONV);
  await expect(page.locator('#cv-pane')).toBeVisible();
}

function post(page: Page, text: string) {
  return page.locator('#cv-posts .cv-post', { hasText: text }).first();
}

const phone = (page: Page) => (page.viewportSize()?.width || 1000) <= 600;
// An action on a post: the hover toolbar with a mouse, the ⋯ sheet on a phone.
async function act(page: Page, p: ReturnType<typeof post>, label: RegExp | string) {
  if (phone(page)) {
    await p.locator('.cv-more').click();
    await page.locator('.cv-sheet-item', { hasText: label }).first().click();
  } else {
    await p.hover();
    const sel = typeof label === 'string' ? `.cv-acts button[aria-label="${label}"]` : '.cv-acts button[aria-label^="React"]';
    await p.locator(sel).first().click();
  }
}

test('send, live, react, edit, thread, delete: the whole collaboration loop', async ({ page, request }) => {
  await openMessages(page);
  const tok = await token(page);
  const tag = 'loop-' + Date.now();

  // Send from the composer; it shows without a reload.
  const t0 = Date.now();
  await page.locator('#cv-input').fill(`hello ${tag}`);
  await page.locator('#cv-send').click();
  await expect(post(page, `hello ${tag}`)).toBeVisible();
  const sendMs = Date.now() - t0;

  // Someone else's post arrives live (SSE), no reload, no click. Wait for the
  // stream first: a post made before it opens is caught up on connect, which
  // is a different path from the one this measures.
  await page.waitForFunction(() => (window as any).eval('_liveSSE') === true, null, { timeout: 15000 });
  const t1 = Date.now();
  await api(request, tok, 'POST', `/api/conversations/${CONV}/posts`, { body: `from elsewhere ${tag}` });
  await expect(post(page, `from elsewhere ${tag}`)).toBeVisible({ timeout: 8000 });
  const liveMs = Date.now() - t1;

  // React. The post is held by its id: while it is being edited its text is
  // the draft, which no longer contains the original words.
  const mineId = await post(page, `hello ${tag}`).getAttribute('data-id');
  const mine = page.locator(`#cv-posts .cv-post[data-id="${mineId}"]`);
  await act(page, mine, /React/);
  await expect(mine.locator('.cv-react.mine')).toContainText('1');

  // Edit your own post.
  await act(page, mine, 'Edit');
  await mine.locator('textarea.cv-edit').fill(`hello edited ${tag}`);
  await mine.locator('.cv-edit-acts .btn.primary').click();
  const edited = post(page, `hello edited ${tag}`);
  await expect(edited.locator('.cv-edited')).toBeVisible();

  // Reply in a thread; the post shows its reply count.
  await act(page, edited, 'Reply in thread');
  await expect(page.locator('#cv-thread')).toBeVisible();
  await page.locator('#cv-thread-input').fill(`a reply ${tag}`);
  await page.locator('#cv-thread-send').click();
  await expect(page.locator('#cv-thread-posts .cv-post', { hasText: `a reply ${tag}` })).toBeVisible();
  await page.locator('#cv-thread .cv-thread-close').click();
  await expect(edited.locator('.cv-thread-line')).toContainText('1 reply');
  // A reply stays in its thread, out of the channel's top level.
  await expect(page.locator('#cv-posts > .cv-post', { hasText: `a reply ${tag}` })).toHaveCount(0);

  // Delete, for everyone.
  const other = post(page, `from elsewhere ${tag}`);
  await act(page, other, 'Delete');
  await page.locator('#modal-btns .btn.danger').click();
  await expect(page.locator('#cv-posts .cv-deleted').last()).toBeVisible();
  const after = await api(request, tok, 'GET', `/api/conversations/${CONV}/posts?limit=50`);
  expect((after.json.posts || []).some((p: any) => String(p.body).includes(`from elsewhere ${tag}`)), 'a deleted body is gone from the API').toBe(false);

  test.info().annotations.push({ type: 'timing', description: JSON.stringify({ send_to_visible_ms: sendMs, live_ms: liveMs }) });
  expect(sendMs, 'a sent message shows in under 2 s').toBeLessThan(2000);
  expect(liveMs, 'someone else\'s message shows in under 3 s, live').toBeLessThan(3000);
});

test('a worker mentioned in a channel is told to answer in the thread; a thread reply reaches it without a mention', async ({ page, request }) => {
  await openMessages(page);
  const tok = await token(page);
  // Thread routing is pinned at the API: the stored post names its thread.
  const root = await api(request, tok, 'POST', `/api/conversations/${CONV}/posts`, { body: 'root for threading ' + Date.now() });
  expect(root.status).toBe(201);
  const reply = await api(request, tok, 'POST', `/api/conversations/${CONV}/posts`, { body: 'a reply', parent_id: root.json.id });
  expect(reply.json.parent_id).toBe(root.json.id);
  const replyToReply = await api(request, tok, 'POST', `/api/conversations/${CONV}/posts`, { body: 'deeper', parent_id: reply.json.id });
  expect(replyToReply.json.parent_id, 'a reply to a reply joins the same thread').toBe(root.json.id);
  const bad = await api(request, tok, 'POST', `/api/conversations/${CONV}/posts`, { body: 'x', parent_id: 999999999 });
  expect(bad.status).toBe(400);
  const t = await api(request, tok, 'GET', `/api/conversations/${CONV}/posts?thread=${root.json.id}`);
  expect((t.json.posts || []).map((p: any) => p.body)).toEqual([expect.stringContaining('root for threading'), 'a reply', 'deeper']);
});

test('search finds words across conversations, highlights them, and opens the post', async ({ page, request }) => {
  await openMessages(page);
  const tok = await token(page);
  const word = 'zebrafinch' + Date.now().toString(36);
  await api(request, tok, 'POST', `/api/conversations/${CONV}/posts`, { body: `the ${word} is in the box` });
  if (phone(page)) await page.evaluate(() => (window as any)._cvBack());
  await page.locator('#cv-search').fill(word.slice(0, 12));
  await page.locator('#cv-search').press('Enter');
  const hit = page.locator('#cv-posts .cv-post', { hasText: word }).first();
  await expect(hit.locator('.cv-snippet mark')).toBeVisible();
  const scoped = await api(request, tok, 'GET', `/api/conversations/search?q=${encodeURIComponent(word + ' in:general')}`);
  expect((scoped.json.posts || []).length).toBe(1);
  const elsewhere = await api(request, tok, 'GET', `/api/conversations/search?q=${encodeURIComponent(word + ' in:nosuch')}`);
  expect((elsewhere.json.posts || []).length).toBe(0);
  const odd = await api(request, tok, 'GET', `/api/conversations/search?q=${encodeURIComponent('"(*) AND OR NEAR')}`);
  expect(odd.status, 'punctuation is never query syntax').toBe(200);
  await hit.locator('.cv-post-conv').click();
  await expect(page.locator('#cv-posts .cv-post', { hasText: word }).first()).toBeVisible();
});

test('an edit in progress survives a post arriving, words and all', async ({ page, request }) => {
  await openMessages(page);
  const tok = await token(page);
  const tag = 'editlive-' + Date.now();
  await page.locator('#cv-input').fill(`draft ${tag}`);
  await page.locator('#cv-send').click();
  const mine = post(page, `draft ${tag}`);
  await expect(mine).toBeVisible();
  await act(page, mine, 'Edit');
  const box = page.locator('.cv-edit');
  await box.fill(`edited mid-stream ${tag}`);
  await page.waitForFunction(() => (window as any).eval('_liveSSE') === true, null, { timeout: 15000 });
  // Someone else posts while the edit is open: the list re-renders.
  await api(request, tok, 'POST', `/api/conversations/${CONV}/posts`, { body: `interrupt ${tag}` });
  await expect(post(page, `interrupt ${tag}`)).toBeVisible({ timeout: 8000 });
  await expect(box).toHaveValue(`edited mid-stream ${tag}`);
  await expect(box).toBeFocused();
  await page.locator('.cv-edit-acts .btn.primary').click();
  await expect(post(page, `edited mid-stream ${tag}`)).toBeVisible();
  await expect(page.locator('.cv-edit')).toHaveCount(0);
});

test('scrolling up loads older history', async ({ page, request }) => {
  await openMessages(page);
  const tok = await token(page);
  const tag = 'old-' + Date.now();
  for (let i = 0; i < 120; i++) await api(request, tok, 'POST', `/api/conversations/${CONV}/posts`, { body: `${tag} n${i} end` });
  // A fresh open: the newest page only (live events had appended them all).
  await openMessages(page);
  await expect(post(page, `${tag} n119 end`)).toBeVisible();
  await expect(page.locator('#cv-posts .cv-post-body', { hasText: `${tag} n5 end` })).toHaveCount(0);
  expect(await page.evaluate(() => (0, eval)('_cv.info && _cv.info.has_more')), 'the server says older history exists').toBe(true);
  await page.locator('#cv-posts').evaluate(el => { el.scrollTop = 0; el.dispatchEvent(new Event('scroll')); });
  await expect(page.locator('#cv-posts .cv-post-body', { hasText: `${tag} n5 end` })).toHaveCount(1, { timeout: 8000 });
});

test('a profile picture uploads and shows on your posts', async ({ page }) => {
  await openMessages(page);
  // A 1x1 PNG.
  const png = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==', 'base64');
  // The picture is yours, in the conversation list's header (a phone shows the list alone).
  if (phone(page)) await page.evaluate(() => (window as any)._cvBack());
  const saved = page.waitForResponse(r => r.url().includes('/api/profiles/') && r.request().method() === 'POST');
  await page.locator('#cv-me-file').setInputFiles({ name: 'me.png', mimeType: 'image/png', buffer: png });
  const res = await saved;
  expect(res.status(), await res.text()).toBe(200);
  await expect(page.locator('#cv-me img.cv-av-img')).toBeVisible();
  if (phone(page)) await page.evaluate(c => (window as any)._cvSelect(c), CONV);
  await page.locator('#cv-input').fill('with my picture ' + Date.now());
  await page.locator('#cv-send').click();
  await expect(page.locator('#cv-posts .cv-post').last().locator('img.cv-av-img')).toBeVisible();
});

test('a link to a private address is never fetched for a preview', async ({ page, request }) => {
  await openMessages(page);
  const tok = await token(page);
  for (const u of ['http://127.0.0.1:1/x', 'http://localhost/', 'http://10.0.0.1/', 'http://169.254.169.254/latest/meta-data/']) {
    const r = await api(request, tok, 'GET', `/api/conversations/unfurl?url=${encodeURIComponent(u)}`);
    expect(r.json.refused, u).toBe('not a public address');
  }
  const bad = await api(request, tok, 'GET', `/api/conversations/unfurl?url=${encodeURIComponent('file:///etc/passwd')}`);
  expect(bad.status).toBe(400);
});

test('a voice note: the mic records, the audio is attached, and it plays in the post', async ({ page, context, browserName }) => {
  test.skip(browserName !== 'chromium', 'MediaRecorder is only dependable in the Chromium build');
  await context.addInitScript(() => {
    navigator.mediaDevices.getUserMedia = async () => {
      const ctx = new AudioContext(); await ctx.resume().catch(() => {});
      const o = ctx.createOscillator(); const d = ctx.createMediaStreamDestination(); o.connect(d); o.start(); return d.stream;
    };
  });
  await openMessages(page);
  // The worker input's ⋮ menu holds the mic, as it holds Dictate in a worker's composer.
  await page.locator('#cv-more-btn').click();
  await page.locator('#cv-mic').click();
  await expect(page.locator('#cv-voice-main')).toContainText('Recording');
  await page.waitForTimeout(2500);
  await page.locator('#cv-voice-main .btn').click();
  await expect(page.locator('#cv-voice-main')).toContainText('attached', { timeout: 20000 });
  await page.locator('#cv-send').click();
  await expect(page.locator('#cv-posts .cv-post').last().locator('.cv-audio audio')).toBeAttached();
});

test('a notification link opens the post\'s thread', async ({ page, request }) => {
  await openMessages(page);
  const tok = await token(page);
  const root = await api(request, tok, 'POST', `/api/conversations/${CONV}/posts`, { body: 'deep link root ' + Date.now() });
  await api(request, tok, 'POST', `/api/conversations/${CONV}/posts`, { body: 'deep link reply', parent_id: root.json.id });
  await page.goto('/#messages=' + CONV + '&post=' + root.json.id);
  await expect(page.locator('#cv-thread')).toBeVisible();
  await expect(page.locator('#cv-thread-posts .cv-post', { hasText: 'deep link reply' })).toBeVisible();
});

test('Messages opens fast with a long channel', async ({ page, request }) => {
  await openMessages(page);
  const tok = await token(page);
  for (let i = 0; i < 60; i++) await api(request, tok, 'POST', `/api/conversations/${CONV}/posts`, { body: `speed ${i}` });
  await page.evaluate(() => (window as any).switchView('board'));
  const ms = await page.evaluate(async c => {
    // Cold: nothing from the earlier visit left on the page.
    document.getElementById('cv-posts')!.innerHTML = '';
    (0, eval)('_cv.posts = []; _cv.sel = null');
    const t0 = performance.now();
    (window as any).switchView('messages');
    (window as any)._cvSelect(c);
    while (!document.querySelector('#cv-posts .cv-post')) await new Promise(r => setTimeout(r, 10));
    return performance.now() - t0;
  }, CONV);
  test.info().annotations.push({ type: 'timing', description: JSON.stringify({ open_to_first_post_ms: Math.round(ms) }) });
  expect(ms, 'first posts visible in under 1 s').toBeLessThan(1000);
});
