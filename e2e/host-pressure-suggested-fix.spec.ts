// The Host strained dialog says what to do about it (Ethan, MSG-90406: "this
// should give me a suggested action to run to fix it"): the biggest consumers
// first, a Pause button for a worker, a command to copy for anything amux does
// not own.
import { test, expect } from './fixtures';

const strained = (util: any) => ({
  measured: true, level: 'strained', reasons: ['load 1.91/core over 5 min (53.6 on 28 cores)'],
  readings: { load5: 53.6, ncpu: 28, mem_level: 1, disk_free_gb: 247 }, deferred: { 'board-dispatch': 5, schedules: 22 }, runaways: [],
  utilization: { cpu_percent_total: 2000, capacity_percent: 2800, workers_cpu_percent: 500, unattributed_cpu_percent: 1500, kinds: {}, ...util },
});

test('Host strained suggests a fix for each big consumer, and the buttons act', async ({ page, context }, info) => {
  if (info.project.name === 'desktop') await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await page.route('**/api/metrics/host/pressure', r => r.fulfill({ json: strained({
    workers: [{ name: 'busy-worker', cpu_percent: 320, rss_mb: 2048, n_processes: 9 }, { name: 'quiet', cpu_percent: 40, rss_mb: 100, n_processes: 2 }],
    top_unattributed: [
      { kind: 'vm', cpu_percent: 537, root_pid: 1083, command: '/System/Library/.../com.apple.Virtualization.VirtualMachine', n_processes: 1, rss_mb: 35000 },
      { kind: 'detached', cpu_percent: 180, root_pid: 4242, command: '/usr/local/bin/ffmpeg', n_processes: 1, rss_mb: 300 },
    ] }) }));
  const pauses: string[] = [];
  await page.route('**/api/workers/*/pause', r => { pauses.push(r.request().url()); return r.fulfill({ json: { ok: true } }); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).refreshHostPressure === 'function');
  await page.evaluate(() => (window as any).refreshHostPressure());
  const pill = page.locator('#host-pressure-pill');
  await expect(pill).toBeVisible();
  await pill.click();
  const dlg = page.locator('#host-pressure-detail');
  await expect(dlg).toContainText('Suggested fix');
  const items = dlg.locator('.host-fix-list > li');
  await expect(items).toHaveCount(3);
  // Biggest first: the VM (5.4 cores), the worker (3.2), the stray process (1.8).
  await expect(items.nth(0)).toContainText('virtual machine');
  await expect(items.nth(0).locator('.host-fix-cmd')).toContainText('docker ps');
  await expect(items.nth(1)).toContainText('busy-worker');
  await expect(items.nth(2).locator('.host-fix-cmd')).toHaveText('renice -n 10 -p 4242');
  await expect(dlg.locator('.host-fix-list')).not.toContainText('quiet');

  await items.nth(2).locator('[data-host-fix-copy]').click();
  if (info.project.name === 'desktop') {
    await expect(page.locator('#toast')).toContainText('Copied: renice -n 10 -p 4242');
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe('renice -n 10 -p 4242');
  } else {
    await expect(page.locator('#toast')).toContainText(/Cop(ied|y failed)/);
  }
  await items.nth(1).locator('[data-host-fix-pause]').click();
  await expect.poll(() => pauses.length).toBe(1);
  expect(pauses[0]).toContain('/api/workers/busy-worker/pause');
});

test('with no single big consumer it says so and points at the full split', async ({ page }) => {
  await page.route('**/api/metrics/host/pressure', r => r.fulfill({ json: strained({
    workers: [{ name: 'a', cpu_percent: 60, rss_mb: 100, n_processes: 2 }], top_unattributed: [{ kind: 'system', cpu_percent: 90, root_pid: 1, command: 'launchd', n_processes: 400 }] }) }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).refreshHostPressure === 'function');
  await page.evaluate(() => (window as any).refreshHostPressure());
  await page.locator('#host-pressure-pill').click();
  const items = page.locator('#host-pressure-detail .host-fix-list > li');
  await expect(items).toHaveCount(1);
  await expect(items.first()).toContainText('spread across many small processes');
  await expect(items.first().locator('.host-fix-cmd')).toHaveText('amux resources');
});

// The fix names the busiest process in a tree, not the tree's root (a shell
// or a service using nothing itself), and does not offer a renice for a
// process that is already low priority (2026-10-27: a deploy build's rustc at
// 181% was shown as "systemd" with `renice -n 10 -p 24876`).
test('the fix names the busy process under a root, and skips renice when it is already niced', async ({ page }) => {
  await page.route('**/api/metrics/host/pressure', r => r.fulfill({ json: strained({
    workers: [],
    top_unattributed: [
      { kind: 'detached', cpu_percent: 182, root_pid: 800, command: 'bash', n_processes: 3, rss_mb: 2400,
        top_pid: 802, top_command: 'rustc', top_cpu_percent: 181, top_nice: 10 },
      { kind: 'detached', cpu_percent: 150, root_pid: 900, command: 'bash', n_processes: 2, rss_mb: 300,
        top_pid: 901, top_command: '/usr/bin/ffmpeg', top_cpu_percent: 149, top_nice: 0 },
    ] }) }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).refreshHostPressure === 'function');
  await page.evaluate(() => (window as any).refreshHostPressure());
  await page.locator('#host-pressure-pill').click();
  const items = page.locator('#host-pressure-detail .host-fix-list > li');
  await expect(items).toHaveCount(2);
  await expect(items.nth(0)).toContainText('rustc (pid 802, under bash)');
  await expect(items.nth(0)).toContainText('already runs at low priority');
  await expect(items.nth(0).locator('.host-fix-cmd')).toHaveText('ps -o pid,ni,etime,args -p 802');
  await expect(items.nth(1)).toContainText('ffmpeg (pid 901, under bash)');
  await expect(items.nth(1).locator('.host-fix-cmd')).toHaveText('renice -n 10 -p 901');
  await expect(page.locator('#host-pressure-detail .host-fix-list')).not.toContainText('renice -n 10 -p 800');
});
