// A forced reconnect while on the polling fallback opens the stream at once
// (implicit state machines inventory 2026-10-31, #32). connectSSE() returned
// early while _sseFallback was set, so it waited for the 30 s poll retry.
import { test, expect } from './fixtures';

test('a forced reconnect during polling opens the stream now', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._forceSseReconnect === 'function');
  const opened = await page.evaluate(() => {
    eval('if (_sse) { _sse.close(); _sse = null; } _sseFallback = true;');
    (window as any)._forceSseReconnect('test');
    return eval('!!_sse && _sseFallback === false');
  });
  expect(opened).toBe(true);
});
