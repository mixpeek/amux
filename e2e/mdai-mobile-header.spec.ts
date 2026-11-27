// The .mdai node view on a phone (Ethan, 2026-10-14: "fix and test this on
// mobile ... all of the real estate at the top and everything is clean"). The
// header and its button row used to take 129px at 390px wide, the tab counts ran
// into their icons, and the preview was a card inside a card. Phone widths only;
// a wide screen keeps its layout. Runs are served from a stub, so no model runs.
import { test, expect, allowUnusedRoute } from './fixtures';
import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const runs = [
  { id: 2, ts: Date.now() / 1000 - 3600, model: 'haiku', dur_ms: 182700, status: 'ok', output: 'The newest run.', tokens_in: null, tokens_out: null, cost_usd: null },
  { id: 1, ts: Date.now() / 1000 - 7200, model: 'haiku', dur_ms: 99800, status: 'ok', output: 'An older run.', tokens_in: null, tokens_out: null, cost_usd: null },
];

test('phone header is one row, tabs fit, the preview is one card, a run is one line', async ({ page }, info) => {
  const vw = page.viewportSize()!.width;
  test.skip(vw > 600, 'phone layout only');
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route('**/api/mdai/history**', r => r.fulfill({ json: { runs, total: 90 } }));
  await page.route('**/api/mdai/run', r => r.fulfill({ status: 503, json: { error: 'no model in this test' } }));
  allowUnusedRoute(page, '**/api/mdai/run'); // expected zero: autorun is off
  const dir = await mkdtemp(join(tmpdir(), 'mdai-mobile-'));
  const file = join(dir, 'Priorities.mdai');
  await writeFile(file, '---\nmodel: "opus"\nsources: []\n---\n# Priorities\n\nBody.\n');
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openMdaiNode === 'function');
  await page.evaluate(p => (window as any).openMdaiNode(p, { autorun: false }), file);
  await page.evaluate(() => (window as any)._mdaiSetTab('preview'));
  await expect(page.locator('#mdai-title')).toHaveText('Priorities.mdai');

  const box = async (s: string) => (await page.locator(s).boundingBox())!;
  const head = await box('#mdai-overlay .mdai-dhead');
  expect(head.height, 'the header is one compact row').toBeLessThanOrEqual(72);
  // Back, title and actions share the first row.
  const back = await box('#mdai-overlay .mdai-back');
  const run = await box('#mdai-run-btn');
  expect(Math.abs(run.y - back.y), 'Run sits on the back button row').toBeLessThanOrEqual(4);
  expect(back.width).toBeGreaterThanOrEqual(44);
  expect(run.height).toBeGreaterThanOrEqual(44);
  // The title is not cut for a short name.
  expect(await page.locator('#mdai-title').evaluate(e => e.scrollWidth <= e.clientWidth)).toBe(true);
  // "Saved" is a state: hidden while clean.
  await expect(page.locator('#mdai-save-btn')).toBeHidden();

  // Tabs fit without sideways scrolling, and a count is spaced off its label.
  const tabs = page.locator('#mdai-overlay .mdai-tabs');
  expect(await tabs.evaluate(e => e.scrollWidth - e.clientWidth)).toBeLessThanOrEqual(1);
  const runsTab = page.locator('#mdai-overlay .mdai-tab[data-tab="runs"]');
  await expect(runsTab.locator('.mdai-tab-n')).toHaveText('90');
  expect(await runsTab.locator('.mdai-tab-n').evaluate(e => parseFloat(getComputedStyle(e).marginLeft))).toBeGreaterThanOrEqual(4);
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(1);

  // The preview: run meta and the older/newer arrows on one line, one card.
  const meta = await box('#mdai-body .mdai-prev-head');
  expect(meta.height, 'meta and arrows share a line').toBeLessThanOrEqual(52);
  expect(await page.locator('#mdai-body').evaluate(e => getComputedStyle(e).borderTopWidth)).toBe('0px');
  // No row of three dashes when the run reported no tokens or cost.
  await expect(page.locator('#mdai-body .mdai-stats')).toHaveCount(0);
  await page.screenshot({ path: info.outputPath('mdai-mobile-preview.png') });

  // Runs: one line per run, no labels, no empty "In/Out/Cost: —".
  await page.evaluate(() => (window as any)._mdaiSetTab('runs'));
  const row = page.locator('#mdai-body .mdai-run:not(.mdai-run-h)').first();
  await expect(row).toBeVisible();
  await expect(page.locator('#mdai-body .mdai-run-h')).toBeHidden();
  expect((await row.boundingBox())!.height, 'one run is one short row').toBeLessThanOrEqual(64);
  await expect(row.locator('span[data-l="Cost"]')).toBeHidden();
  await page.screenshot({ path: info.outputPath('mdai-mobile-runs.png') });
});
