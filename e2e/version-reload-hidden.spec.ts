// 2026-10-22: every Mac app tab reloaded at once on each server build, hidden
// or not, and each reload re-read the board and the fleet. A hidden tab now
// keeps its reload until it is shown, then reloads at once.
import { test, expect } from './fixtures';

test('a hidden tab reloads for a new server build only when shown', async ({ page }) => {
  test.setTimeout(60_000);
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    const w = window as any;
    w.__hidden = false;
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => w.__hidden });
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => (w.__hidden ? 'hidden' : 'visible') });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await expect.poll(() => page.evaluate(() => !!(globalThis as any).eval('_sse && _sse.onmessage'))).toBe(true);
  let loads = 0;
  page.on('load', () => loads++);
  const beacons: any[] = [];
  page.on('request', r => {
    if (r.method() !== 'POST' || !r.url().includes('/api/client-debug')) return;
    try { const b = JSON.parse(r.postData() || '{}'); if (b.kind === 'version-reload') beacons.push(b); } catch {}
  });

  // The server's ping names a build this page is not running.
  await page.evaluate(() => {
    sessionStorage.removeItem('amux_ver_reload');
    (window as any).__hidden = true;
    document.dispatchEvent(new Event('visibilitychange'));
    (globalThis as any).eval('_sse').onmessage({ data: JSON.stringify({ type: 'ping', v: 'newer-build' }) });
  });
  await page.waitForTimeout(4000);
  expect(loads).toBe(0);
  expect(await page.evaluate(() => (globalThis as any).eval('_verReloadWhenShown'))).toBe('newer-build');

  await page.evaluate(() => { (window as any).__hidden = false; document.dispatchEvent(new Event('visibilitychange')); });
  await expect.poll(() => loads, { timeout: 15_000 }).toBeGreaterThanOrEqual(1);
  expect(beacons.map(b => b.verdict)).toContain('reload_deferred_until_shown');
});
