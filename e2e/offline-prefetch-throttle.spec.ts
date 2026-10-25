// The offline prefetch peeks every running worker. On 2026-10-23 the Mac app's
// windows each ran it every 10-60s instead of every 10 minutes (9,005 peeks in
// 15 minutes, amux-server-rs over 300% CPU). Automatic passes now run at most
// once per 10 minutes across every window of the origin; a manual sync still
// runs at once. Workers and their peeks are fakes.
import { test, expect } from './fixtures';

const WORKERS = ['zz-pf-a', 'zz-pf-b'];

test('automatic offline prefetch runs once per window of time across windows; manual still runs', async ({ context }) => {
  const peeks: string[] = [];
  // page.route on each window (the fixture counts page stubs; it does not wrap context.route).
  const stub = async (p: any) => {
    await p.route(/\/api\/sessions(?:\?.*)?$/, (r: any) => r.fulfill({ json: WORKERS.map(name => (
      { name, dir: '/tmp/' + name, running: true, status: 'active', flags: '', steering: [] })) }));
    await p.route(/\/api\/sessions\/zz-pf-[ab]\/peek\?lines=200$/, (r: any) => {
      peeks.push(r.request().url());
      return r.fulfill({ json: { output: 'frame\n', history: 'line\n' }, headers: { ETag: '"e' + peeks.length + '"' } });
    });
  };
  const beacons: string[] = [];
  const watch = (p: any) => p.on('request', (r: any) => { const b = r.postData() || ''; if (b.includes('offline_prefetch_throttled')) beacons.push(b); });
  const boot = async () => {
    const p = await context.newPage(); watch(p); await stub(p);
    await p.goto('/');
    await p.waitForFunction((n) => typeof (window as any)._offlinePrefetch === 'function'
      && (globalThis as any).eval('sessions').filter((s: any) => s.running).length === n, WORKERS.length);
    return p;
  };
  const a = await boot();
  // Start from a clean slate, whatever the boot-time pass did.
  await a.evaluate(() => localStorage.removeItem('amux_offline_prefetch_last'));
  const before = peeks.length;
  await a.evaluate(() => (window as any)._offlinePrefetch(false, 'online'));
  expect(peeks.length - before, 'the first automatic pass peeks every running worker').toBe(WORKERS.length);

  // Another window's trigger inside the window of time is skipped and reported.
  const b = await boot();
  const mid = peeks.length;
  await b.evaluate(() => (window as any)._offlinePrefetch(false, 'online'));
  await a.evaluate(() => (window as any)._offlinePrefetch(false, 'interval'));
  expect(peeks.length - mid, 'no second automatic pass inside 10 minutes, in either window').toBe(0);
  await expect.poll(() => beacons.length).toBeGreaterThan(0);
  expect(beacons.some(x => x.includes('"trigger":"online"'))).toBe(true);

  // A person asking for it is never throttled.
  await b.evaluate(() => (window as any)._offlinePrefetch(true));
  expect(peeks.length - mid, 'a manual sync runs at once').toBe(WORKERS.length);
});
