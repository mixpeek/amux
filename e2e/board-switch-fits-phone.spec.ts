// The worker Board tab's one switch. 2026-10-20: on a phone the buttons
// squeezed the Board description into a column a few words wide. 2026-10-23
// (Ethan: one control row per tab): the switch sits in the Board toolbar and
// the description opens from the info button beside it, at the panel's width.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

test('the Board switch description keeps the row width on a phone', async ({ page, request }, testInfo) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const walkthrough = page.locator('#wt-overlay.open');
  await walkthrough.waitFor({ state: 'visible', timeout: 2_000 }).catch(() => {});
  if (await walkthrough.isVisible()) await page.locator('#wt-tooltip .wt-skip').click();

  const name = `boardfit-${testInfo.project.name}-${Date.now()}`;
  const created = await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp' } });
  expect(created.status()).toBe(201);
  try {
    await page.reload();
    await page.waitForFunction(n => typeof (window as any).openPeek === 'function'
      && (globalThis as any).eval('typeof sessions') !== 'undefined'
      && (globalThis as any).eval('sessions').some((s: any) => s.name === n), name);
    await page.evaluate(n => (window as any).openPeek(n), name);
    await page.locator('#peek-tab-issues').click();
    const bar = page.locator('#peek-issues-panel .peek-board-toolbar');
    const sw = page.locator('#peek-board-config .board-one-switch');
    await expect(sw).toBeVisible();
    const m = await page.evaluate(() => {
      const bar = document.querySelector('#peek-issues-panel .peek-board-toolbar')!.getBoundingClientRect();
      const sw = document.querySelector('#peek-board-config .board-one-switch')!.getBoundingClientRect();
      return { barTop: bar.top, barBottom: bar.bottom, barH: bar.height, swMid: sw.top + sw.height / 2, swH: sw.height };
    });
    expect(m.swMid, 'the switch is in the toolbar row').toBeGreaterThan(m.barTop);
    expect(m.swMid).toBeLessThan(m.barBottom);
    expect(m.barH, `the toolbar is one row (${m.barH}px)`).toBeLessThanOrEqual(64);
    expect(m.swH).toBeGreaterThanOrEqual(44);
    // The description opens from the info button at the panel's width.
    await page.locator('#peek-board-config .row-note > summary').click();
    const help = page.locator('#peek-board-config .row-note-body .pbc-help');
    await expect(help).toBeVisible();
    const w = await page.evaluate(() => ({
      help: document.querySelector('#peek-board-config .row-note-body')!.getBoundingClientRect().width,
      panel: document.getElementById('peek-issues-panel')!.getBoundingClientRect().width,
    }));
    expect(w.help, `description ${w.help}px of a ${w.panel}px panel`).toBeGreaterThan(w.panel * 0.85);
    void bar;
  } finally {
    await cleanup('delete fixture worker', () => request.delete(`/api/sessions/${encodeURIComponent(name)}`, { headers: auth }));
  }
});
