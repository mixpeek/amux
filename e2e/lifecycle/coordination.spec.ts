import { test, expect } from '../fixtures';
import { boot, auth, checkpoint, deleteOwnedWorkers, getSessionsResilient } from './evidence';

test('LC-COORD-POLICY: peer task awareness spans groups; explicit deny and isolation refuse delivery', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  await boot(page);
  const headers = await auth(page);
  const suffix = `${info.project.name}-${Date.now()}`;
  const author = `lc-author-${suffix}`, peer = `lc-peer-${suffix}`, outside = `lc-outside-${suffix}`, raw = `lc-raw-${suffix}`;
  const names = [author, peer, outside, raw];
  const grants: string[] = [];
  try {
    for (const [name, group, isolated] of [[author, 'build', false], [peer, 'build', false],
      [outside, 'review', false], [raw, 'build', true]] as const) {
      const made = await request.post('/api/sessions', { headers,
        data: { name, dir: '/tmp', tags: [`lc-${group}-${suffix}`], isolated } });
      expect(made.status()).toBe(201);
    }
    const cards: any[] = [];
    for (const name of [peer, outside]) {
      const made = await request.post('/api/board', { headers,
        data: { title: `Peer review owned by ${name}`, session: name, status: 'todo', type: 'chore' } });
      expect(made.status()).toBe(201);
      cards.push(await made.json());
    }
    const workerHeaders = { ...headers, 'X-Amux-Worker': author };
    const roster = await getSessionsResilient(request, workerHeaders);
    expect(roster.ok()).toBeTruthy();
    const visible = (await roster.json()).map((row: any) => row.name);
    expect(visible).toEqual(expect.arrayContaining([author, peer, outside]));
    expect(visible).not.toContain(raw);
    for (const card of cards) {
      const detail = await request.get(`/api/board/${encodeURIComponent(card.id)}`, { headers: workerHeaders });
      expect(detail.ok(), 'peer can inspect actual task context').toBeTruthy();
      expect(await detail.json()).toMatchObject({ id: card.id, session: card.session, title: card.title });
      await page.goto(`/#issue=${encodeURIComponent(card.id)}`);
      await expect(page.locator('#bd-key')).toHaveText(card.id);
      await checkpoint(page, info, `peer-task-${card.id}`);
    }
    const deny = await request.patch(`/api/sessions/${author}/config`, { headers, data: { send_allow: '' } });
    expect(deny.ok()).toBeTruthy();
    // Only refusal paths here: no real model launches in the deterministic harness.
    // REFUSAL PRECEDENCE, as the send path documents it. Isolation is checked
    // first and is absolute (403 isolated_target: no grant can authorize peer
    // delivery). The lifecycle/liveness check comes next and deliberately
    // outranks the group gate (409 target_not_running): a cross-group refusal
    // mints a single-use approval grant, and no grant can make a lane that is
    // not running into a peer, so that answer must never reach the grant
    // branch. These fixtures are registered but never launched — no model
    // launches in this deterministic harness — so `outside` is refused for
    // liveness before the group policy is consulted. The group policy itself
    // is unit-covered (cross_group_send_ok and friends in session_verbs.rs).
    const expected: Record<string, { status: number; code: string; error: RegExp }> = {
      [raw]: { status: 403, code: 'isolated_target', error: /isolated/i },
      [outside]: { status: 409, code: 'target_not_running', error: /not running/i },
    };
    for (const target of [outside, raw]) {
      const refused = await request.post(`/api/sessions/${target}/send`, {
        headers: workerHeaders, data: { text: `lc-denied-${suffix}` } });
      const want = expected[target];
      expect(refused.status(), `${target} refusal status`).toBe(want.status);
      const body = await refused.json();
      if (body.grant_id) grants.push(body.grant_id);
      expect(body.code).toBe(want.code);
      expect(body.error).toMatch(want.error);
      await info.attach(`refused-${target}`, { body: JSON.stringify(body), contentType: 'application/json' });
    }
    await page.locator('#board-detail-overlay.active > .overlay-header').getByRole('button', { name: /Back/ }).click();
    const closeWorker = page.locator('#peek-overlay.active').getByRole('button', { name: 'Close worker', exact: true });
    if (await closeWorker.isVisible()) await closeWorker.click();
    await page.goto('/');
    for (const id of grants) {
      const deny = page.locator(`button[onclick*="_grantReject('${id}'"]`);
      await expect(deny).toBeVisible();
      await deny.click();
      await expect(deny).toHaveCount(0);
    }
    const pending = await request.get('/api/grants', { headers });
    expect(pending.ok()).toBeTruthy();
    const remaining = JSON.stringify(await pending.json());
    for (const id of grants) expect(remaining).not.toContain(id);
  } finally {
    // A failed assertion must not leave a permission banner covering later tests.
    for (const id of grants) await request.post(`/api/grants/${id}/reject`, { headers });
    await deleteOwnedWorkers(page, request, headers, names);
  }
});
