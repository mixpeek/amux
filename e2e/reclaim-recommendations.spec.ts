import { test, expect } from './fixtures';

// The Disk tab's recommendations (owner ask, 2026-10-18): findings grouped by
// the tool that owns them, a safe / review split, and a script you copy and run
// yourself. Nothing on this surface may write: every non-GET is refused here.
test.use({ serviceWorkers: 'block' });

const GB = 1024 ** 3;
const findings = [
  { id: 1, category: 'cache', bytes: 7.7 * GB, file_count: 9, path: '/Users/x/Library/pnpm/store', detail: 'pnpm', regenerable: true },
  { id: 2, category: 'cache', bytes: 6.9 * GB, file_count: 9, path: '/Users/x/.npm/_cacache', detail: 'npm', regenerable: true },
  { id: 3, category: 'build', bytes: 2 * GB, file_count: 9, path: '/Users/x/a/node_modules', detail: 'node_modules', regenerable: true },
  { id: 4, category: 'build', bytes: 1 * GB, file_count: 9, path: "/Users/x/it's b/node_modules", detail: 'node_modules', regenerable: true },
  { id: 5, category: 'build', bytes: 1 * GB, file_count: 9, path: '/Users/x/c/node_modules', detail: 'node_modules', regenerable: true },
  { id: 6, category: 'large', bytes: 4.4 * GB, file_count: 1, path: '/Users/x/.cache/huggingface/hub/blobs/aa', detail: 'model', regenerable: false },
  { id: 8, category: 'build', bytes: 0.2 * GB, file_count: 9, path: '/Users/x/infra/a/.terraform', detail: '.terraform', regenerable: true },
  { id: 9, category: 'build', bytes: 0.2 * GB, file_count: 9, path: '/Users/x/infra/b/.terraform', detail: '.terraform', regenerable: true },
  { id: 7, category: 'stale', bytes: 3 * GB, file_count: 1, path: '/Users/x/Documents/master.mp4', detail: 'media', regenerable: false },
  // Owned paths the server marks protected: a running worker's scratch and
  // git-tracked customer data. Large, regenerable-looking, and never selectable.
  { id: 10, category: 'build', bytes: 31 * GB, file_count: 9, path: '/Users/x/.amux/tmp/mxp-gs12', detail: 'tmp', regenerable: true,
    protected: { class: 'live_worker_scratch', reason: 'scratch space of running worker mxp-gs12' } },
  { id: 11, category: 'large', bytes: 17 * GB, file_count: 9, path: '/Users/x/Dev/mixpeek/customers/tubescience/parity', detail: 'data', regenerable: false,
    protected: { class: 'git_tracked', reason: '1747 git-tracked file(s) in /Users/x/Dev/mixpeek' } },
];

