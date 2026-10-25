// Ethan, 2026-10-20: "make sure i can actually do disk cleanup from the disk
// cleanup tab in amux, also there should be easy ways of cleaning up host in
// safe ways". The reclaim API is stubbed: a real scan or quarantine here would
// walk and move the developer's own home directory.
import {test, expect, allowUnusedRoute} from './fixtures';

const GB = 1024 ** 3;

async function stub(page: any, opts: {staged: number}) {
  const calls: string[] = [];
  const scan = {
    scan: {id: 'scan-1', status: 'done', finished_at: Math.floor(Date.now() / 1000) - 600,
      df_free: 4 * GB, df_total: 1800 * GB, dirs_walked: 10, files_walked: 10, bytes_seen: 1},
    findings: [
      {category: 'build', path: '/Users/x/proj/node_modules', bytes: 3 * GB, file_count: 9, regenerable: true, detail: 'node_modules', protected: null},
      {category: 'large', path: '/Users/x/video.mp4', bytes: 5 * GB, file_count: 1, regenerable: false, detail: 'video', protected: null},
    ],
    totals: [], skipped: [], ownership: {measured: true, n_considered: 2},
    volume_now: {measured: true, df_free: 280 * GB, df_total: 1800 * GB, staged_bytes: opts.staged, staged_batches: opts.staged ? 1 : 0},
  };
  const quar = {root: '/Users/x/.amux/quarantine', items: [],
    batches: opts.staged ? [{id: 'B1', status: 'staged', item_count: 75, bytes: opts.staged, created_at: 1}] : [],
    volume_now: scan.volume_now};
  await page.route(/\/api\/reclaim\/scan\/latest/, (r: any) => r.fulfill({json: scan}));
  await page.route(/\/api\/reclaim\/tree\//, (r: any) => r.fulfill({json: {nodes: []}}));
  await page.route(/\/api\/reclaim\/quarantine(\?.*)?$/, (r: any) => {
    if (r.request().method() === 'POST') {
      calls.push('POST quarantine ' + r.request().postData());
      return r.fulfill({json: {batch_id: 'B2', moved: 1, refused: [], bytes: 3 * GB}});
    }
    return r.fulfill({json: quar});
  });
  await page.route(/\/api\/reclaim\/quarantine\/[^/?]+\?confirm=/, (r: any) => {
    calls.push(r.request().method() + ' ' + new URL(r.request().url()).pathname);
    return r.fulfill({json: {actually_freed: 3 * GB}});
  });
  await page.route(/\/api\/schedules(\?.*)?$/, (r: any) => r.fulfill({json: [
    {id: 'SCHED-465', command: 'bash scripts/mac-cleanup-tick.sh', enabled: 1}]}));
  let ran = false;
  await page.route(/\/api\/schedules\/SCHED-465\/run/, (r: any) => { ran = true; calls.push('RUN SCHED-465'); return r.fulfill({json: {ok: true}}); });
  await page.route(/\/api\/schedules\/runs/, (r: any) => r.fulfill({json: ran
    ? [{id: 9, schedule_id: 'SCHED-465', exit_code: 0, output_tail: 'mac-cleanup: claude tmp: removed 2 (40M)\nmac-cleanup: done elapsed=300s'}]
    : []}));
  // One stub set serves every test; each test asserts the calls it depends on
  // through `calls`, so a stub another test needs may go unused here.
  for (const re of [/\/api\/reclaim\/tree\//, /\/api\/reclaim\/quarantine\/[^/?]+\?confirm=/,
    /\/api\/schedules(\?.*)?$/, /\/api\/schedules\/SCHED-465\/run/, /\/api\/schedules\/runs/,
    /\/api\/reclaim\/quarantine(\?.*)?$/]) allowUnusedRoute(page, re);
  return calls;
}

async function openDisk(page: any) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  page.on('dialog', (d: any) => d.type() === 'prompt' ? d.accept('PURGE') : d.accept());
  await page.goto('/#view=disk', {waitUntil: 'domcontentloaded'});
  await expect(page.locator('#reclaim-actions')).toBeVisible();
}

test('the header shows free space now, and staged quarantine is up top with one delete', async ({page}) => {
  const calls = await stub(page, {staged: 105 * GB});
  await openDisk(page);
  await expect(page.locator('#reclaim-free-now')).toContainText('280');
  await expect(page.locator('#reclaim-free-now')).toContainText('now');
  await expect(page.locator('#reclaim-staged-banner')).toContainText('105');
  await page.locator('#reclaim-purge-all').click();
  await expect.poll(() => calls).toContain('DELETE /api/reclaim/quarantine/B1');
});

test('Clean rebuildable files quarantines only the rebuildable item, then deletes that batch', async ({page}) => {
  const calls = await stub(page, {staged: 0});
  await openDisk(page);
  await expect(page.locator('#reclaim-staged-banner')).toHaveCount(0);
  await expect(page.locator('#reclaim-clean-safe')).toContainText('Clean 1 item');
  await page.locator('#reclaim-clean-safe').click();
  await expect.poll(() => calls.find(c => c.startsWith('POST'))).toContain('node_modules');
  expect(calls.find(c => c.startsWith('POST'))).not.toContain('video.mp4');
  await expect.poll(() => calls).toContain('DELETE /api/reclaim/quarantine/B2');
});

test('Run host cleanup now runs the host check and shows what it did', async ({page}) => {
  test.setTimeout(60_000);
  const calls = await stub(page, {staged: 0});
  await openDisk(page);
  await page.locator('#reclaim-host-cleanup').click();
  await expect.poll(() => calls).toContain('RUN SCHED-465');
  await expect(page.locator('#reclaim-host-result')).toContainText('removed 2', {timeout: 30_000});
});
