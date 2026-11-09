// A published app's own amux access (app_identity.rs, governance PR 5). A
// worker can ask for access when it publishes; the Apps tab shows what was
// asked on the pending card and in the approve prompt, and approving allows
// exactly that. A live app shows what it may call.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';
import * as net from 'net';

const freePort = () => new Promise<number>(res => {
  const s = net.createServer();
  s.listen(0, '127.0.0.1', () => { const p = (s.address() as net.AddressInfo).port; s.close(() => res(p)); });
});

test('the Apps tab shows the amux access an app asks for, and approving allows exactly it', async ({ page, playwright }, testInfo) => {
  const anon = await playwright.request.newContext({ ignoreHTTPSErrors: true });
  let id = '';
  let base = '';
  let token = '';
  try {
    await page.goto('/');
    await page.waitForFunction(() => !!(window as any)._AMUX_AUTH_TOKEN, { timeout: 20000 });
    token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
    base = new URL(page.url()).origin;
    const title = `asks for access ${testInfo.project.name}`;
    // The bearer alone is not a person, so this lands pending, like a worker's.
    const asked = await anon.post(`${base}/api/page-links`, {
      headers: { Authorization: 'Bearer ' + token, 'Content-Type': 'application/json' },
      data: { kind: 'app', port: await freePort(), title, api: ['GET /api/sessions'] },
    });
    expect(asked.status()).toBe(201);
    const link = await asked.json();
    id = link.id;
    expect(link.pending).toBe(true);
    expect(link.api_requested).toEqual(['GET /api/sessions']);
    expect(link.api_allow ?? []).toEqual([]);
    expect(typeof link.app_token).toBe('string');

    await page.goto('/#view=apps');
    const card = page.locator(`[data-pub="${id}"]`);
    await expect(card).toContainText('Waiting for your approval');
    await expect(card.locator(`[data-pub-api="${id}"]`)).toHaveText('asks for amux access: GET /api/sessions');
    await card.getByRole('button', { name: 'Approve' }).click();
    await expect(page.locator('#modal-msg')).toContainText('It also asks to call amux: GET /api/sessions. Approving allows exactly these.');
    await page.locator('#modal-btns').getByRole('button', { name: 'Approve' }).click();
    await expect(card).not.toContainText('Waiting for your approval', { timeout: 15000 });
    await expect(page.locator(`[data-pub-api="${id}"]`)).toHaveText('amux access: GET /api/sessions');

    const list = await (await anon.get(`${base}/api/page-links`, { headers: { Authorization: 'Bearer ' + token } })).json();
    const row = (list.links || []).find((l: any) => l.id === id);
    expect(row.api_allow).toEqual(['GET /api/sessions']);
    expect(JSON.stringify(list)).not.toContain(link.app_token);
  } finally {
    if (id) {
      await cleanup('remove the published app link', async () => {
        await anon.delete(`${base}/api/page-links/${id}`, { headers: { Authorization: 'Bearer ' + token } });
      }, testInfo);
    }
    await cleanup('dispose the anonymous request context', () => anon.dispose(), testInfo);
  }
});
