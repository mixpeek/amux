// Isolated workers are deliberately raw LLM lanes: amux must not turn their
// existence or refused peer traffic into board work, and peers must not be able
// to discover or target them.  This uses the real HTTP server and its on-disk
// session registry, but deliberately stops before tmux/LLM delivery: a peer
// relay must be refused at the API boundary, before it could auto-wake a model.
import { test, expect, Page } from './fixtures';
import { boot, checkpoint, deleteOwnedWorkers, getSessionsResilient } from './lifecycle/evidence';

async function appToken(page: Page): Promise<string> {
  await boot(page);
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  expect(token, 'served bootstrap must provide the API token').toBeTruthy();
  return token;
}

test('LC-ISOLATED-BOUNDARY: refused peer sends create no board work or queued messages', async ({ page, request }, testInfo) => {
  test.setTimeout(90_000);
  const token = await appToken(page);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const suffix = `${testInfo.project.name}-${Date.now()}`;
  const isolated = `lc-raw-${suffix}`;
  const peer = `lc-peer-${suffix}`;
  const outside = `lc-outside-${suffix}`;

  try {
    // Create both in the same group: group policy must not be what hides the
    // raw lane, and the ordinary peer is the negative control for discovery.
    for (const [name, raw] of [[isolated, true], [peer, false], [outside, false]] as const) {
      const created = await request.post('/api/sessions', {
        headers: auth,
        data: { name, dir: '/tmp', tags: [name === outside ? 'e2e-outside' : 'e2e-isolation'], isolated: raw },
      });
      expect(created.status(), `create ${name}`).toBe(201);
    }

    // The owner dashboard remains able to see the raw lane.
    const owner = await getSessionsResilient(request, auth);
    expect(owner.status()).toBe(200);
    const ownerNames = (await owner.json()).map((row: any) => row.name);
    expect(ownerNames).toContain(isolated);
    expect(ownerNames).toContain(peer);

    // A worker-originated fleet lookup is the discovery path available to a
    // peer. It retains an ordinary same-group worker and removes only the raw
    // lane, proving this is isolation rather than an empty/broken roster.
    const peerView = await request.get('/api/sessions', {
      headers: { ...auth, 'X-Amux-Worker': peer },
    });
    expect(peerView.status()).toBe(200);
    const peerNames = (await peerView.json()).map((row: any) => row.name);
    expect(peerNames).toContain(peer);
    expect(peerNames).not.toContain(isolated);

    // A peer relay is rejected before the send path can wake tmux or an LLM.
    // It must also leave no task card behind: raw lanes do not participate in
    // the board as a side effect of amux-mediated traffic.
    for (const sender of [peer, outside]) {
      // An explicit allowance cannot punch through isolation, either.
      expect((await request.patch(`/api/sessions/${sender}/config`, {
        headers: auth, data: { send_allow: '*' },
      })).ok()).toBe(true);
      for (const route of ['send', 'steer']) {
        const relay = await request.post(`/api/sessions/${isolated}/${route}`, {
          headers: { ...auth, 'X-Amux-Worker': sender },
          data: { text: `Refused ${sender} ${route} assignment`, record_history: true },
        });
        expect(relay.status(), `${sender} must not bypass isolation through ${route}`).toBe(403);
        const refusal = await relay.json();
        expect(refusal.error).toContain('isolated');
        expect(refusal.blocked).toBe('isolated');
        expect(refusal.code).toBe('isolated_target');
        expect(refusal.grant_id).toBeUndefined();
      }
    }
    for (const endpoint of [`/api/sessions/${isolated}/steer`, `/api/history?session=${isolated}`]) {
      const rows = await request.get(endpoint, { headers: auth });
      expect(rows.ok()).toBe(true);
      expect(await rows.json(), 'refused traffic must leave no queued message/history').toEqual([]);
    }

    const board = await request.get('/api/board?done_limit=0', { headers: auth });
    expect(board.status()).toBe(200);
    const rawCards = (await board.json()).filter((card: any) => card.session === isolated);
    expect(rawCards, 'a refused peer relay must not create an isolated worker board card').toEqual([]);
  } finally {
    // Exercise and verify the actual dashboard delete path. A resolved HTTP
    // request is not proof that a worker was unregistered.
    await deleteOwnedWorkers(page, request, auth, [isolated, peer, outside]);
  }
});

