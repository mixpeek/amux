import { test, expect } from './fixtures';
import { createServer } from 'node:http';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';

test('Forget can be cancelled, then disconnects only its accounts and cancels late consent', async ({ page, request }, info) => {
  const home = process.env.AMUX_FORGET_E2E_HOME || '';
  test.skip(!home, 'Requires an explicitly owned disposable server home');
  expect(home).toMatch(/^\/private\/tmp\/amux-ce-forget/);
  const token = readFileSync(home + '/auth_token', 'utf8').trim();
  const auth = { Authorization: 'Bearer ' + token };
  const id = 'forget-' + info.project.name + '-' + Date.now();
  const keep = id + '-keep';
  let exchanges = 0;
  const provider = createServer((req, res) => {
    if (req.url === '/token') exchanges++;
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ access_token: 'synthetic-late-token', refresh_token: 'synthetic-refresh', expires_in: 3600 }));
  });
  await new Promise<void>(resolve => provider.listen(0, '127.0.0.1', resolve));
  const port = (provider.address() as any).port;
  const definition = { id, label: 'Disposable Forget OAuth ' + info.project.name, kind: 'oauth2',
    client_id_env: 'FORGET_FIXTURE_ID', client_secret_env: 'FORGET_FIXTURE_SECRET',
    authorize_url: 'https://example.test/authorize', token_url: `http://127.0.0.1:${port}/token`, scopes: 'fixture:read' };
  try {
    expect((await request.post('/api/connectors', { headers: auth, data: definition })).status()).toBe(200);
    expect((await request.post('/api/connectors', { headers: auth, data: { id: keep, label: 'Unrelated fixture key', kind: 'api_key', key_env: 'FORGET_KEEP_KEY', test_url: `http://127.0.0.1:${port}/canary` } })).status()).toBe(200);
    expect((await request.post(`/api/connectors/${id}/credentials`, { headers: auth, data: { FORGET_FIXTURE_ID: 'fixture-client', FORGET_FIXTURE_SECRET: 'fixture-secret' } })).status()).toBe(200);
    expect((await request.post(`/api/connectors/${keep}/credentials`, { headers: auth, data: { FORGET_KEEP_KEY: 'synthetic-retained-key' } })).status()).toBe(200);
    const grantDir = home + '/connectors/' + id;
    mkdirSync(grantDir, { recursive: true });
    for (const account of ['alice', 'beth']) writeFileSync(grantDir + '/' + account + '.json', JSON.stringify({ token: 'synthetic-' + account,
      expires_at: 4_000_000_000, token_uri: definition.token_url, client_id: 'fixture-client', client_secret: 'fixture-secret', scopes: 'fixture:read' }), { mode: 0o600 });
    // Prove the old grant is usable, not merely an inert file.
    const positive = await request.post(`/api/connectors/${id}/token?account=beth`, { headers: auth, data: {} });
    expect(positive.status()).toBe(200);
    expect((await positive.json()).access_token).toBe('synthetic-beth');
    const pending = await request.post(`/api/connectors/${id}/auth?account=late`, { headers: auth });
    expect(pending.status()).toBe(200);
    const authorization = new URL((await pending.json()).authorize_url);
    expect(authorization.searchParams.get('redirect_uri')).toBe(new URL(`/api/connectors/${id}/callback`, info.project.use.baseURL).href);
    const state = authorization.searchParams.get('state');
    expect(state).toBeTruthy();
    const envBefore = readFileSync(home + '/server.env', 'utf8');
    const grantsBefore = ['alice', 'beth'].map(a => readFileSync(grantDir + '/' + a + '.json', 'utf8'));
    const events: any[] = [];
    let deletes = 0;
    page.on('request', req => {
      if (new URL(req.url()).pathname === '/api/connectors/' + id && req.method() === 'DELETE') deletes++;
      if (new URL(req.url()).pathname === '/api/client-debug' && req.method() === 'POST') {
        try { const e = req.postDataJSON(); if (e.kind === 'connector-forget' && e.connector === id) events.push(e); } catch {}
      }
    });
    await page.goto('/?view=connectors');
    await page.locator('.cx-card').filter({ hasText: definition.label }).click();
    await page.locator('.cx-drawer').getByRole('button', { name: 'Configuration', exact: true }).click();
    await page.getByRole('button', { name: 'Forget', exact: true }).click();
    const modal = page.locator('#modal-backdrop');
    await expect(modal).toHaveClass(/open/);
    await expect(modal).toContainText('disconnects its saved accounts');
    await expect(modal).toContainText('Cancels pending sign-ins');
    await expect(modal).toContainText('server.env are left alone');
    await page.screenshot({ path: info.outputPath('forget-confirmation.png'), fullPage: true });
    await modal.getByRole('button', { name: 'Cancel', exact: true }).click();
    await expect(modal).not.toHaveClass(/open/);
    expect(deletes).toBe(0);
    expect(['alice', 'beth'].map(a => readFileSync(grantDir + '/' + a + '.json', 'utf8'))).toEqual(grantsBefore);
    expect(readFileSync(home + '/connectors/pending.json', 'utf8')).toContain(state!);
    await page.getByRole('button', { name: 'Forget', exact: true }).click();
    await expect(modal).toHaveClass(/open/);
    const removed = page.waitForResponse(res => new URL(res.url()).pathname === '/api/connectors/' + id && res.request().method() === 'DELETE');
    await modal.getByRole('button', { name: 'Forget connector', exact: true }).click();
    expect((await removed).status()).toBe(200);
    await expect(page.locator('.cx-card').filter({ hasText: definition.label })).toHaveCount(0);
    expect(deletes).toBe(1);
    expect(readFileSync(home + '/server.env', 'utf8')).toBe(envBefore);
    expect(JSON.parse(readFileSync(home + '/connectors/custom.json', 'utf8')).some((d: any) => d.id === keep)).toBe(true);
    for (const account of ['alice', 'beth']) expect(JSON.parse(readFileSync(grantDir + '/' + account + '.json', 'utf8'))).toEqual({ disconnected: true });
    expect(readFileSync(home + '/connectors/pending.json', 'utf8')).not.toContain(state!);
    // Actual late redirect navigation must fail without contacting the provider.
    const callback = await page.goto(`/api/connectors/${id}/callback?state=${state}&code=late-only`);
    expect([400, 404]).toContain(callback!.status());
    expect(exchanges).toBe(0);
    expect((await request.post('/api/connectors', { headers: auth, data: definition })).status()).toBe(200);
    const mint = await request.post(`/api/connectors/${id}/token`, { headers: auth, data: {} });
    expect(mint.status()).toBe(400);
    expect((await mint.json()).stored_accounts).toEqual([]);
    for (const account of ['alice', 'beth']) expect((await request.post(`/api/connectors/${id}/token?account=${account}`, { headers: auth, data: {} })).status()).toBe(404);
    expect(exchanges).toBe(0);
    expect(events.map(e => e.verdict)).toEqual(['cancelled', 'removed']);
    expect(events.every(e => e.measured === true && e.n_considered === 1)).toBe(true);
    await page.screenshot({ path: info.outputPath('late-callback-refused.png'), fullPage: true });
  } finally {
    await request.delete('/api/connectors/' + id, { headers: auth });
    await request.delete('/api/connectors/' + keep, { headers: auth });
    await new Promise<void>(resolve => provider.close(() => resolve()));
  }
});
