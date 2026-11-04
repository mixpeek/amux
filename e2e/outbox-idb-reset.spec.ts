// A hung IndexedDB read must not hang every outbox pass after it. In the Mac
// app WebKit can leave a connection whose requests never finish, with no error
// and no onclose; the outbox kept reusing it and logged 1,331 upload-list
// timeouts in a day (2026-10-23). A timeout now drops the connection, so the
// next pass opens a fresh one, and says whether a reset had already happened.
import { test, expect } from './fixtures';

test('a hung upload-list read resets the IndexedDB connection; the next read opens a fresh one', async ({ page }, testInfo) => {
  test.setTimeout(60_000);
  const beacons: any[] = [];
  page.on('request', r => {
    const b = r.postData() || '';
    if (/outbox_upload_list_timeout|outbox_upload_list_recovered|idb_connection_reset/.test(b)) { try { beacons.push(JSON.parse(b)); } catch { /* not JSON */ } }
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
  // CI's Linux WebKit has no navigator.storage at all, so nothing asks for an
  // estimate there (rust run 41486647673). Give WebKit one, on every host, so the
  // size and hung-estimate halves below run on it too.
  if (testInfo.project.name === 'ios-safari') {
    await page.evaluate(() => {
      Object.defineProperty(navigator, 'storage', {
        configurable: true,
        value: { estimate: () => Promise.resolve({ usage: 1024, quota: 1048576 }) },
      });
    });
  }
  const rows = await page.evaluate(() => (window as any)._upqListBounded(500));
  expect(rows).toEqual([]);
  await expect.poll(() => beacons.filter(b => b.verdict === 'idb_connection_reset').length).toBe(1);
  expect(beacons.find(b => b.verdict === 'idb_connection_reset').had_connection).toBe(true);
  const first = beacons.find(b => b.verdict === 'outbox_upload_list_timeout' && b.waited_ms === 500);
  expect(first?.after_reset).toBe(0);
  expect(first?.episode_start, 'the first timeout of an episode reports at once').toBe(1);

  // Still hung: the second timeout knows a reset already happened, and is
  // folded into the episode rather than beaconed again (AF-1117: 3,846
  // identical reports in one log from two clients).
  await page.evaluate(() => (window as any)._upqListBounded(500));
  const timeouts = () => beacons.filter(b => b.verdict === 'outbox_upload_list_timeout');
  expect(await page.evaluate(() => (0, eval)('_upqResetSinceOk'))).toBe(true);
  expect(timeouts().length, 'a timeout inside the report interval is folded, not sent').toBe(1);

  // Still hung after a reset: a pass's default read gives up in 250ms, not 5s,
  // so the messages behind it are not held (AF-1008).
  const deadMs = await page.evaluate(async () => {
    const t0 = performance.now();
    await (window as any)._upqListBounded();
    return performance.now() - t0;
  });
  expect(deadMs, 'a read on storage that stayed dead holds the pass under a second').toBeLessThan(1000);
  // The episode's first report asked for the storage estimate. A report carries
  // the last answer, so wait for that answer: on a loaded CI runner it can take
  // longer than the reads above.
  await expect.poll(() => page.evaluate(() => (0, eval)('_upqEstimate').answered)).toBeGreaterThan(0);
  // Past the report interval the next timeout reports once, with how many it
  // folded in and how long the episode has run.
  await page.evaluate(async () => {
    (0, eval)('_upqEpisode').last -= 11 * 60 * 1000;
    await (window as any)._upqListBounded();
  });
  await expect.poll(() => timeouts().length).toBe(2);
  const periodic = timeouts()[1];
  expect(periodic.waited_ms, 'the stayed-dead default waits 250').toBe(250);
  expect(periodic.folded, 'it counts the timeouts it folded in').toBeGreaterThanOrEqual(2);
  expect(typeof periodic.episode_s).toBe('number');
  // AF-1166: the report says how big the origin's storage is, from the
  // estimate the episode start asked for, and that the estimate answered.
  expect(typeof periodic.usage_bytes, 'the origin usage rides the report').toBe('number');
  expect(periodic.quota_bytes).toBeGreaterThan(0);
  expect(periodic.estimates_unanswered).toBe(0);
  // A storage process that does not answer the estimate either says so on the
  // next report, rather than repeating the last size it saw.
  await page.evaluate(async () => {
    (navigator.storage as any).estimate = () => new Promise(() => {});
    for (let i = 0; i < 2; i++) {
      (0, eval)('_upqEpisode').last -= 11 * 60 * 1000;
      await (window as any)._upqListBounded();
    }
  });
  await expect.poll(() => timeouts().length).toBe(4);
  expect(timeouts()[3].estimates_unanswered, 'the hung estimate is counted').toBe(1);

  // Healthy again: the read opens a NEW connection rather than reusing the dropped one.
  const opened = await page.evaluate(async (before) => {
    const g = window as any;
    g._idb.getUploads = g.__realGetUploads;
    await g._upqListBounded(5000);
    return g.__opens - before;
  }, opensBefore);
  expect(opened, 'the next read opened a fresh connection').toBeGreaterThan(0);
  await expect.poll(() => beacons.filter(b => b.verdict === 'outbox_upload_list_recovered').length,
    { message: 'the first good read reports the episode once' }).toBe(1);
  expect(beacons.find(b => b.verdict === 'outbox_upload_list_recovered').timeouts).toBeGreaterThanOrEqual(3);
});
