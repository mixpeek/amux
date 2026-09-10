import { test, expect, Page, APIRequestContext, TestInfo } from '@playwright/test';
import { boot, auth, checkpoint, getSessionsResilient } from './evidence';

// Reuse the completed pair; this phase must not create a third worker.
export async function runSonnetCrossgroup({ page, request }: { page: Page, request: APIRequestContext }, info: TestInfo) {
  test.setTimeout(900_000);
  expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
  const run = process.env.AMUX_LIFECYCLE_PAIR_RUN!;
  const cwd = process.env.AMUX_LIFECYCLE_LAB_WORKSPACE!;
  expect(run).toMatch(/^lc-sonnet-/);
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
      !b.tags.includes(`${run}-team`) && [a, b].every(r => /sonnet/i.test(`${r.model} ${r.flags}`));
  }).toBe(true);
  await checkpoint(page, info, 'crossgroup-membership');
  if (!observeOnly) {
    for (const [name, peer, marker] of [[reviewer, author, 'CROSS_ACK'], [author, reviewer, 'CROSS_REQUEST']]) {
      await action(name, 'peek-terminal');
      const file = `${run}-cross-${name === author ? 'author' : 'reviewer'}.json`;
      await page.locator('#peek-cmd-input').fill(`Authorized cross-group follow-up for ${run}. Work only in ${cwd}, communicate only with ${peer}. You now belong to different Amux groups. Use Bash amux send for every peer message, never Claude's native SendMessage. Create your own chore card for cross-group awareness. Use the actual Amux roster to discover the peer's group and read its completed review-cycle board card ${priorCards[peer].id}. Write ${file} as JSON {peer:"${peer}",peer_task_id:"${priorCards[peer].id}",peer_title:"actual fetched title",peer_status:"actual fetched status",peer_group:"actual group",own_task_id:"your new chore ID"}. ${name === reviewer ? `Wait for CROSS_REQUEST from ${author}, then send CROSS_ACK naming both actual task IDs and your proof file.` : `Send CROSS_REQUEST to ${reviewer} naming both actual task IDs and your proof file, then wait for CROSS_ACK and send CROSS_DONE.`} Finish your own chore with the actual read/message evidence and idle. Do not modify any peer cards or old completed cards. Discard your own automatically captured duplicate FYI cards with a reference to your real card. Do not contact production, external services or any other worker.`);
      const sent = page.waitForResponse(r => r.url().endsWith(`/${name}/send`) && r.request().method() === 'POST', { timeout: 90_000 });
      await page.locator('#peek-overlay .send-split-main').click();
      const response = await sent;
      expect(response.ok()).toBe(true);
      expect((await response.json()).submitted).toBe(true);
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
        proofs.push(proof);
        const board = await request.get(`/api/board?session=${name}&done_limit=0`, { headers });
        const history = await request.get(`/api/history?session=${name}&limit=200`, { headers });
        if (!board.ok() || !history.ok()) return false;
        const owned = await board.json();
        if (!owned.some((c: any) => c.id === proof.own_task_id && c.session === name && ['done', 'verified'].includes(c.status) && String(c.evidence || '').includes(`${run}-cross-${role}.json`))) return false;
        cards.push(...owned); messages.push(...await history.json());
      }
      // This phase proves the two cross-group work cards. The following queue
      // phase requires the entire run-owned board, including parked captures,
      // to reach terminal states after real backlog/todo pickup.
      return [[author, reviewer, 'CROSS_REQUEST'], [reviewer, author, 'CROSS_ACK'], [author, reviewer, 'CROSS_DONE']]
          .every(([from, to, marker]) => messages.some(m => m.origin === from && m.session === to && String(m.text).startsWith(marker)));
    }, { timeout: 720_000, intervals: [5000, 15000, 30000], message: 'cross-group peers must read real task metadata, exchange Amux messages and finish their own work' }).toBe(true);
    for (const size of [{ width: 1280, height: 800 }, { width: 375, height: 667 }]) {
      await page.setViewportSize(size);
      for (const [name, marker] of [[author, 'CROSS_ACK'], [reviewer, 'CROSS_DONE']]) {
        await action(name, 'peek-terminal');
        await page.getByRole('button', { name: 'Find in terminal', exact: true }).click();
        await page.locator('#peek-search').fill(marker);
        await expect(page.locator('#peek-body .peek-highlight').first()).toBeVisible({ timeout: 30_000 });
        await page.waitForFunction(() => getComputedStyle(document.querySelector('#peek-overlay')!).opacity === '1');
        await expect(page.locator('#peek-body .peek-highlight.current').first(), 'selected terminal result must be inside the visible output').toBeInViewport();
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
