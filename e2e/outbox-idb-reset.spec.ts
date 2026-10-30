// A hung IndexedDB read must not hang every outbox pass after it. In the Mac
// app WebKit can leave a connection whose requests never finish, with no error
// and no onclose; the outbox kept reusing it and logged 1,331 upload-list
// timeouts in a day (2026-10-23). A timeout now drops the connection, so the
// next pass opens a fresh one, and says whether a reset had already happened.
import { test, expect } from './fixtures';

test('a hung upload-list read resets the IndexedDB connection; the next read opens a fresh one', async ({ page }) => {
  test.setTimeout(60_000);
  const beacons: any[] = [];
  page.on('request', r => {
    const b = r.postData() || '';
    if (/outbox_upload_list_timeout|idb_connection_reset/.test(b)) { try { beacons.push(JSON.parse(b)); } catch { /* not JSON */ } }
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._upqListBounded === 'function' && typeof (0, eval)('_idb')?.reset === 'function');
  // Count opens, then make the next getUploads hang the way the dead handle did.
  const opensBefore = await page.evaluate(async () => {
    const g = window as any;
    g._idb = (0, eval)('_idb');           // a top-level const, not a window property
    g.__opens = 0;
    const open = indexedDB.open.bind(indexedDB);
    (indexedDB as any).open = (...a: any[]) => { g.__opens++; return open(...a); };
    await g._idb.getUploads();            // a good read: the connection is open
    g.__realGetUploads = g._idb.getUploads;
    g._idb.getUploads = () => new Promise(() => {});
    return g.__opens;
  });
  const rows = await page.evaluate(() => (window as any)._upqListBounded(500));
  expect(rows).toEqual([]);
  await expect.poll(() => beacons.filter(b => b.verdict === 'idb_connection_reset').length).toBe(1);
  expect(beacons.find(b => b.verdict === 'idb_connection_reset').had_connection).toBe(true);
  expect(beacons.find(b => b.verdict === 'outbox_upload_list_timeout')?.after_reset).toBe(0);

  // Still hung: the second timeout says a reset already happened.
  await page.evaluate(() => (window as any)._upqListBounded(500));
  await expect.poll(() => beacons.filter(b => b.verdict === 'outbox_upload_list_timeout').length).toBe(2);
  expect(beacons.filter(b => b.verdict === 'outbox_upload_list_timeout')[1].after_reset).toBe(1);

  // Still hung after a reset: a pass's default read gives up in 250ms, not 5s,
  // so the messages behind it are not held (AF-1008).
  const deadMs = await page.evaluate(async () => {
    const t0 = performance.now();
    await (window as any)._upqListBounded();
    return performance.now() - t0;
  });
  expect(deadMs, 'a read on storage that stayed dead holds the pass under a second').toBeLessThan(1000);
  await expect.poll(() => beacons.filter(b => b.verdict === 'outbox_upload_list_timeout').length).toBe(3);
  expect(beacons.filter(b => b.verdict === 'outbox_upload_list_timeout')[2].waited_ms).toBe(250);

  // Healthy again: the read opens a NEW connection rather than reusing the dropped one.
  const opened = await page.evaluate(async (before) => {
    const g = window as any;
    g._idb.getUploads = g.__realGetUploads;
    await g._upqListBounded(5000);
    return g.__opens - before;
  }, opensBefore);
  expect(opened, 'the next read opened a fresh connection').toBeGreaterThan(0);
});
