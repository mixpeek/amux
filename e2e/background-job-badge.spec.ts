import { test, expect, Page } from './fixtures';

// MSG-99658 (2026-10-28): tubescience-parity read idle for 16 minutes while
// `bash etl_rest2.sh` had been running for 1h02m. The worker card's badge now
// names the job and its age, and a watch loop alone does not count as a job.
const base = { running: true, pinned: false, dir: '/tmp/', provider: 'claude', tags: [], status: 'active', activity: 'background_shell', turn_state: 'completed' };
const workers = [
  { ...base, name: 'etl-lane', last_activity: 200, idle_reason: 'own_job_running',
    background_jobs: [{ pid: '102', label: 'etl_rest2.sh', elapsed_s: 3725, waiting: false },
                      { pid: '103', label: 'until grep -q done log; do sleep 15; done', elapsed_s: 900, waiting: true },
                      { pid: '104', label: 'cargo build', elapsed_s: 300, waiting: false }] },
  { ...base, name: 'plain-lane', last_activity: 100 },
];

async function prepare(page: Page) {
  await page.addInitScript(w => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.setItem('amux_sessions_cache', JSON.stringify(w));
  }, workers);
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: workers }));
  await page.goto('/');
  await expect(page.locator('#cards .card[data-session]')).toHaveCount(workers.length);
}

// The badge is a status name with the job's age; the job's command line
// (a label like "cd /Users/... && VAR=... python3 ...") is only in the tooltip
// (Ethan, 2026-10-28: "these status names are wrong").
test('a background job shows as a status with its age, and the tooltip names the job', async ({ page }) => {
  await prepare(page);
  const badge = page.locator('#cards .card[data-session="etl-lane"] .status-badge.background');
  await expect(badge).toHaveText('background job 1h02m +1');
  await expect(badge).not.toContainText('etl_rest2');
  const tip = await badge.getAttribute('title');
  expect(tip).toContain('etl_rest2.sh (1h02m)');
  expect(tip).toContain('cargo build (5m)');
  expect(tip).not.toContain('until grep');
  // A lane whose row carries no job list keeps the old wording.
  await expect(page.locator('#cards .card[data-session="plain-lane"] .status-badge.background')).toHaveText('background job');
});