// No live model is launched here: durable acceptance must stay distinguishable
// from delivery. Native pickup/output belongs to the live-provider cases.
test('LC-ISOLATED-OWNER: owner queue survives reload and UI isolation toggles invalidate peer discovery', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  const token = await appToken(page);
  const headers = { Authorization: `Bearer ${token}` };
  const name = `lc-isolated-owner-${info.project.name}-${Date.now()}`;
  const peer = `lc-control-${info.project.name}-${Date.now()}`;
  try {
    for (const [worker, isolated] of [[name, true], [peer, false]] as const) {
      const created = await request.post('/api/sessions', { headers,
        data: { name: worker, dir: '/tmp', tags: ['e2e-isolation'], isolated } });
      expect(created.status()).toBe(201);
    }
    const text = `Owner-only queued assignment ${name}`;
    const accepted = await request.post(`/api/sessions/${name}/steer`, {
      headers, data: { text, msg_id: name, record_history: true },
    });
    expect(accepted.ok()).toBe(true);
    const receipt = await accepted.json();
    expect(typeof receipt.id).toBe('string');
    expect(receipt.deliverable, 'a stopped raw worker has not consumed this message').toBe(false);
    const peerHeaders = { ...headers, 'X-Amux-Worker': peer };
    async function roster(workerHeaders = headers) {
      const response = await getSessionsResilient(request, workerHeaders);
      expect(response.ok()).toBe(true);
      return response.json();
    }
    async function openConfiguration() {
      const close = page.locator('#peek-overlay.active').getByRole('button', { name: 'Close worker', exact: true });
      if (await close.isVisible()) await close.click();
      await page.goto('/#view=sessions');
      const card = page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
      await expect(card).toBeVisible();
      await card.locator('.card-menu-btn').click();
      await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
      await page.getByRole('button', { name: /Configurations$/ }).click();
      return page.locator('[data-worker-config="isolated"]').getByRole('switch');
    }
    // Warm both cached rosters before changing the durable setting.
    expect((await roster()).find((r: any) => r.name === name)?.isolated).toBe(true);
    expect((await roster(peerHeaders)).some((r: any) => r.name === name)).toBe(false);
    for (const enabled of [false, true]) {
      const toggle = await openConfiguration();
      await toggle.click();
      await expect.poll(async () => (await roster()).find((r: any) => r.name === name)?.isolated).toBe(enabled);
      await page.reload();
      const restored = await openConfiguration();
      await expect(restored).toHaveAttribute('aria-checked', String(enabled));
      await expect.poll(async () => (await roster(peerHeaders)).some((r: any) => r.name === name)).toBe(!enabled);
      expect((await roster(peerHeaders)).some((r: any) => r.name === peer)).toBe(true);
      const pending = await request.get(`/api/sessions/${name}/steer`, { headers });
      expect(pending.ok()).toBe(true);
      const rows = await pending.json();
      expect(rows).toHaveLength(1);
      expect(rows[0]).toMatchObject({ id: receipt.id, text });
      expect(rows[0].delivered_at || 0).toBe(0);
      await restored.scrollIntoViewIfNeeded();
      await expect(restored).toBeInViewport();
      await checkpoint(page, info, `isolated-${enabled}-configuration`);
    }
    // A retry of the same owner operation keeps its original queue identity.
    const retry = await request.post(`/api/sessions/${name}/steer`, {
      headers, data: { text, msg_id: name, record_history: true },
    });
    expect(retry.ok()).toBe(true);
    const pending = await request.get(`/api/sessions/${name}/steer`, { headers });
    expect(await pending.json()).toEqual([expect.objectContaining({ id: receipt.id, text })]);
  } finally {
    await deleteOwnedWorkers(page, request, headers, [name, peer]);
  }
});
