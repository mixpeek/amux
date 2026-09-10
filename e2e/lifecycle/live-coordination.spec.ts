import { test, expect, Page } from '@playwright/test';
import { boot, auth, checkpoint } from './evidence';

async function send(page: Page, name: string, text: string) {
  await page.goto('/');
  await page.locator('#tab-sessions').click();
  const card = page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
  await expect(card).toBeVisible({ timeout: 30_000 });
  await card.locator('.card-menu-btn').click();
  await page.locator('.card-menu.open .card-menu-item', { hasText: 'Peek terminal' }).click();
  await page.locator('#peek-cmd-input').fill(text);
  const delivered = page.waitForResponse(r => r.url().endsWith(`/${name}/send`) && r.request().method() === 'POST', { timeout: 90_000 });
  await page.locator('#peek-overlay .send-split-main').getByText('Send', { exact: true }).click();
  const response = await delivered;
  expect(response.ok()).toBe(true);
  expect((await response.json()).submitted).toBe(true);
}

for (const crossGroup of [false, true]) {
  test(`LC-COORD-LIVE: ${crossGroup ? 'cross-group' : 'same-group'} awareness, rejected review, revision, approval and dependent handoff`, async ({ page, request }, info) => {
    test.setTimeout(1_800_000);
    expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
    const cwd = process.env.AMUX_LIFECYCLE_LAB_WORKSPACE;
    expect(cwd).toBeTruthy();
    await boot(page);
    const headers = await auth(page);
    const run = `coord-${crossGroup ? 'cross' : 'same'}-${Date.now()}`;
    const author = `${run}-author`, reviewer = `${run}-reviewer`, consumer = `${run}-consumer`;
    const names = [author, reviewer, consumer];
    const healthResponse = await request.get('/health');
    expect(healthResponse.ok()).toBeTruthy();
    const health = await healthResponse.json();
    const source = `${run}.mjs`, reviewed = `${run}-review.json`, integrated = `${run}-integrated.json`;
    const provider = process.env.AMUX_LIFECYCLE_PROVIDER || 'claude';
    for (const name of names) {
      const group = crossGroup && name !== author ? `${run}-quality` : `${run}-build`;
      const made = await request.post('/api/sessions', { headers, data: { name, dir: cwd,
        tags: [group], provider, ...(provider === 'claude' ? { flags: '--model sonnet' } : {}) } });
      expect(made.status()).toBe(201);
    }
    const rosterResponse = await request.get('/api/sessions', { headers });
    expect(rosterResponse.ok()).toBeTruthy();
    const roster = await rosterResponse.json();
    for (const name of names) {
      const expectedGroup = crossGroup && name !== author ? `${run}-quality` : `${run}-build`;
      expect(roster.find((row: any) => row.name === name)?.tags).toContain(expectedGroup);
      if (provider === 'claude') expect(`${roster.find((row: any) => row.name === name)?.flags}`).toMatch(/sonnet/);
    }
    const common = `This is an authorized coordination acceptance run ${run}. Work only in ${cwd},
only with ${names.join(', ')}. Use Bash amux send for peer messages (not Claude native SendMessage), and your own board tasks.
Read peers' actual board tasks for context; preserve ownership, link dependencies, and record IDs,
commands/results and artifacts. Follow existing gates. Never forge another worker's review or output.
No external email or unrelated peers. Drive owned tasks to done/verified when honestly complete.`;
    await send(page, reviewer, `${common}
You are the independent reviewer. Wait for ${author}'s review request. Discover and read its actual
board task and ${source}. Independently test mean([]); the initial implementation intentionally has
a defect. Send ${author} a REVIEW_CHANGES message with the actual task ID and failing test result.
Create your own review task. After a real revision, rerun tests for mean([2,4])=3, mean([-2,2])=0,
and mean([])=0. Only then send REVIEW_APPROVED with the source task ID and write ${reviewed} as
JSON {reviewer:"${reviewer}", author:"${author}", task_id:"actual author task ID", decision:"approved",
change_requested:true, test_command:"actual command", test_result:"actual result"}. Finish your review task.`);
    await send(page, consumer, `${common}
You are the downstream consumer. Wait for a peer handoff from ${author} and actual approval from
${reviewer}. Discover their board tasks, create your own integration task with real dependencies.
Do not complete before review approval. Independently import ${source}, test mean([2,4])=3 and
mean([])=0, then write ${integrated} as JSON {consumer:"${consumer}", reviewer:"${reviewer}",
author_task_id:"actual ID", reviewer_task_id:"actual ID", result:3, empty_result:0}.
Complete your task with real evidence and send ${author} HANDOFF_DONE with all relevant IDs.`);
    await send(page, author, `${common}
You own implementation. Create a task for ${source}, export mean(xs) initially as
xs.reduce((a,b)=>a+b,0)/xs.length. This deliberate empty-input bug is the review challenge.
Set ${reviewer} as that task's reviewer using the normal board review flow.
Discover ${reviewer}, read its board context, request review by peer message with task ID and file.
Do not fix the bug before receiving REVIEW_CHANGES. Then make mean([]) return 0, run meaningful
tests, record revision evidence and request re-review. After REVIEW_APPROVED, hand off to ${consumer}
with the source/review task IDs and artifact. Wait for HANDOFF_DONE, then finish your own task.
Do not complete any peer's task or write their review/integration files.`);
    const timeline: any[] = [];
    let cards: any[] = [], histories: any[] = [];
    try {
      await expect.poll(async () => {
        cards = []; histories = [];
        for (const name of names) {
          const response = await request.get(`/api/board?session=${name}&done_limit=0`, { headers });
          expect(response.ok()).toBeTruthy();
          cards.push(...await response.json());
          const messages = await request.get(`/api/history?session=${name}&limit=200`, { headers });
          expect(messages.ok()).toBeTruthy();
          histories.push(...await messages.json());
        }
        timeline.push({ at: new Date().toISOString(), cards, histories });
        const peerMessage = (from: string, to: string, marker: string) => histories.some(row =>
          row.origin === from && row.session === to && String(row.text).includes(marker));
        return names.every(name => cards.some(card => card.session === name)) &&
          cards.every(card => ['done', 'verified'].includes(card.status)) &&
          peerMessage(author, reviewer, source) && peerMessage(author, consumer, source) &&
          peerMessage(reviewer, author, 'REVIEW_CHANGES') &&
          peerMessage(reviewer, author, 'REVIEW_APPROVED') &&
          peerMessage(consumer, author, 'HANDOFF_DONE');
      }, { timeout: 1_650_000, intervals: [5000, 15000, 30000], message: 'all actors must coordinate and finish their own work' }).toBe(true);
      const messageTime = (from: string, to: string, marker: string) => Math.min(...histories
        .filter(row => row.origin === from && row.session === to && String(row.text).includes(marker))
        .map(row => Number(row.ts)));
      const changesAt = messageTime(reviewer, author, 'REVIEW_CHANGES');
      const approvalAt = messageTime(reviewer, author, 'REVIEW_APPROVED');
      const handoffAt = messageTime(consumer, author, 'HANDOFF_DONE');
      expect(Number.isFinite(changesAt)).toBe(true);
      expect(approvalAt, 'approval must follow a real changes request').toBeGreaterThan(changesAt);
      expect(handoffAt, 'integration must follow approval').toBeGreaterThan(approvalAt);
      const sourceResponse = await request.get(`/api/fs/read?path=${encodeURIComponent(`${cwd}/${source}`)}`, { headers });
      expect(sourceResponse.ok()).toBeTruthy();
      const sourceBody = await sourceResponse.json();
      const sandbox = await page.context().browser()!.newContext();
      try {
        const probe = await sandbox.newPage();
        await probe.route('**/*', route => route.abort());
        const actual = await probe.evaluate(async code => {
          const url = URL.createObjectURL(new Blob([code], { type: 'text/javascript' }));
          try { const { mean } = await import(url); return [mean([2, 4]), mean([-2, 2]), mean([])]; }
          finally { URL.revokeObjectURL(url); }
        }, sourceBody.content);
        expect(actual, 'independent reviewer cannot approve a still-broken artifact').toEqual([3, 0, 0]);
      } finally { await sandbox.close(); }
      for (const [file, actor] of [[reviewed, reviewer], [integrated, consumer]]) {
        const response = await request.get(`/api/fs/read?path=${encodeURIComponent(`${cwd}/${file}`)}`, { headers });
        expect(response.ok()).toBeTruthy();
        const result = JSON.parse((await response.json()).content);
        if (actor === reviewer) {
          expect(result).toMatchObject({ reviewer, author, decision: 'approved', change_requested: true });
          expect(cards.some(card => card.id === result.task_id && card.session === author)).toBe(true);
          const task = await request.get(`/api/board/${encodeURIComponent(result.task_id)}`, { headers });
          expect(task.ok()).toBeTruthy();
          expect((await task.json()).reviewer).toBe(reviewer);
          expect(result.test_command).toBeTruthy(); expect(result.test_result).toBeTruthy();
        } else {
          expect(result).toMatchObject({ consumer, reviewer, result: 3, empty_result: 0 });
          expect(cards.some(card => card.id === result.author_task_id && card.session === author)).toBe(true);
          expect(cards.some(card => card.id === result.reviewer_task_id && card.session === reviewer)).toBe(true);
        }
        await info.attach(file, { body: JSON.stringify(result, null, 2), contentType: 'application/json' });
      }
      for (const card of cards) {
        const response = await request.get(`/api/board/${encodeURIComponent(card.id)}`, { headers });
        expect(response.ok()).toBeTruthy();
        expect(String((await response.json()).evidence || '').length).toBeGreaterThan(20);
        await page.goto(`/#issue=${encodeURIComponent(card.id)}`);
        await expect(page.locator('#bd-key')).toHaveText(card.id);
        await checkpoint(page, info, `coord-final-${card.id}`);
      }
      const after = await request.get('/health');
      expect((await after.json()).build).toEqual(health.build);
    } finally {
      await info.attach('coordination-timeline', { body: JSON.stringify({ run, names, crossGroup, cwd, health, timeline }, null, 2), contentType: 'application/json' });
    }
  });
}
