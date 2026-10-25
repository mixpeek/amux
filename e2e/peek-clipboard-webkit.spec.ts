// Cmd+V and Cmd+C in a worker's terminal (the peek) need no clipboard permission.
// Ethan, 2026-10-21: "i can't copy/paste it gets error". With the terminal output
// focused, Cmd+V called navigator.clipboard.readText(), which WebKit (the amux
// desktop app, Safari) refuses from a key handler: "Paste failed — clipboard
// blocked?" and nothing pasted. Now Cmd+V focuses the composer and the native
// paste lands there; readText is only a fallback when no paste event arrives.
// The worker and every write are fakes.
import { test, expect, allowUnusedRoute } from './fixtures';

test('Cmd+V from the terminal output pastes into the composer without the clipboard API', async ({ page }) => {
  const worker = { name: 'clip-worker', running: true, status: 'idle', lifecycle: 'active', tags: [], dir: '/tmp', provider: 'claude' };
  await page.route('**/api/events**', r => r.abort());
  allowUnusedRoute(page, '**/api/events**');
  const sessionsList = /\/api\/sessions(?:\?.*)?$/;
  await page.route(sessionsList, r => r.request().method() === 'GET' ? r.fulfill({ json: [worker] }) : r.fallback());
  allowUnusedRoute(page, sessionsList);
  const beacons: any[] = [];
  await page.route('**/api/client-debug', r => { try { beacons.push(JSON.parse(r.request().postData() || '{}')); } catch (e) {} return r.fulfill({ json: { ok: true } }); });
  allowUnusedRoute(page, '**/api/client-debug');
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    // The refusal WebKit gives: any readText call fails, and is counted.
    (window as any).__reads = 0;
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: {
      readText: () => { (window as any).__reads++; return Promise.reject(new DOMException('not allowed', 'NotAllowedError')); },
      read: () => { (window as any).__reads++; return Promise.reject(new DOMException('not allowed', 'NotAllowedError')); },
      writeText: () => Promise.reject(new DOMException('not allowed', 'NotAllowedError')),
    } });
  });
  await page.goto('/');
  await page.waitForFunction("typeof openPeek === 'function' && typeof sessions !== 'undefined' && sessions.length > 0");
  await page.evaluate(() => (window as any).openPeek('clip-worker'));
  await expect(page.locator('#peek-cmd-input')).toBeAttached();
  // Focus is on the terminal output, as after clicking or selecting in it.
  await page.evaluate(() => { const b = document.getElementById('peek-body')!; b.tabIndex = -1; b.focus(); });

  // The native paste: the browser delivers it to whatever is focused AFTER keydown.
  await page.evaluate(() => document.addEventListener('keydown', e => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'v' && !e.defaultPrevented) {
      const dt = new DataTransfer(); dt.setData('text/plain', 'pasted-natively');
      const t = document.activeElement as HTMLTextAreaElement;
      const ev = new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true });
      if (t.dispatchEvent(ev) && 'value' in t) { t.value += 'pasted-natively'; }
    }
  }));
  await page.keyboard.press('Meta+v');
  await page.waitForTimeout(300);
  expect(await page.evaluate(() => document.activeElement?.id)).toBe('peek-cmd-input');
  await expect(page.locator('#peek-cmd-input')).toHaveValue(/pasted-natively/);
  expect(await page.evaluate(() => (window as any).__reads)).toBe(0);
  await expect(page.getByText('Paste failed')).toHaveCount(0);
});

test('Cmd+C on selected terminal output copies through the native copy event', async ({ page }) => {
  const worker = { name: 'clip-worker', running: true, status: 'idle', lifecycle: 'active', tags: [], dir: '/tmp', provider: 'claude' };
  await page.route('**/api/events**', r => r.abort());
  allowUnusedRoute(page, '**/api/events**');
  const sessionsList = /\/api\/sessions(?:\?.*)?$/;
  await page.route(sessionsList, r => r.request().method() === 'GET' ? r.fulfill({ json: [worker] }) : r.fallback());
  allowUnusedRoute(page, sessionsList);
  await page.route('**/api/client-debug', r => r.fulfill({ json: { ok: true } }));
  allowUnusedRoute(page, '**/api/client-debug');
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: {
      writeText: () => Promise.reject(new DOMException('not allowed', 'NotAllowedError')),
      readText: () => Promise.reject(new DOMException('not allowed', 'NotAllowedError')),
    } });
  });
  await page.goto('/');
  await page.waitForFunction("typeof openPeek === 'function' && typeof sessions !== 'undefined' && sessions.length > 0");
  await page.evaluate(() => {
    (window as any).openPeek('clip-worker');
    const b = document.getElementById('peek-body')!;
    b.textContent = 'line one of output\nselect me please\n';
    (window as any).__copied = null;
    // Registered after the app's own copy listener, so it reads what that one set.
    document.addEventListener('copy', e => { (window as any).__copied = (e as ClipboardEvent).clipboardData?.getData('text/plain') ?? null; });
    const r = document.createRange(); r.setStart(b.firstChild!, 19); r.setEnd(b.firstChild!, 35);
    getSelection()!.removeAllRanges(); getSelection()!.addRange(r);
  });
  await page.keyboard.press('Meta+c');
  await page.waitForTimeout(200);
  expect(await page.evaluate(() => (window as any).__copied)).toBe('select me please');
  await expect(page.getByText('Copy failed')).toHaveCount(0);
});
