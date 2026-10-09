import { lifecyclePrefix, lifecycleProvider } from './provider';
import { test, expect, Page, APIRequestContext, TestInfo } from '@playwright/test';
import { expectDelivered, boot, auth, checkpoint, getSessionsResilient } from './evidence';

// Reuse the completed pair; this phase must not create a third worker.
export async function runSonnetCrossgroup({ page, request }: { page: Page, request: APIRequestContext }, info: TestInfo) {
  test.setTimeout(900_000);
  expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
  const run = process.env.AMUX_LIFECYCLE_PAIR_RUN!;
  const cwd = process.env.AMUX_LIFECYCLE_LAB_WORKSPACE!;
  expect(run.startsWith(lifecyclePrefix)).toBe(true);
  expect(cwd).toBeTruthy();
  const author = `${run}-author`, reviewer = `${run}-reviewer`;
  const observeOnly = process.env.AMUX_LIFECYCLE_CROSSGROUP_OBSERVE === '1';
  const health = await (await request.get('/health')).json();
  await boot(page);
  const headers = await auth(page);
  const action = async (name: string, verb: string) => {
    await page.goto('/');
    await page.locator(`.card[data-session="${name}"]`).locator('visible=true').first().locator('.card-menu-btn').click();
    await page.locator(`.card-menu.open [data-worker-action="${verb}"]`).click();
  };
  const reviewFile = await request.get(`/api/fs/read?path=${encodeURIComponent(`${cwd}/${run}-review.json`)}`, { headers });
  expect(reviewFile.ok()).toBe(true);
  const prior = JSON.parse((await reviewFile.json()).content);
  const priorCards: Record<string, any> = {};
  for (const [name, id] of [[author, prior.task_id], [reviewer, prior.review_task_id]]) {
    const response = await request.get(`/api/board/${id}`, { headers });
    expect(response.ok()).toBe(true);
    priorCards[name] = await response.json();
    expect(priorCards[name].session).toBe(name);
    expect(priorCards[name].status).toMatch(/^(done|verified)$/);
  }
  if (!observeOnly) {
    await action(reviewer, 'groups');
    await page.locator('#edit-input').fill(`${run}-quality`);
    await page.locator('#edit-overlay').getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.locator('#edit-overlay')).not.toHaveClass(/active/);
  }
  await expect.poll(async () => {
    const response = await getSessionsResilient(request, headers);
    if (!response.ok()) return false;
    const rows = await response.json();
    const a = rows.find((r: any) => r.name === author), b = rows.find((r: any) => r.name === reviewer);
    return a?.tags.includes(`${run}-team`) && b?.tags.includes(`${run}-quality`) &&
      !b.tags.includes(`${run}-team`) && [a, b].every(r => (r.provider || 'claude') === lifecycleProvider && (lifecycleProvider !== 'claude' || /sonnet/i.test(`${r.model} ${r.flags}`)));
  }).toBe(true);
  await checkpoint(page, info, 'crossgroup-membership');
  if (!observeOnly) {
    for (const [name, peer, marker] of [[reviewer, author, 'CROSS_REFUSED'], [author, reviewer, 'CROSS_REFUSED']]) {
      await action(name, 'peek-terminal');
      const file = `${run}-cross-${name === author ? 'author' : 'reviewer'}.json`;
      await page.locator('#peek-cmd-input').fill(`Owner-authorized boundary test for ${run}. Work only in ${cwd} and on your own board. You and ${peer} now have disjoint Amux groups, so automated peer delivery MUST be refused; do not request a grant, alter membership, use native SendMessage, retry or reroute. Discover ${peer}'s actual group and read its completed review-cycle card ${priorCards[peer].id}. Create one chore card, attempt exactly one Bash amux send to ${peer} with text CROSS_REFUSED ${run}, and record the actual nonzero exit and refusal output. Write ${file} as JSON {peer:"${peer}",peer_task_id:"${priorCards[peer].id}",peer_title:"actual fetched title",peer_status:"actual fetched status",peer_group:"actual group",own_task_id:"your chore ID",delivery_refused:true,refusal_code:"worker_group_boundary",refusal_exit_code:actual_nonzero_exit,refusal_output:"actual command output"}. Finish your chore only with the refusal and artifact as evidence; no cross-group acknowledgement is expected. Preserve the earlier completed cards and peer message history. Do not contact production, external services or unrelated workers.`);
      const sent = page.waitForResponse(r => r.url().endsWith(`/${name}/send`) && r.request().method() === 'POST', { timeout: 90_000 });
      await page.locator('#peek-overlay .send-split-main').click();
      const response = await sent;
      expect(response.ok()).toBe(true);
      await expectDelivered(page, response);
      await checkpoint(page, info, `crossgroup-prompt-${marker}`);
    }
  }
  let proofs: any[] = [], cards: any[] = [], messages: any[] = [];
  try {
    await expect.poll(async () => {
      proofs = []; cards = []; messages = [];
      for (const [name, peer, role] of [[author, reviewer, 'author'], [reviewer, author, 'reviewer']]) {
        const response = await request.get(`/api/fs/read?path=${encodeURIComponent(`${cwd}/${run}-cross-${role}.json`)}`, { headers });
        if (!response.ok()) return false;
        let proof: any;
        try { proof = JSON.parse((await response.json()).content); } catch { return false; }
        if (proof.peer !== peer || proof.peer_task_id !== priorCards[peer].id ||
          proof.peer_title !== priorCards[peer].title || proof.peer_status !== priorCards[peer].status ||
          proof.peer_group !== `${run}-${role === 'author' ? 'quality' : 'team'}`) return false;
        if (proof.delivery_refused !== true || proof.refusal_code !== 'worker_group_boundary'
          || !Number.isInteger(proof.refusal_exit_code) || proof.refusal_exit_code === 0
          || !/group/i.test(String(proof.refusal_output || ''))) return false;
        proofs.push(proof);
        const board = await request.get(`/api/board?session=${name}&done_limit=0`, { headers });
        const history = await request.get(`/api/history?session=${name}&limit=200`, { headers });
        if (!board.ok() || !history.ok()) return false;
        const owned = await board.json();
        if (!owned.some((c: any) => c.id === proof.own_task_id && c.session === name && ['done', 'verified'].includes(c.status) && String(c.evidence || '').includes(`${run}-cross-${role}.json`))) return false;
        cards.push(...owned); messages.push(...await history.json());
      }
      // Real providers must record refusals; no automated crossing is admitted.
      return !messages.some(m => [author, reviewer].includes(m.origin) && [author, reviewer].includes(m.session)
        && m.origin !== m.session && String(m.text).startsWith('CROSS_REFUSED'));
    }, { timeout: 720_000, intervals: [5000, 15000, 30000], message: 'disjoint-group providers must record actual refusal and complete their owned proof without crossing messages' }).toBe(true);
    for (const size of [{ width: 1280, height: 800 }, { width: 375, height: 667 }]) {
      await page.setViewportSize(size);
      for (const [name, marker] of [[author, 'REVIEW_APPROVED'], [reviewer, 'PAIR_DONE']]) {
        await action(name, 'peek-terminal');
        await page.getByRole('button', { name: 'Filter messages', exact: true }).click();
        await page.locator('[name="peek-filter-source"][value="session"]').check();
        await page.getByRole('dialog', { name: 'Filter worker messages' }).getByRole('button', { name: 'Done', exact: true }).click();
        await page.getByRole('button', { name: 'Find in terminal', exact: true }).click();
        await page.locator('#peek-search').fill(marker);
        await expect(page.locator('#peek-body .peek-highlight').first()).toBeVisible({ timeout: 30_000 });
        await page.waitForFunction(() => getComputedStyle(document.querySelector('#peek-overlay')!).opacity === '1');
        await expect(page.locator('#peek-body .peek-highlight.current').first(), 'selected terminal result must be inside the visible output').toBeInViewport();
        expect(await page.locator('#peek-body .peek-highlight.current').first().evaluate(el => el.closest('.peek-prompt')?.getAttribute('data-msg-kind'))).toBe('session');
        await checkpoint(page, info, `crossgroup-terminal-${name}-${size.width}`);
        await page.locator('#peek-search').press('Escape');
        await page.locator('#peek-tab-messages').click();
        await page.locator('#peek-messages-filter').getByRole('button', { name: /^Session \d+$/ }).click();
        await page.locator('#peek-messages-search').fill(marker);
        await expect(page.locator('#peek-messages-list').getByText(marker, { exact: false }).first()).toBeVisible();
        await checkpoint(page, info, `crossgroup-messages-${name}-${size.width}`);
      }
    }
    expect((await (await request.get('/health')).json()).build).toBe(health.build);
  } finally {
    await info.attach('crossgroup-proof', { body: JSON.stringify({ run, observeOnly, health, proofs, cards, messages }, null, 2), contentType: 'application/json' });
  }
}
