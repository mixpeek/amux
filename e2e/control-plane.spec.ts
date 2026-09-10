// Control-plane visibility in the REAL dashboard (RR-0075).
//
// The SPA renders workers from the legacy-shaped GET /api/sessions. This
// asserts a worker created through the modern API becomes VISIBLE in the
// real UI — the strangler-fig's load-bearing promise: the old dashboard
// keeps working while the new server takes over underneath.
import { test, expect } from '@playwright/test';

test('worker created via API appears in the dashboard session list', async ({ page, request }) => {
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
  expect(token, 'served bootstrap must carry the auth token').toBeTruthy();

  const name = `rr0075-${Date.now()}`;
  const created = await request.post('/api/workers', {
    headers: { Authorization: `Bearer ${token}` },
    data: { display_name: name, cwd: '/tmp', provider: 'claude-code' },
  });
  expect(created.status()).toBe(201);

  // The legacy shape endpoint serves it immediately...
  const legacy = await request.get('/api/sessions', {
    headers: { Authorization: `Bearer ${token}` },
  });
  const arr = await legacy.json();
  const row = arr.find((s: any) => s.name === name);
  expect(row, 'legacy array carries the worker').toBeTruthy();
  expect(row.status).toBe(''); // stopped renders blank, Python vocabulary

  // ...and the real SPA shows it. The SPA refreshes via its own SSE/poll
  // cycle; reload is the deterministic path for this assertion (the
  // sub-2s SSE-push variant needs the legacy EVENT shape, tracked in the
  // runbook's known-incomplete list).
  await page.reload();
  await expect(page.locator('body')).toContainText(name, { timeout: 10_000 });
});
