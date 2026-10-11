import { test, expect } from './fixtures';
import { cleanup } from './teardown';
import { createServer } from 'node:http';
import { readFileSync, writeFileSync } from 'node:fs';

test('Test refreshes saved health, survives reopen/reload, and preserves a switched drawer', async ({ page, request }, info) => {
  const home = process.env.AMUX_CONNECTOR_HEALTH_E2E_HOME || '';
  test.skip(!home, 'Requires an explicitly owned disposable server home');
  expect(home).toMatch(/^\/private\/tmp\/amux-ce-testhealth-/);
  const auth = { Authorization: 'Bearer ' + readFileSync(home + '/auth_token', 'utf8').trim() };
  const id = 'testhealth-' + info.project.name + '-' + Date.now();
  const keep = id + '-keep';
  let code = 200;
  let calls = 0;
  let authenticated = true;
  let release: (() => void) | undefined;
  let hold: Promise<void> | undefined;
  const provider = createServer(async (req, res) => {
    authenticated &&= req.headers.authorization === 'Bearer synthetic-health-key';
    calls++;
    if (hold) await hold;
    res.writeHead(code, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ ok: code === 200 }));
  });
  await new Promise<void>(resolve => provider.listen(0, '127.0.0.1', resolve));
  const port = (provider.address() as any).port;
  const label = 'Disposable health ' + info.project.name;
  const events: any[] = [];
  page.on('request', req => {
    if (new URL(req.url()).pathname === '/api/client-debug' && req.method() === 'POST') {
      try { const e = req.postDataJSON(); if (e.kind === 'connector-test-refresh') events.push(e); } catch {}
    }
  });
  const saved = async () => {
    const catalog = await (await request.get('/api/connectors', { headers: auth })).json();
    return catalog.connectors.find((c: any) => c.id === id);
  };
  const open = async (name: string) => {
    await page.locator('.cx-card').filter({ hasText: name }).click();
    await expect(page.locator('.cx-drawer .cx-name').first()).toHaveText(name);
  };
  try {
    for (const [cid, name] of [[id, label], [keep, 'Unrelated health fixture']]) {
      expect((await request.post('/api/connectors', { headers: auth, data: { id: cid, label: name, kind: 'api_key', key_env: 'HEALTH_FIXTURE_KEY', test_url: `http://127.0.0.1:${port}/canary` } })).status()).toBe(200);
    }
    expect((await request.post(`/api/connectors/${id}/credentials`, { headers: auth, data: { HEALTH_FIXTURE_KEY: 'synthetic-health-key' } })).status()).toBe(200);
    const before = await (await request.get('/health')).json();
    await page.goto('/?view=connectors');
    await open(label);
    const checks: any[] = [];
    for (const status of [200, 401, 200]) {
      code = status;
      const prior = (await saved()).last_test?.at || 0;
      const response = page.waitForResponse(r => new URL(r.url()).pathname === `/api/connectors/${id}/test`);
      await page.locator('.cx-drawer').getByRole('button', { name: 'Test connection', exact: true }).click();
      const result = await (await response).json();
      expect(result.measured).toBe(true);
      expect(result.ok).toBe(status === 200);
      await expect.poll(async () => (await saved()).last_test?.at).toBeGreaterThan(prior);
      // The POST changed durable state. The drawer/card must agree without
      // clicking Refresh health, reopening, or reloading to repair the display.
      await expect(page.locator('.cx-test')).toHaveClass(status === 200 ? /ok/ : /bad/);
      await expect(page.locator('.cx-test')).toContainText(status === 200 ? 'live provider call passed' : 'provider rejected');
      await expect(page.locator('.cx-dhead .cx-pill')).toHaveText(status === 200 ? 'Connected' : 'Needs attention');
      await page.screenshot({ path: info.outputPath('health-' + checks.length + '.png'), fullPage: true });
      await page.locator('.cx-drawer').getByRole('button', { name: 'Close', exact: true }).click();
      await open(label);
      await expect(page.locator('.cx-test')).toHaveClass(status === 200 ? /ok/ : /bad/);
      await page.reload();
      await open(label);
      await expect(page.locator('.cx-test')).toHaveClass(status === 200 ? /ok/ : /bad/);
      checks.push({ provider_http: status, durable: (await saved()).last_test, reopen_and_reload: true });
    }
    expect(calls).toBe(3);
    hold = new Promise<void>(resolve => { release = resolve; });
    await page.locator('.cx-drawer').getByRole('button', { name: 'Test connection', exact: true }).click();
    await expect.poll(() => calls).toBe(4);
    await page.locator('.cx-drawer').getByRole('button', { name: 'Close', exact: true }).click();
    await open('Unrelated health fixture');
    release!();
    await expect.poll(() => events.filter(e => e.connector === id && e.verdict === 'passed').length).toBe(3);
    await expect(page.locator('.cx-drawer .cx-name').first()).toHaveText('Unrelated health fixture');
    await page.locator('.cx-drawer').getByRole('button', { name: 'Close', exact: true }).click();
    await open(label);
    await expect(page.locator('.cx-test')).toHaveClass(/ok/);
    // Built-in unconfigured connectors also refresh their durable refusal;
    // no external provider call or new consent is made for these cases.
    for (const name of ['Slack', 'Telegram', 'Mattermost']) {
      await page.locator('.cx-drawer').getByRole('button', { name: 'Close', exact: true }).click();
      await page.locator('.cx-card').filter({ hasText: name }).click();
      await page.locator('.cx-drawer').getByRole('button', { name: 'Test connection', exact: true }).click();
      await expect(page.locator('.cx-test')).toHaveClass(/bad/);
      await expect(page.locator('.cx-test')).toContainText(name === 'Telegram' ? 'needs_credentials' : 'connect first');
      await expect(page.locator('.cx-dhead .cx-pill')).toHaveText('Needs credentials');
    }
    expect(calls).toBe(4);
    expect(authenticated).toBe(true);
    await expect.poll(() => events.length).toBe(7);
    expect(events.every(e => e.measured === true && e.n_considered === 1)).toBe(true);
    expect(JSON.stringify(events)).not.toContain('synthetic-health-key');
    const after = await (await request.get('/health')).json();
    expect(after.build).toBe(before.build);
    expect(after.pid).toBe(before.pid);
    writeFileSync(info.outputPath('receipt.json'), JSON.stringify({ result: 'PASS', before, after, checks, calls, events, preserved_switched_drawer: true, unconfigured_refusals: 3, limit: 'Private real API/provider; custom measured transitions and built-in unconfigured refusals, not new live vendor grants' }, null, 2));
  } finally {
    release?.();
    await cleanup('close fixture provider', () => new Promise<void>(resolve => provider.close(() => resolve())), info);
    for (const cid of [id, keep]) await cleanup('delete fixture connector ' + cid, () => request.delete('/api/connectors/' + cid, { headers: auth }), info);
  }
});
