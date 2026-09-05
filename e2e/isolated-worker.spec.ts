// Isolated workers are deliberately raw LLM lanes: amux must not turn their
// existence or peer traffic into board work, and peer workers must not be able
// to discover or target them.  This uses the real HTTP server and its on-disk
// session registry, but deliberately stops before tmux/LLM delivery: a peer
// relay must be refused at the API boundary, before it could auto-wake a model.
import { test, expect, Page } from '@playwright/test';

async function appToken(page: Page): Promise<string> {
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  expect(token, 'served bootstrap must provide the API token').toBeTruthy();
  return token;
}

test('isolated worker stays off the board and out of peer discovery', async ({ page, request }, testInfo) => {
  const token = await appToken(page);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const suffix = `${testInfo.project.name}-${Date.now()}`;
  const isolated = `e2e-raw-${suffix}`;
  const peer = `e2e-peer-${suffix}`;

  try {
    // Create both in the same group: group policy must not be what hides the
    // raw lane, and the ordinary peer is the negative control for discovery.
    for (const [name, raw] of [[isolated, true], [peer, false]] as const) {
      const created = await request.post('/api/sessions', {
        headers: auth,
        data: { name, dir: '/tmp', tags: ['e2e-isolation'], isolated: raw },
      });
      expect(created.status(), `create ${name}`).toBe(201);
    }

    // The owner dashboard remains able to see the raw lane.
    const owner = await request.get('/api/sessions', { headers: auth });
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
    const relay = await request.post(`/api/sessions/${isolated}/send`, {
      headers: { ...auth, 'X-Amux-Worker': peer },
      data: { text: 'Implement the isolated-worker acceptance fixture.' },
    });
    expect(relay.status()).toBe(403);
    const refusal = await relay.json();
    expect(refusal.error).toContain('isolated');
    expect(refusal.blocked).toBe('isolated');
    expect(refusal.code).toBe('isolated_target');
    expect(refusal.grant_id, 'approval cannot make a raw isolated lane peer-reachable').toBeUndefined();

    const board = await request.get('/api/board?done_limit=0', { headers: auth });
    expect(board.status()).toBe(200);
    const rawCards = (await board.json()).filter((card: any) => card.session === isolated);
    expect(rawCards, 'a refused peer relay must not create an isolated worker board card').toEqual([]);
  } finally {
    // Each Playwright project owns a throwaway AMUX_HOME, but clean up anyway
    // so this test remains isolated when a project later shares a server.
    await request.delete(`/api/sessions/${isolated}`, { headers: auth }).catch(() => {});
    await request.delete(`/api/sessions/${peer}`, { headers: auth }).catch(() => {});
  }
});
