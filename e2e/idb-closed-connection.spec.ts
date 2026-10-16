// A closed IndexedDB connection is reopened, not reused (2026-10-15). iOS closes
// a suspended PWA's connection; the wrapper kept the dead handle, so every
// transaction threw "The database connection is closing" and a message sat on
// "Saved on this device" with "Sync failed" until the app was killed. Here the
// real connection is closed underneath the wrapper, which is that state.
import { test, expect } from './fixtures';

test('after the browser closes the connection, writes and reads still work and the reopen is reported', async ({ page }) => {
  await page.addInitScript(() => {
    // On the prototype: WebKit ignores an override on the indexedDB instance.
    const open = IDBFactory.prototype.open;
    (window as any).__idbs = [];
    IDBFactory.prototype.open = function (this: IDBFactory, ...args: any[]) {
      const req = (open as any).apply(this, args);
      req.addEventListener('success', () => (window as any).__idbs.push(req.result));
      return req;
    } as any;
  });
  const beacons: any[] = [];
  page.on('request', r => { if (r.url().endsWith('/api/client-debug') && r.method() === 'POST') { try { beacons.push(r.postDataJSON()); } catch (_) {} } });
  await page.goto('/');
  await page.waitForFunction(() => typeof (globalThis as any).eval('_idb') === 'object');
  await page.evaluate(async () => { const i = (globalThis as any).eval('_idb'); await i.set('zz_probe', 'before'); });
  expect(await page.evaluate(() => (globalThis as any).eval('_idb').get('zz_probe'))).toBe('before');

  // Close every connection the wrapper holds, without telling it.
  const closed = await page.evaluate(() => { const all = (window as any).__idbs; all.forEach((d: IDBDatabase) => d.close()); return all.length; });
  expect(closed).toBeGreaterThan(0);

  const r = await page.evaluate(async () => {
    const i = (globalThis as any).eval('_idb');
    try { await i.set('zz_probe', 'after'); return { ok: true, value: await i.get('zz_probe') }; }
    catch (e: any) { return { ok: false, error: String(e && e.message || e) }; }
  });
  expect(r, 'a write after the close succeeds instead of "connection is closing"').toEqual({ ok: true, value: 'after' });
  await expect.poll(() => beacons.filter(b => b.kind === 'idb-reopened').length).toBeGreaterThan(0);
});
