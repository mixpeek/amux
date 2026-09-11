import { lifecyclePrefix, expectLifecycleWorker, selectLifecycleProvider, expectLifecycleTerminal } from './provider';
import { test, expect, Page } from '@playwright/test';
import { runSonnetUpload } from './sonnet-upload';
import { runSonnetCrossgroup } from './sonnet-crossgroup';
import { runSonnetQueue } from './sonnet-queue';
import { boot, auth, checkpoint, getSessionsResilient } from './evidence';

test.describe.configure({ mode: 'serial' });

async function workerAction(page: Page, name: string, action: string) {
  await page.goto('/');
  await page.locator('#tab-sessions').click();
  const card = page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
  await expect(card).toBeVisible({ timeout: 30_000 });
  await card.locator('.card-menu-btn').click();
  await page.locator(`.card-menu.open [data-worker-action="${action}"]`).click();
}

async function send(page: Page, name: string, text: string) {
  await workerAction(page, name, 'peek-terminal');
  await page.locator('#peek-cmd-input').fill(text);
  const delivered = page.waitForResponse(r => r.url().endsWith(`/${name}/send`) && r.request().method() === 'POST', { timeout: 60_000 });
  await page.locator('#peek-overlay .send-split-main').click();
  const response = await delivered;
  expect(response.ok(), await response.text()).toBeTruthy();
  expect((await response.json()).submitted).toBe(true);
  await expect(page.locator('#peek-cmd-input')).toHaveValue('', { timeout: 60_000 });
}