async function openDisk(page: any, mutations: string[], batches: any[] = [], onWrite?: (route: any, req: any) => Promise<any>) {
  await page.route('**/api/reclaim/**', async (route: any) => {
    const req = route.request();
    if (req.method() !== 'GET') {
      mutations.push(req.method() + ' ' + req.url());
      if (onWrite) return onWrite(route, req);
      return route.fulfill({ status: 409, json: { error: 'this fixture permits reads only' } });
    }
    const path = new URL(req.url()).pathname;
    if (path.endsWith('/scan/latest')) {
      return route.fulfill({ json: {
        scan: { id: 'recs', status: 'done', snapshot_count: 0, df_free: 27 * GB, df_total: 926 * GB, bytes_seen: 600 * GB,
          finished_at: Math.floor(Date.now() / 1000) - 5 * 86400 },
        findings, totals: [], skipped: [] } });
    }
    if (path.endsWith('/quarantine')) return route.fulfill({ json: { batches, items: [] } });
    if (path.includes('/tree')) return route.fulfill({ json: { root: null, children: [] } });
    throw new Error('Unexpected reclaim GET: ' + path);
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await page.evaluate(() => (window as any).switchView('disk'));
  await expect(page.locator('#reclaim-content .reclaim-recs')).toBeVisible();
}

test('the Disk tab splits findings into safe and review, and builds a script without running anything', async ({ page }, info) => {
  const mutations: string[] = [];
  await openDisk(page, mutations);
  const c = page.locator('#reclaim-content');

  // Headline: free, safe (pnpm + npm + node_modules = 18.6 GB), review (7.4 GB), not mapped (899 - 600).
  const cards = c.locator('.reclaim-summary .metrics-card');
  await expect(cards).toHaveCount(4);
  await expect(cards.nth(0)).toContainText('27');
  // The free figure is the scan's, so the card says when the scan ran.
  await expect(cards.nth(0)).toContainText('Free at last scan');
  await expect(cards.nth(0)).toContainText('scanned');
  await expect(cards.nth(0)).not.toContainText('right now');
  await expect(cards.nth(1)).toContainText('Safe to remove');
  await expect(cards.nth(1)).toContainText('4 items');
  await expect(cards.nth(2)).toContainText('2 items');
  await expect(cards.nth(3)).toContainText('Not mapped');

  const recs = c.locator('.reclaim-recs');
  await expect(recs).toContainText('pnpm store prune');
  await expect(recs).toContainText('npm cache clean --force');
  await expect(recs).toContainText('node_modules in 3 projects');
  await expect(recs).toContainText('.terraform in 2 projects');
  await expect(recs).toContainText('huggingface-cli delete-cache');
  const copy = recs.getByRole('button', { name: 'Copy script' });
  await expect(copy).toBeDisabled();

  await recs.getByRole('button', { name: 'Select safe' }).click();
  await expect(copy).toBeEnabled();
  await recs.locator('details.reclaim-script summary').click();
  const script = recs.locator('details.reclaim-script pre');
  await expect(script).toContainText('pnpm store prune');
  await expect(script).toContainText('npm cache clean --force');
  // Paths go through the quarantine API so the server's guards apply. The
  // script never carries an rm for a path (2026-10-20 incidents).
  await expect(script).toContainText('/api/reclaim/quarantine');
  await expect(script).toContainText('X-Amux-Session');
  await expect(script).toContainText('"/Users/x/a/node_modules"');
  await expect(script).toContainText('"/Users/x/it\'s b/node_modules"');
  await expect(script).not.toContainText('rm -rf');
  // A protected finding is never in the script, even after Select safe.
  await expect(script).not.toContainText('mxp-gs12');
  await expect(script).not.toContainText('huggingface');
  await expect(script).not.toContainText('master.mp4');

  // Protected findings: shown in their own group with the server's reason,
  // greyed in the table, checkbox disabled, and left out of "Select safe".
  await expect(recs.locator('.reclaim-chip.protected')).toBeVisible();
  await expect(recs.locator('.reclaim-protected', { hasText: 'mxp-gs12' })).toContainText('scratch space of running worker mxp-gs12');
  const protRow = c.locator('tr.reclaim-row-protected[data-protected="git_tracked"]');
  await expect(protRow).toContainText('1747 git-tracked file(s)');
  await expect(protRow.locator('input[type=checkbox]')).toBeDisabled();
  await expect(c.locator('tr.reclaim-row-protected[data-protected="live_worker_scratch"] input')).toBeDisabled();
  // A direct toggle call cannot select one either.
  await page.evaluate(() => (window as any)._reclaimToggle('/Users/x/.amux/tmp/mxp-gs12'));
  // `_reclaimSel` is a script-level let, not a window property.
  expect(await page.evaluate("_reclaimSel.has('/Users/x/.amux/tmp/mxp-gs12')")).toBe(false);

  // A review item, once ticked, is in the script commented out.
  await recs.locator('.reclaim-rec', { hasText: 'Hugging Face models' }).locator('input').check();
  await recs.locator('details.reclaim-script summary').click();
  await expect(recs.locator('details.reclaim-script pre')).toContainText('# huggingface-cli delete-cache');

  // State survives a re-render (an SSE refresh calls the same renderer).
  await page.evaluate(() => (window as any)._reclaimRender());
  await expect(c.locator('.reclaim-rec', { hasText: 'pnpm store' }).locator('input')).toBeChecked();
  await expect(c.locator('.reclaim-rec', { hasText: 'Hugging Face models' }).locator('input')).toBeChecked();
  await expect(c.locator('.reclaim-rec', { hasText: 'master.mp4' }).locator('input')).not.toBeChecked();

  // Touch targets and fit at whatever width this project runs.
  const vw = page.viewportSize()!.width;
  for (const el of [recs, copy, c.locator('.reclaim-rec').first()]) {
    const b = (await el.boundingBox())!;
    expect(b.x).toBeGreaterThanOrEqual(0);
    expect(b.x + b.width).toBeLessThanOrEqual(vw + 1);
  }
  expect((await copy.boundingBox())!.height).toBeGreaterThanOrEqual(vw < 600 ? 44 : 20);
  expect((await c.locator('.reclaim-rec').first().boundingBox())!.height).toBeGreaterThanOrEqual(44);

  const shot = info.outputPath('reclaim-recommendations.png');
  await page.screenshot({ path: shot, fullPage: true });
  await info.attach('reclaim-recommendations', { path: shot, contentType: 'image/png' });
  expect(mutations).toEqual([]);
});

test('"Purge batches older than 7 days" purges only old staged batches, each with its own confirm, and totals the server deltas', async ({ page }) => {
  const now = Math.floor(Date.now() / 1000);
  const batches = [
    { id: 'OLD1', created_at: now - 9 * 86400, status: 'staged', item_count: 2, bytes: 3 * GB, df_free_before: 1, df_free_after: 1 },
    { id: 'OLD2', created_at: now - 30 * 86400, status: 'staged', item_count: 1, bytes: 1 * GB, df_free_before: 1, df_free_after: 1 },
    { id: 'NEW1', created_at: now - 2 * 86400, status: 'staged', item_count: 1, bytes: 5 * GB, df_free_before: 1, df_free_after: 1 },
    { id: 'GONE', created_at: now - 40 * 86400, status: 'purged', item_count: 1, bytes: 2 * GB, df_free_before: 1, df_free_after: 1 },
  ];
  const mutations: string[] = [];
  await openDisk(page, mutations, batches, async (route: any, req: any) => {
    const u = new URL(req.url());
    const id = u.pathname.split('/').pop();
    if (req.method() === 'DELETE' && u.searchParams.get('confirm') === id) {
      return route.fulfill({ json: { ok: true, batch_id: id, actually_freed: id === 'OLD1' ? 2 * GB : 1 * GB, note: '' } });
    }
    return route.fulfill({ status: 400, json: { error: 'purge requires ?confirm=<batch_id>' } });
  });
  page.on('dialog', (d: any) => d.type() === 'prompt' ? d.accept('PURGE') : d.accept());
  const btn = page.locator('#reclaim-purge-old');
  await expect(btn).toContainText('Purge 2 batches older than 7 days');
  await btn.click();
  // The total is the sum of the two server deltas (2 GB + 1 GB), not staged bytes (4 GB).
  await expect(page.locator('#toast')).toContainText('Purged 2 of 2 batches');
  await expect(page.locator('#toast')).toContainText('3');
  const deletes = mutations.filter(m => m.startsWith('DELETE'));
  expect(deletes).toHaveLength(2);
  expect(deletes.some(m => m.includes('/quarantine/OLD1?confirm=OLD1'))).toBe(true);
  expect(deletes.some(m => m.includes('/quarantine/OLD2?confirm=OLD2'))).toBe(true);
  expect(mutations.join(' ')).not.toContain('NEW1');
  expect(mutations.join(' ')).not.toContain('GONE');
});
