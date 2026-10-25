// The amux calendar shows no schedules (Ethan, 2026-10-24: "i dont want
// schedulers on the amux calendar - its too bloated"). Every enabled
// schedule's next run used to be drawn as an event; they live in the
// Scheduler tab, and the calendar has no layer or toggle for them.
import { test, expect } from './fixtures';

test('a schedule with a next run puts nothing on the calendar', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => (globalThis as any).eval('typeof _fcGetEvents') === 'function');
  const r = await page.evaluate(() => (globalThis as any).eval(`
    schedules = [{id:'SCHED-zz', title:'zz nightly probe', enabled:1, next_run:new Date(Date.now()+3600e3).toISOString(), next_run_ms:Date.now()+3600e3}];
    calEvents = [{id:'EVT-zz', title:'zz real event', start:new Date().toISOString()}];
    ({ events: _fcGetEvents().map(e => e.title), layers: Object.keys(_CAL_LAYERS), panel: _calPanelHtml() })`));
  expect(r.events).toContain('zz real event');
  expect(r.events.some((t: string) => /zz nightly probe/.test(t))).toBe(false);
  expect(r.layers).not.toContain('sched');
  expect(r.panel).not.toContain('Scheduled tasks');
});
