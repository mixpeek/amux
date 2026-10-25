import {test, expect} from './fixtures';

// Ethan, 2026-10-21, two screenshots of amux-homepage: "put the demos link in
// the header and footer too" sat in Steering as "1 queued" while the peek's
// footer said "Delivered while the worker was busy. Claude Code took it into
// the running turn". A queued send writes its history row when it is queued,
// so the row alone does not mean the worker has it. A message the worker's
// queue still holds is not listed as delivered; one taken mid-turn still is.
test('a message still in the queue is not shown as delivered while busy', async ({page}) => {
  const now = Date.now();
  const queued = 'put the demos link in the header and footer too';
  const taken = 'also check the mobile nav spacing';
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [{
    name: 'busyw', running: true, status: 'active', dir: '/tmp',
    session_created: Math.floor(now / 1000) - 3600,
    steering: [{id: 7, transport_id: 'q-7', text: queued, queued_at: Math.floor(now / 1000) - 300}],
  }]}));
  await page.route(/\/api\/history(?:\?.*)?$/, r => r.fulfill({json: [
    {id: 3, session: 'busyw', type: 'user', kind: 'human', text: queued, ts: now - 300_000, delivery: 'queued'},
    {id: 2, session: 'busyw', type: 'user', kind: 'human', text: taken, ts: now - 400_000, delivery: 'direct'},
    {id: 1, session: 'busyw', type: 'user', kind: 'human', text: 'update the homepage CTAs', ts: now - 600_000, delivery: 'direct'},
  ]}));
  await page.route(/\/api\/sessions\/busyw\/peek\?/, r => r.fulfill({json: {
    name: 'busyw', history: '',
    live: '❯ update the homepage CTAs\n\n⏺ Working on the hero buttons\n\n✻ Tempering… (6m 8s)\n',
  }}));
  await page.route('**/api/sessions/busyw/subagents', r => r.fulfill({json: {session: 'busyw', subagents: []}}));

  await page.goto('/');
  await page.evaluate(() => (window as any).openPeek('busyw'));
  const banner = page.locator('.peek-unechoed');
  // The mid-turn message proves the banner renders at all in this setup.
  await expect(banner).toContainText(taken, {timeout: 15000});
  await expect(banner).not.toContainText(queued);
});

// Ethan, 2026-10-22 11:47 (mixpeek-ops-server): the terminal printed the prompt
// "[11:46 AM] can you tell @mixpeek-cicd" while the recorded text wrapped the
// mention in backticks. The match on raw characters missed it, and a message
// that was printed was listed as taken without a prompt.
test('a printed prompt whose record differs only in markup is not listed as unprinted', async ({page}) => {
  const now = Date.now();
  const recorded = 'can you tell `@mixpeek-cicd`\n\n[amux: @-mentions above are amux workers, NOT files. Reach them via HTTP API, never tmux directly]';
  const unprinted = 'and ping the frontend lane about the nav';
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [{
    name: 'opsw', running: true, status: 'active', dir: '/tmp', session_created: Math.floor(now / 1000) - 3600, steering: [],
  }]}));
  await page.route(/\/api\/history(?:\?.*)?$/, r => r.fulfill({json: [
    {id: 3, session: 'opsw', type: 'user', kind: 'human', text: unprinted, ts: now - 30_000, delivery: 'direct'},
    {id: 2, session: 'opsw', type: 'user', kind: 'human', text: recorded, ts: now - 60_000, delivery: 'direct'},
    {id: 1, session: 'opsw', type: 'user', kind: 'human', text: 'check the ops server health', ts: now - 600_000, delivery: 'direct'},
  ]}));
  await page.route(/\/api\/sessions\/opsw\/peek\?/, r => r.fulfill({json: {
    name: 'opsw', history: '',
    live: '❯ check the ops server health\n\n⏺ Done.\n\n❯ [11:46 AM] can you tell @mixpeek-cicd\n\n  [amux: @-mentions above are amux workers, NOT files. Reach them via HTTP API, never tmux directly]\n\n· Moseying… (29s)\n',
  }}));
  await page.route('**/api/sessions/opsw/subagents', r => r.fulfill({json: {session: 'opsw', subagents: []}}));
  await page.goto('/');
  await page.evaluate(() => (window as any).openPeek('opsw'));
  const banner = page.locator('.peek-unechoed');
  // Control: the message the terminal really does not show is still listed.
  await expect(banner).toContainText(unprinted, {timeout: 15000});
  await expect(banner).not.toContainText('can you tell');
});

// Ethan, 2026-10-22 16:41, "this weirdness is still happening": the block sat
// under the input box and the status line, so a delivered message read as an
// unsent draft, and it kept listing a message from an earlier turn under a
// newer printed prompt. It is drawn as a ❯ prompt above the input box, and
// only for the turn the newest printed prompt started.
test('a message taken mid-turn is drawn above the input box, for the current turn only', async ({page}) => {
  const now = Date.now();
  const earlier = 'rename the export button please';
  const taken = 'i want to drag this up and down';
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [{
    name: 'midw', running: true, status: 'active', dir: '/tmp', session_created: Math.floor(now / 1000) - 3600, steering: [],
  }]}));
  await page.route(/\/api\/history(?:\?.*)?$/, r => r.fulfill({json: [
    {id: 4, session: 'midw', type: 'user', kind: 'human', text: taken, ts: now - 60_000, delivery: 'direct'},
    {id: 3, session: 'midw', type: 'user', kind: 'human', text: 'now fix the header grip', ts: now - 300_000, delivery: 'direct'},
    {id: 2, session: 'midw', type: 'user', kind: 'human', text: earlier, ts: now - 500_000, delivery: 'direct'},
    {id: 1, session: 'midw', type: 'user', kind: 'human', text: 'start the split panes', ts: now - 900_000, delivery: 'direct'},
  ]}));
  const rule = '─'.repeat(60);
  await page.route(/\/api\/sessions\/midw\/peek\?/, r => r.fulfill({json: {
    name: 'midw', history: '',
    live: `❯ start the split panes\n\n⏺ Done.\n\n❯ now fix the header grip\n\n⏺ Working on it\n\n✻ Booping… (1m 39s)\n\n${rule}\n❯ \n${rule}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n`,
  }}));
  await page.route('**/api/sessions/midw/subagents', r => r.fulfill({json: {session: 'midw', subagents: []}}));
  await page.goto('/');
  await page.evaluate(() => (window as any).openPeek('midw'));
  const block = page.locator('#peek-body .peek-unechoed');
  await expect(block).toContainText(taken, {timeout: 15000});
  await expect(block).toContainText('taken mid-turn');
  await expect(block).not.toContainText(earlier);
  // Above the input box: the block comes before the worker footer in the page.
  const order = await page.evaluate(() => {
    const b = document.querySelector('#peek-body .peek-unechoed'), f = document.querySelector('#peek-body .peek-worker-footer');
    return b && f ? !!(b.compareDocumentPosition(f) & Node.DOCUMENT_POSITION_FOLLOWING) : null;
  });
  expect(order).toBe(true);
});
