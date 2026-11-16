// Governance PR 3: loopback is not a credential. The suite's servers run with
// AMUX_RS_NO_LOOPBACK_BYPASS=1, which now means AMUX_LOOPBACK_TRUST=enforce,
// so this spec proves the dashboard keeps working under enforce (it sends the
// owner bearer on every call) while a request from the same machine with no
// credential is refused, and the counters say so.
import { test, expect } from './fixtures';

test('the dashboard works under loopback enforce; a bare local request does not', async ({ page, request }) => {
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._authHeaders === 'function');

  const r = await page.evaluate(async () => {
    const h = (window as any)._authHeaders({ 'Content-Type': 'application/json' });
    const counters = await fetch('/api/contract/counters', { headers: h }).then((x) => x.json());
    const title = 'loopback spec ' + Math.random().toString(36).slice(2, 8);
    const created = await fetch('/api/board', {
      method: 'POST', headers: h, body: JSON.stringify({ title, type: 'chore', status: 'backlog' }),
    });
    const created_body = await created.json().catch(() => ({}));
    if (created_body && created_body.id) {
      await fetch('/api/board/' + created_body.id, { method: 'DELETE', headers: h }).catch(() => {});
    }
    return { loopback: counters.loopback, created: created.status };
  });
  // The page's own fetch adds the owner credential to every call (app.js wraps
  // window.fetch), so the bare local request goes through Playwright's client,
  // from the same machine, with nothing on it.
  const bare = await request.get('/api/sessions');
  const bareBody = await bare.json().catch(() => ({}));

  expect(r.loopback.mode).toBe('enforce');
  expect(r.loopback.measured).toBe(true);
  expect(r.loopback.enforceable).toBe(true);
  expect(r.created, 'the dashboard writes with the owner bearer').toBe(201);
  expect(bare.status(), 'a local request with no credential is refused').toBe(401);
  expect(bareBody.reason).toBe('loopback_not_trusted');
  await expect(page.locator('#board-view')).toBeAttached();
  expect(errors).toEqual([]);
});
