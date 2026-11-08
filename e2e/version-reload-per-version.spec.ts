// The version reload is throttled per target version (implicit state machines
// inventory 2026-10-31, #34): the same version is tried once in 10 minutes so
// a reload that did not take cannot loop, but a newer deploy inside that
// window still reloads. A flat throttle left a tab on old code after two
// deploys close together.
import { test, expect } from './fixtures';

test('a second, newer version inside the throttle window still reloads', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._reloadForServerVersion === 'function');
  // Huge delays: the decision is recorded at once, the reload never fires here.
  const target = (v: string) => page.evaluate(v => {
    (window as any)._reloadForServerVersion(v, 1e9);
    return JSON.parse(sessionStorage.getItem('amux_ver_reload') || '{}');
  }, v);
  const first = await target('ver-A');
  expect(first.v).toBe('ver-A');
  const again = await target('ver-A');
  expect(again.at, 'the same version is not retried inside 10 minutes').toBe(first.at);
  const newer = await target('ver-B');
  expect(newer.v, 'a newer deploy is not swallowed by the throttle').toBe('ver-B');
});
