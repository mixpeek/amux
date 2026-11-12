// The Calls tab (api/calls.rs, docs/design/phone-calls.md): calls seeded
// through the API show newest first, a held call is approved from its row, the
// approved call reaches a fake bridge, the bridge's result lands, and the row's
// drawer shows the transcript. On a phone the filters stay one row.
import * as http from 'http';
import type { AddressInfo } from 'net';
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

type Received = { call_id: string; result_token: string; instructions: string };

function startFakeBridge(received: Received[]): Promise<http.Server> {
  return new Promise((resolve) => {
    const server = http.createServer((req, res) => {
      let body = '';
      req.on('data', (c) => { body += c; });
      req.on('end', () => {
        if (req.method === 'POST' && req.url === '/calls') {
          received.push(JSON.parse(body));
          res.writeHead(200, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ provider_sid: 'CA-e2e' }));
        } else {
          res.writeHead(404);
          res.end();
        }
      });
    });
    server.listen(0, '127.0.0.1', () => resolve(server));
  });
}

test('Calls tab: list, approve a held call, read the transcript, one row on a phone', async ({ page, request }, testInfo) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const received: Received[] = [];
  const bridge = await startFakeBridge(received);
  const port = (bridge.address() as AddressInfo).port;
  const worker = `calls-e2e-${testInfo.project.name}-${Date.now() % 100000}`;
  const ids: string[] = [];
  try {
    const cred = await request.post('/api/connectors/phone/credentials', {
      headers: auth, data: { AMUX_CALL_BRIDGE_URL: `http://127.0.0.1:${port}` },
    });
    expect(cred.status(), await cred.text()).toBe(200);
    for (const goal of ['Book a table for two on Friday', 'Ask the pharmacy about the refill']) {
      const r = await request.post('/api/calls', {
        headers: { ...auth, 'X-Amux-Session': worker },
        data: { to: '+1 555 010 ' + String(2000 + ids.length), goal, context: 'Name: Steininger' },
      });
      expect(r.status(), await r.text()).toBe(202);
      ids.push((await r.json()).id);
    }
    const [booking, pharmacy] = ids;

    await page.waitForFunction("typeof switchView === 'function' && typeof _callsLoad === 'function'");
    await page.evaluate("switchView('calls')");
    const row = (id: string) => page.locator(`#calls-list .calls-row[data-id="${id}"]`);
    await expect(row(booking)).toBeVisible();
    await expect(row(pharmacy)).toBeVisible();
    await expect(row(booking)).toContainText('Book a table for two on Friday');
    await expect(row(booking).locator('.cx-pill')).toHaveText('held');
    // Newest first.
    const order = await page.locator('#calls-list .calls-row').evaluateAll((els) => els.map((e) => (e as HTMLElement).dataset.id));
    expect(order.indexOf(pharmacy)).toBeLessThan(order.indexOf(booking));

    // The phone keeps one row of controls above the list.
    if (page.viewportSize()!.width <= 600) {
      const bar = (await page.locator('#calls-bar').boundingBox())!;
      expect(bar.height, 'the filter bar is one row').toBeLessThanOrEqual(64);
      for (const chip of await page.locator('#calls-bar .calls-chip').all()) {
        const b = await chip.boundingBox();
        if (!b) continue;
        const mid = b.y + b.height / 2;
        expect(mid).toBeGreaterThanOrEqual(bar.y);
        expect(mid).toBeLessThanOrEqual(bar.y + bar.height);
        expect(b.height, 'chips are touch targets').toBeGreaterThanOrEqual(44);
      }
      expect(await page.locator('#calls-bar').evaluate((e) => getComputedStyle(e).flexWrap)).toBe('nowrap');
      expect(await page.locator('#calls-bar').evaluate((e) => getComputedStyle(e).overflowX)).toBe('auto');
      const list = (await page.locator('#calls-list').boundingBox())!;
      expect(list.height, 'the list gets most of the screen').toBeGreaterThan(page.viewportSize()!.height * 0.5);
    }

    // Filter chips: one worker's calls.
    await page.locator(`#calls-filters .calls-chip[data-kind="worker"][data-v="${worker}"]`).click();
    await expect(page.locator('#calls-list .calls-row')).toHaveCount(2);

    // Approve from the row: the call reaches the fake bridge.
    await row(booking).locator('.calls-approve').click();
    await expect.poll(() => received.find((r) => r.call_id === booking) ? 1 : 0).toBe(1);
    const sent = received.find((r) => r.call_id === booking)!;
    expect(sent.instructions.startsWith('You are an AI assistant calling on behalf of ')).toBe(true);
    await expect(row(booking).locator('.cx-pill')).toHaveText('dialing');

    // The bridge reports back with this call's token.
    const result = await request.post(`/api/calls/${booking}/result`, {
      headers: { ...auth, 'X-Amux-Call-Token': sent.result_token },
      data: {
        status: 'completed', outcome: 'booked for Friday 7pm', summary: 'Table for two confirmed.', duration_s: 42,
        transcript: [
          { who: 'agent', text: 'Hi, this is an AI assistant calling on behalf of Ethan.', t: 0.4 },
          { who: 'callee', text: 'Friday at seven works, see you then.', t: 5.1 },
        ],
      },
    });
    expect(result.status(), await result.text()).toBe(200);

    await page.evaluate('_callsLoad()');
    await expect(row(booking).locator('.cx-pill')).toHaveText('completed');
    await expect(row(booking)).toContainText('booked for Friday 7pm');
    await row(booking).click();
    const drawer = page.locator('#calls-drawer');
    await expect(drawer).toBeVisible();
    await expect(drawer.locator('.calls-turn')).toHaveCount(2);
    await expect(drawer).toContainText('Friday at seven works, see you then.');
    await expect(drawer).toContainText('Book a table for two on Friday');
    await expect(drawer).toContainText('Table for two confirmed.');
    await drawer.locator('.cx-x').click();
    await expect(drawer).toHaveCount(0);
  } finally {
    for (const id of ids) {
      await cleanup('decline a seeded call', () => request.post(`/api/calls/${id}/decline`, { headers: auth }), testInfo);
    }
    await cleanup('stop the fake bridge', () => new Promise<void>((r) => bridge.close(() => r())), testInfo);
  }
});
