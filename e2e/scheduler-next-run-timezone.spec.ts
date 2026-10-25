// A browser in a different time zone from the server sees the right "Next run".
//
// The server stores next_run as its own wall-clock string with no offset. A
// browser parsing that string as ITS local time was off by the zone difference:
// on an amux Cloud server (UTC) watched from New York, an every-1m schedule read
// "in 4h" (Codex round 3, 2026-10-18). The server now publishes next_run_ms, an
// absolute instant, and the page renders that. UTC+14 differs from any host this
// harness runs on, so a regression shows here as hours instead of minutes.
import { test, expect } from './fixtures';

test.use({ timezoneId: 'Pacific/Kiritimati' });

test('next run of an every-1m schedule reads minutes away in a far time zone', async ({ page, request }) => {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).apiCall === 'function');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => {
    await bar.locator('button').last().click();
  });
  const tok = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const title = `sx-tz-${Date.now()}`;
  const r = await request.post('/api/schedules', {
    headers: { Authorization: `Bearer ${tok}`, 'Content-Type': 'application/json', 'X-Amux-Session': 'e2e-scheduler' },
    data: { title, kind: 'shell', command: 'true', schedule_expr: 'every 1m' },
  });
  expect(r.status(), await r.text()).toBe(201);
  const id = (await r.json()).id;

  // The server's instant is within a minute or two of now, whatever its zone.
  const one = await request.get('/api/schedules/' + id, { headers: { Authorization: `Bearer ${tok}` } });
  const body = await one.json();
  expect(typeof body.next_run_ms, 'server publishes next_run_ms').toBe('number');
  expect(Math.abs(body.next_run_ms - Date.now())).toBeLessThan(3 * 60_000);

  await page.evaluate("switchView('scheduler')");
  await page.fill('#sched-search', title);
  const cell = page.locator(`[data-sched-id="${id}"] .sx-next`).first();
  await expect(cell).toBeVisible();
  // "1m", "<1m" or "due now"; never hours or days.
  await expect(cell).toHaveText(/^(<1m|[1-3]m|due now)$/);

  await request.delete('/api/schedules/' + id, { headers: { Authorization: `Bearer ${tok}`, 'X-Amux-Session': 'e2e-scheduler' } });
});