// Two actual selected-provider processes, one group, real review/revision and actual terminal
// output. No route stubs and no operator advancing cards or writing peer evidence.
test('LC-SONNET-PAIR: two same-group workers coordinate and their messages are navigable on desktop and mobile', async ({ page, request }, info) => {
  test.setTimeout(1_800_000);
  expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
  const cwd = process.env.AMUX_LIFECYCLE_LAB_WORKSPACE!;
  expect(cwd).toBeTruthy();
  const observeOnly = process.env.AMUX_LIFECYCLE_PAIR_OBSERVE === '1';
  if (observeOnly) expect(process.env.AMUX_LIFECYCLE_PAIR_RUN).toBeTruthy();
  const run = process.env.AMUX_LIFECYCLE_PAIR_RUN || `${lifecyclePrefix}${Date.now()}`;
  const author = `${run}-author`, reviewer = `${run}-reviewer`, group = `${run}-team`;
  const names = [author, reviewer];
  const source = `${run}.mjs`, review = `${run}-review.json`;
  const health = await (await request.get('/health')).json();
  const timeline: any[] = [];
  await info.attach('run-identities', { body: JSON.stringify({ run, names, group, cwd, source, review, observeOnly }), contentType: 'application/json' });
  await boot(page);
  const headers = await auth(page);
  for (const name of names) {
    const existing = await getSessionsResilient(request, headers);
    expect(existing.ok()).toBeTruthy();
    if ((await existing.json()).some((row: any) => row.name === name)) continue;
    expect(observeOnly, 'observation never creates replacement workers').toBe(false);
    await page.goto('/');
    await page.locator('#tab-sessions').click();
    await page.locator('[onclick*="toggleAddMenu"]').click();
    await page.locator('.card-menu-item', { hasText: 'New worker' }).click();
    await page.locator('#create-name').fill(name);
    await selectLifecycleProvider(page);
    await page.locator('#create-dir').fill(cwd);
    await checkpoint(page, info, `create-${name}`);
    await page.locator('#create-overlay').getByRole('button', { name: 'Create', exact: true }).click();
    await expect(page.locator('#create-overlay')).not.toHaveClass(/active/, { timeout: 60_000 });
    await workerAction(page, name, 'groups');
    await page.locator('#edit-input').fill(group);
    await page.locator('#edit-overlay').getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.locator('#edit-overlay')).not.toHaveClass(/active/);
  }
  await expect.poll(async () => {
    const response = await getSessionsResilient(request, headers);
    if (!response.ok()) return false;
    const rows = await response.json();
    return names.every(name => rows.find((row: any) => row.name === name)?.tags.includes(group));
  }, { timeout: 15_000, message: 'saved membership must reach the roster' }).toBe(true);
  const roster = await getSessionsResilient(request, headers);
  expect(roster.ok()).toBeTruthy();
  for (const name of names) {
    const row = (await roster.json()).find((r: any) => r.name === name);
    expect(row.tags).toContain(group);
    expectLifecycleWorker(row);
    await workerAction(page, name, 'peek-terminal');
    await expectLifecycleTerminal(page);
    await checkpoint(page, info, `running-provider-${name}`);
  }
  const common = `Authorized test ${run}. Work only in ${cwd}, and communicate only with ${names.join(' and ')}.
Use the amux CLI and board workflow. Send all peer messages with amux send through Bash;
Claude's native SendMessage tool bypasses Amux's message history and is not the transport under test.
Create your own chore task; discover the peer's actual
board card and preserve ownership. Record real test commands/results and file paths as evidence.
Do not fake review, bypass gates, close the peer's cards, contact production or external recipients.
Finish your own cards honestly and then idle. If Amux auto-captures this prompt or FYI messages as
additional cards, discard your own duplicate captures with a reason pointing to your real work card;
do not leave them doing/backlog or falsely acknowledge code-merge gates for chore work.
Every peer message must name the relevant real task ID.`;
  if (!observeOnly) {
  await send(page, reviewer, `${common}
You are reviewer. Create your review task and send ${author} PEER_READY with your task ID. Wait for
a review request naming ${source}. Test mean([]); the initial implementation intentionally returns
NaN. Send REVIEW_CHANGES with the failing result to ${author}. Wait for the actual revision;
independently test mean([2,4])=3, mean([-2,2])=0 and mean([])=0. After passing, write ${review} as
JSON {reviewer:"${reviewer}",author:"${author}",task_id:"actual author task ID",review_task_id:"your actual task ID",
decision:"approved",change_requested:true,test_command:"actual command",test_result:"actual result"}.
Send REVIEW_APPROVED to ${author}, finish your review task, and send REVIEW_DONE with its ID.`);
  await page.setViewportSize({ width: 375, height: 667 });
  await send(page, author, `${common}
You are author. Create your task for ${source}. Export mean(xs) initially as
xs.reduce((a,b)=>a+b,0)/xs.length (the deliberate review defect). Discover ${reviewer} and its board
task, assign ${reviewer} as your task's reviewer using the normal review flow, and send a review
request with your task ID and ${source}. Wait for REVIEW_CHANGES before fixing the empty input to
return 0. Run tests and request re-review. After REVIEW_APPROVED and REVIEW_DONE, render ${run}-result.html
with the visible text '${run} complete' and the actual test result. Use readable typography and
wrap long titles and preformatted lines so the page fits 375px and 1280px without horizontal overflow. Finish your own task with evidence.
Send PAIR_DONE with your task ID to ${reviewer}. Do not write the review JSON yourself.`);
  await checkpoint(page, info, 'mobile-prompt-sent');
  }
  let cards: any[] = [], messages: any[] = [];
  try {
    await expect.poll(async () => {
      cards = []; messages = [];
      try {
      for (const name of names) {
        const board = await request.get(`/api/board?session=${name}&done_limit=0`, { headers });
        if (board.status() >= 500) return false;
        expect(board.ok()).toBeTruthy(); cards.push(...await board.json());
        const history = await request.get(`/api/history?session=${name}&limit=200`, { headers });
        if (history.status() >= 500) return false;
        expect(history.ok()).toBeTruthy(); messages.push(...await history.json());
      }
      } catch (error) {
        if (!/ECONNREFUSED|ECONNRESET|socket hang up/.test(String(error))) throw error;
        timeline.push({ at: new Date().toISOString(), readError: String(error) });
        return false;
      }
      timeline.push({ at: new Date().toISOString(), cards, messages });
      const sent = (from: string, to: string, marker: string) => messages.some(m => m.origin === from && m.session === to && String(m.text).startsWith(marker));
      return names.every(name => cards.some(c => c.session === name)) && cards.every(c => ['done', 'verified', 'discarded', 'cancelled'].includes(c.status)) &&
        sent(reviewer, author, 'REVIEW_CHANGES') && sent(reviewer, author, 'REVIEW_APPROVED') && sent(author, reviewer, 'PAIR_DONE');
    }, { timeout: 1_500_000, intervals: [5000, 15000, 30000], message: 'both Sonnet workers must finish their real review cycle' }).toBe(true);
    cards = [...new Map(cards.map(c => [c.id, c])).values()];
    const reviewed = await request.get(`/api/fs/read?path=${encodeURIComponent(`${cwd}/${review}`)}`, { headers });
    expect(reviewed.ok()).toBeTruthy();
    const result = JSON.parse((await reviewed.json()).content);
    expect(result).toMatchObject({ reviewer, author, decision: 'approved' });
    // Durable peer-message order proves the prior change request; a boolean
    // in the final report can mean current outstanding changes instead.
    for (const id of [result.task_id, result.review_task_id]) expect(cards.find(c => c.id === id)?.status).toMatch(/^(done|verified)$/);
    expect(cards.some(c => c.id === result.task_id && c.session === author)).toBe(true);
    expect(cards.some(c => c.id === result.review_task_id && c.session === reviewer)).toBe(true);
    const sourceResponse = await request.get(`/api/fs/read?path=${encodeURIComponent(`${cwd}/${source}`)}`, { headers });
    expect(sourceResponse.ok()).toBeTruthy();
    const htmlResponse = await request.get(`/api/fs/read?path=${encodeURIComponent(`${cwd}/${run}-result.html`)}`, { headers });
    expect(htmlResponse.ok()).toBeTruthy();
    const sandbox = await page.context().browser()!.newContext();
    try {
      const output = await sandbox.newPage();
      await output.route('**/*', route => route.abort());
      const values = await output.evaluate(async code => {
        const url = URL.createObjectURL(new Blob([code], { type: 'text/javascript' }));
        try { const { mean } = await import(url); return [mean([2, 4]), mean([-2, 2]), mean([])]; }
        finally { URL.revokeObjectURL(url); }
      }, (await sourceResponse.json()).content);
      expect(values).toEqual([3, 0, 0]);
      const html = (await htmlResponse.json()).content;
      for (const size of [{ width: 1280, height: 800 }, { width: 375, height: 667 }]) {
        await output.setViewportSize(size);
        await output.setContent(html);
        await expect(output.getByText(`${run} complete`, { exact: false })).toBeVisible();
        await checkpoint(output, info, `actual-result-${size.width}`);
      }
      await info.attach('independent-mean-results', { body: JSON.stringify(values), contentType: 'application/json' });
    } finally { await sandbox.close(); }
    const changes = messages.find(m => m.origin === reviewer && m.session === author && m.text.startsWith('REVIEW_CHANGES'));
    const approved = messages.find(m => m.origin === reviewer && m.session === author && m.text.startsWith('REVIEW_APPROVED'));
    expect(Number(approved.ts)).toBeGreaterThan(Number(changes.ts));
    for (const size of [{ width: 1280, height: 800 }, { width: 375, height: 667 }]) {
      await page.setViewportSize(size);
      for (const [name, marker] of [[author, 'REVIEW_APPROVED'], [reviewer, 'PAIR_DONE']]) {
        await workerAction(page, name, 'peek-terminal');
        await page.getByRole('button', { name: 'Filter messages', exact: true }).click();
        await page.locator('[name="peek-filter-source"][value="session"]').check();
        await page.getByRole('dialog', { name: 'Filter worker messages' }).getByRole('button', { name: 'Done', exact: true }).click();
        await page.getByRole('button', { name: 'Find in terminal', exact: true }).click();
        await page.locator('#peek-search').fill(marker);
        await expect(page.locator('#peek-body .peek-highlight').first(), 'real delivered message must be findable in terminal').toBeVisible({ timeout: 30_000 });
        await page.getByRole('button', { name: 'Next message', exact: true }).click();
        await page.getByRole('button', { name: 'Previous message', exact: true }).click();
        await page.waitForFunction(() => getComputedStyle(document.querySelector('#peek-overlay')!).opacity === '1');
        await expect(page.locator('#peek-body .peek-highlight.current').first(), 'selected terminal result must be inside the visible output').toBeInViewport();
        expect(await page.locator('#peek-body .peek-highlight.current').first().evaluate(el => el.closest('.peek-prompt')?.getAttribute('data-msg-kind'))).toBe('session');
        await checkpoint(page, info, `terminal-${name}-${size.width}`);
        await page.locator('#peek-search').press('Escape');
        await page.locator('#peek-tab-messages').click();
        await page.locator('#peek-messages-filter').getByRole('button', { name: /^Session \d+$/ }).click();
        await page.locator('#peek-messages-search').fill(marker);
        await expect(page.locator('#peek-messages-list')).toContainText(marker);
        await expect(page.locator('#peek-messages-list').getByText(marker, { exact: false }).first()).toBeVisible();
        await checkpoint(page, info, `messages-${name}-${size.width}`);
      }
      for (const card of cards.filter(c => [result.task_id, result.review_task_id].includes(c.id))) {
        const detail = await (await request.get(`/api/board/${card.id}`, { headers })).json();
        expect(String(detail.evidence || '').length).toBeGreaterThan(20);
        await page.goto(`/#issue=${card.id}`);
        await expect(page.locator('#bd-key')).toHaveText(card.id);
        await checkpoint(page, info, `complete-${card.id}-${size.width}`);
      }
    }
    expect((await (await request.get('/health')).json()).build).toBe(health.build);
  } finally {
    await info.attach('pair-timeline', { body: JSON.stringify({ run, names, group, health, timeline }, null, 2), contentType: 'application/json' });
  }
});

test('LC-SONNET-UPLOAD: same selected-provider worker reads a real UI upload and finishes its receipt task', runSonnetUpload);

test('LC-SONNET-CROSSGROUP: the same pair discovers peer tasks and exchanges messages across groups', runSonnetCrossgroup);

test('LC-SONNET-QUEUE: both workers pick up backlog and todo with dependencies and reach evidenced terminal states', runSonnetQueue);
