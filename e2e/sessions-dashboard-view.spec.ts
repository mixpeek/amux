// Performance pass, 2026-10-19: the worker list carried two fields the
// dashboard never reads (worktree_integration, status_evidence: 246KB of a
// 920KB list over 197 workers), and the dashboard parses and re-stringifies the
// whole list on every change. It now asks for the dashboard view; every other
// caller keeps the full shape.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

test('the dashboard reads the slim worker list and other callers keep the full one', async ({ page, request }, info) => {
  test.setTimeout(60_000);
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const uiToken = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const name = `slim-view-${info.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp', start: false } })).status()).toBe(201);
  try {
    const seen: any[] = [];
    page.on('response', async r => {
      const u = new URL(r.url());
      if (u.pathname === '/api/sessions' && r.request().method() === 'GET' && r.status() === 200) {
        seen.push({ view: r.request().headers()['x-amux-view'], rows: await r.json().catch(() => null) });
      }
    });
    await page.reload();
    await page.waitForFunction(() => typeof (window as any).switchView === 'function');
    // The list is built behind a short shared cache, so the new worker can
    // first appear on a later read (an SSE invalidation refetches it).
    await expect.poll(() => seen.filter(s => Array.isArray(s.rows) && s.rows.some((r: any) => r.name === name)).length,
      { timeout: 30_000 }).toBeGreaterThan(0);
    const dash = seen.find(s => Array.isArray(s.rows) && s.rows.some((r: any) => r.name === name));
    expect(dash.view).toBe('dashboard');

    const full = await (await request.get('/api/sessions', { headers: auth })).json();
    const mine = full.find((r: any) => r.name === name);
    // Premise: the full shape carries the field, or the absence below proves nothing.
    expect(mine).toHaveProperty('status_evidence');
    const slim = dash.rows.find((r: any) => r.name === name);
    expect(slim).not.toHaveProperty('status_evidence');
    expect(slim).not.toHaveProperty('worktree_integration');
    expect(slim.status).toBe(mine.status);
    // The query form answers the same for a curl.
    const viaQuery = await (await request.get('/api/sessions?view=dashboard', { headers: auth })).json();
    expect(viaQuery.find((r: any) => r.name === name)).not.toHaveProperty('status_evidence');
    await expect(page.locator(`#cards .card[data-session="${name}"]`).first()).toBeAttached();
  } finally {
    await cleanup('delete fixture worker', () => request.post(`/api/sessions/${encodeURIComponent(name)}/delete`,
      { headers: { ...auth, 'X-Amux-UI-Token': uiToken }, data: {} }), info);
  }
});
